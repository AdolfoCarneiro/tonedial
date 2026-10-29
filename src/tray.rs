//! Ícone na bandeja do KDE (StatusNotifierItem, via `ksni`).

use std::cell::RefCell;
use std::sync::mpsc::Sender;

use ksni::blocking::{Handle, TrayMethods};
use ksni::menu::{MenuItem, StandardItem};

use crate::events::AppEvent;

const ICON_32: &[u8] = include_bytes!("../assets/icons/tonedial-32.png");
const ICON_64: &[u8] = include_bytes!("../assets/icons/tonedial-64.png");

#[derive(Debug, Clone, PartialEq)]
pub struct TrayState {
    pub recording: bool,
    pub tooltip: String,
}

/// RGBA (do `eframe`) para o ARGB32 em ordem de rede que o StatusNotifierItem espera.
fn rgba_to_argb(rgba: &[u8]) -> Vec<u8> {
    rgba.chunks_exact(4).flat_map(|p| [p[3], p[0], p[1], p[2]]).collect()
}

/// Ponto vermelho no canto inferior direito: está gravando.
fn add_red_dot(argb: &mut [u8], width: usize, height: usize) {
    let r = width.min(height) as f32 * 0.22;
    let (cx, cy) = (width as f32 - r - 0.5, height as f32 - r - 0.5);
    for y in 0..height {
        for x in 0..width {
            let (dx, dy) = (x as f32 - cx, y as f32 - cy);
            if dx * dx + dy * dy <= r * r {
                let i = (y * width + x) * 4;
                argb[i..i + 4].copy_from_slice(&[255, 230, 40, 40]);
            }
        }
    }
}

fn pixmaps(recording: bool) -> Vec<ksni::Icon> {
    [ICON_32, ICON_64]
        .iter()
        .filter_map(|png| eframe::icon_data::from_png_bytes(png).ok())
        .map(|icon| {
            let mut data = rgba_to_argb(&icon.rgba);
            if recording {
                add_red_dot(&mut data, icon.width as usize, icon.height as usize);
            }
            ksni::Icon { width: icon.width as i32, height: icon.height as i32, data }
        })
        .collect()
}

struct ToneTray {
    state: TrayState,
    tx: Sender<AppEvent>,
    idle_icon: Vec<ksni::Icon>,
    recording_icon: Vec<ksni::Icon>,
}

impl ToneTray {
    fn send(&self, e: AppEvent) {
        let _ = self.tx.send(e);
    }
}

fn item(label: &str, event: AppEvent) -> MenuItem<ToneTray> {
    StandardItem {
        label: label.into(),
        activate: Box::new(move |t: &mut ToneTray| t.send(event.clone())),
        ..Default::default()
    }
    .into()
}

impl ksni::Tray for ToneTray {
    fn id(&self) -> String {
        "tonedial".into()
    }

    fn title(&self) -> String {
        "Tone Dial".into()
    }

    fn icon_pixmap(&self) -> Vec<ksni::Icon> {
        if self.state.recording { self.recording_icon.clone() } else { self.idle_icon.clone() }
    }

    fn tool_tip(&self) -> ksni::ToolTip {
        ksni::ToolTip { title: self.state.tooltip.clone(), ..Default::default() }
    }

    fn activate(&mut self, _x: i32, _y: i32) {
        self.send(AppEvent::ShowWindow);
    }

    fn menu(&self) -> Vec<MenuItem<Self>> {
        let rec = if self.state.recording {
            item("Parar gravação", AppEvent::StopRecording)
        } else {
            item("Gravar call", AppEvent::RecordCall)
        };
        vec![
            item("Abrir Tone Dial", AppEvent::ShowWindow),
            rec,
            MenuItem::Separator,
            item("Sair", AppEvent::Quit),
        ]
    }
}

pub struct TrayHandle {
    handle: Handle<ToneTray>,
    last: RefCell<TrayState>,
}

impl TrayHandle {
    /// Atualiza ícone, tooltip e menu; só fala com o D-Bus se algo mudou.
    pub fn set(&self, s: TrayState) {
        if *self.last.borrow() == s {
            return;
        }
        let next = s.clone();
        self.handle.update(move |t| t.state = next);
        *self.last.borrow_mut() = s;
    }
}

/// Sobe a bandeja. `None` se não deu (sem D-Bus ou sem StatusNotifierWatcher): o app continua.
pub fn spawn(tx: Sender<AppEvent>) -> Option<TrayHandle> {
    let state = TrayState { recording: false, tooltip: "Tone Dial".into() };
    let tray = ToneTray {
        state: state.clone(),
        tx,
        idle_icon: pixmaps(false),
        recording_icon: pixmaps(true),
    };
    match tray.spawn() {
        Ok(handle) => Some(TrayHandle { handle, last: RefCell::new(state) }),
        Err(e) => {
            eprintln!("bandeja indisponível: {e}");
            None
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn rgba_vira_argb() {
        assert_eq!(rgba_to_argb(&[1, 2, 3, 4, 5, 6, 7, 8]), vec![4, 1, 2, 3, 8, 5, 6, 7]);
    }

    #[test]
    fn ponto_vermelho_no_canto_inferior_direito() {
        let (w, h) = (32, 32);
        let mut px = vec![0u8; w * h * 4];
        add_red_dot(&mut px, w, h);
        let at = |x: usize, y: usize| &px[(y * w + x) * 4..(y * w + x) * 4 + 4];
        assert_eq!(at(w - 4, h - 4), &[255, 230, 40, 40]);
        assert_eq!(at(0, 0), &[0, 0, 0, 0]);
        assert_eq!(at(w - 4, 0), &[0, 0, 0, 0]);
    }

    #[test]
    fn icones_decodificam_nos_dois_tamanhos() {
        let icons = pixmaps(false);
        assert_eq!(icons.iter().map(|i| i.width).collect::<Vec<_>>(), vec![32, 64]);
        assert!(icons.iter().all(|i| i.data.len() == (i.width * i.height * 4) as usize));
    }
}
