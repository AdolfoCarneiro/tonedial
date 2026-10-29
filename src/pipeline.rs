use std::collections::HashSet;
use std::path::{Path, PathBuf};
use std::sync::mpsc::Sender;

use anyhow::{Context, Result};
use chrono::NaiveDateTime;

use crate::asr::{self, Device};
use crate::audio::decode;
use crate::diarize::{Diarizer, SpeakrsDiarizer};
use crate::models::{VAD_MODEL_FILE, WHISPER_MODEL_FILE};
use crate::naming::{resolve_recorded_at, unique_output_stem};
use crate::transcript::{Header, SpeakerSegment, Word, align, align_call, drop_echo, render_markdown};

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Stage {
    Preparing,
    Transcribing,
    Diarizing,
    Writing,
}

#[derive(Debug, Clone)]
pub enum Progress {
    Stage(Stage),
    /// Fração 0.0..=1.0 da etapa atual.
    Fraction(f32),
    Notice(String),
    Done(Outcome),
    Failed(String),
}

#[derive(Debug, Clone)]
pub struct Outcome {
    pub md_path: PathBuf,
    pub speakers: u32,
    pub duration_ms: u64,
}

#[derive(Debug, Clone)]
pub enum Source {
    Recording {
        wav: PathBuf,
        started_at: NaiveDateTime,
    },
    File(PathBuf),
    /// Call: o mic e o som do computador gravados em separado.
    Call {
        mic: PathBuf,
        system: PathBuf,
        started_at: NaiveDateTime,
    },
}

#[derive(Debug, Clone)]
pub struct Job {
    pub source: Source,
    pub output_dir: PathBuf,
    /// Contém o modelo do whisper, o VAD e os arquivos do speakrs (tudo na mesma pasta).
    pub models_dir: PathBuf,
}

/// Fábrica do diarizador; chamada só depois de o whisper ter sido solto da memória.
pub type DiarizerFactory<'a> = Box<dyn FnOnce() -> Result<Box<dyn Diarizer>> + Send + 'a>;

/// Transcrição injetável (o padrão carrega o whisper); recebe as amostras 16 kHz mono.
pub(crate) type Transcriber<'a> =
    Box<dyn FnMut(&[f32], &Sender<Progress>) -> Result<Vec<Word>> + 'a>;

const CPU_NOTICE: &str = "GPU indisponível ou sem memória livre; usando CPU (mais lento)";
pub(crate) const DIARIZE_FAILED_NOTICE: &str = "Não consegui separar os falantes; o texto sai sem separação.";

/// Roda o pipeline completo com whisper padrão e speakrs. Bloqueante: o chamador põe numa thread.
/// Sempre termina enviando `Done` ou `Failed`.
pub fn run(job: Job, tx: Sender<Progress>) {
    let model_path = job.models_dir.join(WHISPER_MODEL_FILE);
    let models_dir = job.models_dir.clone();
    let factory: DiarizerFactory = Box::new(move || {
        Ok(Box::new(SpeakrsDiarizer::new(&models_dir)?) as Box<dyn Diarizer>)
    });
    run_with(job, &model_path, factory, tx);
}

/// Como `run`, mas com o caminho do modelo do whisper e o diarizador escolhidos (usado pelo CLI).
pub fn run_with(job: Job, model_path: &Path, make_diarizer: DiarizerFactory, tx: Sender<Progress>) {
    let vad = job.models_dir.join(VAD_MODEL_FILE);
    let mut model = None;
    let transcriber: Transcriber =
        Box::new(move |samples, tx| whisper_transcribe(&mut model, model_path, &vad, samples, tx));
    run_inner(job, transcriber, make_diarizer, decode::check_ffmpeg, tx);
}

/// Whisper: o modelo carrega na primeira chamada e fica no transcritor, que `execute` solta
/// antes de diarizar (VRAM).
fn whisper_transcribe(
    model: &mut Option<asr::LoadedModel>,
    model_path: &Path,
    vad: &Path,
    samples: &[f32],
    tx: &Sender<Progress>,
) -> Result<Vec<Word>> {
    if model.is_none() {
        *model = Some(asr::load_model(model_path).context("Falha ao carregar o modelo de transcrição")?);
    }
    let model = model.as_mut().expect("carregado acima");
    let mut notified = false;
    if model.device == Device::Cpu {
        let _ = tx.send(Progress::Notice(CPU_NOTICE.into()));
        notified = true;
    }
    let ptx = tx.clone();
    let words = asr::transcribe(model, model_path, vad, samples, move |f| {
        let _ = ptx.send(Progress::Fraction(f));
    })?;
    if !notified && model.device == Device::Cpu {
        let _ = tx.send(Progress::Notice(CPU_NOTICE.into()));
    }
    Ok(words)
}

fn run_inner(
    job: Job,
    transcribe: Transcriber,
    make_diarizer: DiarizerFactory,
    check: impl FnOnce() -> Result<()>,
    tx: Sender<Progress>,
) {
    let wavs = match &job.source {
        Source::Recording { wav, .. } => vec![wav.clone()],
        Source::Call { mic, system, .. } => vec![mic.clone(), system.clone()],
        Source::File(_) => Vec::new(),
    };
    let mut ogg: Option<PathBuf> = None;
    match execute(job, transcribe, make_diarizer, check, &mut ogg, &tx) {
        Ok(outcome) => {
            let _ = tx.send(Progress::Done(outcome));
        }
        Err(e) => {
            let mut msg = format!("{e:#}");
            // Gravação: diz onde o áudio ficou (o .ogg, se já existe; senão o(s) WAV(s)).
            if !wavs.is_empty() {
                let saved = match ogg.filter(|o| o.is_file()) {
                    Some(o) => vec![o],
                    None => wavs,
                };
                let saved: Vec<String> = saved.iter().map(|p| p.display().to_string()).collect();
                msg.push_str(&format!("\nO áudio está salvo em: {}", saved.join(" e ")));
            }
            let _ = tx.send(Progress::Failed(msg));
        }
    }
}

/// Separa os falantes; falha (ou panic) vira "sem segmentos" com aviso.
fn diarize_or_fallback(
    make_diarizer: DiarizerFactory,
    samples: &[f32],
    tx: &Sender<Progress>,
) -> Vec<SpeakerSegment> {
    let send = |p: Progress| {
        let _ = tx.send(p);
    };
    send(Progress::Stage(Stage::Diarizing));
    let diarized = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
        let diarizer = make_diarizer()?;
        diarizer.diarize(samples, &mut |f| send(Progress::Fraction(f)))
    }));
    match diarized {
        Ok(Ok(segments)) => segments,
        Ok(Err(e)) => {
            eprintln!("separação de falantes falhou: {e:#}");
            send(Progress::Notice(DIARIZE_FAILED_NOTICE.into()));
            Vec::new()
        }
        Err(_) => {
            send(Progress::Notice(DIARIZE_FAILED_NOTICE.into()));
            Vec::new()
        }
    }
}

fn execute(
    job: Job,
    mut transcribe: Transcriber,
    make_diarizer: DiarizerFactory,
    check: impl FnOnce() -> Result<()>,
    saved_ogg: &mut Option<PathBuf>,
    tx: &Sender<Progress>,
) -> Result<Outcome> {
    let send = |p: Progress| {
        let _ = tx.send(p);
    };
    send(Progress::Stage(Stage::Preparing));
    check()?;

    if let Source::Call { mic, system, started_at } = &job.source {
        return execute_call(&job, mic, system, *started_at, transcribe, make_diarizer, saved_ogg, tx);
    }

    let (input, recording) = match &job.source {
        Source::Recording { wav, started_at } => (wav.clone(), Some(*started_at)),
        Source::File(p) => (p.clone(), None),
        Source::Call { .. } => unreachable!("call tratada acima"),
    };
    let info = decode::probe(&input)?;
    let recorded_at = match recording {
        Some(at) => at,
        None => {
            let name = input
                .file_name()
                .map(|n| n.to_string_lossy().into_owned())
                .unwrap_or_default();
            let mtime = std::fs::metadata(&input)
                .and_then(|m| m.modified())
                .map(|t| chrono::DateTime::<chrono::Local>::from(t).naive_local())
                .unwrap_or_else(|_| chrono::Local::now().naive_local());
            resolve_recorded_at(info.creation_time, &name, mtime)
        }
    };
    let stem = unique_output_stem(&job.output_dir, recorded_at, Some(&input))?;
    let samples = decode::decode_16k_mono(&input)?;

    let source_name = if recording.is_some() {
        let ogg = job.output_dir.join(format!("{stem}.ogg"));
        decode::encode_ogg(&input, &ogg)?;
        if ogg.exists() {
            let _ = std::fs::remove_file(&input);
            *saved_ogg = Some(ogg);
        }
        format!("{stem}.ogg")
    } else {
        input
            .file_name()
            .map(|n| n.to_string_lossy().into_owned())
            .unwrap_or_default()
    };

    send(Progress::Stage(Stage::Transcribing));
    let words = transcribe(&samples, tx)?;

    // Falha (ou panic) na separação não joga fora a transcrição: tudo vira Falante 1.
    let segments = if words.is_empty() {
        Vec::new()
    } else {
        diarize_or_fallback(make_diarizer, &samples, tx)
    };

    send(Progress::Stage(Stage::Writing));
    let utterances = align(&words, &segments);
    let speakers = utterances.iter().map(|u| u.speaker).collect::<HashSet<_>>().len() as u32;
    let header = Header {
        recorded_at,
        duration_ms: info.duration_ms,
        source_name,
    };
    let md_path = job.output_dir.join(format!("{stem}.md"));
    std::fs::write(&md_path, render_markdown(&header, &utterances))
        .with_context(|| format!("Não consegui salvar {}", md_path.display()))?;
    Ok(Outcome {
        md_path,
        speakers,
        duration_ms: info.duration_ms,
    })
}

#[allow(clippy::too_many_arguments)]
fn execute_call(
    job: &Job,
    mic: &Path,
    system: &Path,
    started_at: NaiveDateTime,
    mut transcribe: Transcriber,
    make_diarizer: DiarizerFactory,
    saved_ogg: &mut Option<PathBuf>,
    tx: &Sender<Progress>,
) -> Result<Outcome> {
    let send = |p: Progress| {
        let _ = tx.send(p);
    };
    let mic_info = decode::probe(mic)?;
    let sys_info = decode::probe(system)?;
    let stem = unique_output_stem(&job.output_dir, started_at, Some(mic))?;
    let mic_samples = decode::decode_16k_mono(mic)?;
    let sys_samples = decode::decode_16k_mono(system)?;

    let ogg = job.output_dir.join(format!("{stem}.ogg"));
    decode::encode_ogg_stereo(mic, system, &ogg)?;
    if ogg.exists() {
        let _ = std::fs::remove_file(mic);
        let _ = std::fs::remove_file(system);
        *saved_ogg = Some(ogg);
    }

    send(Progress::Stage(Stage::Transcribing));
    let mic_words = transcribe(&mic_samples, tx)?;
    send(Progress::Stage(Stage::Transcribing));
    let sys_words = transcribe(&sys_samples, tx)?;
    // Solta o whisper (VRAM) antes de o speakrs carregar.
    drop(transcribe);

    let segments = if sys_words.is_empty() {
        Vec::new()
    } else {
        diarize_or_fallback(make_diarizer, &sys_samples, tx)
    };

    send(Progress::Stage(Stage::Writing));
    let utterances = align_call(&drop_echo(mic_words, &sys_words), &sys_words, &segments);
    let speakers = utterances.iter().map(|u| u.speaker).collect::<HashSet<_>>().len() as u32;
    let duration_ms = mic_info.duration_ms.max(sys_info.duration_ms);
    let header = Header {
        recorded_at: started_at,
        duration_ms,
        source_name: format!("{stem}.ogg (call: mic + som do computador)"),
    };
    let md_path = job.output_dir.join(format!("{stem}.md"));
    std::fs::write(&md_path, render_markdown(&header, &utterances))
        .with_context(|| format!("Não consegui salvar {}", md_path.display()))?;
    Ok(Outcome { md_path, speakers, duration_ms })
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::mpsc;
    use std::sync::{Arc, Mutex};

    use crate::transcript::SpeakerSegment;

    fn no_diarizer<'a>() -> DiarizerFactory<'a> {
        Box::new(|| anyhow::bail!("não deveria chegar aqui"))
    }

    fn no_transcriber<'a>() -> Transcriber<'a> {
        Box::new(|_, _| anyhow::bail!("não deveria chegar aqui"))
    }

    fn words_transcriber<'a>() -> Transcriber<'a> {
        Box::new(|_, _| {
            Ok(vec![
                Word { start_ms: 0, end_ms: 400, text: "olá".into() },
                Word { start_ms: 500, end_ms: 900, text: "mundo".into() },
            ])
        })
    }

    /// WAV de 2 s (tom) gerado pelo ffmpeg.
    fn wav_fixture(dir: &Path) -> PathBuf {
        let p = dir.join("gravacao.wav");
        let st = std::process::Command::new("ffmpeg")
            .args(["-v", "error", "-y", "-f", "lavfi", "-i", "sine=frequency=440:duration=2"])
            .arg(&p)
            .status()
            .unwrap();
        assert!(st.success());
        p
    }

    fn started() -> NaiveDateTime {
        chrono::NaiveDate::from_ymd_opt(2026, 10, 28).unwrap().and_hms_opt(14, 30, 0).unwrap()
    }

    #[test]
    fn diarizer_error_falls_back_to_single_speaker() {
        let d = tempfile::tempdir().unwrap();
        let audio = wav_fixture(d.path());
        let out = tempfile::tempdir().unwrap();
        let (tx, rx) = mpsc::channel();
        let job = Job {
            source: Source::File(audio),
            output_dir: out.path().to_path_buf(),
            models_dir: d.path().to_path_buf(),
        };
        run_inner(job, words_transcriber(), Box::new(|| anyhow::bail!("sem modelo")), || Ok(()), tx);
        let msgs: Vec<Progress> = rx.try_iter().collect();
        assert!(
            msgs.iter().any(|m| matches!(m, Progress::Notice(n) if n == DIARIZE_FAILED_NOTICE)),
            "{msgs:?}"
        );
        let Some(Progress::Done(o)) = msgs.last() else { panic!("esperava Done: {msgs:?}") };
        assert_eq!(o.speakers, 1);
        let md = std::fs::read_to_string(&o.md_path).unwrap();
        assert!(md.contains("olá mundo"), "{md}");
    }

    #[test]
    fn diarizer_panic_falls_back_to_single_speaker() {
        let d = tempfile::tempdir().unwrap();
        let audio = wav_fixture(d.path());
        let out = tempfile::tempdir().unwrap();
        let (tx, rx) = mpsc::channel();
        let job = Job {
            source: Source::File(audio),
            output_dir: out.path().to_path_buf(),
            models_dir: d.path().to_path_buf(),
        };
        run_inner(job, words_transcriber(), Box::new(|| panic!("boom")), || Ok(()), tx);
        let msgs: Vec<Progress> = rx.try_iter().collect();
        assert!(msgs.iter().any(|m| matches!(m, Progress::Notice(n) if n == DIARIZE_FAILED_NOTICE)));
        let Some(Progress::Done(o)) = msgs.last() else { panic!("esperava Done: {msgs:?}") };
        assert!(o.md_path.is_file());
    }

    #[test]
    fn no_words_skips_diarization() {
        let d = tempfile::tempdir().unwrap();
        let audio = wav_fixture(d.path());
        let out = tempfile::tempdir().unwrap();
        let (tx, rx) = mpsc::channel();
        let job = Job {
            source: Source::File(audio),
            output_dir: out.path().to_path_buf(),
            models_dir: d.path().to_path_buf(),
        };
        run_inner(job, Box::new(|_, _| Ok(vec![])), Box::new(|| panic!("não devia diarizar")), || Ok(()), tx);
        let msgs: Vec<Progress> = rx.try_iter().collect();
        assert!(!msgs.iter().any(|m| matches!(m, Progress::Stage(Stage::Diarizing) | Progress::Notice(_))), "{msgs:?}");
        assert!(matches!(msgs.last(), Some(Progress::Done(_))), "{msgs:?}");
    }

    #[test]
    fn recording_failure_before_ogg_says_where_the_wav_is() {
        let d = tempfile::tempdir().unwrap();
        let wav = wav_fixture(d.path());
        let out = tempfile::tempdir().unwrap();
        let (tx, rx) = mpsc::channel();
        let job = Job {
            source: Source::Recording { wav: wav.clone(), started_at: started() },
            output_dir: out.path().to_path_buf(),
            models_dir: d.path().to_path_buf(),
        };
        run_inner(job, no_transcriber(), no_diarizer(), || decode::check_ffmpeg_at("/nao/existe"), tx);
        let Some(Progress::Failed(m)) = rx.try_iter().last() else { panic!("esperava Failed") };
        assert!(m.starts_with("ffmpeg não encontrado"), "{m}");
        assert!(m.ends_with(&format!("\nO áudio está salvo em: {}", wav.display())), "{m}");
        assert!(wav.is_file());
    }

    #[test]
    fn recording_failure_after_ogg_says_where_the_ogg_is() {
        let d = tempfile::tempdir().unwrap();
        let wav = wav_fixture(d.path());
        let out = tempfile::tempdir().unwrap();
        let (tx, rx) = mpsc::channel();
        let job = Job {
            source: Source::Recording { wav: wav.clone(), started_at: started() },
            output_dir: out.path().to_path_buf(),
            models_dir: d.path().to_path_buf(),
        };
        run_inner(job, Box::new(|_, _| anyhow::bail!("whisper quebrou")), no_diarizer(), || Ok(()), tx);
        let Some(Progress::Failed(m)) = rx.try_iter().last() else { panic!("esperava Failed") };
        let ogg = out.path().join("2026-10-28-1430.ogg");
        assert!(ogg.is_file());
        assert!(!wav.exists());
        assert!(m.ends_with(&format!("\nO áudio está salvo em: {}", ogg.display())), "{m}");
    }

    /// WAV de `secs` segundos (tom) gerado pelo ffmpeg.
    fn wav_secs(dir: &Path, name: &str, secs: u32) -> PathBuf {
        let p = dir.join(name);
        let st = std::process::Command::new("ffmpeg")
            .args(["-v", "error", "-y", "-f", "lavfi", "-i"])
            .arg(format!("sine=frequency=440:duration={secs}"))
            .arg(&p)
            .status()
            .unwrap();
        assert!(st.success());
        p
    }

    fn call_job(d: &Path, out: &Path, mic_secs: u32, sys_secs: u32) -> (Job, PathBuf, PathBuf) {
        let mic = wav_secs(d, "gravacao-20261028-143000-mic.wav", mic_secs);
        let sys = wav_secs(d, "gravacao-20261028-143000-sistema.wav", sys_secs);
        let job = Job {
            source: Source::Call { mic: mic.clone(), system: sys.clone(), started_at: started() },
            output_dir: out.to_path_buf(),
            models_dir: d.to_path_buf(),
        };
        (job, mic, sys)
    }

    fn channels(path: &Path) -> String {
        let o = std::process::Command::new("ffprobe")
            .args(["-v", "error", "-show_entries", "stream=channels", "-of", "csv=p=0"])
            .arg(path)
            .output()
            .unwrap();
        String::from_utf8_lossy(&o.stdout).trim().to_string()
    }

    struct FixedDiarizer {
        seen: Arc<Mutex<usize>>,
    }

    impl Diarizer for FixedDiarizer {
        fn diarize(&self, samples: &[f32], _: &mut dyn FnMut(f32)) -> Result<Vec<SpeakerSegment>> {
            *self.seen.lock().unwrap() = samples.len();
            Ok(vec![SpeakerSegment { start_ms: 0, end_ms: 2000, speaker: 7 }])
        }
    }

    #[test]
    fn call_gera_md_com_eu_e_falante_e_ogg_estereo() {
        let d = tempfile::tempdir().unwrap();
        let out = tempfile::tempdir().unwrap();
        let (job, mic, sys) = call_job(d.path(), out.path(), 2, 2);
        let (tx, rx) = mpsc::channel();
        let mut calls = 0;
        let transcriber: Transcriber = Box::new(move |_, _| {
            calls += 1;
            Ok(if calls == 1 {
                vec![Word { start_ms: 0, end_ms: 400, text: "oi".into() }]
            } else {
                vec![Word { start_ms: 1000, end_ms: 1400, text: "olá".into() }]
            })
        });
        let diarizer: DiarizerFactory = Box::new(|| {
            Ok(Box::new(FixedDiarizer { seen: Arc::new(Mutex::new(0)) }) as Box<dyn Diarizer>)
        });
        run_inner(job, transcriber, diarizer, || Ok(()), tx);
        let msgs: Vec<Progress> = rx.try_iter().collect();
        let Some(Progress::Done(o)) = msgs.last() else { panic!("esperava Done: {msgs:?}") };
        assert_eq!(o.speakers, 2);
        let md = std::fs::read_to_string(&o.md_path).unwrap();
        assert!(md.contains("**Eu** [00:00:00]: oi"), "{md}");
        assert!(md.contains("**Falante 1** [00:00:01]: olá"), "{md}");
        assert!(md.contains("2026-10-28-1430.ogg (call: mic + som do computador)"), "{md}");
        let ogg = out.path().join("2026-10-28-1430.ogg");
        assert_eq!(channels(&ogg), "2");
        assert!(!mic.exists() && !sys.exists());
    }

    #[test]
    fn call_ogg_dura_o_tanto_do_lado_mais_longo() {
        let d = tempfile::tempdir().unwrap();
        let out = tempfile::tempdir().unwrap();
        let (job, _, _) = call_job(d.path(), out.path(), 1, 3);
        let (tx, rx) = mpsc::channel();
        let transcriber: Transcriber = Box::new(|_, _| Ok(vec![]));
        run_inner(job, transcriber, no_diarizer(), || Ok(()), tx);
        let msgs: Vec<Progress> = rx.try_iter().collect();
        let Some(Progress::Done(o)) = msgs.last() else { panic!("esperava Done: {msgs:?}") };
        assert!((2_900..=3_200).contains(&o.duration_ms), "{}", o.duration_ms);
        let ms = decode::probe(&out.path().join("2026-10-28-1430.ogg")).unwrap().duration_ms;
        assert!((2_900..=3_200).contains(&ms), "{ms}");
    }

    #[test]
    fn call_sem_palavras_no_sistema_pula_diarizacao_e_so_tem_eu() {
        let d = tempfile::tempdir().unwrap();
        let out = tempfile::tempdir().unwrap();
        let (job, _, _) = call_job(d.path(), out.path(), 2, 2);
        let (tx, rx) = mpsc::channel();
        let mut calls = 0;
        let transcriber: Transcriber = Box::new(move |_, _| {
            calls += 1;
            Ok(if calls == 1 {
                vec![Word { start_ms: 0, end_ms: 400, text: "oi".into() }]
            } else {
                vec![]
            })
        });
        run_inner(job, transcriber, Box::new(|| panic!("não devia diarizar")), || Ok(()), tx);
        let msgs: Vec<Progress> = rx.try_iter().collect();
        assert!(!msgs.iter().any(|m| matches!(m, Progress::Stage(Stage::Diarizing) | Progress::Notice(_))), "{msgs:?}");
        let Some(Progress::Done(o)) = msgs.last() else { panic!("esperava Done: {msgs:?}") };
        assert_eq!(o.speakers, 1);
        let md = std::fs::read_to_string(&o.md_path).unwrap();
        assert!(md.contains("**Eu** [00:00:00]: oi") && !md.contains("Falante"), "{md}");
    }

    #[test]
    fn call_diariza_so_o_sistema() {
        let d = tempfile::tempdir().unwrap();
        let out = tempfile::tempdir().unwrap();
        let (job, _, _) = call_job(d.path(), out.path(), 1, 2);
        let (tx, rx) = mpsc::channel();
        let seen = Arc::new(Mutex::new(0));
        let s2 = seen.clone();
        let diarizer: DiarizerFactory =
            Box::new(move || Ok(Box::new(FixedDiarizer { seen: s2 }) as Box<dyn Diarizer>));
        let transcriber: Transcriber =
            Box::new(|_, _| Ok(vec![Word { start_ms: 0, end_ms: 400, text: "oi".into() }]));
        run_inner(job, transcriber, diarizer, || Ok(()), tx);
        let msgs: Vec<Progress> = rx.try_iter().collect();
        assert!(matches!(msgs.last(), Some(Progress::Done(_))), "{msgs:?}");
        let n = *seen.lock().unwrap();
        assert!((31_000..=33_000).contains(&n), "{n}");
    }

    #[test]
    fn call_com_falha_antes_do_ogg_diz_onde_estao_os_wavs() {
        let d = tempfile::tempdir().unwrap();
        let out = tempfile::tempdir().unwrap();
        let (job, mic, sys) = call_job(d.path(), out.path(), 1, 1);
        let (tx, rx) = mpsc::channel();
        run_inner(job, no_transcriber(), no_diarizer(), || decode::check_ffmpeg_at("/nao/existe"), tx);
        let Some(Progress::Failed(m)) = rx.try_iter().last() else { panic!("esperava Failed") };
        assert!(m.contains(&mic.display().to_string()), "{m}");
        assert!(m.contains(&sys.display().to_string()), "{m}");
    }

    #[test]
    fn file_failure_message_has_no_location() {
        let d = tempfile::tempdir().unwrap();
        let (tx, rx) = mpsc::channel();
        let job = Job {
            source: Source::File(d.path().join("x.m4a")),
            output_dir: d.path().to_path_buf(),
            models_dir: d.path().to_path_buf(),
        };
        run_inner(job, no_transcriber(), no_diarizer(), || Ok(()), tx);
        let Some(Progress::Failed(m)) = rx.try_iter().last() else { panic!("esperava Failed") };
        assert!(!m.contains("O áudio está salvo em"), "{m}");
    }

    #[test]
    fn missing_ffmpeg_reports_failed() {
        let (tx, rx) = mpsc::channel();
        let job = Job {
            source: Source::File(PathBuf::from("/nao/existe.m4a")),
            output_dir: PathBuf::from("."),
            models_dir: PathBuf::from("."),
        };
        // Equivale a TONEDIAL_FFMPEG=/nao/existe, sem mexer no ambiente do processo.
        run_inner(
            job,
            no_transcriber(),
            no_diarizer(),
            || decode::check_ffmpeg_at("/nao/existe"),
            tx,
        );
        let msgs: Vec<Progress> = rx.try_iter().collect();
        match msgs.last() {
            Some(Progress::Failed(m)) => {
                assert_eq!(m, "ffmpeg não encontrado. Instale com: sudo dnf install ffmpeg")
            }
            other => panic!("esperava Failed, veio {other:?}"),
        }
    }

    #[test]
    fn bad_output_dir_reports_failed() {
        let d = tempfile::tempdir().unwrap();
        let (tx, rx) = mpsc::channel();
        let job = Job {
            source: Source::File(d.path().join("x.m4a")),
            output_dir: d.path().join("sumiu"),
            models_dir: d.path().to_path_buf(),
        };
        run_inner(job, no_transcriber(), no_diarizer(), || Ok(()), tx);
        assert!(matches!(rx.try_iter().last(), Some(Progress::Failed(_))));
    }

    #[test]
    #[ignore = "requer modelos e TONEDIAL_TEST_AUDIO (+ TONEDIAL_TEST_MODEL, TONEDIAL_TEST_MODELS_DIR)"]
    fn writes_markdown_end_to_end() {
        let audio = PathBuf::from(std::env::var("TONEDIAL_TEST_AUDIO").expect("TONEDIAL_TEST_AUDIO"));
        let models_dir = PathBuf::from(std::env::var("TONEDIAL_TEST_MODELS_DIR").expect("TONEDIAL_TEST_MODELS_DIR"));
        let model = std::env::var("TONEDIAL_TEST_MODEL")
            .map(PathBuf::from)
            .unwrap_or_else(|_| models_dir.join(WHISPER_MODEL_FILE));
        let out = tempfile::tempdir().unwrap();
        let (tx, rx) = mpsc::channel();
        let job = Job {
            source: Source::File(audio),
            output_dir: out.path().to_path_buf(),
            models_dir: models_dir.clone(),
        };
        let md = models_dir.clone();
        run_with(
            job,
            &model,
            Box::new(move || Ok(Box::new(SpeakrsDiarizer::new(&md)?) as Box<dyn Diarizer>)),
            tx,
        );
        let msgs: Vec<Progress> = rx.try_iter().collect();
        let Some(Progress::Done(o)) = msgs.last() else {
            panic!("esperava Done: {:?}", msgs.last());
        };
        assert!(std::fs::read_to_string(&o.md_path).unwrap().starts_with("# Transcrição"));
    }
}
