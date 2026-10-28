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
use crate::transcript::{Header, align, render_markdown};

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

const CPU_NOTICE: &str = "Sem memória na GPU, usando CPU (mais lento)";

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
    run_inner(job, model_path, make_diarizer, decode::check_ffmpeg, tx);
}

fn run_inner(
    job: Job,
    model_path: &Path,
    make_diarizer: DiarizerFactory,
    check: impl FnOnce() -> Result<()>,
    tx: Sender<Progress>,
) {
    match execute(job, model_path, make_diarizer, check, &tx) {
        Ok(outcome) => {
            let _ = tx.send(Progress::Done(outcome));
        }
        Err(e) => {
            let _ = tx.send(Progress::Failed(format!("{e:#}")));
        }
    }
}

fn execute(
    job: Job,
    model_path: &Path,
    make_diarizer: DiarizerFactory,
    check: impl FnOnce() -> Result<()>,
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
        }
        format!("{stem}.ogg")
    } else {
        input
            .file_name()
            .map(|n| n.to_string_lossy().into_owned())
            .unwrap_or_default()
    };

    // Whisper: o modelo é solto ao fim deste bloco, antes de diarizar (VRAM).
    send(Progress::Stage(Stage::Transcribing));
    let words = {
        let mut model = asr::load_model(model_path).context("Falha ao carregar o modelo de transcrição")?;
        let mut notified = false;
        if model.device == Device::Cpu {
            send(Progress::Notice(CPU_NOTICE.into()));
            notified = true;
        }
        let vad = job.models_dir.join(VAD_MODEL_FILE);
        let ptx = tx.clone();
        let words = asr::transcribe(&mut model, model_path, &vad, &samples, move |f| {
            let _ = ptx.send(Progress::Fraction(f));
        })?;
        if !notified && model.device == Device::Cpu {
            send(Progress::Notice(CPU_NOTICE.into()));
        }
        words
    };

    send(Progress::Stage(Stage::Diarizing));
    let diarizer = make_diarizer()?;
    let segments = diarizer.diarize(&samples, &mut |f| send(Progress::Fraction(f)))?;

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
            Path::new("m.bin"),
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
        run_inner(job, Path::new("m.bin"), no_diarizer(), || Ok(()), tx);
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
