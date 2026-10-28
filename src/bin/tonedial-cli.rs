//! CLI de desenvolvimento: `tonedial-cli <audio> [--out DIR] [--model ARQ] [--diarizer speakrs|sherpa]`
use std::path::PathBuf;
use std::process::ExitCode;
use std::sync::mpsc;

use tonedial::diarize::{Diarizer, SpeakrsDiarizer};
use tonedial::models::{WHISPER_MODEL_FILE, models_dir};
use tonedial::pipeline::{DiarizerFactory, Job, Progress, Source, Stage, run_with};

const USAGE: &str = "uso: tonedial-cli <audio> [--out DIR] [--model ARQ] [--diarizer speakrs|sherpa]";

/// Arquivos do sherpa dentro de models_dir (só com a feature `sherpa`).
#[cfg(feature = "sherpa")]
const SHERPA_SEGMENTATION: &str = "sherpa-segmentation.onnx";
#[cfg(feature = "sherpa")]
const SHERPA_EMBEDDING: &str = "sherpa-embedding.onnx";

fn parse() -> Result<(PathBuf, PathBuf, Option<PathBuf>, String), String> {
    let mut args = std::env::args().skip(1);
    let (mut audio, mut out, mut model, mut diarizer) = (None, PathBuf::from("."), None, "speakrs".to_string());
    while let Some(a) = args.next() {
        let mut value = |name: &str| args.next().ok_or_else(|| format!("{name} precisa de um valor\n{USAGE}"));
        match a.as_str() {
            "--out" => out = PathBuf::from(value("--out")?),
            "--model" => model = Some(PathBuf::from(value("--model")?)),
            "--diarizer" => diarizer = value("--diarizer")?,
            "-h" | "--help" => return Err(USAGE.into()),
            s if s.starts_with("--") => return Err(format!("opção desconhecida: {s}\n{USAGE}")),
            _ if audio.is_none() => audio = Some(PathBuf::from(a)),
            _ => return Err(format!("argumento a mais: {a}\n{USAGE}")),
        }
    }
    let audio = audio.ok_or_else(|| USAGE.to_string())?;
    Ok((audio, out, model, diarizer))
}

fn make_factory<'a>(kind: &str, dir: PathBuf) -> Result<DiarizerFactory<'a>, String> {
    match kind {
        "speakrs" => Ok(Box::new(move || Ok(Box::new(SpeakrsDiarizer::new(&dir)?) as Box<dyn Diarizer>))),
        #[cfg(feature = "sherpa")]
        "sherpa" => Ok(Box::new(move || {
            Ok(Box::new(tonedial::diarize::SherpaDiarizer::new(
                &dir.join(SHERPA_SEGMENTATION),
                &dir.join(SHERPA_EMBEDDING),
            )?) as Box<dyn Diarizer>)
        })),
        #[cfg(not(feature = "sherpa"))]
        "sherpa" => Err("este binário foi compilado sem a feature `sherpa` (cargo build --features sherpa)".into()),
        other => Err(format!("diarizador desconhecido: {other} (use speakrs ou sherpa)")),
    }
}

fn main() -> ExitCode {
    let (audio, out, model, kind) = match parse() {
        Ok(v) => v,
        Err(m) => {
            eprintln!("{m}");
            return ExitCode::from(2);
        }
    };
    let dir = models_dir();
    let factory = match make_factory(&kind, dir.clone()) {
        Ok(f) => f,
        Err(m) => {
            eprintln!("{m}");
            return ExitCode::from(2);
        }
    };
    let model = model.unwrap_or_else(|| dir.join(WHISPER_MODEL_FILE));
    let job = Job { source: Source::File(audio), output_dir: out, models_dir: dir };
    let (tx, rx) = mpsc::channel();
    let worker = std::thread::spawn(move || run_with(job, &model, factory, tx));
    let mut code = ExitCode::SUCCESS;
    // `run_with` sempre termina com Done/Failed; não dependemos do fechamento do canal.
    for p in rx {
        match p {
            Progress::Stage(s) => eprintln!(
                "\n[{}]",
                match s {
                    Stage::Preparing => "preparando",
                    Stage::Transcribing => "transcrevendo",
                    Stage::Diarizing => "separando falantes",
                    Stage::Writing => "escrevendo",
                }
            ),
            Progress::Fraction(f) => eprint!("\r  {:3.0}%", f * 100.0),
            Progress::Notice(n) => eprintln!("\naviso: {n}"),
            Progress::Done(o) => {
                eprintln!("\npronto: {} ({} falantes)", o.md_path.display(), o.speakers);
                println!("{}", o.md_path.display());
                break;
            }
            Progress::Failed(m) => {
                eprintln!("\nerro: {m}");
                code = ExitCode::FAILURE;
                break;
            }
        }
    }
    let _ = worker.join();
    code
}
