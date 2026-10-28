use std::path::PathBuf;

pub const WHISPER_MODEL_FILE: &str = "ggml-large-v3-q8_0.bin";
pub const VAD_MODEL_FILE: &str = "ggml-silero-v5.1.2.bin";

/// `~/.local/share/tonedial/models` (via `dirs::data_dir()`).
pub fn models_dir() -> PathBuf {
    dirs::data_dir()
        .unwrap_or_else(|| PathBuf::from("."))
        .join("tonedial")
        .join("models")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn models_dir_ends_with_tonedial_models() {
        assert!(models_dir().ends_with("tonedial/models"));
    }
}
