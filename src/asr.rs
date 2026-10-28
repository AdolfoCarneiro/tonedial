use std::path::Path;
use std::sync::{Arc, Mutex};

use anyhow::{Context, Result};
use whisper_rs::{
    FullParams, SamplingStrategy, WhisperContext, WhisperContextParameters, WhisperVadParams,
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

/// Carrega o modelo, preferindo a GPU (Vulkan) e caindo para CPU se falhar.
pub fn load_model(path: &Path) -> Result<LoadedModel> {
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
        if t.bytes.starts_with(b"[_") || t.bytes.starts_with(b"<|") {
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

fn run_once(
    ctx: &WhisperContext,
    vad_model: &Path,
    samples: &[f32],
    progress: impl FnMut(i32) + 'static,
) -> Result<Vec<Word>> {
    let vad_str = vad_model
        .to_str()
        .with_context(|| format!("caminho do modelo VAD inválido: {}", vad_model.display()))?;
    let mut state = ctx
        .create_state()
        .map_err(|e| anyhow::anyhow!("falha ao criar o estado do whisper: {e}"))?;

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
    params.set_vad_model_path(Some(vad_str));
    params.enable_vad(true);
    params.set_vad_params(WhisperVadParams::new());
    params.set_progress_callback_safe(progress);

    state
        .full(params, samples)
        .map_err(|e| anyhow::anyhow!("falha na transcrição: {e}"))?;

    let mut tokens = Vec::new();
    for seg in state.as_iter() {
        for i in 0..seg.n_tokens() {
            let Some(tok) = seg.get_token(i) else { continue };
            let data = tok.token_data();
            let bytes = tok
                .to_bytes()
                .map_err(|e| anyhow::anyhow!("token inválido: {e}"))?
                .to_vec();
            tokens.push(Tok {
                bytes,
                t0: data.t0,
                t1: data.t1,
            });
        }
    }
    Ok(dedupe_repeats(words_from_tokens(&tokens)))
}

/// Executa `run` no contexto atual; se falhar na GPU, recarrega em CPU via `reload_cpu`
/// (trocando o contexto em `model`) e tenta uma vez mais.
fn run_with_cpu_fallback(
    model: &mut LoadedModel,
    run: impl Fn(&WhisperContext) -> Result<Vec<Word>>,
    reload_cpu: impl FnOnce() -> Result<WhisperContext>,
) -> Result<Vec<Word>> {
    match run(&model.ctx) {
        Ok(words) => Ok(words),
        Err(_) if model.device == Device::Gpu => {
            model.ctx = reload_cpu()?;
            model.device = Device::Cpu;
            run(&model.ctx)
        }
        Err(e) => Err(e),
    }
}

/// Transcreve amostras 16 kHz mono. `on_progress` recebe 0.0..=1.0.
/// Se falhar na GPU, recarrega o modelo em CPU (`model.device` passa a `Cpu`) e tenta de novo.
pub fn transcribe(
    model: &mut LoadedModel,
    model_path: &Path,
    vad_model: &Path,
    samples: &[f32],
    on_progress: impl FnMut(f32) + Send + 'static,
) -> Result<Vec<Word>> {
    let cb = Arc::new(Mutex::new(on_progress));
    run_with_cpu_fallback(
        model,
        |ctx| {
            let cb = Arc::clone(&cb);
            run_once(ctx, vad_model, samples, move |p| {
                if let Ok(mut f) = cb.lock() {
                    f((p as f32 / 100.0).clamp(0.0, 1.0));
                }
            })
        },
        || load_context(model_path, false),
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
                    anyhow::bail!("sem memória na GPU");
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
                anyhow::bail!("falhou")
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
}
