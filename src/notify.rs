//! Notificações do KDE com botões. Cada uma roda numa thread própria, porque esperar o
//! clique bloqueia.

use std::path::{Path, PathBuf};
use std::sync::mpsc::Sender;
use std::sync::{Arc, Mutex};

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

/// Id da notificação mostrada, para poder fechá-la depois.
pub type ShownId = Arc<Mutex<Option<u32>>>;

/// Mostra, guarda o id em `shown` e espera o clique numa thread. Ao terminar (clique,
/// fechada ou expirada), esquece o id se ainda for o dela.
fn show_tracked(n: Notification, shown: ShownId, on_action: impl FnOnce(&str) + Send + 'static) {
    std::thread::spawn(move || match n.show() {
        Ok(h) => {
            let id = h.id();
            *shown.lock().unwrap_or_else(|e| e.into_inner()) = Some(id);
            h.wait_for_action(on_action);
            let mut cur = shown.lock().unwrap_or_else(|e| e.into_inner());
            if *cur == Some(id) {
                *cur = None;
            }
        }
        Err(e) => eprintln!("notificação: {e}"),
    });
}

/// `generation` identifica a pergunta: um "Gravar" de uma sessão de uso que já acabou não vale.
pub fn ask_start(app: Option<&str>, generation: u64, tx: Sender<AppEvent>, start_id: ShownId) {
    let mut n = base("Tone Dial", &start_body(app));
    n.action("gravar", "Gravar").action("agora-nao", "Agora não");
    show_tracked(n, start_id, move |a| {
        if a == "gravar" {
            let _ = tx.send(AppEvent::StartAnswer { generation });
        }
    });
}

pub fn ask_end(generation: u64, tx: Sender<AppEvent>, end_id: ShownId) {
    let mut n = base("Tone Dial", "A call acabou? Parar e processar?");
    n.action("parar", "Parar").action("continuar", "Continuar gravando");
    show_tracked(n, end_id, move |a| {
        let stop = match a {
            "parar" => true,
            "continuar" => false,
            _ => return,
        };
        let _ = tx.send(AppEvent::EndAnswer { generation, stop });
    });
}

/// Fecha a notificação que ainda estiver na tela (pergunta de início ou de fim).
pub fn close_shown(shown: &ShownId) {
    let Some(id) = shown.lock().unwrap_or_else(|e| e.into_inner()).take() else { return };
    std::thread::spawn(move || {
        let r = zbus::blocking::Connection::session().and_then(|c| {
            c.call_method(
                Some("org.freedesktop.Notifications"),
                "/org/freedesktop/Notifications",
                Some("org.freedesktop.Notifications"),
                "CloseNotification",
                &(id,),
            )
        });
        if let Err(e) = r {
            eprintln!("não consegui fechar a notificação: {e}");
        }
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
    fn ask_start(&self, app: Option<&str>, generation: u64);
    /// Fecha a pergunta de início pendente, se houver.
    fn close_start(&self);
    fn ask_end(&self, generation: u64);
    /// Fecha a pergunta de fim pendente, se houver.
    fn close_end(&self);
    fn done(&self, md: &Path, notices: &[String]);
    fn failed(&self, msg: &str);
    /// Gravação pedida pela bandeja ou notificação não começou.
    fn record_failed(&self, msg: &str);
}

pub struct DesktopNotifier {
    tx: Sender<AppEvent>,
    start_id: ShownId,
    end_id: ShownId,
}

impl DesktopNotifier {
    pub fn new(tx: Sender<AppEvent>) -> Self {
        DesktopNotifier { tx, start_id: ShownId::default(), end_id: ShownId::default() }
    }
}

impl Notifier for DesktopNotifier {
    fn ask_start(&self, app: Option<&str>, generation: u64) {
        close_shown(&self.start_id);
        ask_start(app, generation, self.tx.clone(), self.start_id.clone());
    }
    fn close_start(&self) {
        close_shown(&self.start_id);
    }
    fn ask_end(&self, generation: u64) {
        // uma pergunta por vez: a anterior (se ainda aberta) sai da tela
        close_shown(&self.end_id);
        ask_end(generation, self.tx.clone(), self.end_id.clone());
    }
    fn close_end(&self) {
        close_shown(&self.end_id);
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
    fn ask_start(&self, _: Option<&str>, _: u64) {}
    fn close_start(&self) {}
    fn ask_end(&self, _: u64) {}
    fn close_end(&self) {}
    fn done(&self, _: &Path, _: &[String]) {}
    fn failed(&self, _: &str) {}
    fn record_failed(&self, _: &str) {}
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Mostra a pergunta de fim de verdade e fecha pelo D-Bus. `cargo test -- --ignored`.
    #[test]
    #[ignore = "mostra uma notificação de verdade"]
    fn pergunta_de_fim_fecha_pelo_dbus() {
        let end_id = ShownId::default();
        ask_end(1, std::sync::mpsc::channel().0, end_id.clone());
        let deadline = std::time::Instant::now() + std::time::Duration::from_secs(5);
        while end_id.lock().unwrap().is_none() {
            assert!(std::time::Instant::now() < deadline, "notificação não apareceu");
            std::thread::sleep(std::time::Duration::from_millis(50));
        }
        close_shown(&end_id);
        assert!(end_id.lock().unwrap().is_none());
        std::thread::sleep(std::time::Duration::from_secs(1));
    }

    #[test]
    fn texto_da_pergunta_de_inicio() {
        assert_eq!(start_body(Some("Opera")), "O Opera está usando o microfone. Gravar a call?");
        assert_eq!(start_body(None), "Parece que você está numa call. Gravar?");
    }
}
