use std::path::{Path, PathBuf};

use anyhow::Context;
use serde::{Deserialize, Serialize};

#[derive(Serialize, Deserialize, Default, Debug, Clone, PartialEq)]
pub struct Config {
    pub output_dir: Option<PathBuf>,
    pub last_mic: Option<String>,
    /// Gravar também o som do computador (call).
    #[serde(default)]
    pub record_system: bool,
}

impl Config {
    /// Caminho padrão: `~/.config/tonedial/config.json`.
    fn default_path() -> Option<PathBuf> {
        dirs::config_dir().map(|d| d.join("tonedial").join("config.json"))
    }

    /// Arquivo ausente ou JSON inválido viram `Config::default()`, sem erro.
    pub fn load() -> Config {
        Self::default_path()
            .map(|p| Self::load_from(&p))
            .unwrap_or_default()
    }

    pub fn load_from(p: &Path) -> Config {
        std::fs::read_to_string(p)
            .ok()
            .and_then(|s| serde_json::from_str(&s).ok())
            .unwrap_or_default()
    }

    pub fn save(&self) -> anyhow::Result<()> {
        let p = Self::default_path()
            .ok_or_else(|| anyhow::anyhow!("não foi possível achar a pasta de configuração"))?;
        self.save_to(&p)
    }

    pub fn save_to(&self, p: &Path) -> anyhow::Result<()> {
        if let Some(dir) = p.parent() {
            std::fs::create_dir_all(dir)
                .with_context(|| format!("não foi possível criar {}", dir.display()))?;
        }
        let json = serde_json::to_string_pretty(self)?;
        std::fs::write(p, json)
            .with_context(|| format!("não foi possível salvar {}", p.display()))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn config_roundtrip() {
        let d = std::env::temp_dir().join(format!("td-cfg-{}", std::process::id()));
        let p = d.join("sub/config.json");
        let c = Config {
            output_dir: Some("/x/y".into()),
            last_mic: Some("Mic".into()),
            record_system: true,
        };
        c.save_to(&p).unwrap();
        assert_eq!(Config::load_from(&p), c);
        let _ = std::fs::remove_dir_all(&d);
    }

    #[test]
    fn config_bad_json_is_default() {
        let d = std::env::temp_dir().join(format!("td-cfg-bad-{}", std::process::id()));
        std::fs::create_dir_all(&d).unwrap();
        let p = d.join("config.json");
        std::fs::write(&p, "{ não é json").unwrap();
        assert_eq!(Config::load_from(&p), Config::default());
        assert_eq!(Config::load_from(&d.join("nao-existe.json")), Config::default());
        let _ = std::fs::remove_dir_all(&d);
    }

    #[test]
    fn config_antigo_sem_record_system_carrega() {
        let d = tempfile::tempdir().unwrap();
        let p = d.path().join("config.json");
        std::fs::write(&p, r#"{"output_dir":"/x","last_mic":null}"#).unwrap();
        let c = Config::load_from(&p);
        assert_eq!(c.output_dir.as_deref(), Some(std::path::Path::new("/x")));
        assert!(!c.record_system);
    }
}
