use std::io::ErrorKind;
use std::path::Path;
use std::process::{Command, Output, Stdio};

use anyhow::{Result, anyhow, bail};
use chrono::{DateTime, NaiveDateTime};

const MISSING_FFMPEG: &str = "ffmpeg não encontrado. Instale com: sudo dnf install ffmpeg";

#[derive(Debug, Clone)]
pub struct ProbeInfo {
    pub duration_ms: u64,
    pub creation_time: Option<NaiveDateTime>,
}

fn ffmpeg_bin() -> String {
    std::env::var("TONEDIAL_FFMPEG").unwrap_or_else(|_| "ffmpeg".into())
}

fn ffprobe_bin() -> String {
    std::env::var("TONEDIAL_FFPROBE").unwrap_or_else(|_| "ffprobe".into())
}

fn file_name(path: &Path) -> String {
    path.file_name()
        .map(|n| n.to_string_lossy().into_owned())
        .unwrap_or_else(|| path.display().to_string())
}

fn unreadable(path: &Path) -> anyhow::Error {
    anyhow!("Não consegui ler o áudio: {}", file_name(path))
}

fn run(bin: &str, args: &[&std::ffi::OsStr]) -> Result<Output> {
    Command::new(bin)
        .args(args)
        .stdin(Stdio::null())
        .output()
        .map_err(|e| {
            if e.kind() == ErrorKind::NotFound {
                anyhow!(MISSING_FFMPEG)
            } else {
                anyhow!("Falha ao executar {bin}: {e}")
            }
        })
}

pub(crate) fn check_ffmpeg_at(bin: &str) -> Result<()> {
    let out = run(bin, &["-version".as_ref()])?;
    if !out.status.success() {
        bail!(MISSING_FFMPEG);
    }
    Ok(())
}

pub fn check_ffmpeg() -> Result<()> {
    check_ffmpeg_at(&ffmpeg_bin())?;
    let out = run(&ffprobe_bin(), &["-version".as_ref()])?;
    if !out.status.success() {
        bail!(MISSING_FFMPEG);
    }
    Ok(())
}

fn parse_creation_time<Tz: chrono::TimeZone>(s: &str, tz: &Tz) -> Option<NaiveDateTime> {
    DateTime::parse_from_rfc3339(s)
        .ok()
        .map(|dt| dt.with_timezone(tz).naive_local())
}

pub fn probe(path: &Path) -> Result<ProbeInfo> {
    let out = run(
        &ffprobe_bin(),
        &[
            "-v".as_ref(),
            "error".as_ref(),
            "-show_entries".as_ref(),
            "format=duration:format_tags=creation_time".as_ref(),
            "-of".as_ref(),
            "json".as_ref(),
            path.as_os_str(),
        ],
    )?;
    if !out.status.success() {
        return Err(unreadable(path));
    }
    let json: serde_json::Value =
        serde_json::from_slice(&out.stdout).map_err(|_| unreadable(path))?;
    let secs: f64 = json["format"]["duration"]
        .as_str()
        .and_then(|d| d.parse().ok())
        .ok_or_else(|| unreadable(path))?;
    if !secs.is_finite() || secs < 0.0 {
        return Err(unreadable(path));
    }
    let creation_time = json["format"]["tags"]["creation_time"]
        .as_str()
        .and_then(|t| parse_creation_time(t, &chrono::Local));
    Ok(ProbeInfo {
        duration_ms: (secs * 1000.0).round() as u64,
        creation_time,
    })
}

pub fn decode_16k_mono(path: &Path) -> Result<Vec<f32>> {
    let out = run(
        &ffmpeg_bin(),
        &[
            "-v".as_ref(),
            "error".as_ref(),
            "-i".as_ref(),
            path.as_os_str(),
            "-ac".as_ref(),
            "1".as_ref(),
            "-ar".as_ref(),
            "16000".as_ref(),
            "-f".as_ref(),
            "f32le".as_ref(),
            "-".as_ref(),
        ],
    )?;
    if !out.status.success() || out.stdout.is_empty() {
        return Err(unreadable(path));
    }
    Ok(out
        .stdout
        .chunks_exact(4)
        .map(|b| f32::from_le_bytes([b[0], b[1], b[2], b[3]]))
        .collect())
}

pub fn encode_ogg(wav: &Path, out: &Path) -> Result<()> {
    let res = run(
        &ffmpeg_bin(),
        &[
            "-v".as_ref(),
            "error".as_ref(),
            "-y".as_ref(),
            "-i".as_ref(),
            wav.as_os_str(),
            "-c:a".as_ref(),
            "libopus".as_ref(),
            "-b:a".as_ref(),
            "64k".as_ref(),
            "-ac".as_ref(),
            "1".as_ref(),
            out.as_os_str(),
        ],
    )?;
    if !res.status.success() {
        bail!(
            "Não consegui gerar o .ogg: {}",
            String::from_utf8_lossy(&res.stderr).trim()
        );
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::process::Command;

    fn fixture(dir: &Path) -> std::path::PathBuf {
        let p = dir.join("tone.m4a");
        let st = Command::new("ffmpeg")
            .args(["-v", "error", "-y", "-f", "lavfi", "-i", "sine=frequency=440:duration=2", "-ar", "44100", "-ac", "2"])
            .arg(&p)
            .status()
            .unwrap();
        assert!(st.success());
        p
    }

    #[test]
    fn decode_resamples_to_16k_mono() {
        let d = tempfile::tempdir().unwrap();
        let n = decode_16k_mono(&fixture(d.path())).unwrap().len();
        assert!((31_000..=33_000).contains(&n), "{n}");
    }

    #[test]
    fn probe_reads_duration() {
        let d = tempfile::tempdir().unwrap();
        let info = probe(&fixture(d.path())).unwrap();
        assert!((1900..=2100).contains(&info.duration_ms), "{}", info.duration_ms);
    }

    #[test]
    fn encode_ogg_roundtrip() {
        let d = tempfile::tempdir().unwrap();
        let src = fixture(d.path());
        let out = d.path().join("out.ogg");
        encode_ogg(&src, &out).unwrap();
        assert!(out.exists());
        let ms = probe(&out).unwrap().duration_ms;
        assert!((1900..=2100).contains(&ms), "{ms}");
    }

    #[test]
    fn creation_time_converted_to_given_zone() {
        let tz = chrono::FixedOffset::west_opt(3 * 3600).unwrap();
        let got = parse_creation_time("2026-10-28T00:30:00.000000Z", &tz).unwrap();
        let want = chrono::NaiveDate::from_ymd_opt(2026, 10, 27)
            .unwrap()
            .and_hms_opt(21, 30, 0)
            .unwrap();
        assert_eq!(got, want);
        assert_eq!(parse_creation_time("lixo", &tz), None);
        assert_eq!(parse_creation_time("", &tz), None);
    }

    #[test]
    fn missing_ffmpeg_message() {
        let e = check_ffmpeg_at("/nao/existe").unwrap_err();
        assert_eq!(e.to_string(), "ffmpeg não encontrado. Instale com: sudo dnf install ffmpeg");
    }

    #[test]
    fn unreadable_file_message() {
        let d = tempfile::tempdir().unwrap();
        let p = d.path().join("fake.m4a");
        std::fs::write(&p, "isto nao e audio").unwrap();
        let e = decode_16k_mono(&p).unwrap_err();
        assert!(e.to_string().starts_with("Não consegui ler o áudio"), "{e}");
    }
}
