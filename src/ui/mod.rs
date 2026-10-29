//! A janela do Tone Dial (egui). Uma tela só, trocando de estado.

mod done;
mod error;
mod idle;
mod processing;
mod recording;
mod setup;

use std::cell::RefCell;
use std::path::PathBuf;
use std::rc::Rc;
use std::sync::mpsc::{self, Receiver, Sender};
use std::time::{Duration, Instant};

use chrono::NaiveDateTime;

use crate::audio::call::CallRecorder;
use crate::audio::decode;
use crate::audio::record::{
    Failure, Levels, Monitor, Recorder, list_input_devices, pending_recordings, recordings_dir,
    temp_wav_path, wav_duration_ms,
};
use crate::config::Config;
use crate::events::AppEvent;
use crate::inhibit::SleepGuard;
use crate::models::{SetupProgress, ensure_models, missing_models, models_dir};
use crate::pipeline::{self, Job, Outcome, Progress, Source, Stage};

#[derive(Debug, Clone, Default, PartialEq)]
pub struct SetupState {
    pub file: String,
    pub done: u64,
    pub total: u64,
    pub quantizing: bool,
}

pub struct ProcessingState {
    pub name: String,
    pub stage: Stage,
    pub fraction: f32,
    pub stage_started: Instant,
    pub notice: Option<String>,
    /// Aviso fixo (ex.: microfone desconectado).
    pub note: Option<String>,
}

impl ProcessingState {
    pub fn new(name: String, note: Option<String>) -> Self {
        ProcessingState {
            name,
            stage: Stage::Preparing,
            fraction: 0.0,
            stage_started: Instant::now(),
            notice: None,
            note,
        }
    }
}

/// Gravação que ficou em `gravacoes/` sem virar transcrição.
#[derive(Debug, Clone, PartialEq)]
pub struct Pending {
    pub wav: PathBuf,
    pub started_at: NaiveDateTime,
    pub duration_ms: Option<u64>,
    /// Som do computador, se for uma call (o par do `wav`, que é o mic).
    pub system: Option<PathBuf>,
}

/// Gravação em andamento: só o mic ou call (mic + som do computador).
pub enum Rec {
    Mic(Recorder),
    Call(CallRecorder),
}

impl Rec {
    fn mic(&self) -> &Recorder {
        match self {
            Rec::Mic(r) => r,
            Rec::Call(c) => &c.mic,
        }
    }

    /// Ondinhas do mic e, em call, as do som do computador.
    pub fn levels(&self) -> (&Levels, Option<&Levels>) {
        match self {
            Rec::Mic(r) => (&r.levels, None),
            Rec::Call(c) => (&c.mic.levels, c.system.as_ref().map(|s| &s.levels)),
        }
    }

    pub fn is_call(&self) -> bool {
        matches!(self, Rec::Call(_))
    }

    pub fn elapsed(&self) -> Duration {
        self.mic().elapsed()
    }

    pub fn failed(&self) -> Option<Failure> {
        self.mic().failed()
    }

    pub fn started_at(&self) -> NaiveDateTime {
        self.mic().started_at
    }

    /// Aviso do som do computador, se houver.
    pub fn note(&self) -> Option<String> {
        match self {
            Rec::Mic(_) => None,
            Rec::Call(c) => c.note(),
        }
    }
}

pub enum Screen {
    Setup(SetupState),
    Idle,
    Recording(Rec),
    Processing(ProcessingState),
    Done(Outcome),
    Error(String),
}

#[derive(Clone, Copy)]
enum Kind {
    Setup,
    Idle,
    Recording,
    Processing,
    Done,
    Error,
}

fn kind(s: &Screen) -> Kind {
    match s {
        Screen::Setup(_) => Kind::Setup,
        Screen::Idle => Kind::Idle,
        Screen::Recording(_) => Kind::Recording,
        Screen::Processing(_) => Kind::Processing,
        Screen::Done(_) => Kind::Done,
        Screen::Error(_) => Kind::Error,
    }
}

pub struct App {
    screen: Screen,
    config: Config,
    devices: Vec<String>,
    monitor: Option<Monitor>,
    /// Modal "Sair?" (um `Quit` chegou com algo em andamento).
    confirm_close: bool,
    events_tx: Sender<AppEvent>,
    events_rx: Receiver<AppEvent>,
    window_open: bool,
    show_requested: bool,
    quit: bool,
    models_dir: PathBuf,
    mic_error: Option<String>,
    setup_rx: Option<Receiver<SetupProgress>>,
    proc_rx: Option<Receiver<Progress>>,
    recordings_dir: PathBuf,
    pending: Vec<Pending>,
    /// Gravação pendente esperando confirmação de "Descartar".
    confirm_discard: Option<Pending>,
    sleep_guard: Option<SleepGuard>,
    /// Desligado no construtor puro, para os testes não chamarem o `systemd-inhibit`.
    inhibit_enabled: bool,
    /// Aviso da preparação dos modelos (ex.: não apagou o f16), mostrado no Idle.
    setup_warning: Option<String>,
    /// `RecordCall` sem pasta de saída e sem janela: grava ao abrir a janela, que pergunta a pasta.
    call_after_show: bool,
}

/// Título da tela de processamento de uma gravação.
pub fn recording_title(started_at: NaiveDateTime) -> String {
    format!("Gravação de {}", started_at.format("%H:%M"))
}

/// "Gravação não processada de 28/10/2026 14:30 (12 min)".
pub fn pending_label(started_at: NaiveDateTime, duration_ms: Option<u64>) -> String {
    let base = format!("Gravação não processada de {}", started_at.format("%d/%m/%Y %H:%M"));
    match duration_ms {
        Some(ms) => format!("{base} ({})", format_duration_ms(ms)),
        None => base,
    }
}

/// Texto do modal de sair: gravando ou processando.
pub fn quit_message(recording: bool) -> &'static str {
    if recording {
        "Parar a gravação e sair? O áudio fica salvo e você pode processar depois."
    } else {
        "Sair agora? O processamento será perdido, mas o áudio continua salvo."
    }
}

pub(crate) const NO_SYSTEM_NOTE: &str = "Não consegui gravar o som do computador; gravando só o microfone";

/// O que processar de uma gravação pendente: um par vira call.
fn source_for(p: &Pending) -> Source {
    match &p.system {
        Some(system) => Source::Call { mic: p.wav.clone(), system: system.clone(), started_at: p.started_at },
        None => Source::Recording { wav: p.wav.clone(), started_at: p.started_at },
    }
}

/// Fonte de uma call que acabou de parar. Sem som do computador (ou com o WAV dele vazio,
/// quando o `pw-record` morreu na hora) vira gravação só do mic, com aviso.
fn source_after_call(
    mic: PathBuf,
    system: Option<PathBuf>,
    started_at: NaiveDateTime,
    note: Option<String>,
) -> (Source, Option<String>) {
    let system = system.filter(|s| {
        let ok = wav_duration_ms(s).is_some_and(|ms| ms > 0);
        if !ok {
            let _ = std::fs::remove_file(s);
        }
        ok
    });
    let note = if system.is_none() { Some(NO_SYSTEM_NOTE.to_string()) } else { note };
    (source_for(&Pending { wav: mic, started_at, duration_ms: None, system }), note)
}

/// Segura a suspensão enquanto grava, processa ou baixa os modelos.
fn inhibit_wanted(screen: &Screen, setup_running: bool) -> bool {
    match screen {
        Screen::Recording(_) | Screen::Processing(_) => true,
        Screen::Setup(_) => setup_running,
        Screen::Idle | Screen::Done(_) | Screen::Error(_) => false,
    }
}

/// Tempo restante por extrapolação linear; vazio nos primeiros 5% da etapa.
pub fn estimate_remaining(elapsed: Duration, fraction: f32) -> Option<Duration> {
    if !(0.05..=1.0).contains(&fraction) {
        return None;
    }
    Some(elapsed.mul_f32((1.0 - fraction) / fraction))
}

/// "faltam ~8 min" / "faltam ~40 s".
pub fn format_remaining(d: Duration) -> String {
    let s = d.as_secs();
    if s < 60 {
        format!("faltam ~{} s", s.max(1))
    } else {
        format!("faltam ~{} min", s.div_ceil(60))
    }
}

/// `HH:MM:SS`.
pub fn format_clock(d: Duration) -> String {
    let s = d.as_secs();
    format!("{:02}:{:02}:{:02}", s / 3600, (s / 60) % 60, s % 60)
}

/// `1h52` ou `12 min`.
pub fn format_duration_ms(ms: u64) -> String {
    let min = ms / 60_000;
    if min >= 60 {
        format!("{}h{:02}", min / 60, min % 60)
    } else {
        format!("{} min", min.max(1))
    }
}

impl App {
    /// Construtor puro (sem áudio, sem threads): usado também nos testes.
    pub fn with_state(config: Config, devices: Vec<String>, screen: Screen) -> Self {
        App::with_events(config, devices, screen, mpsc::channel())
    }

    fn with_events(
        config: Config,
        devices: Vec<String>,
        screen: Screen,
        (events_tx, events_rx): (Sender<AppEvent>, Receiver<AppEvent>),
    ) -> Self {
        App {
            screen,
            config,
            devices,
            monitor: None,
            confirm_close: false,
            events_tx,
            events_rx,
            window_open: false,
            show_requested: false,
            quit: false,
            models_dir: models_dir(),
            mic_error: None,
            setup_rx: None,
            proc_rx: None,
            recordings_dir: recordings_dir(),
            pending: Vec::new(),
            confirm_discard: None,
            sleep_guard: None,
            inhibit_enabled: false,
            setup_warning: None,
            call_after_show: false,
        }
    }

    /// Relê `gravacoes/`. Só chamar fora da gravação (o WAV em andamento também apareceria).
    fn refresh_pending(&mut self) {
        self.pending = pending_recordings(&self.recordings_dir)
            .into_iter()
            .map(|p| Pending {
                duration_ms: wav_duration_ms(&p.wav),
                wav: p.wav,
                started_at: p.started_at,
                system: p.system,
            })
            .collect();
    }

    /// Botão "Descartar": só pede confirmação.
    fn request_discard(&mut self, i: usize) {
        self.confirm_discard = self.pending.get(i).cloned();
    }

    /// "Descartar" confirmado: apaga o WAV (os dois, se for call).
    fn discard_confirmed(&mut self) {
        let Some(p) = self.confirm_discard.take() else { return };
        for wav in std::iter::once(&p.wav).chain(p.system.as_ref()) {
            if let Err(e) = std::fs::remove_file(wav) {
                if wav.exists() {
                    self.mic_error = Some(format!("Não consegui apagar {}: {e}", wav.display()));
                }
            }
        }
        self.refresh_pending();
    }

    /// Botão "Processar" de uma gravação pendente.
    fn process_pending(&mut self, i: usize) {
        let Some(p) = self.pending.get(i).cloned() else { return };
        if self.ensure_output_dir().is_none() {
            return;
        }
        self.monitor = None;
        let (source, note) = match &p.system {
            Some(_) => source_after_call(p.wav.clone(), p.system.clone(), p.started_at, None),
            None => (source_for(&p), None),
        };
        self.start_processing(source, recording_title(p.started_at), note);
    }

    /// Liga/desliga o bloqueio de suspensão conforme a tela.
    fn sync_sleep_guard(&mut self) {
        let want = self.inhibit_enabled && inhibit_wanted(&self.screen, self.setup_rx.is_some());
        if want && self.sleep_guard.is_none() {
            self.sleep_guard = Some(SleepGuard::acquire("Gravando/processando áudio"));
        } else if !want {
            self.sleep_guard = None;
        }
    }

    /// Atualiza o estado a partir de uma mensagem do pipeline. Puro.
    pub fn on_progress(&mut self, p: Progress) {
        match p {
            Progress::Done(o) => {
                if matches!(self.screen, Screen::Processing(_)) {
                    self.screen = Screen::Done(o);
                }
            }
            Progress::Failed(m) => {
                if matches!(self.screen, Screen::Processing(_)) {
                    self.screen = Screen::Error(m);
                }
            }
            other => {
                let Screen::Processing(s) = &mut self.screen else { return };
                match other {
                    Progress::Stage(st) => {
                        s.stage = st;
                        s.fraction = 0.0;
                        s.stage_started = Instant::now();
                    }
                    Progress::Fraction(f) => s.fraction = f.clamp(0.0, 1.0),
                    Progress::Notice(m) => s.notice = Some(m),
                    Progress::Done(_) | Progress::Failed(_) => {}
                }
            }
        }
    }

    pub fn new(events: (Sender<AppEvent>, Receiver<AppEvent>)) -> Self {
        let devices = list_input_devices();
        let mut app = App::with_events(Config::load(), devices, Screen::Idle, events);
        app.inhibit_enabled = true;
        app.go_idle();
        app
    }

    /// Para outras threads (instância única, bandeja, detector) falarem com o App.
    pub fn events(&self) -> Sender<AppEvent> {
        self.events_tx.clone()
    }

    /// Todo o trabalho que não desenha: roda com ou sem janela.
    pub fn tick(&mut self) {
        self.poll_setup();
        self.poll_progress();

        // microfone sumiu durante a gravação: para e processa o que existe
        if let Screen::Recording(rec) = &self.screen {
            if let Some(f) = rec.failed() {
                self.stop_recording(Some(f));
            }
        }
        if self.window_open && std::mem::take(&mut self.call_after_show) {
            self.record_call();
        }

        let events: Vec<_> = self.events_rx.try_iter().collect();
        for e in events {
            self.on_event(e);
        }
        self.sync_sleep_guard();
    }

    fn on_event(&mut self, e: AppEvent) {
        match e {
            AppEvent::ShowWindow => self.show_requested = true,
            AppEvent::Quit => {
                if self.wants_close_confirmation() {
                    self.confirm_close = true;
                    self.show_requested = true;
                } else {
                    self.quit = true;
                }
            }
            AppEvent::RecordCall => self.record_call(),
            AppEvent::StopRecording => self.stop_recording(None),
            // ligados nas próximas tarefas (detector, bandeja)
            AppEvent::MicUsage { .. }
            | AppEvent::EndAnswer { .. } => {}
        }
    }

    /// Bandeja ou notificação pediram para gravar a call. Ocupado, ignora. Sem pasta de
    /// saída e sem janela, abre a janela e grava por lá (o seletor de pasta precisa dela).
    fn record_call(&mut self) {
        if matches!(self.screen, Screen::Recording(_) | Screen::Processing(_) | Screen::Setup(_)) {
            return;
        }
        if self.config.output_dir.is_none() && !self.window_open {
            self.call_after_show = true;
            self.show_requested = true;
            return;
        }
        self.start_recording(true);
    }

    pub fn window_opened(&mut self) {
        self.window_open = true;
        if matches!(self.screen, Screen::Idle) && self.monitor.is_none() {
            self.start_monitor();
        }
    }

    /// Sem janela não há ondinhas: solta o microfone.
    pub fn window_closed(&mut self) {
        self.window_open = false;
        self.monitor = None;
        self.confirm_close = false;
    }

    /// Alguém pediu para mostrar a janela desde a última chamada.
    pub fn take_show_request(&mut self) -> bool {
        std::mem::take(&mut self.show_requested)
    }

    pub fn should_quit(&self) -> bool {
        self.quit
    }

    fn selected_mic(&self) -> Option<String> {
        match &self.config.last_mic {
            Some(m) if self.devices.contains(m) => Some(m.clone()),
            _ => self.devices.first().cloned(),
        }
    }

    fn start_monitor(&mut self) {
        self.monitor = None;
        self.mic_error = None;
        match self.selected_mic() {
            None => self.mic_error = Some("Nenhum microfone encontrado".into()),
            Some(mic) => match Monitor::start(&mic) {
                Ok(m) => self.monitor = Some(m),
                Err(e) => self.mic_error = Some(format!("{e:#}")),
            },
        }
    }

    /// Volta para o início: Setup se faltam modelos, senão Idle com o Monitor ligado.
    fn go_idle(&mut self) {
        self.monitor = None;
        self.proc_rx = None;
        if !missing_models(&self.models_dir).is_empty() {
            self.start_setup();
        } else {
            self.setup_rx = None;
            self.screen = Screen::Idle;
            self.refresh_pending();
            if self.window_open {
                self.start_monitor();
            }
        }
    }

    fn start_setup(&mut self) {
        let (tx, rx) = mpsc::channel();
        let dir = self.models_dir.clone();
        std::thread::spawn(move || ensure_models(&dir, tx));
        self.setup_rx = Some(rx);
        self.screen = Screen::Setup(SetupState::default());
    }

    fn poll_setup(&mut self) {
        let Some(rx) = &self.setup_rx else { return };
        let msgs: Vec<_> = rx.try_iter().collect();
        for m in msgs {
            match m {
                SetupProgress::Downloading { file, done, total } => {
                    self.screen = Screen::Setup(SetupState { file, done, total, quantizing: false });
                }
                SetupProgress::Quantizing => {
                    if let Screen::Setup(s) = &mut self.screen {
                        s.quantizing = true;
                    }
                }
                SetupProgress::Warning(w) => self.setup_warning = Some(w),
                SetupProgress::Finished => {
                    self.setup_rx = None;
                    self.go_idle();
                    return;
                }
                SetupProgress::Failed(e) => {
                    self.setup_rx = None;
                    self.screen = Screen::Error(format!("Não foi possível preparar os modelos: {e}"));
                    return;
                }
            }
        }
    }

    fn poll_progress(&mut self) {
        let Some(rx) = &self.proc_rx else { return };
        let msgs: Vec<_> = rx.try_iter().collect();
        for m in msgs {
            self.on_progress(m);
        }
    }

    fn save_config(&mut self) {
        if let Err(e) = self.config.save() {
            self.mic_error = Some(format!("{e:#}"));
        }
    }

    fn set_mic(&mut self, mic: String) {
        self.config.last_mic = Some(mic);
        self.save_config();
        self.start_monitor();
    }

    /// Abre o seletor de pasta; devolve `true` se o usuário escolheu uma.
    fn pick_output_dir(&mut self) -> bool {
        let mut dlg = rfd::FileDialog::new().set_title("Pasta para salvar as transcrições");
        if let Some(d) = &self.config.output_dir {
            dlg = dlg.set_directory(d);
        }
        match dlg.pick_folder() {
            Some(dir) => {
                self.config.output_dir = Some(dir);
                self.save_config();
                true
            }
            None => false,
        }
    }

    /// Garante uma pasta de saída válida. `None` se o usuário cancelou ou a pasta sumiu
    /// (neste caso a tela vira Error).
    fn ensure_output_dir(&mut self) -> Option<PathBuf> {
        if self.config.output_dir.is_none() && !self.pick_output_dir() {
            return None;
        }
        let dir = self.config.output_dir.clone()?;
        if !dir.is_dir() {
            self.monitor = None;
            self.screen = Screen::Error(MISSING_DIR_MSG.into());
            return None;
        }
        Some(dir)
    }

    /// Botão "Escolher outra pasta" da tela de erro.
    fn choose_output_dir_from_error(&mut self) {
        if self.pick_output_dir() {
            self.go_idle();
        }
    }

    /// `call`: grava também o som do computador.
    pub fn start_recording(&mut self, call: bool) {
        if self.ensure_output_dir().is_none() {
            return;
        }
        // Sem ffmpeg a gravação não viraria texto: avisa antes de gravar.
        if let Err(e) = decode::check_ffmpeg() {
            self.monitor = None;
            self.screen = Screen::Error(format!("{e:#}"));
            return;
        }
        let Some(mic) = self.selected_mic() else { return };
        if let Err(e) = std::fs::create_dir_all(&self.recordings_dir) {
            self.monitor = None;
            self.screen = Screen::Error(format!(
                "Não foi possível criar {}: {e}",
                self.recordings_dir.display()
            ));
            return;
        }
        self.monitor = None; // o Recorder abre o próprio stream
        let now = chrono::Local::now().naive_local();
        let rec = if call {
            CallRecorder::start(&mic, &self.recordings_dir, now).map(Rec::Call)
        } else {
            Recorder::start(&mic, temp_wav_path(&self.recordings_dir, now)).map(Rec::Mic)
        };
        match rec {
            Ok(r) => self.screen = Screen::Recording(r),
            Err(e) => self.screen = Screen::Error(format!("Não foi possível gravar: {e:#}")),
        }
    }

    /// Para a gravação e processa o que foi capturado. `failure`: por que parou sozinha.
    fn stop_recording(&mut self, failure: Option<Failure>) {
        let Screen::Recording(rec) = std::mem::replace(&mut self.screen, Screen::Idle) else {
            return;
        };
        let wav = rec.mic().wav_path.clone();
        let started_at = rec.started_at();
        // (WAV do sistema, aviso) de uma call, lidos antes de parar
        let call = match &rec {
            Rec::Mic(_) => None,
            Rec::Call(c) => Some((c.system.as_ref().map(|s| s.wav_path.clone()), c.note())),
        };
        let stopped = match rec {
            Rec::Mic(r) => r.stop().map(|_| None),
            Rec::Call(c) => c.stop().map(|(_, system)| system),
        };
        let system = match stopped {
            Ok(system) => system,
            Err(e) if !wav.is_file() => {
                self.screen = Screen::Error(format!("Não foi possível finalizar a gravação: {e:#}"));
                return;
            }
            // o mic falhou ao finalizar mas o arquivo existe; o do sistema é finalizado antes
            Err(_) => call.as_ref().and_then(|(system, _)| system.clone()),
        };
        let (source, call_note) = match call {
            None => (Source::Recording { wav, started_at }, None),
            Some((_, note)) => source_after_call(wav, system, started_at, note),
        };
        let failure = failure.map(|f| f.note().to_string());
        let note = match (failure, call_note) {
            (Some(a), Some(b)) => Some(format!("{a}\n{b}")),
            (a, b) => a.or(b),
        };
        self.start_processing(source, recording_title(started_at), note);
    }

    fn open_file(&mut self, path: PathBuf) {
        if self.ensure_output_dir().is_none() {
            return;
        }
        self.monitor = None;
        let name = file_name_of(&path);
        self.start_processing(Source::File(path), name, None);
    }

    fn start_processing(&mut self, source: Source, name: String, note: Option<String>) {
        let Some(output_dir) = self.config.output_dir.clone() else {
            self.screen = Screen::Error("Escolha a pasta de saída primeiro.".into());
            return;
        };
        let job = Job { source, output_dir, models_dir: self.models_dir.clone() };
        self.proc_rx = Some(spawn_pipeline(job));
        self.screen = Screen::Processing(ProcessingState::new(name, note));
    }

    fn wants_close_confirmation(&self) -> bool {
        matches!(self.screen, Screen::Recording(_) | Screen::Processing(_))
    }

    /// Botão "Sair" do modal. Gravando, finaliza o WAV em `gravacoes/` (oferecido na
    /// próxima abertura).
    fn quit_now(&mut self, ctx: &egui::Context) {
        // os WAVs ficam em `gravacoes/` e voltam como pendentes (um par, se for call)
        match std::mem::replace(&mut self.screen, Screen::Idle) {
            Screen::Recording(Rec::Mic(r)) => drop(r.stop()),
            Screen::Recording(Rec::Call(c)) => drop(c.stop()),
            _ => {}
        }
        self.sleep_guard = None;
        self.confirm_close = false;
        self.quit = true;
        ctx.send_viewport_cmd(egui::ViewportCommand::Close);
    }

    fn show_close_modal(&mut self, ctx: &egui::Context) {
        let mut close = false;
        let mut keep = false;
        let recording = matches!(self.screen, Screen::Recording(_));
        egui::Modal::new(egui::Id::unique("confirm_close")).show(ctx, |ui| {
            ui.set_width(300.0);
            ui.heading("Sair?");
            ui.add_space(6.0);
            ui.label(quit_message(recording));
            ui.add_space(10.0);
            ui.horizontal(|ui| {
                if ui.button("Sair").clicked() {
                    close = true;
                }
                if ui.button("Continuar").clicked() {
                    keep = true;
                }
            });
        });
        if close {
            self.quit_now(ctx);
        } else if keep {
            self.confirm_close = false;
        }
    }

    fn show_discard_modal(&mut self, ctx: &egui::Context) {
        let mut discard = false;
        let mut cancel = false;
        egui::Modal::new(egui::Id::unique("confirm_discard")).show(ctx, |ui| {
            ui.set_width(300.0);
            ui.heading("Descartar a gravação?");
            ui.add_space(6.0);
            ui.label("O áudio será apagado e não dá para desfazer.");
            ui.add_space(10.0);
            ui.horizontal(|ui| {
                if ui.button("Descartar").clicked() {
                    discard = true;
                }
                if ui.button("Cancelar").clicked() {
                    cancel = true;
                }
            });
        });
        if discard {
            self.discard_confirmed();
        } else if cancel {
            self.confirm_discard = None;
        }
    }
}

const MISSING_DIR_MSG: &str = "A pasta de saída não existe mais. Escolha outra.";

fn file_name_of(p: &std::path::Path) -> String {
    p.file_name().map(|n| n.to_string_lossy().into_owned()).unwrap_or_default()
}

/// Roda o pipeline numa thread; um panic vira `Failed`, para a UI nunca ficar presa.
fn spawn_pipeline(job: Job) -> Receiver<Progress> {
    let (tx, rx) = mpsc::channel();
    std::thread::spawn(move || {
        let tx2 = tx.clone();
        let r = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| pipeline::run(job, tx2)));
        if let Err(payload) = r {
            let msg = payload
                .downcast_ref::<&str>()
                .map(|s| s.to_string())
                .or_else(|| payload.downcast_ref::<String>().cloned())
                .unwrap_or_else(|| "desconhecido".into());
            let _ = tx.send(Progress::Failed(format!("Erro inesperado: {msg}")));
        }
    });
    rx
}

/// A janela: um `run_native` por abertura. O `App` fica fora dela e sobrevive ao fechar.
pub struct Shell(pub Rc<RefCell<App>>);

impl eframe::App for Shell {
    fn logic(&mut self, ctx: &egui::Context, _frame: &mut eframe::Frame) {
        let mut app = self.0.borrow_mut();
        app.tick();
        if app.should_quit() {
            ctx.send_viewport_cmd(egui::ViewportCommand::Close);
        } else if app.take_show_request() {
            // já está aberta: só traz para a frente
            ctx.send_viewport_cmd(egui::ViewportCommand::Minimized(false));
            ctx.send_viewport_cmd(egui::ViewportCommand::Focus);
        }
        // minimizada o `ui()` não roda; sem isto o `logic()` (e o tick) também pararia
        ctx.request_repaint_after(Duration::from_millis(250));
    }

    fn ui(&mut self, ui: &mut egui::Ui, _frame: &mut eframe::Frame) {
        self.0.borrow_mut().draw(ui);
    }
}

impl App {
    fn draw(&mut self, ui: &mut egui::Ui) {
        let ctx = ui.ctx().clone();
        egui::CentralPanel::default().show(ui, |ui| match kind(&self.screen) {
            Kind::Setup => setup::show(self, ui),
            Kind::Idle => idle::show(self, ui),
            Kind::Recording => recording::show(self, ui),
            Kind::Processing => processing::show(self, ui),
            Kind::Done => done::show(self, ui),
            Kind::Error => error::show(self, ui),
        });

        if self.confirm_close {
            self.show_close_modal(&ctx);
        } else if self.confirm_discard.is_some() {
            self.show_discard_modal(&ctx);
        }
        self.sync_sleep_guard();

        let animated = matches!(
            self.screen,
            Screen::Setup(_) | Screen::Idle | Screen::Recording(_) | Screen::Processing(_)
        );
        // mesmo parado, acorda de vez em quando para ler os eventos (ex.: ShowWindow)
        let every = if animated { 33 } else { 250 };
        ctx.request_repaint_after(Duration::from_millis(every));
    }
}

/// Barras verticais espelhadas dos níveis (0..1) numa faixa de `height` px; o mais recente à direita.
pub(crate) fn draw_levels(ui: &mut egui::Ui, levels: &[f32], height: f32) {
    let (rect, _) =
        ui.allocate_exact_size(egui::vec2(ui.available_width(), height), egui::Sense::hover());
    let painter = ui.painter_at(rect);
    painter.rect_filled(rect, 6.0, ui.visuals().extreme_bg_color);
    let (bar_w, gap) = (3.0, 2.0);
    let n = ((rect.width() - 12.0) / (bar_w + gap)).floor() as usize;
    let tail = &levels[levels.len().saturating_sub(n)..];
    let mid = rect.center().y;
    let color = ui.visuals().hyperlink_color;
    let x0 = rect.right() - 6.0 - bar_w;
    for (i, v) in tail.iter().rev().enumerate() {
        let h = (v.clamp(0.0, 1.0).sqrt() * (rect.height() / 2.0 - 6.0)).max(1.0);
        let x = x0 - i as f32 * (bar_w + gap);
        painter.rect_filled(
            egui::Rect::from_min_max(egui::pos2(x, mid - h), egui::pos2(x + bar_w, mid + h)),
            1.0,
            color,
        );
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn processing_app() -> App {
        App::with_state(
            Config::default(),
            vec![],
            Screen::Processing(ProcessingState::new("a.m4a".into(), None)),
        )
    }

    #[test]
    fn progress_done_goes_to_done_screen() {
        let mut app = processing_app();
        app.on_progress(Progress::Done(Outcome {
            md_path: "/x/a.md".into(),
            speakers: 2,
            duration_ms: 60_000,
        }));
        assert!(matches!(app.screen, Screen::Done(ref o) if o.speakers == 2));
    }

    #[test]
    fn progress_failed_goes_to_error_screen() {
        let mut app = processing_app();
        app.on_progress(Progress::Failed("deu ruim".into()));
        assert!(matches!(app.screen, Screen::Error(ref m) if m == "deu ruim"));
    }

    #[test]
    fn notice_is_kept_in_processing_state() {
        let mut app = processing_app();
        app.on_progress(Progress::Stage(Stage::Transcribing));
        app.on_progress(Progress::Fraction(0.4));
        app.on_progress(Progress::Notice("usando CPU".into()));
        match &app.screen {
            Screen::Processing(s) => {
                assert_eq!(s.notice.as_deref(), Some("usando CPU"));
                assert_eq!(s.stage, Stage::Transcribing);
                assert!((s.fraction - 0.4).abs() < 1e-6);
            }
            _ => panic!("deveria continuar em Processing"),
        }
    }

    #[test]
    fn stage_change_resets_fraction() {
        let mut app = processing_app();
        app.on_progress(Progress::Fraction(0.9));
        app.on_progress(Progress::Stage(Stage::Diarizing));
        match &app.screen {
            Screen::Processing(s) => assert_eq!(s.fraction, 0.0),
            _ => panic!(),
        }
    }

    #[test]
    fn remaining_estimate_is_none_before_5_percent() {
        assert_eq!(estimate_remaining(Duration::from_secs(10), 0.04), None);
        assert_eq!(estimate_remaining(Duration::from_secs(10), 0.0), None);
    }

    #[test]
    fn remaining_estimate_extrapolates_linearly() {
        assert_eq!(
            estimate_remaining(Duration::from_secs(60), 0.5),
            Some(Duration::from_secs(60))
        );
    }

    fn at(h: u32, m: u32) -> chrono::NaiveDateTime {
        chrono::NaiveDate::from_ymd_opt(2026, 10, 28).unwrap().and_hms_opt(h, m, 5).unwrap()
    }

    #[test]
    fn recording_title_uses_the_start_time() {
        assert_eq!(recording_title(at(9, 5)), "Gravação de 09:05");
    }

    #[test]
    fn pending_label_text() {
        assert_eq!(
            pending_label(at(14, 30), Some(720_000)),
            "Gravação não processada de 28/10/2026 14:30 (12 min)"
        );
        assert_eq!(pending_label(at(14, 30), None), "Gravação não processada de 28/10/2026 14:30");
    }

    #[test]
    fn quit_message_depends_on_state() {
        assert_eq!(
            quit_message(true),
            "Parar a gravação e sair? O áudio fica salvo e você pode processar depois."
        );
        assert_eq!(
            quit_message(false),
            "Sair agora? O processamento será perdido, mas o áudio continua salvo."
        );
    }

    #[test]
    fn tick_sem_janela_processa_progresso() {
        let mut app = processing_app();
        let (tx, rx) = mpsc::channel();
        app.proc_rx = Some(rx);
        tx.send(Progress::Done(Outcome { md_path: "/x/a.md".into(), speakers: 1, duration_ms: 1 }))
            .unwrap();
        app.tick();
        assert!(matches!(app.screen, Screen::Done(_)));
    }

    #[test]
    fn show_window_vira_pedido_de_mostrar() {
        let mut app = App::with_state(Config::default(), vec![], Screen::Idle);
        assert!(!app.take_show_request());
        app.events().send(AppEvent::ShowWindow).unwrap();
        app.tick();
        assert!(app.take_show_request());
        assert!(!app.take_show_request());
    }

    #[test]
    fn quit_parado_sai_e_quit_gravando_pede_confirmacao() {
        let mut idle = App::with_state(Config::default(), vec![], Screen::Idle);
        idle.events().send(AppEvent::Quit).unwrap();
        idle.tick();
        assert!(idle.should_quit());

        let mut busy = processing_app();
        busy.events().send(AppEvent::Quit).unwrap();
        busy.tick();
        assert!(!busy.should_quit());
        assert!(busy.take_show_request());
        assert!(busy.confirm_close);
        assert!(busy.sleep_guard.is_none(), "construtor puro não chama o systemd-inhibit");
    }

    #[test]
    fn abrir_a_janela_liga_o_monitor_so_no_idle() {
        // sem microfones, tentar ligar o Monitor deixa o aviso: é o sinal de que tentou
        let mut busy = processing_app();
        busy.window_opened();
        assert!(busy.mic_error.is_none());

        let mut idle = App::with_state(Config::default(), vec![], Screen::Idle);
        assert!(idle.mic_error.is_none());
        idle.window_opened();
        assert_eq!(idle.mic_error.as_deref(), Some("Nenhum microfone encontrado"));
        idle.window_closed();
        assert!(idle.monitor.is_none());
    }

    #[test]
    fn inhibit_only_while_busy() {
        assert!(inhibit_wanted(&Screen::Processing(ProcessingState::new("x".into(), None)), false));
        assert!(inhibit_wanted(&Screen::Setup(SetupState::default()), true));
        assert!(!inhibit_wanted(&Screen::Setup(SetupState::default()), false));
        assert!(!inhibit_wanted(&Screen::Idle, false));
        assert!(!inhibit_wanted(&Screen::Done(Outcome { md_path: "/a.md".into(), speakers: 1, duration_ms: 1 }), false));
        assert!(!inhibit_wanted(&Screen::Error("x".into()), false));
    }

    #[test]
    fn discarding_a_pending_recording_asks_first_then_deletes() {
        let d = tempfile::tempdir().unwrap();
        let wav = d.path().join("gravacao-20261028-143005.wav");
        std::fs::write(&wav, b"x").unwrap();
        let mut app = App::with_state(Config::default(), vec![], Screen::Idle);
        app.pending = vec![Pending { wav: wav.clone(), started_at: at(14, 30), duration_ms: None, system: None }];
        app.request_discard(0);
        assert_eq!(app.confirm_discard.as_ref().map(|p| p.wav.as_path()), Some(wav.as_path()));
        assert!(wav.exists(), "só apaga depois de confirmar");
        app.discard_confirmed();
        assert!(!wav.exists());
        assert!(app.pending.is_empty());
        assert!(app.confirm_discard.is_none());
    }

    #[test]
    fn refresh_pending_lists_leftover_wavs() {
        let d = tempfile::tempdir().unwrap();
        let wav = crate::audio::record::temp_wav_path(d.path(), at(14, 30));
        std::fs::write(&wav, b"x").unwrap();
        let mut app = App::with_state(Config::default(), vec![], Screen::Idle);
        app.recordings_dir = d.path().to_path_buf();
        app.refresh_pending();
        assert_eq!(app.pending.len(), 1);
        assert_eq!(app.pending[0].wav, wav);
        assert_eq!(app.pending[0].started_at, at(14, 30));
    }

    fn write_wav(path: &std::path::Path, samples: usize) {
        let mut s = crate::audio::record::WavSink::create(path, 48_000).unwrap();
        s.push(&vec![0.1f32; samples]).unwrap();
        s.finish().unwrap();
    }

    #[test]
    fn processar_par_pendente_vira_source_call() {
        let par = Pending {
            wav: "/g/a-mic.wav".into(),
            started_at: at(14, 30),
            duration_ms: None,
            system: Some("/g/a-sistema.wav".into()),
        };
        match source_for(&par) {
            Source::Call { mic, system, started_at } => {
                assert_eq!(mic, PathBuf::from("/g/a-mic.wav"));
                assert_eq!(system, PathBuf::from("/g/a-sistema.wav"));
                assert_eq!(started_at, at(14, 30));
            }
            other => panic!("esperava Call, veio {other:?}"),
        }
        let simples = Pending { system: None, ..par };
        assert!(matches!(source_for(&simples), Source::Recording { ref wav, .. } if wav == &PathBuf::from("/g/a-mic.wav")));
    }

    #[test]
    fn descartar_par_apaga_os_dois() {
        let d = tempfile::tempdir().unwrap();
        let (mic, sis) = crate::audio::record::temp_call_paths(d.path(), at(14, 30));
        std::fs::write(&mic, b"x").unwrap();
        std::fs::write(&sis, b"x").unwrap();
        let mut app = App::with_state(Config::default(), vec![], Screen::Idle);
        app.recordings_dir = d.path().to_path_buf();
        app.pending = vec![Pending { wav: mic.clone(), started_at: at(14, 30), duration_ms: None, system: Some(sis.clone()) }];
        app.request_discard(0);
        assert!(mic.exists() && sis.exists(), "só apaga depois de confirmar");
        app.discard_confirmed();
        assert!(!mic.exists());
        assert!(!sis.exists());
        assert!(app.pending.is_empty());
    }

    #[test]
    fn call_sem_sistema_grava_so_mic_com_aviso() {
        let d = tempfile::tempdir().unwrap();
        let (mic, sis) = crate::audio::record::temp_call_paths(d.path(), at(14, 30));
        write_wav(&mic, 4800);

        // pw-record nem abriu
        let (src, note) = source_after_call(mic.clone(), None, at(14, 30), None);
        assert!(matches!(src, Source::Recording { ref wav, .. } if wav == &mic));
        assert_eq!(note.as_deref(), Some(NO_SYSTEM_NOTE));

        // pw-record abriu e morreu na hora: WAV do sistema vazio é apagado
        write_wav(&sis, 0);
        let (src, note) = source_after_call(mic.clone(), Some(sis.clone()), at(14, 30), None);
        assert!(matches!(src, Source::Recording { .. }));
        assert_eq!(note.as_deref(), Some(NO_SYSTEM_NOTE));
        assert!(!sis.exists());

        // WAV do sistema sumiu
        let (src, _) = source_after_call(mic.clone(), Some(sis.clone()), at(14, 30), None);
        assert!(matches!(src, Source::Recording { .. }));

        // com som do computador: Call, e o aviso que veio passa adiante
        write_wav(&sis, 4800);
        let (src, note) = source_after_call(mic.clone(), Some(sis.clone()), at(14, 30), Some("parou".into()));
        assert!(matches!(src, Source::Call { ref system, .. } if system == &sis));
        assert_eq!(note.as_deref(), Some("parou"));
        assert!(sis.exists());
    }

    #[test]
    fn sair_gravando_deixa_par_pendente() {
        let d = tempfile::tempdir().unwrap();
        let (mic, sis) = crate::audio::record::temp_call_paths(d.path(), at(14, 30));
        write_wav(&mic, 48_000);
        write_wav(&sis, 48_000);
        let mut app = App::with_state(Config::default(), vec![], Screen::Idle);
        app.recordings_dir = d.path().to_path_buf();
        app.refresh_pending();
        assert_eq!(app.pending.len(), 1);
        assert_eq!(app.pending[0].wav, mic);
        assert_eq!(app.pending[0].system.as_deref(), Some(sis.as_path()));
        assert_eq!(app.pending[0].duration_ms, Some(1000));
    }

    #[test]
    fn record_call_ocupado_e_ignorado() {
        let mut app = processing_app();
        app.config.output_dir = Some(std::env::temp_dir());
        app.events().send(AppEvent::RecordCall).unwrap();
        app.tick();
        assert!(matches!(app.screen, Screen::Processing(_)));
        assert!(!app.take_show_request());
    }

    #[test]
    fn record_call_sem_pasta_e_sem_janela_abre_a_janela() {
        let mut app = App::with_state(Config::default(), vec![], Screen::Idle);
        app.events().send(AppEvent::RecordCall).unwrap();
        app.tick();
        assert!(app.take_show_request());
        assert!(app.call_after_show);
        assert!(matches!(app.screen, Screen::Idle));
    }

    #[test]
    fn stop_recording_sem_gravacao_nao_faz_nada() {
        let mut app = App::with_state(Config::default(), vec![], Screen::Idle);
        app.events().send(AppEvent::StopRecording).unwrap();
        app.tick();
        assert!(matches!(app.screen, Screen::Idle));
    }

    #[test]
    fn formatting() {
        assert_eq!(format_clock(Duration::from_secs(3725)), "01:02:05");
        assert_eq!(format_duration_ms(6_720_000), "1h52");
        assert_eq!(format_remaining(Duration::from_secs(450)), "faltam ~8 min");
    }
}
