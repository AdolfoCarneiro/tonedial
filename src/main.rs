use std::cell::RefCell;
use std::rc::Rc;
use std::sync::mpsc;
use std::time::Duration;

use tonedial::autostart;
use tonedial::call::pipewire;
use tonedial::events::AppEvent;
use tonedial::instance::{self, Instance};
use tonedial::tray;
use tonedial::ui::{App, Shell};

const ICON_PNG: &[u8] = include_bytes!("../assets/icons/tonedial-256.png");

fn options() -> eframe::NativeOptions {
    let mut viewport = egui::ViewportBuilder::default()
        .with_title("Tone Dial")
        .with_app_id("tonedial")
        .with_inner_size([460.0, 560.0])
        .with_min_inner_size([380.0, 480.0]);
    // No Wayland o KDE usa o Icon= do tonedial.desktop (pelo app_id); isto cobre o X11.
    if let Ok(icon) = eframe::icon_data::from_png_bytes(ICON_PNG) {
        viewport = viewport.with_icon(icon);
    }
    // Fechar a janela só encerra o run_native; o processo continua (esconder não funciona no Wayland).
    eframe::NativeOptions { viewport, run_and_return: true, ..Default::default() }
}

fn main() -> eframe::Result {
    let background = std::env::args().any(|a| a == "--background");

    let listener = match instance::acquire(&instance::socket_path()) {
        Ok(Instance::Primary(l)) => Some(l),
        Ok(Instance::Secondary) => return Ok(()),
        Err(e) => {
            eprintln!("instância única indisponível: {e}");
            None
        }
    };

    if let (Some(dir), Ok(exe)) = (autostart::default_dir(), std::env::current_exe()) {
        if let Err(e) = autostart::ensure(&dir, &exe) {
            eprintln!("não consegui criar o autostart: {e}");
        }
    }

    let app = Rc::new(RefCell::new(App::new(mpsc::channel())));
    if let Some(l) = listener {
        let tx = app.borrow().events();
        instance::serve(l, move || {
            let _ = tx.send(AppEvent::ShowWindow);
        });
    }

    let tray = tray::spawn(app.borrow().events());
    app.borrow_mut().set_tray(tray);
    let tx = app.borrow().events();
    pipewire::spawn_watcher(std::process::id(), move |in_use, name| {
        let _ = tx.send(AppEvent::MicUsage { in_use, app: name });
    });

    let mut show = !background;
    loop {
        if show {
            app.borrow_mut().window_opened();
            let shell = Shell(app.clone());
            let r = eframe::run_native("Tone Dial", options(), Box::new(|_cc| Ok(Box::new(shell))));
            app.borrow_mut().window_closed();
            r?;
        }
        if app.borrow().should_quit() {
            return Ok(());
        }
        // sem janela: segue gravando/processando até pedirem a janela ou para sair
        loop {
            app.borrow_mut().tick();
            if app.borrow().should_quit() {
                return Ok(());
            }
            if app.borrow_mut().take_show_request() {
                break;
            }
            std::thread::sleep(Duration::from_millis(200));
        }
        show = true;
    }
}
