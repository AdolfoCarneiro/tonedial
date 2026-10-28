use std::collections::VecDeque;
use std::path::{Path, PathBuf};
use std::sync::mpsc::{self, Receiver, Sender};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::thread::JoinHandle;
use std::time::{Duration, Instant};

use anyhow::{Context, anyhow};
use chrono::{Local, NaiveDateTime};
use cpal::traits::{DeviceTrait, HostTrait, StreamTrait};
use cpal::{ErrorKind, FromSample, Sample, SampleFormat};

const MAX_LEVELS: usize = 300;
/// Um valor de pico a cada ~33 ms.
const LEVEL_WINDOW_MS: u64 = 33;

fn lock<T>(m: &Mutex<T>) -> std::sync::MutexGuard<'_, T> {
    m.lock().unwrap_or_else(|e| e.into_inner())
}

/// Dispositivos que não são microfone de verdade (o `null` do ALSA grava silêncio).
fn is_unusable(name: &str) -> bool {
    name.starts_with("Discard all samples")
}

/// Padrão primeiro, depois os demais na ordem original; sem duplicatas e sem `null`.
fn order_devices(all: Vec<String>, default: Option<String>) -> Vec<String> {
    let mut out: Vec<String> = Vec::new();
    let mut push = |n: String| {
        if !is_unusable(&n) && !out.contains(&n) {
            out.push(n);
        }
    };
    if let Some(d) = &default
        && all.contains(d)
    {
        push(d.clone());
    }
    for n in all {
        push(n);
    }
    out
}

/// Nomes dos microfones disponíveis (host padrão), o padrão do sistema primeiro.
pub fn list_input_devices() -> Vec<String> {
    let host = cpal::default_host();
    let Ok(devices) = host.input_devices() else {
        return Vec::new();
    };
    let default_id = host.default_input_device().and_then(|d| d.id().ok());
    let mut default_name = None;
    let mut all = Vec::new();
    for d in devices {
        let Ok(desc) = d.description() else { continue };
        let name = desc.name().to_string();
        // o padrão é achado pelo id: o nome do próprio "default" não aparece na lista
        if default_name.is_none() && default_id.is_some() && d.id().ok() == default_id {
            default_name = Some(name.clone());
        }
        all.push(name);
    }
    order_devices(all, default_name)
}

/// Últimos ~300 valores de pico (0..1), um a cada ~33 ms.
#[derive(Clone, Default)]
pub struct Levels(Arc<Mutex<VecDeque<f32>>>);

impl Levels {
    pub fn snapshot(&self) -> Vec<f32> {
        lock(&self.0).iter().copied().collect()
    }

    fn push(&self, v: f32) {
        let mut q = lock(&self.0);
        if q.len() >= MAX_LEVELS {
            q.pop_front();
        }
        q.push_back(v);
    }
}

/// Acumula picos e publica um valor a cada janela de ~33 ms.
struct Meter {
    levels: Levels,
    window: usize,
    count: usize,
    peak: f32,
}

impl Meter {
    fn new(levels: Levels, sample_rate: u32) -> Self {
        let window = (sample_rate as u64 * LEVEL_WINDOW_MS / 1000).max(1) as usize;
        Meter { levels, window, count: 0, peak: 0.0 }
    }

    fn feed(&mut self, mono: &[f32]) {
        for &s in mono {
            self.peak = self.peak.max(s.abs());
            self.count += 1;
            if self.count >= self.window {
                self.levels.push(self.peak.min(1.0));
                self.count = 0;
                self.peak = 0.0;
            }
        }
    }
}

/// WAV mono f32 na taxa nativa do dispositivo.
pub struct WavSink {
    writer: hound::WavWriter<std::io::BufWriter<std::fs::File>>,
    since_flush: usize,
    flush_every: usize,
}

impl WavSink {
    pub fn create(path: &Path, sample_rate: u32) -> anyhow::Result<Self> {
        let spec = hound::WavSpec {
            channels: 1,
            sample_rate,
            bits_per_sample: 32,
            sample_format: hound::SampleFormat::Float,
        };
        let writer = hound::WavWriter::create(path, spec)
            .with_context(|| format!("não foi possível criar {}", path.display()))?;
        Ok(WavSink { writer, since_flush: 0, flush_every: sample_rate as usize })
    }

    pub fn push(&mut self, mono: &[f32]) -> anyhow::Result<()> {
        for &s in mono {
            self.writer.write_sample(s)?;
        }
        self.since_flush += mono.len();
        if self.since_flush >= self.flush_every {
            // flush também atualiza o cabeçalho: o arquivo fica legível mesmo se o app cair
            self.writer.flush()?;
            self.since_flush = 0;
        }
        Ok(())
    }

    pub fn finish(self) -> anyhow::Result<()> {
        self.writer.finalize()?;
        Ok(())
    }
}

/// `<output_dir>/.tonedial-gravando-<AAAAMMDD-HHMMSS>.wav`
pub fn temp_wav_path(output_dir: &Path, at: NaiveDateTime) -> PathBuf {
    output_dir.join(format!(".tonedial-gravando-{}.wav", at.format("%Y%m%d-%H%M%S")))
}

/// Stream aberto. `cpal::Stream` não é `Send`: fica na thread da UI.
struct Capture {
    _stream: cpal::Stream,
}

fn mono_mix<T: Sample>(data: &[T], channels: usize, buf: &mut Vec<f32>)
where
    f32: FromSample<T>,
{
    buf.clear();
    if channels <= 1 {
        buf.extend(data.iter().map(|&s| f32::from_sample(s)));
    } else {
        buf.extend(data.chunks_exact(channels).map(|frame| {
            frame.iter().map(|&s| f32::from_sample(s)).sum::<f32>() / channels as f32
        }));
    }
}

fn find_device(name: &str) -> anyhow::Result<cpal::Device> {
    let host = cpal::default_host();
    if !name.is_empty()
        && let Ok(devices) = host.input_devices()
    {
        for d in devices {
            if d.description().map(|x| x.name() == name).unwrap_or(false) {
                return Ok(d);
            }
        }
    }
    // microfone salvo sumiu (ou nome vazio): cai para o padrão
    host.default_input_device()
        .ok_or_else(|| anyhow!("nenhum microfone encontrado"))
}

/// Abre o microfone e entrega blocos mono (média dos canais) ao callback.
/// `make_cb` recebe a taxa nativa e devolve o callback de dados.
fn open_capture<F>(
    device: &str,
    make_cb: impl FnOnce(u32) -> anyhow::Result<F>,
    on_error: impl Fn(String) + Send + 'static,
) -> anyhow::Result<Capture>
where
    F: FnMut(&[f32]) + Send + 'static,
{
    let dev = find_device(device)?;
    let supported = dev
        .default_input_config()
        .map_err(|e| anyhow!("não foi possível configurar o microfone: {e}"))?;
    let sample_rate = supported.sample_rate();
    let channels = supported.channels() as usize;
    let format = supported.sample_format();
    let config: cpal::StreamConfig = supported.into();
    let mut on_mono = make_cb(sample_rate)?;

    let err_cb = move |e: cpal::Error| match e.kind() {
        ErrorKind::DeviceChanged | ErrorKind::Xrun | ErrorKind::RealtimeDenied => {}
        _ => on_error(e.to_string()),
    };

    macro_rules! build {
        ($t:ty) => {{
            let mut buf: Vec<f32> = Vec::new();
            dev.build_input_stream(
                config,
                move |data: &[$t], _: &_| {
                    mono_mix(data, channels, &mut buf);
                    on_mono(&buf);
                },
                err_cb,
                None,
            )
        }};
    }
    let stream = match format {
        SampleFormat::F32 => build!(f32),
        SampleFormat::I16 => build!(i16),
        SampleFormat::I32 => build!(i32),
        SampleFormat::I8 => build!(i8),
        SampleFormat::U16 => build!(u16),
        other => return Err(anyhow!("formato de áudio não suportado: {other}")),
    }
    .map_err(|e| anyhow!("não foi possível abrir o microfone: {e}"))?;
    stream
        .play()
        .map_err(|e| anyhow!("não foi possível iniciar o microfone: {e}"))?;
    Ok(Capture { _stream: stream })
}

/// Stream aberto só para mostrar as ondinhas (sem gravar nada).
pub struct Monitor {
    pub levels: Levels,
    _capture: Capture,
}

impl Monitor {
    pub fn start(device: &str) -> anyhow::Result<Monitor> {
        let levels = Levels::default();
        let lv = levels.clone();
        let capture = open_capture(
            device,
            move |rate| {
                let mut meter = Meter::new(lv, rate);
                Ok(move |mono: &[f32]| meter.feed(mono))
            },
            |_| {},
        )?;
        Ok(Monitor { levels, _capture: capture })
    }
}

enum Msg {
    Data(Vec<f32>),
    Stop,
}

fn writer_loop(
    rx: Receiver<Msg>,
    mut sink: WavSink,
    failure: Arc<Mutex<Option<String>>>,
) -> anyhow::Result<()> {
    let mut broken = false;
    while let Ok(Msg::Data(chunk)) = rx.recv() {
        if broken {
            continue;
        }
        if let Err(e) = sink.push(&chunk) {
            // disco cheio etc.: guarda o motivo e mantém o que já foi escrito
            *lock(&failure) = Some(format!("erro ao gravar o arquivo: {e}"));
            broken = true;
        }
    }
    sink.finish()
}

pub struct Recorder {
    pub levels: Levels,
    pub wav_path: PathBuf,
    pub started_at: NaiveDateTime,
    started: Instant,
    failure: Arc<Mutex<Option<String>>>,
    tx: Sender<Msg>,
    writer: Option<JoinHandle<anyhow::Result<()>>>,
    capture: Option<Capture>,
}

impl Recorder {
    pub fn start(device: &str, wav_path: PathBuf) -> anyhow::Result<Recorder> {
        let levels = Levels::default();
        let failure: Arc<Mutex<Option<String>>> = Arc::default();
        let (tx, rx) = mpsc::channel::<Msg>();

        // A taxa nativa só é conhecida ao abrir o dispositivo; o WAV é criado nesse momento.
        let sink_slot: Arc<Mutex<Option<WavSink>>> = Arc::default();
        let (slot, lv, path, tx_cb) = (sink_slot.clone(), levels.clone(), wav_path.clone(), tx.clone());
        let fail_cb = failure.clone();
        let got_data = Arc::new(AtomicBool::new(false));
        let got_cb = got_data.clone();
        let capture = open_capture(
            device,
            move |rate| {
                *lock(&slot) = Some(WavSink::create(&path, rate)?);
                let mut meter = Meter::new(lv, rate);
                Ok(move |mono: &[f32]| {
                    got_cb.store(true, Ordering::Release);
                    meter.feed(mono);
                    let _ = tx_cb.send(Msg::Data(mono.to_vec()));
                })
            },
            move |msg| {
                let mut f = lock(&fail_cb);
                if f.is_none() {
                    *f = Some(msg);
                }
            },
        )?;
        let sink = lock(&sink_slot)
            .take()
            .ok_or_else(|| anyhow!("falha ao preparar o arquivo de áudio"))?;
        // O PipeWire pode levar uns décimos de segundo para acordar o microfone: espera o
        // primeiro bloco para o cronômetro e o WAV começarem juntos.
        let deadline = Instant::now() + Duration::from_secs(3);
        while !got_data.load(Ordering::Acquire) {
            if let Some(e) = lock(&failure).clone() {
                return Err(anyhow!("{e}"));
            }
            if Instant::now() > deadline {
                return Err(anyhow!("o microfone não respondeu"));
            }
            std::thread::sleep(Duration::from_millis(5));
        }
        let fail_w = failure.clone();
        let writer = std::thread::spawn(move || writer_loop(rx, sink, fail_w));
        Ok(Recorder {
            levels,
            wav_path,
            started_at: Local::now().naive_local(),
            started: Instant::now(),
            failure,
            tx,
            writer: Some(writer),
            capture: Some(capture),
        })
    }

    pub fn elapsed(&self) -> Duration {
        self.started.elapsed()
    }

    /// Mensagem de erro se o microfone sumiu ou a escrita falhou.
    pub fn failed(&self) -> Option<String> {
        lock(&self.failure).clone()
    }

    /// Finaliza o WAV e devolve o caminho. Funciona também depois de `failed()`.
    pub fn stop(mut self) -> anyhow::Result<PathBuf> {
        drop(self.capture.take()); // para os callbacks
        let _ = self.tx.send(Msg::Stop);
        if let Some(h) = self.writer.take() {
            h.join().map_err(|_| anyhow!("thread de gravação falhou"))??;
        }
        Ok(self.wav_path.clone())
    }
}

#[cfg(test)]
mod tests {
    fn v(a: &[&str]) -> Vec<String> {
        a.iter().map(|s| s.to_string()).collect()
    }

    #[test]
    fn padrao_vem_primeiro_e_null_some() {
        let all = v(&["Discard all samples (playback) or generate zero samples (capture)", "PipeWire", "Padrao", "HDA"]);
        assert_eq!(order_devices(all, Some("Padrao".into())), v(&["Padrao", "PipeWire", "HDA"]));
    }

    #[test]
    fn sem_duplicatas() {
        let all = v(&["HDA", "HDA", "Padrao", "Padrao"]);
        assert_eq!(order_devices(all, Some("Padrao".into())), v(&["Padrao", "HDA"]));
    }

    #[test]
    fn sem_padrao_mantem_ordem_sem_null() {
        let all = v(&["Discard all samples (x)", "B", "A"]);
        assert_eq!(order_devices(all.clone(), None), v(&["B", "A"]));
        assert_eq!(order_devices(all, Some("fora da lista".into())), v(&["B", "A"]));
    }

    #[test]
    fn padrao_null_nao_e_usado() {
        let all = v(&["Discard all samples (x)", "B"]);
        assert_eq!(order_devices(all, Some("Discard all samples (x)".into())), v(&["B"]));
    }

    #[test]
    fn is_unusable_so_pega_null() {
        assert!(is_unusable("Discard all samples (playback) or generate zero samples (capture)"));
        assert!(!is_unusable("HDA Intel PCH, ALC257 Analog"));
        assert!(!is_unusable("PipeWire Sound Server"));
    }

    use super::*;

    fn tmp(name: &str) -> PathBuf {
        std::env::temp_dir().join(format!("td-{}-{}.wav", name, std::process::id()))
    }

    #[test]
    fn wav_sink_writes_readable_file() {
        let p = tmp("full");
        let mut s = WavSink::create(&p, 48_000).unwrap();
        s.push(&vec![0.1; 48_000]).unwrap();
        s.finish().unwrap();
        let r = hound::WavReader::open(&p).unwrap();
        assert_eq!(r.spec().sample_rate, 48_000);
        assert_eq!(r.spec().channels, 1);
        assert_eq!(r.len(), 48_000);
        let _ = std::fs::remove_file(&p);
    }

    #[test]
    fn wav_sink_finish_after_partial_push_is_valid() {
        let p = tmp("partial");
        let mut s = WavSink::create(&p, 16_000).unwrap();
        s.push(&vec![0.2; 8_000]).unwrap();
        s.push(&vec![-0.2; 4_000]).unwrap();
        s.finish().unwrap();
        let r = hound::WavReader::open(&p).unwrap();
        assert_eq!(r.len(), 12_000);
        assert_eq!(r.duration() as f32 / 16_000.0, 0.75);
        let _ = std::fs::remove_file(&p);
    }

    #[test]
    fn wav_sink_create_in_missing_dir_errors() {
        let p = std::env::temp_dir().join("td-nao-existe-xyz/a.wav");
        assert!(WavSink::create(&p, 16_000).is_err());
    }

    #[test]
    fn meter_emits_one_peak_per_window() {
        let levels = Levels::default();
        let mut m = Meter::new(levels.clone(), 1_000); // janela de 33 amostras
        let mut data = vec![0.1; 66];
        data[10] = -0.8;
        m.feed(&data);
        assert_eq!(levels.snapshot(), vec![0.8, 0.1]);
    }

    #[test]
    fn levels_keep_only_last_300() {
        let l = Levels::default();
        for i in 0..400 {
            l.push(i as f32 / 400.0);
        }
        let v = l.snapshot();
        assert_eq!(v.len(), 300);
        assert_eq!(v[0], 100.0 / 400.0);
    }

    #[test]
    fn mono_mix_averages_channels() {
        let mut buf = Vec::new();
        mono_mix(&[1.0f32, 0.0, 0.5, 0.5], 2, &mut buf);
        assert_eq!(buf, vec![0.5, 0.5]);
    }

    #[test]
    fn temp_wav_path_format() {
        let at = chrono::NaiveDate::from_ymd_opt(2026, 10, 28)
            .unwrap()
            .and_hms_opt(9, 5, 7)
            .unwrap();
        assert_eq!(
            temp_wav_path(Path::new("/out"), at),
            PathBuf::from("/out/.tonedial-gravando-20261028-090507.wav")
        );
    }

    #[test]
    #[ignore = "precisa de microfone"]
    fn records_three_seconds() {
        let p = tmp("mic3s");
        let r = Recorder::start("", p.clone()).unwrap();
        std::thread::sleep(Duration::from_secs(3));
        assert!(r.failed().is_none(), "{:?}", r.failed());
        let path = r.stop().unwrap();
        let rd = hound::WavReader::open(&path).unwrap();
        let secs = rd.duration() as f32 / rd.spec().sample_rate as f32;
        let _ = std::fs::remove_file(&path);
        assert!((2.8..=3.3).contains(&secs), "duração {secs}");
    }
}
