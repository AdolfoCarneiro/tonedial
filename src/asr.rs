use std::path::Path;
use std::sync::{Arc, Mutex};

use anyhow::{Context, Result};
use whisper_rs::{
    FullParams, SamplingStrategy, WhisperContext, WhisperContextParameters, WhisperVadContext,
    WhisperVadContextParams, WhisperVadParams,
};

use crate::transcript::{Word, dedupe_repeats};

#[derive(PartialEq, Debug, Clone, Copy)]
pub enum Device {
    Gpu,
    Cpu,
}

pub struct LoadedModel {
    pub ctx: WhisperContext,
    pub device: Device,
}

/// Escolhe o índice da GPU Vulkan: `TONEDIAL_GPU_DEVICE` se definido; senão a primeira que não
/// seja Intel (o ggml lista a iGPU Intel antes da RTX e o índice 0 cairia nela); senão 0.
fn pick_gpu_device() -> i32 {
    if let Some(n) = std::env::var("TONEDIAL_GPU_DEVICE")
        .ok()
        .and_then(|v| v.parse().ok())
    {
        return n;
    }
    pick_device_index(
        &whisper_rs::vulkan::list_devices()
            .iter()
            .map(|d| d.name.as_str())
            .collect::<Vec<_>>(),
    )
}

fn pick_device_index(names: &[&str]) -> i32 {
    names
        .iter()
        .position(|n| !n.to_lowercase().contains("intel"))
        .unwrap_or(0) as i32
}

fn load_context(path: &Path, use_gpu: bool) -> Result<WhisperContext> {
    let path_str = path
        .to_str()
        .with_context(|| format!("caminho do modelo inválido: {}", path.display()))?;
    let mut params = WhisperContextParameters::default();
    params.use_gpu(use_gpu);
    if use_gpu {
        params.gpu_device = pick_gpu_device();
    }
    WhisperContext::new_with_params(path_str, params)
        .map_err(|e| anyhow::anyhow!("falha ao carregar o modelo {}: {e}", path.display()))
}

/// Folga fixa de VRAM além do tamanho do modelo (buffers de computação do beam 5, KV cache,
/// estado do VAD). Conservadora de propósito: um OOM do Vulkan durante `full` provavelmente
/// aborta o processo (exceção do ggml-vulkan atravessando a fronteira C) e o fallback não pega.
const GPU_MARGIN_BYTES: u64 = 1 << 30;

/// Decide se vale tentar a GPU: VRAM livre precisa cobrir o modelo mais a folga.
fn should_use_gpu(free_bytes: u64, model_bytes: u64) -> bool {
    free_bytes >= model_bytes.saturating_add(GPU_MARGIN_BYTES)
}

/// VRAM livre (bytes) da GPU Vulkan de índice `device`, se o ggml a enxerga.
fn free_vram(device: i32) -> Option<u64> {
    whisper_rs::vulkan::list_devices()
        .into_iter()
        .find(|d| d.id == device)
        .map(|d| d.vram.free as u64)
}

/// Carrega o modelo, preferindo a GPU (Vulkan) e caindo para CPU se falhar ou se a VRAM
/// livre não comporta o modelo (decidido antes de carregar, ver `should_use_gpu`).
pub fn load_model(path: &Path) -> Result<LoadedModel> {
    let model_bytes = std::fs::metadata(path)
        .with_context(|| format!("modelo não encontrado: {}", path.display()))?
        .len();
    if let Some(free) = free_vram(pick_gpu_device())
        && !should_use_gpu(free, model_bytes)
    {
        return Ok(LoadedModel {
            ctx: load_context(path, false)?,
            device: Device::Cpu,
        });
    }
    load_model_with(path, load_context)
}

/// Igual a `load_model`, com a função de carga injetável (para teste).
pub fn load_model_with<F>(path: &Path, load: F) -> Result<LoadedModel>
where
    F: Fn(&Path, bool) -> Result<WhisperContext>,
{
    match load(path, true) {
        Ok(ctx) => Ok(LoadedModel {
            ctx,
            device: Device::Gpu,
        }),
        Err(gpu_err) => {
            let ctx = load(path, false)
                .with_context(|| format!("falha na GPU ({gpu_err:#}) e também na CPU"))?;
            Ok(LoadedModel {
                ctx,
                device: Device::Cpu,
            })
        }
    }
}

struct Tok {
    bytes: Vec<u8>,
    t0: i64,
    t1: i64,
}

/// Junta tokens em palavras: token que começa com espaço abre palavra nova,
/// tokens especiais (`[_`...) são descartados. Timestamps em centésimos de segundo (×10 → ms).
fn words_from_tokens(tokens: &[Tok]) -> Vec<Word> {
    fn flush(cur: &mut Option<(Vec<u8>, u64, u64)>, words: &mut Vec<Word>) {
        if let Some((bytes, start_ms, end_ms)) = cur.take() {
            let text = String::from_utf8_lossy(&bytes).trim().to_string();
            if !text.is_empty() {
                words.push(Word {
                    start_ms,
                    end_ms: end_ms.max(start_ms),
                    text,
                });
            }
        }
    }
    let mut words = Vec::new();
    let mut cur: Option<(Vec<u8>, u64, u64)> = None;
    for t in tokens {
        if is_special(&t.bytes) {
            continue;
        }
        let start = t.t0.max(0) as u64 * 10;
        let end = t.t1.max(0) as u64 * 10;
        match cur.as_mut() {
            Some((bytes, _, e)) if !t.bytes.starts_with(b" ") => {
                bytes.extend_from_slice(&t.bytes);
                *e = end;
            }
            _ => {
                flush(&mut cur, &mut words);
                cur = Some((t.bytes.clone(), start, end));
            }
        }
    }
    flush(&mut cur, &mut words);
    words
}

/// VAD feito por nós. O `whisper_full_with_state` (o que `WhisperState::full` chama) ignora
/// `params.vad`: o VAD do whisper.cpp só roda em `whisper_full` (contexto, não estado), então
/// ligar `enable_vad` no whisper-rs é um no-op silencioso. Por isso rodamos o VAD aqui
/// (`WhisperVadContext`), montamos o áudio só com as falas (com 0,1 s de silêncio entre elas,
/// como o whisper.cpp faria) e mapeamos os tempos dos tokens de volta ao áudio original.
///
/// Um trecho de fala copiado: `len_cs` centésimos que começam em `orig_start_cs` no áudio
/// original e em `proc_start_cs` no áudio comprimido (mapa 1:1 dentro do trecho).
#[derive(Debug, Clone, Copy, PartialEq)]
struct VadPiece {
    proc_start_cs: i64,
    orig_start_cs: i64,
    len_cs: i64,
}

const SAMPLES_PER_CS: usize = 160; // 16 kHz
const VAD_SILENCE_CS: i64 = 10; // 0,1 s entre trechos
const VAD_OVERLAP_CS: i64 = 10; // cauda extra em todos os trechos menos o último (igual ao whisper.cpp)

/// Monta os trechos a partir dos segmentos de fala do VAD (início, fim em centésimos).
fn plan_vad(segments: &[(f32, f32)], n_samples: usize) -> Vec<VadPiece> {
    let total_cs = (n_samples / SAMPLES_PER_CS) as i64;
    let mut pieces: Vec<VadPiece> = Vec::new();
    let mut proc = 0i64;
    for (i, &(s, e)) in segments.iter().enumerate() {
        let start = (s.round() as i64).clamp(0, total_cs);
        let mut end = e.round() as i64;
        if i + 1 < segments.len() {
            end += VAD_OVERLAP_CS;
        }
        let end = end.clamp(0, total_cs);
        if end <= start {
            continue;
        }
        if !pieces.is_empty() {
            proc += VAD_SILENCE_CS;
        }
        pieces.push(VadPiece {
            proc_start_cs: proc,
            orig_start_cs: start,
            len_cs: end - start,
        });
        proc += end - start;
    }
    pieces
}

/// Concatena os trechos de fala com silêncio entre eles.
fn compress_samples(samples: &[f32], pieces: &[VadPiece]) -> Vec<f32> {
    let mut out = Vec::new();
    for (i, p) in pieces.iter().enumerate() {
        if i > 0 {
            out.resize(out.len() + VAD_SILENCE_CS as usize * SAMPLES_PER_CS, 0.0);
        }
        let a = p.orig_start_cs as usize * SAMPLES_PER_CS;
        let b = (a + p.len_cs as usize * SAMPLES_PER_CS).min(samples.len());
        out.extend_from_slice(&samples[a.min(b)..b]);
    }
    out
}

/// Tempo (centésimos) do áudio comprimido -> áudio original. Dentro de um trecho é o deslocamento
/// do trecho. No silêncio inserido entre trechos, um começo de palavra vai para o início do
/// próximo trecho e um fim de palavra para o fim do trecho anterior.
fn map_to_original(pieces: &[VadPiece], t_cs: i64, is_end: bool) -> i64 {
    let Some(first) = pieces.first() else {
        return t_cs;
    };
    if t_cs < first.proc_start_cs {
        return first.orig_start_cs;
    }
    for (i, p) in pieces.iter().enumerate() {
        let p_end = p.proc_start_cs + p.len_cs;
        if t_cs <= p_end && (t_cs < p_end || is_end || i + 1 == pieces.len()) {
            return p.orig_start_cs + (t_cs - p.proc_start_cs);
        }
        if let Some(next) = pieces.get(i + 1)
            && t_cs < next.proc_start_cs
        {
            return if is_end {
                p.orig_start_cs + p.len_cs
            } else {
                next.orig_start_cs
            };
        }
    }
    let last = pieces[pieces.len() - 1];
    last.orig_start_cs + last.len_cs
}

/// Roda o VAD e devolve os segmentos de fala (início, fim) em centésimos do áudio original.
fn detect_speech(vad_model: &Path, samples: &[f32]) -> Result<Vec<(f32, f32)>> {
    let vad_str = vad_model
        .to_str()
        .with_context(|| format!("caminho do modelo VAD inválido: {}", vad_model.display()))?;
    let mut vctx = WhisperVadContext::new(vad_str, WhisperVadContextParams::new())
        .map_err(|e| anyhow::anyhow!("falha ao carregar o modelo VAD: {e}"))?;
    let segs = vctx
        .segments_from_samples(WhisperVadParams::new(), samples)
        .map_err(|e| anyhow::anyhow!("falha ao rodar o VAD: {e}"))?;
    Ok(segs.map(|s| (s.start, s.end)).collect())
}

fn is_special(bytes: &[u8]) -> bool {
    bytes.starts_with(b"[_") || bytes.starts_with(b"<|")
}

/// Erro vindo do backend (`create_state`/`full`): único tipo que dispara o fallback para CPU.
#[derive(Debug)]
struct BackendFailure(String);

impl std::fmt::Display for BackendFailure {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(&self.0)
    }
}

impl std::error::Error for BackendFailure {}

/// `samples` é o áudio já comprimido pelo VAD; `pieces` mapeia os tempos de volta ao original.
fn run_once(
    ctx: &WhisperContext,
    samples: &[f32],
    pieces: &[VadPiece],
    progress: impl FnMut(i32) + 'static,
) -> Result<Vec<Word>> {
    let mut state = ctx.create_state().map_err(|e| {
        anyhow::Error::new(BackendFailure(format!(
            "falha ao criar o estado do whisper: {e}"
        )))
    })?;

    let mut params = FullParams::new(SamplingStrategy::BeamSearch {
        beam_size: 5,
        patience: -1.0,
    });
    params.set_language(Some("pt"));
    params.set_translate(false);
    params.set_no_context(true);
    params.set_token_timestamps(true);
    params.set_suppress_blank(true);
    params.set_suppress_nst(true);
    params.set_print_special(false);
    params.set_print_progress(false);
    params.set_print_realtime(false);
    params.set_print_timestamps(false);
    params.set_progress_callback_safe(progress);

    state
        .full(params, samples)
        .map_err(|e| anyhow::Error::new(BackendFailure(format!("falha na transcrição: {e}"))))?;

    // Uma palavra nunca atravessa segmentos: cada segmento é montado sozinho.
    let mut words = Vec::new();
    for seg in state.as_iter() {
        let mut tokens = Vec::new();
        for i in 0..seg.n_tokens() {
            let Some(tok) = seg.get_token(i) else { continue };
            let data = tok.token_data();
            let bytes = tok
                .to_bytes()
                .map_err(|e| anyhow::anyhow!("token inválido: {e}"))?
                .to_vec();
            tokens.push(Tok {
                bytes,
                t0: map_to_original(pieces, data.t0, false),
                t1: map_to_original(pieces, data.t1, true),
            });
        }
        words.extend(words_from_tokens(&tokens));
    }
    Ok(dedupe_repeats(words))
}

/// Executa `run` no contexto atual; se falhar na GPU com um `BackendFailure`, recarrega em CPU
/// via `reload_cpu` (trocando o contexto em `model`) e tenta uma vez mais. Outros erros
/// (entrada inválida etc.) são devolvidos como estão, sem rebaixar o modelo. Se a CPU também
/// falhar, o erro da GPU vai encadeado no erro final.
fn run_with_cpu_fallback(
    model: &mut LoadedModel,
    run: impl Fn(&WhisperContext) -> Result<Vec<Word>>,
    reload_cpu: impl FnOnce() -> Result<WhisperContext>,
) -> Result<Vec<Word>> {
    match run(&model.ctx) {
        Err(gpu_err)
            if model.device == Device::Gpu && gpu_err.downcast_ref::<BackendFailure>().is_some() =>
        {
            let ctx = reload_cpu()
                .with_context(|| format!("falha na GPU ({gpu_err:#}) e ao recarregar na CPU"))?;
            model.ctx = ctx;
            model.device = Device::Cpu;
            run(&model.ctx).with_context(|| format!("falha na GPU ({gpu_err:#}) e depois na CPU"))
        }
        other => other,
    }
}

fn validate_inputs(model_path: &Path, vad_model: &Path) -> Result<()> {
    anyhow::ensure!(
        model_path.is_file(),
        "modelo não encontrado: {}",
        model_path.display()
    );
    anyhow::ensure!(
        vad_model.is_file(),
        "modelo VAD não encontrado: {}",
        vad_model.display()
    );
    anyhow::ensure!(
        vad_model.to_str().is_some(),
        "caminho do modelo VAD inválido: {}",
        vad_model.display()
    );
    Ok(())
}

/// Transcreve amostras 16 kHz mono. `on_progress` recebe 0.0..=1.0.
/// Se falhar na GPU, recarrega o modelo em CPU (`model.device` passa a `Cpu`) e tenta de novo.
///
/// Atenção: o whisper-rs vaza o closure de progresso (nunca o dropa), então `on_progress`
/// não pode depender de ser liberado, por exemplo para fechar um `Sender`.
pub fn transcribe(
    model: &mut LoadedModel,
    model_path: &Path,
    vad_model: &Path,
    samples: &[f32],
    on_progress: impl FnMut(f32) + Send + 'static,
) -> Result<Vec<Word>> {
    transcribe_with(model, model_path, vad_model, samples, on_progress, || {
        load_context(model_path, false)
    })
}

fn transcribe_with(
    model: &mut LoadedModel,
    model_path: &Path,
    vad_model: &Path,
    samples: &[f32],
    on_progress: impl FnMut(f32) + Send + 'static,
    reload_cpu: impl FnOnce() -> Result<WhisperContext>,
) -> Result<Vec<Word>> {
    validate_inputs(model_path, vad_model)?;
    let speech = detect_speech(vad_model, samples)?;
    let pieces = plan_vad(&speech, samples.len());
    if pieces.is_empty() {
        // Sem fala detectada: nada a transcrever (e nada para o whisper alucinar).
        return Ok(Vec::new());
    }
    let compressed = compress_samples(samples, &pieces);
    let cb = Arc::new(Mutex::new(on_progress));
    run_with_cpu_fallback(
        model,
        |ctx| {
            let cb = Arc::clone(&cb);
            run_once(ctx, &compressed, &pieces, move |p| {
                if let Ok(mut f) = cb.lock() {
                    f((p as f32 / 100.0).clamp(0.0, 1.0));
                }
            })
        },
        reload_cpu,
    )
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::cell::Cell;
    use std::path::PathBuf;
    use std::process::Command;

    fn test_models_dir() -> PathBuf {
        PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("target/test-models")
    }

    /// Baixa (via curl) para target/test-models/. `None` se não houver rede.
    fn fetch(name: &str, url: &str) -> Option<PathBuf> {
        static DOWNLOAD_LOCK: Mutex<()> = Mutex::new(());
        let _guard = DOWNLOAD_LOCK.lock().unwrap_or_else(|e| e.into_inner());
        let dir = test_models_dir();
        std::fs::create_dir_all(&dir).ok()?;
        let dest = dir.join(name);
        if dest.exists() {
            return Some(dest);
        }
        let tmp = dir.join(format!("{name}.part"));
        let ok = Command::new("curl")
            .args(["-fsSL", "--retry", "2", "-o"])
            .arg(&tmp)
            .arg(url)
            .status()
            .map(|s| s.success())
            .unwrap_or(false);
        if !ok {
            let _ = std::fs::remove_file(&tmp);
            return None;
        }
        std::fs::rename(&tmp, &dest).ok()?;
        Some(dest)
    }

    fn tiny_model() -> Option<PathBuf> {
        fetch(
            "ggml-tiny.bin",
            "https://huggingface.co/ggerganov/whisper.cpp/resolve/main/ggml-tiny.bin",
        )
    }

    fn vad_model() -> Option<PathBuf> {
        fetch(
            crate::models::VAD_MODEL_FILE,
            "https://huggingface.co/ggml-org/whisper-vad/resolve/main/ggml-silero-v5.1.2.bin",
        )
    }

    fn tok(s: &str, t0: i64, t1: i64) -> Tok {
        Tok {
            bytes: s.as_bytes().to_vec(),
            t0,
            t1,
        }
    }

    #[test]
    fn picks_discrete_gpu_over_intel_igpu() {
        assert_eq!(
            pick_device_index(&["Intel(R) Graphics (ADL GT2)", "NVIDIA GeForce RTX 2050"]),
            1
        );
        assert_eq!(pick_device_index(&["Intel(R) Graphics (ADL GT2)"]), 0);
        assert_eq!(pick_device_index(&[]), 0);
    }

    #[test]
    fn words_are_assembled_from_tokens() {
        let tokens = vec![
            tok("[_BEG_]", 0, 0),
            tok(" Ol", 10, 12),
            tok("á", 12, 20),
            tok(" mundo", 25, 40),
            tok(",", 40, 41),
            tok("[_TT_500]", 50, 50),
        ];
        let words = words_from_tokens(&tokens);
        assert_eq!(
            words,
            vec![
                Word { start_ms: 100, end_ms: 200, text: "Olá".into() },
                Word { start_ms: 250, end_ms: 410, text: "mundo,".into() },
            ]
        );
    }

    #[test]
    fn punctuation_token_merges_into_previous_word() {
        let tokens = vec![tok(" Bom", 0, 1), tok(",", 1, 2), tok(" vocês", 2, 3)];
        let texts: Vec<String> = words_from_tokens(&tokens).into_iter().map(|w| w.text).collect();
        assert_eq!(texts, vec!["Bom,", "vocês"]);
    }

    #[test]
    fn split_multibyte_token_is_reassembled() {
        let bytes = "ã".as_bytes();
        let tokens = vec![
            Tok { bytes: b" n".to_vec(), t0: 0, t1: 1 },
            Tok { bytes: bytes[..1].to_vec(), t0: 1, t1: 2 },
            Tok { bytes: bytes[1..].to_vec(), t0: 2, t1: 3 },
            tok("o", 3, 4),
        ];
        let words = words_from_tokens(&tokens);
        assert_eq!(words.len(), 1);
        assert_eq!(words[0].text, "não");
    }

    #[test]
    fn falls_back_to_cpu_when_gpu_load_fails() {
        let Some(tiny) = tiny_model() else {
            eprintln!("sem rede: pulando falls_back_to_cpu_when_gpu_load_fails");
            return;
        };
        let calls = Cell::new(0);
        let model = load_model_with(&tiny, |p, use_gpu| {
            calls.set(calls.get() + 1);
            if use_gpu {
                anyhow::bail!("sem vulkan");
            }
            load_context(p, false)
        })
        .unwrap();
        assert_eq!(model.device, Device::Cpu);
        assert_eq!(calls.get(), 2);
    }

    #[test]
    fn run_failure_on_gpu_reloads_on_cpu_and_retries_once() {
        let Some(tiny) = tiny_model() else {
            eprintln!("sem rede: pulando run_failure_on_gpu_reloads_on_cpu_and_retries_once");
            return;
        };
        // Força device=Gpu no wrapper (o contexto em si é CPU) para exercitar o caminho de retry.
        let mut model = LoadedModel {
            ctx: load_context(&tiny, false).unwrap(),
            device: Device::Gpu,
        };
        let runs = Cell::new(0);
        let reloads = Cell::new(0);
        let words = run_with_cpu_fallback(
            &mut model,
            |_| {
                runs.set(runs.get() + 1);
                if runs.get() == 1 {
                    return Err(anyhow::Error::new(BackendFailure("sem memória na GPU".into())));
                }
                Ok(vec![Word { start_ms: 0, end_ms: 1, text: "ok".into() }])
            },
            || {
                reloads.set(reloads.get() + 1);
                load_context(&tiny, false)
            },
        )
        .unwrap();
        assert_eq!(words.len(), 1);
        assert_eq!((runs.get(), reloads.get()), (2, 1));
        assert_eq!(model.device, Device::Cpu);
    }

    #[test]
    fn non_backend_error_on_gpu_does_not_reload() {
        let Some(tiny) = tiny_model() else {
            eprintln!("sem rede: pulando non_backend_error_on_gpu_does_not_reload");
            return;
        };
        let mut model = LoadedModel {
            ctx: load_context(&tiny, false).unwrap(),
            device: Device::Gpu,
        };
        let res = run_with_cpu_fallback(
            &mut model,
            |_| anyhow::bail!("outro erro"),
            || panic!("não deveria recarregar"),
        );
        assert!(res.is_err());
        assert_eq!(model.device, Device::Gpu);
    }

    #[test]
    fn cpu_retry_failure_chains_gpu_error() {
        let Some(tiny) = tiny_model() else {
            eprintln!("sem rede: pulando cpu_retry_failure_chains_gpu_error");
            return;
        };
        let mut model = LoadedModel {
            ctx: load_context(&tiny, false).unwrap(),
            device: Device::Gpu,
        };
        let res = run_with_cpu_fallback(
            &mut model,
            |_| Err(anyhow::Error::new(BackendFailure("oom gpu".into()))),
            || load_context(&tiny, false),
        );
        let msg = format!("{:#}", res.unwrap_err());
        assert!(msg.contains("oom gpu"), "{msg}");
    }

    #[test]
    fn missing_vad_file_errors_without_reload() {
        let Some(tiny) = tiny_model() else {
            eprintln!("sem rede: pulando missing_vad_file_errors_without_reload");
            return;
        };
        let mut model = LoadedModel {
            ctx: load_context(&tiny, false).unwrap(),
            device: Device::Gpu,
        };
        let reloads = Cell::new(0);
        let res = transcribe_with(
            &mut model,
            &tiny,
            Path::new("/nao/existe/vad.bin"),
            &[0.0; 16000],
            |_| {},
            || {
                reloads.set(reloads.get() + 1);
                load_context(&tiny, false)
            },
        );
        assert!(res.is_err());
        assert_eq!(reloads.get(), 0);
        assert_eq!(model.device, Device::Gpu);
    }

    #[test]
    fn should_use_gpu_needs_model_plus_margin() {
        let gib = 1u64 << 30;
        assert!(!should_use_gpu(1_789_919_232, 1_700_000_000)); // RTX 2050 com desktop aberto
        assert!(should_use_gpu(4 * gib, 1_700_000_000));
        assert!(should_use_gpu(2 * gib, gib));
        assert!(!should_use_gpu(gib, gib));
        assert!(!should_use_gpu(0, u64::MAX));
    }

    #[test]
    fn vad_plan_compresses_and_maps_back() {
        // Fala em 0..2 s e 10..12 s de um áudio de 13 s.
        let n = 13 * 16000;
        let pieces = plan_vad(&[(0.0, 200.0), (1000.0, 1200.0)], n);
        assert_eq!(pieces.len(), 2);
        // 1o trecho ganha 0,1 s de cauda; 2o começa após 0,1 s de silêncio inserido.
        assert_eq!(pieces[0], VadPiece { proc_start_cs: 0, orig_start_cs: 0, len_cs: 210 });
        assert_eq!(pieces[1], VadPiece { proc_start_cs: 220, orig_start_cs: 1000, len_cs: 200 });
        let samples = vec![1.0f32; n];
        assert_eq!(compress_samples(&samples, &pieces).len(), (210 + 10 + 200) * 160);
        assert_eq!(map_to_original(&pieces, 50, false), 50);
        assert_eq!(map_to_original(&pieces, 230, false), 1010); // 10 cs dentro do 2o trecho
        assert_eq!(map_to_original(&pieces, 419, true), 1199);
        // No silêncio inserido: começo -> início do próximo, fim -> fim do anterior.
        assert_eq!(map_to_original(&pieces, 215, false), 1000);
        assert_eq!(map_to_original(&pieces, 215, true), 210);
        // Depois do fim: último ponto.
        assert_eq!(map_to_original(&pieces, 9999, true), 1200);
    }

    #[test]
    fn vad_plan_without_speech_is_empty() {
        assert!(plan_vad(&[], 16000).is_empty());
    }

    #[test]
    fn words_do_not_glue_across_segments() {
        let a = [tok(" um", 0, 10), tok("a", 10, 20)];
        let b = [tok("dois", 30, 40)]; // sem espaço inicial, mas é outro segmento
        let mut words = words_from_tokens(&a);
        words.extend(words_from_tokens(&b));
        assert_eq!(words.len(), 2);
    }

    #[test]
    fn run_failure_on_cpu_is_not_retried() {
        let Some(tiny) = tiny_model() else {
            eprintln!("sem rede: pulando run_failure_on_cpu_is_not_retried");
            return;
        };
        let mut model = LoadedModel {
            ctx: load_context(&tiny, false).unwrap(),
            device: Device::Cpu,
        };
        let runs = Cell::new(0);
        let res = run_with_cpu_fallback(
            &mut model,
            |_| {
                runs.set(runs.get() + 1);
                Err(anyhow::Error::new(BackendFailure("falhou".into())))
            },
            || panic!("não deveria recarregar"),
        );
        assert!(res.is_err());
        assert_eq!(runs.get(), 1);
    }

    /// Requer TONEDIAL_TEST_AUDIO (qualquer áudio que o ffmpeg leia) e o modelo grande
    /// (ou TONEDIAL_TEST_MODEL para sobrescrever, ex.: ggml-tiny.bin).
    #[test]
    #[ignore]
    fn transcribes_portuguese_sample() {
        let audio = std::env::var("TONEDIAL_TEST_AUDIO").expect("defina TONEDIAL_TEST_AUDIO");
        let model_path = std::env::var("TONEDIAL_TEST_MODEL")
            .map(PathBuf::from)
            .unwrap_or_else(|_| {
                crate::models::models_dir().join(crate::models::WHISPER_MODEL_FILE)
            });
        let vad = vad_model().expect("modelo VAD indisponível");
        let samples = crate::audio::decode::decode_16k_mono(Path::new(&audio)).unwrap();
        let mut model = load_model(&model_path).unwrap();
        eprintln!("device: {:?}", model.device);
        let words = transcribe(&mut model, &model_path, &vad, &samples, |p| {
            eprintln!("progresso {p:.2}")
        })
        .unwrap();
        eprintln!("{words:?}");
        assert!(!words.is_empty());
        assert!(words.windows(2).all(|w| w[0].start_ms <= w[1].start_ms));
    }

    /// Silêncio de 6 s entre dois enunciados: com VAD, os tempos têm de voltar à linha do
    /// tempo original (a segunda fala começa depois da pausa). Requer espeak-ng, ffmpeg e
    /// modelos tiny/VAD (baixados para target/test-models/).
    #[test]
    #[ignore]
    fn vad_timestamps_stay_on_original_timeline() {
        let (Some(tiny), Some(vad)) = (tiny_model(), vad_model()) else {
            panic!("modelos indisponíveis");
        };
        let dir = test_models_dir().join("gap");
        std::fs::create_dir_all(&dir).unwrap();
        let sh = |prog: &str, args: &[&str]| {
            assert!(Command::new(prog).args(args).status().unwrap().success(), "{prog}");
        };
        let a = dir.join("a.wav");
        let sil = dir.join("sil.wav");
        let out = dir.join("gap.wav");
        let txt = "Bom dia a todos. Hoje vamos conversar sobre o projeto.";
        sh("espeak-ng", &["-v", "pt-br", "-s", "140", txt, "-w", a.to_str().unwrap()]);
        sh("ffmpeg", &["-y", "-loglevel", "error", "-f", "lavfi", "-i",
            "anullsrc=r=22050:cl=mono", "-t", "6", sil.to_str().unwrap()]);
        sh("ffmpeg", &["-y", "-loglevel", "error", "-i", a.to_str().unwrap(),
            "-i", sil.to_str().unwrap(), "-i", a.to_str().unwrap(),
            "-filter_complex", "[0][1][2]concat=n=3:v=0:a=1", "-ar", "22050", out.to_str().unwrap()]);
        let clip_ms = crate::audio::decode::decode_16k_mono(&a).unwrap().len() as u64 * 1000 / 16000;
        let samples = crate::audio::decode::decode_16k_mono(&out).unwrap();
        let mut model = load_model(&tiny).unwrap();
        let words = transcribe(&mut model, &tiny, &vad, &samples, |_| {}).unwrap();
        eprintln!("clip_ms={clip_ms} words={words:?}");
        let second_start = clip_ms + 6000;
        let last = words.last().expect("sem palavras");
        assert!(words[0].start_ms < clip_ms, "{words:?}");
        assert!(last.start_ms >= second_start, "última palavra em {} < {second_start}", last.start_ms);
        // A primeira palavra do segundo enunciado  tem de cair logo depois da pausa.
        let second_bom = words.iter().find(|w| w.start_ms >= clip_ms + 1000).expect("sem palavra no segundo enunciado");
        assert!(
            second_bom.start_ms >= second_start - 500 && second_bom.start_ms <= second_start + 1500,
            "segundo enunciado começou em {} (esperado ~{second_start})", second_bom.start_ms
        );
        assert!(words.iter().all(|w| w.end_ms - w.start_ms < 4000), "{words:?}");
        assert!(words.windows(2).all(|w| w[0].start_ms <= w[1].start_ms));
    }
}
