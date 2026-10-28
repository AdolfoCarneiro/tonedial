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
use crate::transcript::{Header, Word, align, render_markdown};

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
    Box<dyn FnOnce(&[f32], &Sender<Progress>) -> Result<Vec<Word>> + 'a>;

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
    let transcriber: Transcriber = Box::new(move |samples, tx| whisper_transcribe(model_path, &vad, samples, tx));
    run_inner(job, transcriber, make_diarizer, decode::check_ffmpeg, tx);
}

/// Whisper: o modelo é solto ao fim desta função, antes de diarizar (VRAM).
fn whisper_transcribe(
    model_path: &Path,
    vad: &Path,
    samples: &[f32],
    tx: &Sender<Progress>,
) -> Result<Vec<Word>> {
    let mut model = asr::load_model(model_path).context("Falha ao carregar o modelo de transcrição")?;
    let mut notified = false;
    if model.device == Device::Cpu {
        let _ = tx.send(Progress::Notice(CPU_NOTICE.into()));
        notified = true;
    }
    let ptx = tx.clone();
    let words = asr::transcribe(&mut model, model_path, vad, samples, move |f| {
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
    let wav = match &job.source {
        Source::Recording { wav, .. } => Some(wav.clone()),
        Source::File(_) => None,
    };
    let mut ogg: Option<PathBuf> = None;
    match execute(job, transcribe, make_diarizer, check, &mut ogg, &tx) {
        Ok(outcome) => {
            let _ = tx.send(Progress::Done(outcome));
        }
        Err(e) => {
            let mut msg = format!("{e:#}");
            // Gravação: diz onde o áudio ficou (o .ogg, se já existe; senão o WAV).
            if let Some(wav) = wav {
                let saved = ogg.filter(|o| o.is_file()).unwrap_or(wav);
                msg.push_str(&format!("\nO áudio está salvo em: {}", saved.display()));
            }
            let _ = tx.send(Progress::Failed(msg));
        }
    }
}

fn execute(
    job: Job,
    transcribe: Transcriber,
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

    let (input, recording) = match &job.source {
        Source::Recording { wav, started_at } => (wav.clone(), Some(*started_at)),
        Source::File(p) => (p.clone(), None),
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
    let stem = unique_output_stem(&job.output_dir, recorded_at)?;
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
        send(Progress::Stage(Stage::Diarizing));
        let diarized = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
            let diarizer = make_diarizer()?;
            diarizer.diarize(&samples, &mut |f| send(Progress::Fraction(f)))
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

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::mpsc;

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
