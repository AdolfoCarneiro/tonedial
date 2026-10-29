use chrono::NaiveDateTime;

#[derive(Debug, Clone, PartialEq)]
pub struct Word {
    pub start_ms: u64,
    pub end_ms: u64,
    pub text: String,
}

#[derive(Debug, Clone, PartialEq)]
pub struct SpeakerSegment {
    pub start_ms: u64,
    pub end_ms: u64,
    pub speaker: u32,
}

#[derive(Debug, Clone, PartialEq)]
pub struct Utterance {
    pub speaker: u32,
    pub start_ms: u64,
    pub text: String,
}

#[derive(Debug, Clone, PartialEq)]
pub struct Header {
    pub recorded_at: NaiveDateTime,
    pub duration_ms: u64,
    pub source_name: String,
}

pub(crate) fn normalize(text: &str) -> String {
    text.chars()
        .filter(|c| !c.is_ascii_punctuation() && !c.is_whitespace())
        .flat_map(|c| c.to_lowercase())
        .collect()
}

/// Remove loops de repetição (alucinação típica do whisper em silêncio):
/// um n-grama de 1 a 16 palavras fica só com a primeira ocorrência quando se repete em sequência:
/// n ≤ 2 só com mais de 3 repetições (preserva "sim sim", "não, não, não"); n ≥ 3 com 3 ou mais.
pub fn dedupe_repeats(words: Vec<Word>) -> Vec<Word> {
    const MAX_N: usize = 16;
    let norm: Vec<String> = words.iter().map(|w| normalize(&w.text)).collect();
    let mut keep = vec![true; words.len()];
    let mut i = 0;
    while i < words.len() {
        let mut skipped = false;
        for n in 1..=MAX_N {
            if i + n > words.len() {
                break;
            }
            let mut reps = 1;
            while i + (reps + 1) * n <= words.len()
                && norm[i..i + n] == norm[i + reps * n..i + (reps + 1) * n]
            {
                reps += 1;
            }
            let min_reps = if n <= 2 { 4 } else { 3 };
            if reps >= min_reps {
                for k in keep.iter_mut().take(i + reps * n).skip(i + n) {
                    *k = false;
                }
                i += reps * n;
                skipped = true;
                break;
            }
        }
        if !skipped {
            i += 1;
        }
    }
    words
        .into_iter()
        .zip(keep)
        .filter_map(|(w, k)| k.then_some(w))
        .collect()
}

/// Atribui cada palavra ao segmento de maior sobreposição (ou ao mais próximo).
fn pick_segment(word: &Word, segments: &[SpeakerSegment]) -> u32 {
    let mut best: Option<(&SpeakerSegment, u64)> = None;
    for s in segments {
        let overlap = word.end_ms.min(s.end_ms).saturating_sub(word.start_ms.max(s.start_ms));
        let better = match best {
            None => true,
            Some((b, bo)) => overlap > bo || (overlap == bo && s.start_ms < b.start_ms),
        };
        if better {
            best = Some((s, overlap));
        }
    }
    if let Some((s, o)) = best
        && o > 0
    {
        return s.speaker;
    }
    // Sem sobreposição: o mais próximo (empate: o que começa primeiro).
    let dist = |s: &SpeakerSegment| {
        if s.end_ms <= word.start_ms {
            word.start_ms - s.end_ms
        } else {
            s.start_ms.saturating_sub(word.end_ms)
        }
    };
    segments
        .iter()
        .min_by_key(|s| (dist(s), s.start_ms))
        .map(|s| s.speaker)
        .unwrap_or(0)
}

pub fn align(words: &[Word], segments: &[SpeakerSegment]) -> Vec<Utterance> {
    let mut order: Vec<u32> = Vec::new();
    let mut raw: Vec<(u32, u64, String)> = Vec::new();
    for w in words {
        let id = pick_segment(w, segments);
        if !order.contains(&id) {
            order.push(id);
        }
        match raw.last_mut() {
            Some((last, _, text)) if *last == id => {
                text.push(' ');
                text.push_str(&w.text);
            }
            _ => raw.push((id, w.start_ms, w.text.clone())),
        }
    }
    raw.into_iter()
        .map(|(id, start_ms, text)| Utterance {
            speaker: order.iter().position(|&x| x == id).unwrap() as u32 + 1,
            start_ms,
            text: text.split_whitespace().collect::<Vec<_>>().join(" "),
        })
        .collect()
}

pub fn format_ts(ms: u64) -> String {
    let s = ms / 1000;
    format!("{:02}:{:02}:{:02}", s / 3600, (s / 60) % 60, s % 60)
}

fn format_duration(ms: u64) -> String {
    let minutes = ms / 60_000;
    if minutes >= 60 {
        format!("{}h{:02}min", minutes / 60, minutes % 60)
    } else {
        format!("{minutes}min")
    }
}

pub fn render_markdown(header: &Header, utterances: &[Utterance]) -> String {
    let mut out = format!(
        "# Transcrição {}\n\n- Duração: {}\n- Áudio: {}\n",
        header.recorded_at.format("%Y-%m-%d %H:%M"),
        format_duration(header.duration_ms),
        header.source_name
    );
    for u in utterances {
        out.push_str(&format!(
            "\n**Falante {}** [{}]: {}\n",
            u.speaker,
            format_ts(u.start_ms),
            u.text
        ));
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;
    use chrono::NaiveDate;

    fn word(start: u64, end: u64, text: &str) -> Word {
        Word { start_ms: start, end_ms: end, text: text.to_string() }
    }
    fn seg(start: u64, end: u64, speaker: u32) -> SpeakerSegment {
        SpeakerSegment { start_ms: start, end_ms: end, speaker }
    }

    #[test]
    fn align_assigns_by_max_overlap() {
        let u = align(&[word(1000, 2000, "x")], &[seg(0, 1200, 1), seg(1100, 2500, 2)]);
        assert_eq!(u.len(), 1);
        // B is the only one seen, renumbered to 1; check via two-word case below
        let u2 = align(
            &[word(0, 500, "a"), word(1000, 2000, "x")],
            &[seg(0, 1200, 10), seg(1100, 2500, 20)],
        );
        assert_eq!(u2.len(), 2);
        assert_eq!(u2[0].text, "a");
        assert_eq!(u2[1].text, "x");
        assert_eq!(u2[1].speaker, 2);
    }

    #[test]
    fn align_renumbers_by_first_appearance() {
        let words = [word(0, 100, "a"), word(200, 300, "b"), word(400, 500, "c")];
        let segs = [seg(0, 150, 7), seg(150, 350, 3), seg(350, 600, 7)];
        let u = align(&words, &segs);
        let ids: Vec<u32> = u.iter().map(|x| x.speaker).collect();
        assert_eq!(ids, vec![1, 2, 1]);
    }

    #[test]
    fn align_merges_consecutive_same_speaker() {
        let words = [word(0, 100, "a"), word(100, 200, " b"), word(200, 300, " c")];
        let u = align(&words, &[seg(0, 1000, 5)]);
        assert_eq!(u.len(), 1);
        assert_eq!(u[0].text, "a b c");
        assert_eq!(u[0].start_ms, 0);
    }

    #[test]
    fn align_without_segments_is_speaker_1() {
        let u = align(&[word(0, 100, "a"), word(100, 200, " b")], &[]);
        assert_eq!(u.len(), 1);
        assert_eq!(u[0].speaker, 1);
    }

    #[test]
    fn align_word_in_gap_goes_to_nearest() {
        let u = align(
            &[word(0, 100, "a"), word(3000, 3100, "b")],
            &[seg(0, 1000, 4), seg(3500, 4000, 9)],
        );
        assert_eq!(u.len(), 2);
        assert_eq!(u[1].speaker, 2);
    }

    #[test]
    fn dedupe_collapses_loop() {
        let w: Vec<Word> = (0..20).map(|i| word(i * 10, i * 10 + 10, " obrigado")).collect();
        assert_eq!(dedupe_repeats(w).len(), 1);

        let mut w = Vec::new();
        for i in 0..6u64 {
            for (j, t) in ["a", " b", " c"].iter().enumerate() {
                let s = (i * 3 + j as u64) * 10;
                w.push(word(s, s + 10, t));
            }
        }
        let r = dedupe_repeats(w);
        let txt: String = r.iter().map(|x| x.text.as_str()).collect();
        assert_eq!(txt, "a b c");

        let w = vec![word(0, 10, "sim"), word(10, 20, " sim")];
        assert_eq!(dedupe_repeats(w.clone()), w);
    }

    fn words_of(text: &str, reps: usize) -> Vec<Word> {
        let mut out = Vec::new();
        for _ in 0..reps {
            for t in text.split_whitespace() {
                let s = out.len() as u64 * 10;
                out.push(word(s, s + 10, t));
            }
        }
        out
    }
    fn join(w: &[Word]) -> String {
        w.iter().map(|x| x.text.as_str()).collect::<Vec<_>>().join(" ")
    }

    #[test]
    fn dedupe_collapses_long_phrase_repeated_three_times() {
        let mut w = words_of("Ah, eu já estou acabando com as minhas reservas.", 1);
        w.extend(words_of("ah eu já estou acabando com as minhas reservas", 1));
        w.extend(words_of("Ah, eu já estou acabando com as minhas reservas.", 1));
        let r = dedupe_repeats(w);
        assert_eq!(join(&r), "Ah, eu já estou acabando com as minhas reservas.");
    }

    #[test]
    fn dedupe_keeps_phrase_repeated_twice() {
        let w = words_of("não sei bem", 2);
        assert_eq!(dedupe_repeats(w.clone()), w);
    }

    #[test]
    fn dedupe_keeps_short_triple() {
        let w = words_of("não, não, não", 1);
        assert_eq!(dedupe_repeats(w.clone()), w);
        let w = words_of("sim", 3);
        assert_eq!(dedupe_repeats(w.clone()), w);
        assert_eq!(dedupe_repeats(words_of("sim", 4)).len(), 1);
    }

    #[test]
    fn dedupe_is_fast_on_large_input() {
        let w: Vec<Word> = (0..20_000u64).map(|i| word(i, i + 1, &format!("w{i}"))).collect();
        let t = std::time::Instant::now();
        let r = dedupe_repeats(w);
        assert_eq!(r.len(), 20_000);
        assert!(t.elapsed().as_secs() < 2);
    }

    #[test]
    fn align_joins_words_without_leading_spaces() {
        let w = [word(0, 10, "Bom,"), word(10, 20, "vocês"), word(20, 30, "poderiam")];
        let u = align(&w, &[seg(0, 1000, 1)]);
        assert_eq!(u[0].text, "Bom, vocês poderiam");
    }

    #[test]
    fn format_ts_pads() {
        assert_eq!(format_ts(3_723_000), "01:02:03");
        assert_eq!(format_ts(0), "00:00:00");
    }

    #[test]
    fn render_markdown_matches_format() {
        let header = Header {
            recorded_at: NaiveDate::from_ymd_opt(2026, 10, 28).unwrap().and_hms_opt(14, 30, 0).unwrap(),
            duration_ms: 6_720_000,
            source_name: "reuniao.m4a".to_string(),
        };
        let utts = [
            Utterance { speaker: 1, start_ms: 3000, text: "Bom dia, pessoal.".into() },
            Utterance { speaker: 2, start_ms: 7000, text: "Bom dia.".into() },
        ];
        let expected = "# Transcrição 2026-10-28 14:30\n\n- Duração: 1h52min\n- Áudio: reuniao.m4a\n\n**Falante 1** [00:00:03]: Bom dia, pessoal.\n\n**Falante 2** [00:00:07]: Bom dia.\n";
        assert_eq!(render_markdown(&header, &utts), expected);

        let short = Header { duration_ms: 52 * 60_000, ..header };
        assert!(render_markdown(&short, &[]).contains("- Duração: 52min\n"));
    }
}
