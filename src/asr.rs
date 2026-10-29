use std::path::Path;
use std::sync::{Arc, Mutex};

use anyhow::{Context, Result};
use whisper_rs::{
    FullParams, SamplingStrategy, WhisperContext, WhisperContextParameters, WhisperVadContext,
    WhisperState, WhisperVadContextParams, WhisperVadParams,
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
const GPU_MARGIN_BYTES: u64 = 3 << 29; // 1,5 GiB: pico medido de ~3,05 GiB com o large-v3 q8_0 (1,66 GB)

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

/// Tamanho máximo (centésimos) de um bloco redecodificado isolado: cabe numa janela de 30 s
/// do Whisper, com folga.
const CHUNK_MAX_CS: i64 = 2800;

/// Fala sem nenhuma palavra (centésimos) a partir da qual um bloco é considerado perdido.
const REDO_UNCOVERED_CS: i64 = 200;

/// Agrupa trechos consecutivos em blocos (início, fim no áudio comprimido, em centésimos) de no
/// máximo `max_cs`; um trecho maior que isso vira um bloco sozinho (o whisper o fatia por dentro).
fn plan_chunks(pieces: &[VadPiece], max_cs: i64) -> Vec<(i64, i64)> {
    let mut chunks: Vec<(i64, i64)> = Vec::new();
    for p in pieces {
        let end = p.proc_start_cs + p.len_cs;
        match chunks.last_mut() {
            Some((start, e)) if end - *start <= max_cs => *e = end,
            _ => chunks.push((p.proc_start_cs, end)),
        }
    }
    chunks
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

fn full_params() -> FullParams<'static, 'static> {
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
    params
}

/// Segmento do whisper com tempos no áudio comprimido: `t0_cs`/`t1_cs` em centésimos e as
/// palavras em milissegundos (ainda não mapeadas para o áudio original).
struct DecodedSeg {
    t0_cs: i64,
    t1_cs: i64,
    text: String,
    words: Vec<Word>,
}

/// Roda `full` em `samples` (que começa em `offset_cs` do áudio comprimido) e devolve os
/// segmentos com os tempos já somados a `offset_cs`.
fn decode(
    state: &mut WhisperState,
    params: FullParams,
    samples: &[f32],
    offset_cs: i64,
) -> Result<Vec<DecodedSeg>> {
    state
        .full(params, samples)
        .map_err(|e| anyhow::Error::new(BackendFailure(format!("falha na transcrição: {e}"))))?;
    let mut segs = Vec::new();
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
                t0: offset_cs + data.t0,
                t1: offset_cs + data.t1,
            });
        }
        segs.push(DecodedSeg {
            t0_cs: offset_cs + seg.start_timestamp(),
            t1_cs: offset_cs + seg.end_timestamp(),
            text: seg.to_str_lossy().map(|t| t.into_owned()).unwrap_or_default(),
            // Uma palavra nunca atravessa segmentos: cada segmento é montado sozinho.
            words: words_from_tokens(&tokens),
        });
    }
    Ok(segs)
}

/// Índice do bloco que contém o instante `t_cs` (o último que começa em ou antes dele).
fn chunk_of(chunks: &[(i64, i64)], t_cs: i64) -> usize {
    chunks
        .iter()
        .rposition(|&(start, _)| start <= t_cs)
        .unwrap_or(0)
}

/// Blocos em que a passada contínua falhou e que precisam ser redecodificados isolados:
/// - laço: dois segmentos seguidos com o mesmo texto (o whisper repetindo a frase anterior);
/// - fala perdida: trechos do VAD sem nenhuma palavra somando `REDO_UNCOVERED_CS` ou mais, ou
///   um buraco desse tamanho sem palavra dentro de um trecho (o `seek` pulou por cima da fala).
fn chunks_to_redo(
    chunks: &[(i64, i64)],
    pieces: &[VadPiece],
    segs: &[(i64, i64, &str)],
    words_cs: &[(i64, i64)],
) -> Vec<usize> {
    let mut redo = vec![false; chunks.len()];
    if chunks.is_empty() {
        return Vec::new();
    }
    for pair in segs.windows(2) {
        let (prev, cur) = (&pair[0], &pair[1]);
        let text = crate::transcript::normalize(cur.2);
        if !text.is_empty() && text == crate::transcript::normalize(prev.2) {
            let (a, b) = (chunk_of(chunks, prev.0), chunk_of(chunks, cur.1));
            for flag in &mut redo[a.min(b)..=a.max(b)] {
                *flag = true;
            }
        }
    }
    let mut sorted = words_cs.to_vec();
    sorted.sort_unstable();
    let mut uncovered = vec![0i64; chunks.len()];
    for p in pieces {
        let (a, b) = (p.proc_start_cs, p.proc_start_cs + p.len_cs);
        let i = chunk_of(chunks, a);
        let inside: Vec<_> = sorted.iter().filter(|&&(w0, w1)| w0 < b && w1 > a).collect();
        if inside.is_empty() {
            uncovered[i] += p.len_cs;
            continue;
        }
        // Buraco dentro do trecho: fala longa sem palavra entre duas palavras (ou nas pontas).
        let mut cursor = a;
        let mut max_gap = 0;
        for &&(w0, w1) in &inside {
            max_gap = max_gap.max(w0 - cursor);
            cursor = cursor.max(w1);
        }
        max_gap = max_gap.max(b - cursor);
        if max_gap >= REDO_UNCOVERED_CS {
            redo[i] = true;
        }
    }
    (0..chunks.len())
        .filter(|&i| redo[i] || uncovered[i] >= REDO_UNCOVERED_CS)
        .collect()
}

/// Ponto médio de uma palavra, em centésimos (as palavras ainda estão em ms).
fn mid_cs(w: &Word) -> i64 {
    ((w.start_ms + w.end_ms) / 20) as i64
}

/// Troca, nos blocos refeitos, as palavras da passada contínua pelas da redecodificação.
/// Cada palavra pertence ao bloco do seu ponto médio (o início pode escorregar para o vizinho:
/// os blocos ficam a só 0,1 s um do outro). Uma palavra refeita fora do seu bloco é descartada
/// (o whisper completa o bloco com silêncio até 30 s e pode inventar algo ali, e um trecho
/// vizinho saudável já tem as suas), e as que ficam são limitadas às bordas do bloco.
fn splice(
    first: Vec<Word>,
    chunks: &[(i64, i64)],
    redo: &[usize],
    redone: Vec<(usize, Vec<Word>)>,
) -> Vec<Word> {
    let mut words: Vec<Word> = first
        .into_iter()
        .filter(|w| !redo.contains(&chunk_of(chunks, mid_cs(w))))
        .collect();
    for (i, block) in redone {
        let (start_ms, end_ms) = (chunks[i].0 as u64 * 10, chunks[i].1 as u64 * 10);
        words.extend(block.into_iter().filter(|w| chunk_of(chunks, mid_cs(w)) == i).map(|w| {
            let start = w.start_ms.clamp(start_ms, end_ms);
            Word {
                start_ms: start,
                end_ms: w.end_ms.clamp(start, end_ms),
                text: w.text,
            }
        }));
    }
    words.sort_by_key(|w| w.start_ms);
    words
}

/// `samples` é o áudio já comprimido pelo VAD; `pieces` mapeia os tempos de volta ao original.
///
/// Primeiro uma passada contínua sobre o áudio todo (o contexto entre janelas ajuda a precisão).
/// Só que nela uma alucinação do decodificador (repetir uma frase, ou um segmento cujo tempo
/// cobre a janela toda) faz o whisper.cpp avançar o `seek` por cima da fala real, e a frase
/// repetida vira o prompt da janela seguinte (`no_context` só limpa o contexto no início do
/// `full`), então o laço pode seguir até o fim do áudio. Isso acontece na CPU e na GPU; a
/// numérica diferente do Vulkan só muda onde. Por isso os blocos em que a passada falhou
/// (`chunks_to_redo`) são redecodificados isolados, sem contexto, e substituem o trecho ruim.
fn run_once(
    ctx: &WhisperContext,
    samples: &[f32],
    pieces: &[VadPiece],
    progress: impl Fn(f32) + Clone + 'static,
) -> Result<Vec<Word>> {
    const FIRST_PASS: f32 = 0.95;
    let mut state = ctx.create_state().map_err(|e| {
        anyhow::Error::new(BackendFailure(format!(
            "falha ao criar o estado do whisper: {e}"
        )))
    })?;

    let mut params = full_params();
    let first = progress.clone();
    params.set_progress_callback_safe(move |p: i32| first(p as f32 / 100.0 * FIRST_PASS));
    let segs = decode(&mut state, params, samples, 0)?;

    let chunks = plan_chunks(pieces, CHUNK_MAX_CS);
    let seg_spans: Vec<(i64, i64, &str)> =
        segs.iter().map(|s| (s.t0_cs, s.t1_cs, s.text.as_str())).collect();
    let mut words: Vec<Word> = segs.iter().flat_map(|s| s.words.iter().cloned()).collect();
    let words_cs: Vec<(i64, i64)> = words
        .iter()
        .map(|w| ((w.start_ms / 10) as i64, (w.end_ms / 10) as i64))
        .collect();
    let redo = chunks_to_redo(&chunks, pieces, &seg_spans, &words_cs);

    if !redo.is_empty() {
        let mut redone = Vec::new();
        for (k, &i) in redo.iter().enumerate() {
            let (start_cs, end_cs) = chunks[i];
            let a = (start_cs as usize * SAMPLES_PER_CS).min(samples.len());
            let b = (end_cs as usize * SAMPLES_PER_CS).min(samples.len());
            let segs = decode(&mut state, full_params(), &samples[a..b], start_cs)?;
            redone.push((i, segs.into_iter().flat_map(|s| s.words).collect()));
            progress(FIRST_PASS + (1.0 - FIRST_PASS) * (k + 1) as f32 / redo.len() as f32);
        }
        words = splice(words, &chunks, &redo, redone);
    }

    let words = words
        .into_iter()
        .map(|w| {
            let start = map_to_original(pieces, (w.start_ms / 10) as i64, false).max(0) as u64;
            let end = map_to_original(pieces, (w.end_ms / 10) as i64, true).max(0) as u64;
            Word {
                start_ms: start * 10,
                end_ms: (end * 10).max(start * 10),
                text: w.text,
            }
        })
        .collect();
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
                    f(p.clamp(0.0, 1.0));
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
        // Folga de 1,5 GiB: o large-v3 q8_0 (1,66 GB) chegou a ~3,05 GiB de pico.
        assert!(!should_use_gpu(3 * gib, 1_656_538_283));
        assert!(should_use_gpu(3 * gib + gib / 4, 1_656_538_283));
        assert!(should_use_gpu(gib * 5 / 2, gib));
        assert!(!should_use_gpu(gib * 5 / 2 - 1, gib));
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
    fn chunks_group_pieces_up_to_the_limit() {
        let p = |proc_start_cs, len_cs| VadPiece { proc_start_cs, orig_start_cs: 0, len_cs };
        // 0..1000, 1010..2010, 2020..2520, 2530..6530 (maior que o limite), 6540..6640
        let pieces = [p(0, 1000), p(1010, 1000), p(2020, 500), p(2530, 4000), p(6540, 100)];
        assert_eq!(
            plan_chunks(&pieces, 2800),
            vec![(0, 2520), (2530, 6530), (6540, 6640)]
        );
        assert_eq!(plan_chunks(&pieces[..3], 2000), vec![(0, 1000), (1010, 2520)]);
        assert!(plan_chunks(&[], 2800).is_empty());
    }

    #[test]
    fn chunk_of_finds_containing_chunk() {
        let chunks = [(0, 2520), (2530, 6530), (6540, 6640)];
        assert_eq!(chunk_of(&chunks, 0), 0);
        assert_eq!(chunk_of(&chunks, 2525), 0); // silêncio entre blocos fica com o anterior
        assert_eq!(chunk_of(&chunks, 2530), 1);
        assert_eq!(chunk_of(&chunks, 99_999), 2);
    }

    /// Regressão do caso da GPU (NURC, Vulkan): a passada contínua entra em laço numa frase
    /// ("Eu estou acabando com as minhas reservas." 30+ vezes, um segmento cobrindo 30 s) e
    /// não sai mais; nenhuma palavra cai nos trechos de fala seguintes. Os blocos do laço e os
    /// que ficaram sem palavras têm de ser refeitos; os saudáveis, não.
    #[test]
    fn redo_flags_loops_and_skipped_speech_only() {
        let p = |proc_start_cs, len_cs| VadPiece { proc_start_cs, orig_start_cs: proc_start_cs, len_cs };
        let pieces = [p(0, 1000), p(1010, 1000), p(3000, 2000), p(5010, 500), p(6000, 2000), p(9000, 1000)];
        let chunks = plan_chunks(&pieces, 2800);
        assert_eq!(chunks, vec![(0, 2010), (3000, 5510), (6000, 8000), (9000, 10000)]);
        let loop_txt = " Eu estou acabando com as minhas reservas.";
        let segs = [
            (0, 900, " Bom dia."),
            (1010, 2000, " Tudo bem?"),
            (3000, 3300, loop_txt),
            (3300, 6300, loop_txt),
            (6300, 6300, " eu estou acabando com as minhas reservas"),
        ];
        // Palavras só no 1o bloco e no começo do laço; nada no 3o; o 4o está saudável.
        let words = [(0, 400), (500, 900), (1010, 1900), (3000, 3300), (9000, 9900)];
        assert_eq!(chunks_to_redo(&chunks, &pieces, &segs, &words), vec![1, 2]);
    }

    #[test]
    fn redo_ignores_small_gaps_and_distinct_text() {
        let p = |proc_start_cs, len_cs| VadPiece { proc_start_cs, orig_start_cs: 0, len_cs };
        let pieces = [p(0, 1000), p(1010, 150)]; // 1,5 s sem palavra: abaixo do limite
        let chunks = plan_chunks(&pieces, 2800);
        let segs = [(0, 500, " Sim."), (500, 1000, " Não."), (1000, 1000, "")];
        assert!(chunks_to_redo(&chunks, &pieces, &segs, &[(0, 900)]).is_empty());
        assert!(chunks_to_redo(&[], &[], &[], &[]).is_empty());
    }

    #[test]
    fn redo_flags_gap_inside_long_piece() {
        // Um trecho só de 25 s com palavras no começo e no fim e 3 s sem nada no meio: nenhum
        // trecho inteiro fica descoberto e não há laço; só o buraco interno dispara.
        let pieces = [VadPiece { proc_start_cs: 0, orig_start_cs: 0, len_cs: 2500 }];
        let chunks = plan_chunks(&pieces, 2800);
        let segs = [(0, 1000, " Primeira parte."), (1300, 2500, " Segunda parte.")];
        let mut words: Vec<(i64, i64)> = (0..10).map(|k| (k * 100, k * 100 + 90)).collect();
        words.extend((13..25).map(|k| (k * 100, k * 100 + 90)));
        assert_eq!(chunks_to_redo(&chunks, &pieces, &segs, &words), vec![0]);
        // Sem o buraco (palavras a cada segundo), nada a refazer.
        let full: Vec<(i64, i64)> = (0..25).map(|k| (k * 100, k * 100 + 90)).collect();
        assert!(chunks_to_redo(&chunks, &pieces, &segs, &full).is_empty());
    }

    fn w(start_ms: u64, end_ms: u64, text: &str) -> Word {
        Word { start_ms, end_ms, text: text.into() }
    }

    #[test]
    fn splice_assigns_words_to_blocks_by_midpoint() {
        // Blocos (cs): 0..1000 saudável, 1010..2000 refeito, 2010..3000 saudável.
        let chunks = [(0, 1000), (1010, 2000), (2010, 3000)];
        let first = vec![
            w(1000, 9000, "antes"),
            // Começo escorregou para dentro do bloco refeito, mas o meio está no saudável: fica.
            w(9500, 10_150, "borda"),
            w(12_000, 13_000, "laço"),
            w(25_000, 26_000, "depois"),
        ];
        let redone = vec![(1, vec![
            w(10_100, 12_000, "novo"),
            // Escorregou para o bloco seguinte (saudável, já tem a sua): descarta, não duplica.
            w(19_900, 20_400, "depois"),
            // Além do fim do bloco (o whisper completa até 30 s): descarta.
            w(28_000, 29_000, "fantasma"),
        ])];
        let out = splice(first, &chunks, &[1], redone);
        let texts: Vec<&str> = out.iter().map(|w| w.text.as_str()).collect();
        assert_eq!(texts, vec!["antes", "borda", "novo", "depois"]);
        assert!(out.windows(2).all(|p| p[0].start_ms <= p[1].start_ms));
    }

    #[test]
    fn splice_clamps_redone_words_to_their_block() {
        let chunks = [(0, 1000), (1010, 2000), (2010, 3000)];
        let redone = vec![(1, vec![w(10_050, 10_500, "cedo"), w(19_500, 20_090, "tarde")])];
        let out = splice(Vec::new(), &chunks, &[1], redone);
        assert_eq!(out, vec![w(10_100, 10_500, "cedo"), w(19_500, 20_000, "tarde")]);
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
