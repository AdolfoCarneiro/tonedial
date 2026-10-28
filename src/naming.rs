//! Nome dos arquivos de saída a partir da data da gravação.

use std::path::Path;

use anyhow::{Context, bail};
use chrono::{NaiveDate, NaiveDateTime};

/// Lê `n` dígitos ASCII em `b[*i..]` e avança `i`.
fn digits(b: &[u8], i: &mut usize, n: usize) -> Option<u32> {
    let chunk = b.get(*i..*i + n)?;
    if !chunk.iter().all(u8::is_ascii_digit) {
        return None;
    }
    *i += n;
    Some(chunk.iter().fold(0, |acc, d| acc * 10 + u32::from(d - b'0')))
}

/// Consome um separador opcional dentre `seps`.
fn skip_sep(b: &[u8], i: &mut usize, seps: &[u8]) {
    if b.get(*i).is_some_and(|c| seps.contains(c)) {
        *i += 1;
    }
}

fn parse_at(b: &[u8]) -> Option<NaiveDateTime> {
    let mut i = 0;
    let year = digits(b, &mut i, 4)?;
    skip_sep(b, &mut i, b"-_.");
    let month = digits(b, &mut i, 2)?;
    skip_sep(b, &mut i, b"-_.");
    let day = digits(b, &mut i, 2)?;
    skip_sep(b, &mut i, b"_- T");
    let hour = digits(b, &mut i, 2)?;
    skip_sep(b, &mut i, b":-.");
    let minute = digits(b, &mut i, 2)?;
    // Segundos são opcionais; só valem se vierem com 2 dígitos.
    let mut j = i;
    skip_sep(b, &mut j, b":-.");
    let second = digits(b, &mut j, 2).unwrap_or(0);
    NaiveDate::from_ymd_opt(i32::try_from(year).ok()?, month, day)?
        .and_hms_opt(hour, minute, second)
}

/// Procura uma data e hora no nome do arquivo (ex.: `20261028_143000.m4a`).
pub fn date_from_filename(name: &str) -> Option<NaiveDateTime> {
    let b = name.as_bytes();
    (0..b.len())
        .filter(|&i| i == 0 || !b[i - 1].is_ascii_digit())
        .find_map(|i| parse_at(&b[i..]))
}

/// Precedência: metadado de criação, depois nome do arquivo, depois mtime.
pub fn resolve_recorded_at(
    creation_time: Option<NaiveDateTime>,
    file_name: &str,
    mtime: NaiveDateTime,
) -> NaiveDateTime {
    creation_time
        .or_else(|| date_from_filename(file_name))
        .unwrap_or(mtime)
}

/// Stem `AAAA-MM-DD-HHMM`, com `-2`, `-3`... se `.md` ou `.ogg` já existir em `dir`.
pub fn unique_output_stem(dir: &Path, at: NaiveDateTime) -> anyhow::Result<String> {
    if !dir.is_dir() {
        bail!("a pasta de saída não existe: {}", dir.display());
    }
    let base = at.format("%Y-%m-%d-%H%M").to_string();
    let taken = |stem: &str| -> anyhow::Result<bool> {
        for ext in ["md", "ogg"] {
            let path = dir.join(format!("{stem}.{ext}"));
            if path
                .try_exists()
                .with_context(|| format!("não foi possível verificar {}", path.display()))?
            {
                return Ok(true);
            }
        }
        Ok(false)
    };
    if !taken(&base)? {
        return Ok(base);
    }
    for n in 2u32.. {
        let candidate = format!("{base}-{n}");
        if !taken(&candidate)? {
            return Ok(candidate);
        }
    }
    unreachable!()
}

#[cfg(test)]
mod tests {
    use super::*;
    use chrono::NaiveDate;

    fn dt(y: i32, mo: u32, d: u32, h: u32, mi: u32, s: u32) -> NaiveDateTime {
        NaiveDate::from_ymd_opt(y, mo, d)
            .unwrap()
            .and_hms_opt(h, mi, s)
            .unwrap()
    }

    #[test]
    fn parses_samsung_style() {
        assert_eq!(
            date_from_filename("20261028_143000.m4a"),
            Some(dt(2026, 10, 28, 14, 30, 0))
        );
    }

    #[test]
    fn parses_dashed_style() {
        assert_eq!(
            date_from_filename("Gravação 2026-10-28 14-30.m4a"),
            Some(dt(2026, 10, 28, 14, 30, 0))
        );
    }

    #[test]
    fn parses_other_styles() {
        assert_eq!(
            date_from_filename("20261028-143000.m4a"),
            Some(dt(2026, 10, 28, 14, 30, 0))
        );
        assert_eq!(
            date_from_filename("2026-10-28_14.30.15.mp3"),
            Some(dt(2026, 10, 28, 14, 30, 15))
        );
        assert_eq!(
            date_from_filename("2026-10-28T14:30.ogg"),
            Some(dt(2026, 10, 28, 14, 30, 0))
        );
    }

    #[test]
    fn no_date_returns_none() {
        assert_eq!(date_from_filename("reuniao.m4a"), None);
        assert_eq!(date_from_filename("20261328_143000.m4a"), None);
        assert_eq!(date_from_filename("20261028_256000.m4a"), None);
    }

    #[test]
    fn creation_time_wins() {
        let c = dt(2026, 1, 2, 3, 4, 5);
        let m = dt(2026, 5, 5, 5, 5, 5);
        assert_eq!(resolve_recorded_at(Some(c), "20261028_143000.m4a", m), c);
    }

    #[test]
    fn filename_beats_mtime() {
        let m = dt(2026, 5, 5, 5, 5, 5);
        assert_eq!(
            resolve_recorded_at(None, "20261028_143000.m4a", m),
            dt(2026, 10, 28, 14, 30, 0)
        );
        assert_eq!(resolve_recorded_at(None, "reuniao.m4a", m), m);
    }

    #[test]
    fn unique_first_is_plain() {
        let tmp = tempfile::tempdir().unwrap();
        let at = dt(2026, 10, 28, 14, 30, 0);
        assert_eq!(unique_output_stem(tmp.path(), at).unwrap(), "2026-10-28-1430");
    }

    #[test]
    fn unique_appends_suffix() {
        let tmp = tempfile::tempdir().unwrap();
        std::fs::write(tmp.path().join("2026-10-28-1430.md"), "").unwrap();
        std::fs::write(tmp.path().join("2026-10-28-1430-2.ogg"), "").unwrap();
        let at = dt(2026, 10, 28, 14, 30, 0);
        assert_eq!(
            unique_output_stem(tmp.path(), at).unwrap(),
            "2026-10-28-1430-3"
        );
    }

    #[test]
    fn unique_errors_on_missing_dir() {
        let tmp = tempfile::tempdir().unwrap();
        let at = dt(2026, 10, 28, 14, 30, 0);
        assert!(unique_output_stem(&tmp.path().join("nao-existe"), at).is_err());
    }
}
