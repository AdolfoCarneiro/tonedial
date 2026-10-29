//! Inicia o Tone Dial em segundo plano no login (`~/.config/autostart`).

use std::path::{Path, PathBuf};

const DESKTOP: &str = include_str!("../assets/tonedial.desktop");

pub fn default_dir() -> Option<PathBuf> {
    dirs::config_dir().map(|d| d.join("autostart"))
}

/// Cria `tonedial.desktop` só se não existir (o usuário pode ter editado ou desligado).
/// `true` se criou.
pub fn ensure(autostart_dir: &Path, exe: &Path) -> std::io::Result<bool> {
    let file = autostart_dir.join("tonedial.desktop");
    if file.exists() {
        return Ok(false);
    }
    std::fs::create_dir_all(autostart_dir)?;
    std::fs::write(&file, desktop_entry(exe))?;
    Ok(true)
}

fn desktop_entry(exe: &Path) -> String {
    let mut out = String::new();
    for line in DESKTOP.lines() {
        if line.starts_with("Exec=") {
            out.push_str(&format!("Exec=\"{}\" --background\n", exe.display()));
        } else {
            out.push_str(line);
            out.push('\n');
        }
    }
    out.push_str("X-GNOME-Autostart-enabled=true\n");
    out
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::path::Path;

    #[test]
    fn cria_o_desktop_de_autostart_uma_vez() {
        let d = tempfile::tempdir().unwrap();
        let dir = d.path().join("autostart");
        let exe = Path::new("/home/x/.cargo/bin/tonedial");
        assert!(ensure(&dir, exe).unwrap());
        let file = dir.join("tonedial.desktop");
        let text = std::fs::read_to_string(&file).unwrap();
        assert!(text.contains("Exec=\"/home/x/.cargo/bin/tonedial\" --background\n"), "{text}");
        assert!(text.contains("X-GNOME-Autostart-enabled=true"));
        assert!(text.contains("Name=Tone Dial"));
        assert_eq!(text.matches("Exec=").count(), 1);

        std::fs::write(&file, "editado à mão").unwrap();
        assert!(!ensure(&dir, exe).unwrap());
        assert_eq!(std::fs::read_to_string(&file).unwrap(), "editado à mão");
    }
}
