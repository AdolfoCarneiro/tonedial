//! Instância única: a primeira escuta num socket Unix; as outras só pedem para mostrar a janela.

use std::io::{ErrorKind, Write};
use std::os::unix::net::{UnixListener, UnixStream};
use std::path::{Path, PathBuf};

/// `$XDG_RUNTIME_DIR/tonedial.sock`.
pub fn socket_path() -> PathBuf {
    dirs::runtime_dir().unwrap_or_else(std::env::temp_dir).join("tonedial.sock")
}

pub enum Instance {
    Primary(UnixListener),
    /// Já existe um Tone Dial rodando; ele foi avisado para mostrar a janela.
    Secondary,
}

pub fn acquire(path: &Path) -> std::io::Result<Instance> {
    match UnixListener::bind(path) {
        Ok(l) => Ok(Instance::Primary(l)),
        Err(e) if e.kind() == ErrorKind::AddrInUse => match UnixStream::connect(path) {
            Ok(mut s) => {
                // a conexão já é o aviso; a primária pode fechar antes de ler (EPIPE)
                let _ = s.write_all(b"show\n");
                Ok(Instance::Secondary)
            }
            // ninguém escutando: sobrou de um processo que morreu
            Err(_) => {
                std::fs::remove_file(path)?;
                Ok(Instance::Primary(UnixListener::bind(path)?))
            }
        },
        Err(e) => Err(e),
    }
}

/// Numa thread: cada conexão de outra instância chama `on_show`.
pub fn serve(listener: UnixListener, on_show: impl Fn() + Send + 'static) {
    std::thread::spawn(move || {
        for conn in listener.incoming() {
            if conn.is_ok() {
                on_show();
            }
        }
    });
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::Arc;
    use std::sync::atomic::{AtomicUsize, Ordering};
    use std::time::{Duration, Instant};

    #[test]
    fn primeira_instancia_e_primaria_e_a_segunda_avisa() {
        let d = tempfile::tempdir().unwrap();
        let path = d.path().join("tonedial.sock");
        let Instance::Primary(listener) = acquire(&path).unwrap() else {
            panic!("a primeira deveria ser primária");
        };
        let shows = Arc::new(AtomicUsize::new(0));
        let s = shows.clone();
        serve(listener, move || {
            s.fetch_add(1, Ordering::SeqCst);
        });
        assert!(matches!(acquire(&path).unwrap(), Instance::Secondary));
        let t = Instant::now();
        while shows.load(Ordering::SeqCst) == 0 && t.elapsed() < Duration::from_secs(1) {
            std::thread::sleep(Duration::from_millis(10));
        }
        assert_eq!(shows.load(Ordering::SeqCst), 1);
    }

    #[test]
    fn socket_velho_e_reaproveitado() {
        let d = tempfile::tempdir().unwrap();
        let path = d.path().join("tonedial.sock");
        drop(std::os::unix::net::UnixListener::bind(&path).unwrap());
        assert!(path.exists(), "o arquivo do socket fica para trás");
        assert!(matches!(acquire(&path).unwrap(), Instance::Primary(_)));
    }
}
