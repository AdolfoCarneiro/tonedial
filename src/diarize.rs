use std::path::{Path, PathBuf};
use std::sync::Mutex;

use crate::transcript::SpeakerSegment;

pub trait Diarizer {
    fn diarize(
        &self,
        samples_16k: &[f32],
        on_progress: &mut dyn FnMut(f32),
    ) -> anyhow::Result<Vec<SpeakerSegment>>;
}

/// Abaixo deste pico absoluto o áudio é tratado como silêncio (nem carrega os modelos).
const SILENCE_PEAK: f32 = 1e-4;

fn is_silent(samples: &[f32]) -> bool {
    samples.iter().all(|s| s.abs() < SILENCE_PEAK)
}

fn secs_to_ms(s: f64) -> u64 {
    (s.max(0.0) * 1000.0).round() as u64
}

/// "SPEAKER_03" -> 3. Sem número no fim, usa o índice de primeira aparição.
fn parse_speaker(label: &str, seen: &mut Vec<String>) -> u32 {
    if let Some(n) = label.rsplit('_').next().and_then(|t| t.parse::<u32>().ok()) {
        return n;
    }
    match seen.iter().position(|l| l == label) {
        Some(i) => i as u32,
        None => {
            seen.push(label.to_string());
            (seen.len() - 1) as u32
        }
    }
}

fn sorted(mut segs: Vec<SpeakerSegment>) -> Vec<SpeakerSegment> {
    segs.sort_by_key(|s| (s.start_ms, s.end_ms, s.speaker));
    segs
}

/// Pipeline pyannote community-1 em Rust (speakrs), CPU nativo, sem ONNX Runtime.
/// `models_dir` precisa conter, lado a lado: segmentation-3.0.safetensors,
/// wespeaker-multimask-tail.safetensors, plda_{lda,tr,mu,psi,mean1,mean2}.npy e
/// wespeaker-voxceleb-resnet34.min_num_samples.txt. Os modelos só são carregados
/// na primeira diarização de áudio não silencioso.
pub struct SpeakrsDiarizer {
    models_dir: PathBuf,
    pipeline: Mutex<Option<speakrs::OwnedDiarizationPipeline>>,
}

impl SpeakrsDiarizer {
    pub fn new(models_dir: &Path) -> anyhow::Result<Self> {
        Ok(Self {
            models_dir: models_dir.to_path_buf(),
            pipeline: Mutex::new(None),
        })
    }
}

impl Diarizer for SpeakrsDiarizer {
    fn diarize(
        &self,
        samples_16k: &[f32],
        on_progress: &mut dyn FnMut(f32),
    ) -> anyhow::Result<Vec<SpeakerSegment>> {
        if samples_16k.is_empty() || is_silent(samples_16k) {
            on_progress(1.0);
            return Ok(vec![]);
        }
        on_progress(0.0);
        let mut guard = self
            .pipeline
            .lock()
            .map_err(|_| anyhow::anyhow!("diarizador em estado inválido"))?;
        if guard.is_none() {
            let p = speakrs::OwnedDiarizationPipeline::from_dir(
                &self.models_dir,
                speakrs::ExecutionMode::Cpu,
            )
            .map_err(|e| {
                anyhow::anyhow!(
                    "não foi possível carregar os modelos de separação de falantes em {}: {e}",
                    self.models_dir.display()
                )
            })?;
            *guard = Some(p);
        }
        let pipeline = guard.as_mut().expect("carregado acima");
        // speakrs não expõe progresso: 0.0 antes e 1.0 depois.
        let result = pipeline
            .run(samples_16k)
            .map_err(|e| anyhow::anyhow!("falha na separação de falantes: {e}"))?;
        let mut seen = Vec::new();
        let segs = result
            .segments
            .iter()
            .map(|s| SpeakerSegment {
                start_ms: secs_to_ms(s.start),
                end_ms: secs_to_ms(s.end),
                speaker: parse_speaker(&s.speaker, &mut seen),
            })
            .collect();
        on_progress(1.0);
        Ok(sorted(segs))
    }
}

/// Plano B (feature `sherpa`): sherpa-onnx com pyannote-segmentation-3.0 e um
/// embedding 3D-Speaker/NeMo. Número de falantes automático por limiar de clusterização.
#[cfg(feature = "sherpa")]
pub struct SherpaDiarizer {
    inner: sherpa_onnx::OfflineSpeakerDiarization,
}

#[cfg(feature = "sherpa")]
impl SherpaDiarizer {
    pub fn new(segmentation_model: &Path, embedding_model: &Path) -> anyhow::Result<Self> {
        use sherpa_onnx::*;
        let config = OfflineSpeakerDiarizationConfig {
            segmentation: OfflineSpeakerSegmentationModelConfig {
                pyannote: OfflineSpeakerSegmentationPyannoteModelConfig {
                    model: Some(segmentation_model.to_string_lossy().into_owned()),
                    ..Default::default()
                },
                num_threads: 4,
                ..Default::default()
            },
            embedding: SpeakerEmbeddingExtractorConfig {
                model: Some(embedding_model.to_string_lossy().into_owned()),
                num_threads: 4,
                ..Default::default()
            },
            clustering: FastClusteringConfig {
                num_clusters: -1,
                threshold: 0.5,
                ..Default::default()
            },
            ..Default::default()
        };
        let inner = OfflineSpeakerDiarization::create(&config)
            .ok_or_else(|| anyhow::anyhow!("não foi possível carregar os modelos do sherpa-onnx"))?;
        Ok(Self { inner })
    }
}

#[cfg(feature = "sherpa")]
impl Diarizer for SherpaDiarizer {
    fn diarize(
        &self,
        samples_16k: &[f32],
        on_progress: &mut dyn FnMut(f32),
    ) -> anyhow::Result<Vec<SpeakerSegment>> {
        if samples_16k.is_empty() || is_silent(samples_16k) {
            on_progress(1.0);
            return Ok(vec![]);
        }
        on_progress(0.0);
        let result = self
            .inner
            .process(samples_16k)
            .ok_or_else(|| anyhow::anyhow!("falha na separação de falantes (sherpa-onnx)"))?;
        let segs = result
            .sort_by_start_time()
            .into_iter()
            .map(|s| SpeakerSegment {
                start_ms: secs_to_ms(s.start as f64),
                end_ms: secs_to_ms(s.end as f64),
                speaker: s.speaker.max(0) as u32,
            })
            .collect();
        on_progress(1.0);
        Ok(sorted(segs))
    }
}

#[cfg(test)]
mod unit {
    use super::*;

    #[test]
    fn parses_speaker_labels() {
        let mut seen = Vec::new();
        assert_eq!(parse_speaker("SPEAKER_03", &mut seen), 3);
        assert_eq!(parse_speaker("alice", &mut seen), 0);
        assert_eq!(parse_speaker("bob", &mut seen), 1);
        assert_eq!(parse_speaker("alice", &mut seen), 0);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn empty_audio_gives_no_segments() {
        let dir = tempfile::tempdir().unwrap();
        let d = SpeakrsDiarizer::new(dir.path()).unwrap();
        let out = d.diarize(&vec![0.0; 16_000], &mut |_| {}).unwrap();
        assert!(out.is_empty());
    }

    #[test]
    #[ignore = "requer TONEDIAL_TEST_AUDIO (2+ vozes) e TONEDIAL_MODELS_DIR"]
    fn two_voices_give_two_speakers() {
        let wav = std::env::var("TONEDIAL_TEST_AUDIO").expect("TONEDIAL_TEST_AUDIO");
        let models = std::env::var("TONEDIAL_MODELS_DIR").expect("TONEDIAL_MODELS_DIR");
        let samples = crate::audio::decode::decode_16k_mono(Path::new(&wav)).unwrap();
        let d = SpeakrsDiarizer::new(Path::new(&models)).unwrap();
        let segs = d.diarize(&samples, &mut |_| {}).unwrap();
        let speakers: std::collections::HashSet<u32> = segs.iter().map(|s| s.speaker).collect();
        assert!(speakers.len() > 1, "{segs:?}");
        assert!(segs.windows(2).all(|w| w[0].start_ms <= w[1].start_ms));
    }
}
