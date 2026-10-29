//! Notificações do KDE com botões. Cada uma roda numa thread própria, porque esperar o
//! clique bloqueia.

use std::path::{Path, PathBuf};
use std::sync::mpsc::Sender;

use notify_rust::{Hint, Notification};

use crate::events::AppEvent;

/// "O Opera está usando o microfone. Gravar a call?"
pub fn start_body(app: Option<&str>) -> String {
    match app {
        Some(a) => format!("O {a} está usando o microfone. Gravar a call?"),
        None => "Parece que você está numa call. Gravar?".to_string(),
    }
}

fn base(summary: &str, body: &str) -> Notification {
    let mut n = Notification::new();
    n.appname("Tone Dial")
        .icon("tonedial")
        .hint(Hint::DesktopEntry("tonedial".into()))
        .summary(summary)
        .body(body);
    n
}

/// Mostra e espera o clique numa thread; `on_action` recebe o id do botão.
fn show(n: Notification, on_action: impl FnOnce(&str) + Send + 'static) {
    std::thread::spawn(move || match n.show() {
        Ok(h) => h.wait_for_action(on_action),
        Err(e) => eprintln!("notificação: {e}"),
    });
}

pub fn ask_start(app: Option<&str>, tx: Sender<AppEvent>) {
    let mut n = base("Tone Dial", &start_body(app));
    n.action("gravar", "Gravar").action("agora-nao", "Agora não");
    show(n, move |a| {
        if a == "gravar" {
            let _ = tx.send(AppEvent::RecordCall);
        }
    });
}

pub fn ask_end(generation: u64, tx: Sender<AppEvent>) {
    let mut n = base("Tone Dial", "A call acabou? Parar e processar?");
    n.action("parar", "Parar").action("continuar", "Continuar gravando");
    show(n, move |a| {
        let stop = match a {
            "parar" => true,
            "continuar" => false,
            _ => return,
        };
        let _ = tx.send(AppEvent::EndAnswer { generation, stop });
    });
}

pub fn done(md: &Path, notices: &[String]) {
    let name = md.file_name().map(|n| n.to_string_lossy().into_owned()).unwrap_or_default();
    let mut n = base(&format!("Transcrição pronta: {name}"), &notices.join("\n"));
    n.action("texto", "Abrir texto").action("pasta", "Abrir pasta");
    let md = md.to_path_buf();
    show(n, move |a| {
        let target: PathBuf = match a {
            "texto" => md,
            "pasta" => md.parent().map(Path::to_path_buf).unwrap_or(md),
            _ => return,
        };
        if let Err(e) = open::that_detached(&target) {
            eprintln!("não consegui abrir {}: {e}", target.display());
        }
    });
}

/// Algo deu errado com a janela fechada; o botão abre a janela (na tela de erro).
fn with_open_button(summary: &str, msg: &str, tx: Sender<AppEvent>) {
    let mut n = base(summary, msg);
    n.action("abrir", "Abrir Tone Dial");
    show(n, move |a| {
        if a == "abrir" || a == "default" {
            let _ = tx.send(AppEvent::ShowWindow);
        }
    });
}

pub fn failed(msg: &str, tx: Sender<AppEvent>) {
    with_open_button("Não consegui transcrever a gravação", msg, tx);
}

pub fn record_failed(msg: &str, tx: Sender<AppEvent>) {
    with_open_button("Não consegui gravar a call", msg, tx);
}

/// O que o `App` usa para avisar; nos testes, uma implementação falsa sem D-Bus.
pub trait Notifier {
    fn ask_start(&self, app: Option<&str>);
    fn ask_end(&self, generation: u64);
    fn done(&self, md: &Path, notices: &[String]);
    fn failed(&self, msg: &str);
    /// Gravação pedida pela bandeja ou notificação não começou.
    fn record_failed(&self, msg: &str);
}

pub struct DesktopNotifier {
    pub tx: Sender<AppEvent>,
}

impl Notifier for DesktopNotifier {
    fn ask_start(&self, app: Option<&str>) {
        ask_start(app, self.tx.clone());
    }
    fn ask_end(&self, generation: u64) {
        ask_end(generation, self.tx.clone());
    }
    fn done(&self, md: &Path, notices: &[String]) {
        done(md, notices);
    }
    fn failed(&self, msg: &str) {
        failed(msg, self.tx.clone());
    }
    fn record_failed(&self, msg: &str) {
        record_failed(msg, self.tx.clone());
    }
}

/// Não mostra nada (construtor puro do `App`).
pub struct SilentNotifier;

impl Notifier for SilentNotifier {
    fn ask_start(&self, _: Option<&str>) {}
    fn ask_end(&self, _: u64) {}
    fn done(&self, _: &Path, _: &[String]) {}
    fn failed(&self, _: &str) {}
    fn record_failed(&self, _: &str) {}
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn texto_da_pergunta_de_inicio() {
        assert_eq!(start_body(Some("Opera")), "O Opera está usando o microfone. Gravar a call?");
        assert_eq!(start_body(None), "Parece que você está numa call. Gravar?");
    }
}
