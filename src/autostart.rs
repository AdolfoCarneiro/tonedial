//! Inicia o Tone Dial em segundo plano no login (`~/.config/autostart`).

use std::path::{Component, Path, PathBuf};

const DESKTOP: &str = include_str!("../assets/tonedial.desktop");
/// Marca do arquivo gerado pelo app. Sem ela, o arquivo é do usuário e não é tocado.
const MARKER: &str = "X-Tonedial-Generated=true";

pub fn default_dir() -> Option<PathBuf> {
    dirs::config_dir().map(|d| d.join("autostart"))
}

/// Executável de build do cargo (`target/debug`, `target/release`...): não vai para o autostart.
fn is_dev_build(exe: &Path) -> bool {
    exe.components().any(|c| c == Component::Normal("target".as_ref()))
}

fn exec_line(exe: &Path) -> String {
    format!("Exec=\"{}\" --background", exe.display())
}

/// Cria `tonedial.desktop` se não existir. Se existe e foi gerado pelo app (tem a marca)
/// com outro executável, reescreve com o atual. Arquivo sem a marca ou com `Hidden=true`
/// (o usuário mexeu ou desligou) fica como está. Build do cargo não cria nem reescreve.
/// `true` se escreveu.
pub fn ensure(autostart_dir: &Path, exe: &Path) -> std::io::Result<bool> {
    if is_dev_build(exe) {
        return Ok(false);
    }
    let file = autostart_dir.join("tonedial.desktop");
    if file.exists() {
        let text = std::fs::read_to_string(&file)?;
        let ours = text.lines().any(|l| l.trim() == MARKER);
        let hidden = text.lines().any(|l| l.trim() == "Hidden=true");
        let same = text.lines().any(|l| l.trim() == exec_line(exe));
        if !ours || hidden || same {
            return Ok(false);
        }
    }
    std::fs::create_dir_all(autostart_dir)?;
    std::fs::write(&file, desktop_entry(exe))?;
    Ok(true)
}

fn desktop_entry(exe: &Path) -> String {
    let mut out = String::new();
    for line in DESKTOP.lines() {
        if line.starts_with("Exec=") {
            out.push_str(&exec_line(exe));
        } else {
            out.push_str(line);
        }
        out.push('\n');
    }
    out.push_str("X-GNOME-Autostart-enabled=true\n");
    out.push_str(MARKER);
    out.push('\n');
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
        assert!(text.contains(MARKER));
        assert_eq!(text.matches("Exec=").count(), 1);

        // mesmo executável: não reescreve
        assert!(!ensure(&dir, exe).unwrap());

        // editado à mão (sem a marca): não toca, nem com outro executável
        std::fs::write(&file, "editado à mão").unwrap();
        assert!(!ensure(&dir, exe).unwrap());
        assert!(!ensure(&dir, Path::new("/usr/bin/tonedial")).unwrap());
        assert_eq!(std::fs::read_to_string(&file).unwrap(), "editado à mão");
    }

    #[test]
    fn reescreve_o_gerado_quando_o_executavel_muda() {
        let d = tempfile::tempdir().unwrap();
        let dir = d.path().join("autostart");
        assert!(ensure(&dir, Path::new("/opt/velho/tonedial")).unwrap());
        assert!(ensure(&dir, Path::new("/home/x/.cargo/bin/tonedial")).unwrap());
        let text = std::fs::read_to_string(dir.join("tonedial.desktop")).unwrap();
        assert!(text.contains("Exec=\"/home/x/.cargo/bin/tonedial\" --background\n"), "{text}");
        assert_eq!(text.matches("Exec=").count(), 1);
    }

    #[test]
    fn gerado_mas_desligado_nao_e_reescrito() {
        let d = tempfile::tempdir().unwrap();
        let dir = d.path().join("autostart");
        let file = dir.join("tonedial.desktop");
        assert!(ensure(&dir, Path::new("/opt/velho/tonedial")).unwrap());
        let off = format!("{}Hidden=true\n", std::fs::read_to_string(&file).unwrap());
        std::fs::write(&file, &off).unwrap();
        assert!(!ensure(&dir, Path::new("/home/x/.cargo/bin/tonedial")).unwrap());
        assert_eq!(std::fs::read_to_string(&file).unwrap(), off);
    }

    #[test]
    fn build_do_cargo_nao_cria_nem_reescreve() {
        let d = tempfile::tempdir().unwrap();
        let dir = d.path().join("autostart");
        let dev = Path::new("/home/x/src/tonedial/target/debug/tonedial");
        assert!(!ensure(&dir, dev).unwrap());
        assert!(!dir.join("tonedial.desktop").exists());

        assert!(ensure(&dir, Path::new("/home/x/.cargo/bin/tonedial")).unwrap());
        assert!(!ensure(&dir, Path::new("/home/x/src/tonedial/target/release/tonedial")).unwrap());
        let text = std::fs::read_to_string(dir.join("tonedial.desktop")).unwrap();
        assert!(text.contains("/home/x/.cargo/bin/tonedial"), "{text}");
    }
}
