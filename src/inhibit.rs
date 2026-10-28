//! Impede o notebook de suspender enquanto grava ou processa (`systemd-inhibit`).

use std::os::unix::process::CommandExt;
use std::process::{Child, ChildStdin, Command, Stdio};

/// Enquanto existir, segura a suspensão e o bloqueio por ociosidade. Soltar = drop.
///
/// O filho do `systemd-inhibit` é um `cat` lendo de um pipe nosso: se o app morrer sem
/// passar pelo drop, o pipe fecha, o `cat` termina e o bloqueio some junto.
pub struct SleepGuard {
    child: Option<Child>,
    _stdin: Option<ChildStdin>,
}

impl SleepGuard {
    /// Falha ao iniciar (sem systemd, por exemplo) não é fatal: só não segura nada.
    pub fn acquire(why: &str) -> SleepGuard {
        let mut cmd = Command::new("systemd-inhibit");
        cmd.args([
            "--what=sleep:idle",
            "--who=Tone Dial",
            &format!("--why={why}"),
            "--mode=block",
            "cat",
        ]);
        Self::spawn(cmd)
    }

    fn spawn(mut cmd: Command) -> SleepGuard {
        // grupo próprio: no drop, o sinal vai para o systemd-inhibit e para o filho dele
        cmd.stdin(Stdio::piped())
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .process_group(0);
        match cmd.spawn() {
            Ok(mut child) => {
                let stdin = child.stdin.take();
                SleepGuard { child: Some(child), _stdin: stdin }
            }
            Err(e) => {
                eprintln!("não consegui impedir a suspensão: {e}");
                SleepGuard { child: None, _stdin: None }
            }
        }
    }

    pub fn pid(&self) -> Option<u32> {
        self.child.as_ref().map(Child::id)
    }
}

impl Drop for SleepGuard {
    fn drop(&mut self) {
        self._stdin = None;
        if let Some(mut child) = self.child.take() {
            // SAFETY: kill(2) com o id do grupo criado no spawn; sem memória envolvida.
            unsafe {
                libc::kill(-(child.id() as i32), libc::SIGTERM);
            }
            let _ = child.kill();
            let _ = child.wait();
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::process::Command;

    fn alive(pid: u32) -> bool {
        // kill(pid, 0): só verifica se o processo existe.
        unsafe { libc::kill(pid as i32, 0) == 0 }
    }

    #[test]
    fn drop_kills_and_reaps_the_child() {
        let mut cmd = Command::new("sleep");
        cmd.arg("30");
        let guard = SleepGuard::spawn(cmd);
        let pid = guard.pid().expect("sleep deveria iniciar");
        assert!(alive(pid));
        drop(guard);
        assert!(!alive(pid));
    }

    #[test]
    fn drop_kills_the_whole_group() {
        // O systemd-inhibit roda um filho; matar só ele deixaria o filho órfão.
        let d = tempfile::tempdir().unwrap();
        let pidfile = d.path().join("neto.pid");
        let mut cmd = Command::new("sh");
        cmd.arg("-c").arg(format!("sleep 30 & echo $! > {}; wait", pidfile.display()));
        let guard = SleepGuard::spawn(cmd);
        let deadline = std::time::Instant::now() + std::time::Duration::from_secs(5);
        let grandchild = loop {
            if let Some(p) = std::fs::read_to_string(&pidfile).ok().and_then(|s| s.trim().parse::<u32>().ok()) {
                break p;
            }
            assert!(std::time::Instant::now() < deadline, "neto não iniciou");
            std::thread::sleep(std::time::Duration::from_millis(10));
        };
        drop(guard);
        // o neto é filho do sh (já morto): vira zumbi do init até ser recolhido; espera sumir
        let deadline = std::time::Instant::now() + std::time::Duration::from_secs(5);
        while alive(grandchild) && std::time::Instant::now() < deadline {
            std::thread::sleep(std::time::Duration::from_millis(10));
        }
        assert!(!alive(grandchild));
    }

    #[test]
    fn spawn_failure_is_not_fatal() {
        let guard = SleepGuard::spawn(Command::new("/nao/existe/systemd-inhibit"));
        assert_eq!(guard.pid(), None);
        drop(guard);
    }
}
