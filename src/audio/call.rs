use std::path::{Path, PathBuf};

use anyhow::Context;
use chrono::NaiveDateTime;

use super::record::{Recorder, temp_call_paths};
use super::system::SystemRecorder;

/// Mic + som do computador, cada um no seu WAV. Se o som do computador não abrir,
/// grava só o mic e deixa o motivo em `system_note`.
pub struct CallRecorder {
    pub mic: Recorder,
    pub system: Option<SystemRecorder>,
    pub system_note: Option<String>,
}

impl CallRecorder {
    pub fn start(device: &str, dir: &Path, at: NaiveDateTime) -> anyhow::Result<CallRecorder> {
        std::fs::create_dir_all(dir).with_context(|| format!("não foi possível criar {}", dir.display()))?;
        let (mic_path, system_path) = temp_call_paths(dir, at);
        let mic = Recorder::start(device, mic_path)?;
        // o instante do primeiro bloco do mic é o zero dos dois WAVs
        let (system, system_note) = match SystemRecorder::start(system_path, mic.started_instant()) {
            Ok(s) => (Some(s), None),
            Err(_) => (
                None,
                Some("Não consegui gravar o som do computador; gravando só o microfone".to_string()),
            ),
        };
        Ok(CallRecorder { mic, system, system_note })
    }

    /// Aviso para mostrar durante a gravação.
    pub fn note(&self) -> Option<String> {
        if let Some(n) = &self.system_note {
            return Some(n.clone());
        }
        let at = self.system.as_ref()?.stopped_at()?;
        Some(format!("O som do computador parou de ser gravado às {}", at.format("%H:%M")))
    }

    /// Finaliza os dois WAVs: (mic, sistema).
    pub fn stop(self) -> anyhow::Result<(PathBuf, Option<PathBuf>)> {
        let CallRecorder { mic, system, .. } = self;
        // o som do computador primeiro: se falhar, o mic ainda é finalizado ao ser descartado
        let sys = match system {
            Some(s) => s.stop().ok(),
            None => None,
        };
        let mic = mic.stop()?;
        Ok((mic, sys))
    }
}
