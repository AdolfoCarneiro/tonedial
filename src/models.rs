//! Modelos: onde ficam, quais faltam e como baixá-los (e quantizar o Whisper)
//! na primeira execução.

use std::fs::File;
use std::io::{BufReader, BufWriter, Read, Write};
use std::path::{Path, PathBuf};
use std::sync::mpsc::Sender;
use std::time::Duration;

use anyhow::{Context, Result, bail};

pub const WHISPER_MODEL_FILE: &str = "ggml-large-v3-q8_0.bin";
pub const VAD_MODEL_FILE: &str = "ggml-silero-v5.1.2.bin";

/// `~/.local/share/tonedial/models` (via `dirs::data_dir()`).
pub fn models_dir() -> PathBuf {
    dirs::data_dir()
        .unwrap_or_else(|| PathBuf::from("."))
        .join("tonedial")
        .join("models")
}

/// Um arquivo de modelo pronto em `models_dir`. `url` e `approx_bytes` descrevem
/// o que é baixado para obtê-lo (para o Whisper q8_0, o f16 que será quantizado).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ModelFile {
    pub name: &'static str,
    pub url: &'static str,
    pub approx_bytes: u64,
}

/// Tipo de quantização suportado pelo quantizador local.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum QuantType {
    Q8_0,
    Q5_0,
}

/// Como o modelo do Whisper chega ao disco.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum WhisperSource {
    /// Baixado pronto de `url`.
    Direct { url: &'static str, approx_bytes: u64 },
    /// Baixa `source_name` de `url`, quantiza localmente para `WHISPER_MODEL_FILE`
    /// e apaga o original.
    Quantized {
        source_name: &'static str,
        url: &'static str,
        source_bytes: u64,
        quant: QuantType,
        output_bytes: u64,
    },
}

/// Decisão atual: large-v3 f16 oficial quantizado localmente para q8_0.
/// Para trocar para o q5_0 oficial: `WHISPER_MODEL_FILE = "ggml-large-v3-q5_0.bin"` e
/// `WhisperSource::Direct { url: ".../ggml-large-v3-q5_0.bin", approx_bytes: 1_081_140_203 }`.
pub const WHISPER_SOURCE: WhisperSource = WhisperSource::Quantized {
    source_name: "ggml-large-v3.bin",
    url: "https://huggingface.co/ggerganov/whisper.cpp/resolve/main/ggml-large-v3.bin",
    source_bytes: 3_095_033_483,
    quant: QuantType::Q8_0,
    output_bytes: 1_660_000_000,
};

/// Modelos do speakrs 0.7: espelho sem gate do pyannote community-1, na revisão
/// que o speakrs fixa. Licenças: segmentation-3.0 MIT; WeSpeaker e PLDA CC-BY-4.0.
#[cfg(test)]
const SPEAKRS_BASE: &str =
    "https://huggingface.co/avencera/speakrs-models/resolve/67f963b6a1d94fcd4678948237b3cc8bd69bf1d8/";

macro_rules! speakrs_model {
    ($name:literal, $bytes:expr) => {
        ModelFile {
            name: $name,
            url: concat!(
                "https://huggingface.co/avencera/speakrs-models/resolve/67f963b6a1d94fcd4678948237b3cc8bd69bf1d8/",
                $name
            ),
            approx_bytes: $bytes,
        }
    };
}

/// Modelos além do Whisper: VAD e os 9 arquivos do diarizador (speakrs, revisão fixa).
const OTHER_MODELS: &[ModelFile] = &[
    ModelFile {
        name: VAD_MODEL_FILE,
        url: "https://huggingface.co/ggml-org/whisper-vad/resolve/main/ggml-silero-v5.1.2.bin",
        approx_bytes: 885_098,
    },
    speakrs_model!("segmentation-3.0.safetensors", 5_897_576),
    speakrs_model!("wespeaker-multimask-tail.safetensors", 26_527_656),
    speakrs_model!("plda_lda.npy", 131_200),
    speakrs_model!("plda_tr.npy", 131_200),
    speakrs_model!("plda_mu.npy", 1_152),
    speakrs_model!("plda_psi.npy", 1_152),
    speakrs_model!("plda_mean1.npy", 2_176),
    speakrs_model!("plda_mean2.npy", 640),
    speakrs_model!("wespeaker-voxceleb-resnet34.min_num_samples.txt", 4),
];

fn whisper_model() -> ModelFile {
    match WHISPER_SOURCE {
        WhisperSource::Direct { url, approx_bytes } => ModelFile {
            name: WHISPER_MODEL_FILE,
            url,
            approx_bytes,
        },
        WhisperSource::Quantized { url, source_bytes, .. } => ModelFile {
            name: WHISPER_MODEL_FILE,
            url,
            approx_bytes: source_bytes,
        },
    }
}

/// Todos os modelos que o app precisa em `models_dir`.
pub fn all_models() -> Vec<ModelFile> {
    let mut v = vec![whisper_model()];
    v.extend_from_slice(OTHER_MODELS);
    v
}

/// Modelos ainda ausentes em `dir`. Um `<nome>.part` não conta como presente.
pub fn missing_models(dir: &Path) -> Vec<ModelFile> {
    all_models()
        .into_iter()
        .filter(|m| !dir.join(m.name).is_file())
        .collect()
}

#[derive(Debug, Clone, PartialEq)]
pub enum SetupProgress {
    Downloading { file: String, done: u64, total: u64 },
    Quantizing,
    Finished,
    Failed(String),
}

/// Baixa (e prepara) tudo o que falta em `dir`, informando o progresso em `tx`.
/// Bloqueante: chamar numa thread. Sempre termina com `Finished` ou `Failed`.
pub fn ensure_models(dir: &Path, tx: Sender<SetupProgress>) {
    run_setup(dir, tx, prepare_models);
}

/// Roda `prepare` e sempre envia `Finished` ou `Failed`, inclusive se ele der panic.
fn run_setup(
    dir: &Path,
    tx: Sender<SetupProgress>,
    prepare: impl FnOnce(&Path, &Sender<SetupProgress>) -> Result<()>,
) {
    let result = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| prepare(dir, &tx)));
    let msg = match result {
        Ok(Ok(())) => SetupProgress::Finished,
        Ok(Err(e)) => SetupProgress::Failed(format!("{e:#}")),
        Err(panic) => {
            let detail = panic
                .downcast_ref::<&str>()
                .map(|s| s.to_string())
                .or_else(|| panic.downcast_ref::<String>().cloned())
                .unwrap_or_else(|| "falha interna".to_string());
            SetupProgress::Failed(format!("Erro inesperado: {detail}"))
        }
    };
    let _ = tx.send(msg);
}

/// Apaga `*.part` deixados por uma execução interrompida (liberam espaço e
/// nunca são retomados).
fn remove_part_files(dir: &Path) {
    let Ok(entries) = std::fs::read_dir(dir) else { return };
    for entry in entries.flatten() {
        let path = entry.path();
        if path.extension().is_some_and(|e| e == "part") && path.is_file() {
            let _ = std::fs::remove_file(&path);
        }
    }
}

/// Folga além do estritamente necessário.
const SPACE_MARGIN: u64 = 200_000_000;

fn prepare_models(dir: &Path, tx: &Sender<SetupProgress>) -> Result<()> {
    let missing = missing_models(dir);
    if missing.is_empty() {
        return Ok(());
    }
    std::fs::create_dir_all(dir)
        .with_context(|| format!("Não foi possível criar a pasta de modelos {}", dir.display()))?;
    remove_part_files(dir);

    // Tudo o que falta baixar + a saída da quantização (o f16 e o q8_0
    // coexistem no disco até o f16 ser apagado).
    let mut needed = SPACE_MARGIN;
    for m in &missing {
        needed += if m.name == WHISPER_MODEL_FILE {
            whisper_bytes_needed(dir)
        } else {
            m.approx_bytes
        };
    }
    check_free_space(dir, needed)?;

    for m in &missing {
        if m.name == WHISPER_MODEL_FILE {
            prepare_whisper(dir, tx)?;
        } else {
            download_reporting(m.url, &dir.join(m.name), m.name, tx)?;
        }
    }
    Ok(())
}

fn whisper_bytes_needed(dir: &Path) -> u64 {
    match WHISPER_SOURCE {
        WhisperSource::Direct { approx_bytes, .. } => approx_bytes,
        WhisperSource::Quantized { source_name, source_bytes, output_bytes, .. } => {
            let source_present = dir.join(source_name).is_file();
            output_bytes + if source_present { 0 } else { source_bytes }
        }
    }
}

fn prepare_whisper(dir: &Path, tx: &Sender<SetupProgress>) -> Result<()> {
    let dest = dir.join(WHISPER_MODEL_FILE);
    match WHISPER_SOURCE {
        WhisperSource::Direct { url, .. } => download_reporting(url, &dest, WHISPER_MODEL_FILE, tx),
        WhisperSource::Quantized { source_name, url, quant, .. } => {
            let source = dir.join(source_name);
            // Um f16 completo de uma execução interrompida é reaproveitado.
            if !source.is_file() {
                download_reporting(url, &source, source_name, tx)?;
            }
            let _ = tx.send(SetupProgress::Quantizing);
            let part = part_path(&dest);
            if let Err(e) = quantize_whisper(&source, &part, quant) {
                let _ = std::fs::remove_file(&part);
                return Err(e.context("Falha ao quantizar o modelo do Whisper"));
            }
            std::fs::rename(&part, &dest)
                .with_context(|| format!("Não foi possível gravar {}", dest.display()))?;
            std::fs::remove_file(&source)
                .with_context(|| format!("Não foi possível apagar {}", source.display()))?;
            Ok(())
        }
    }
}

fn download_reporting(url: &str, dest: &Path, name: &str, tx: &Sender<SetupProgress>) -> Result<()> {
    let mut last_sent: Option<u64> = None;
    download(url, dest, &mut |done, total| {
        // No máximo uma mensagem por MB (mais a primeira e a última).
        let due = match last_sent {
            None => true,
            Some(prev) => done == total || done - prev >= 1 << 20,
        };
        if due {
            last_sent = Some(done);
            let _ = tx.send(SetupProgress::Downloading { file: name.to_string(), done, total });
        }
    })
    .with_context(|| format!("Falha ao baixar {name}"))
}

fn part_path(dest: &Path) -> PathBuf {
    let mut s = dest.as_os_str().to_owned();
    s.push(".part");
    PathBuf::from(s)
}

/// Erro claro (pt-BR) se `dir` não tem `needed` bytes livres.
fn check_free_space(dir: &Path, needed: u64) -> Result<()> {
    let free = free_bytes(dir)?;
    if free < needed {
        bail!(
            "Espaço insuficiente em {}: são necessários {:.1} GB livres para baixar e preparar os modelos, mas há só {:.1} GB.",
            dir.display(),
            needed as f64 / 1e9,
            free as f64 / 1e9
        );
    }
    Ok(())
}

fn free_bytes(dir: &Path) -> Result<u64> {
    use std::os::unix::ffi::OsStrExt;
    let c = std::ffi::CString::new(dir.as_os_str().as_bytes())?;
    // SAFETY: statvfs é POD; zero é um valor válido.
    let mut st: libc::statvfs = unsafe { std::mem::zeroed() };
    // SAFETY: `c` é uma string C válida e `st` aponta para um statvfs.
    if unsafe { libc::statvfs(c.as_ptr(), &mut st) } != 0 {
        return Err(std::io::Error::last_os_error())
            .with_context(|| format!("Não foi possível ver o espaço livre em {}", dir.display()));
    }
    Ok(st.f_bavail as u64 * st.f_frsize as u64)
}

/// Baixa `url` para `dest.part` e renomeia para `dest` só ao terminar.
/// Em erro, apaga o `.part`.
fn download(url: &str, dest: &Path, on_progress: &mut dyn FnMut(u64, u64)) -> Result<()> {
    download_with(&http_agent(READ_TIMEOUT), url, dest, on_progress)
}

/// Sem dados por este tempo, a conexão é considerada morta (o agente padrão
/// do ureq não tem timeout de leitura e travaria para sempre).
const READ_TIMEOUT: Duration = Duration::from_secs(60);

fn http_agent(read_timeout: Duration) -> ureq::Agent {
    ureq::AgentBuilder::new()
        .timeout_connect(Duration::from_secs(30))
        .timeout_read(read_timeout)
        .build()
}

fn download_with(
    agent: &ureq::Agent,
    url: &str,
    dest: &Path,
    on_progress: &mut dyn FnMut(u64, u64),
) -> Result<()> {
    let part = part_path(dest);
    if let Err(e) = download_to(agent, url, &part, on_progress) {
        let _ = std::fs::remove_file(&part);
        return Err(e);
    }
    std::fs::rename(&part, dest).with_context(|| format!("Não foi possível gravar {}", dest.display()))
}

fn download_to(
    agent: &ureq::Agent,
    url: &str,
    path: &Path,
    on_progress: &mut dyn FnMut(u64, u64),
) -> Result<()> {
    let resp = agent
        .get(url)
        .call()
        .with_context(|| format!("Sem resposta de {url} (verifique a conexão)"))?;
    let total: Option<u64> = resp.header("Content-Length").and_then(|v| v.parse().ok());
    let mut reader = resp.into_reader();
    let mut out = BufWriter::new(
        File::create(path).with_context(|| format!("Não foi possível criar {}", path.display()))?,
    );
    let mut buf = vec![0u8; 256 * 1024];
    let mut done = 0u64;
    on_progress(0, total.unwrap_or(0));
    loop {
        let n = match reader.read(&mut buf) {
            Ok(n) => n,
            Err(e) if e.kind() == std::io::ErrorKind::Interrupted => continue,
            Err(e) => return Err(e).context("Conexão interrompida durante o download"),
        };
        if n == 0 {
            break;
        }
        out.write_all(&buf[..n])?;
        done += n as u64;
        on_progress(done, total.unwrap_or(done));
    }
    if let Some(t) = total
        && done != t
    {
        bail!("Download incompleto: {done} de {t} bytes");
    }
    out.into_inner().map_err(|e| e.into_error())?.sync_all()?;
    Ok(())
}

/// Quantiza um modelo ggml do Whisper (f16/f32) para `quant`, como o
/// `whisper-quantize` do whisper.cpp (porta de `examples/quantize/quantize.cpp` e
/// `ggml_common_quantize_0`), usando o `ggml_quantize_chunk` do ggml já linkado
/// pelo whisper-rs-sys.
pub fn quantize_whisper(src: &Path, dst: &Path, quant: QuantType) -> Result<()> {
    use whisper_rs_sys as sys;

    let (qtype, ftype) = match quant {
        QuantType::Q8_0 => (sys::ggml_type_GGML_TYPE_Q8_0, sys::ggml_ftype_GGML_FTYPE_MOSTLY_Q8_0),
        QuantType::Q5_0 => (sys::ggml_type_GGML_TYPE_Q5_0, sys::ggml_ftype_GGML_FTYPE_MOSTLY_Q5_0),
    };
    let mut inp = BufReader::with_capacity(
        1 << 20,
        File::open(src).with_context(|| format!("Não foi possível abrir {}", src.display()))?,
    );
    let mut out = BufWriter::with_capacity(
        1 << 20,
        File::create(dst).with_context(|| format!("Não foi possível criar {}", dst.display()))?,
    );

    // Cabeçalho: magic + 11 hparams; o último é o ftype.
    let magic = read_u32(&mut inp)?;
    if magic != 0x6767_6d6c {
        bail!("{} não é um modelo ggml do Whisper", src.display());
    }
    out.write_all(&magic.to_le_bytes())?;
    for _ in 0..10 {
        out.write_all(&read_i32(&mut inp)?.to_le_bytes())?;
    }
    let ftype_src = read_i32(&mut inp)? % sys::GGML_QNT_VERSION_FACTOR as i32;
    if ftype_src != sys::ggml_ftype_GGML_FTYPE_ALL_F32
        && ftype_src != sys::ggml_ftype_GGML_FTYPE_MOSTLY_F16
    {
        bail!("O modelo de origem já está quantizado (ftype {ftype_src})");
    }
    let ftype_dst = (sys::GGML_QNT_VERSION * sys::GGML_QNT_VERSION_FACTOR) as i32 + ftype;
    out.write_all(&ftype_dst.to_le_bytes())?;

    // Filtros mel: n_mel, n_fft e n_mel*n_fft f32.
    let n_mel = read_i32(&mut inp)?;
    let n_fft = read_i32(&mut inp)?;
    out.write_all(&n_mel.to_le_bytes())?;
    out.write_all(&n_fft.to_le_bytes())?;
    copy_bytes(&mut inp, &mut out, checked_len(n_mel, n_fft, 4)?)?;

    // Vocabulário: n_vocab e, para cada palavra, (len u32, bytes).
    let n_vocab = read_i32(&mut inp)?;
    out.write_all(&n_vocab.to_le_bytes())?;
    for _ in 0..n_vocab.max(0) {
        let len = read_u32(&mut inp)?;
        out.write_all(&len.to_le_bytes())?;
        copy_bytes(&mut inp, &mut out, len as usize)?;
    }

    // Tensores até o fim do arquivo (to_quant = ".*", to_skip como no upstream).
    const TO_SKIP: &[&str] = &[
        "encoder.conv1.bias",
        "encoder.conv2.bias",
        "encoder.positional_embedding",
        "decoder.positional_embedding",
    ];
    let f32_type = sys::ggml_type_GGML_TYPE_F32 as i32;
    let f16_type = sys::ggml_type_GGML_TYPE_F16 as i32;
    let mut f16: Vec<u16> = Vec::new();
    let mut f32s: Vec<f32> = Vec::new();
    let mut work: Vec<u8> = Vec::new();
    while let Some(n_dims) = read_i32_or_eof(&mut inp)? {
        let name_len = read_i32(&mut inp)?;
        let src_type = read_i32(&mut inp)?;
        if !(1..=4).contains(&n_dims) || !(0..=1024).contains(&name_len) {
            bail!("Tensor inválido no modelo (n_dims {n_dims}, nome com {name_len} bytes)");
        }
        let mut ne = [1i32; 4];
        let mut nelements: usize = 1;
        for d in ne.iter_mut().take(n_dims as usize) {
            *d = read_i32(&mut inp)?;
            if *d <= 0 {
                bail!("Dimensão inválida de tensor: {d}");
            }
            nelements = nelements.checked_mul(*d as usize).context("Tensor grande demais")?;
        }
        let mut name = vec![0u8; name_len as usize];
        inp.read_exact(&mut name).context("Modelo truncado")?;
        let name_str = String::from_utf8_lossy(&name);

        let quantize = n_dims == 2 && !TO_SKIP.contains(&name_str.as_ref());
        if src_type != f32_type && src_type != f16_type {
            bail!("Tipo de tensor {src_type} não suportado ({name_str})");
        }
        if quantize {
            // SAFETY: função pura do ggml sobre um tipo válido.
            let blck = unsafe { sys::ggml_blck_size(qtype) };
            if ne[0] as i64 % blck != 0 {
                bail!("Tensor {name_str}: linha de {} elementos não é múltiplo de {blck}", ne[0]);
            }
        }
        let dst_type = if quantize { qtype as i32 } else { src_type };

        out.write_all(&n_dims.to_le_bytes())?;
        out.write_all(&name_len.to_le_bytes())?;
        out.write_all(&dst_type.to_le_bytes())?;
        for d in ne.iter().take(n_dims as usize) {
            out.write_all(&d.to_le_bytes())?;
        }
        out.write_all(&name)?;

        if quantize {
            f32s.resize(nelements, 0.0);
            if src_type == f16_type {
                f16.resize(nelements, 0);
                inp.read_exact(as_bytes_mut(&mut f16)).context("Modelo truncado")?;
                // Sem ggml_init: nesta versão do ggml a conversão fp16->fp32 é
                // calculada (GGML_COMPUTE_FP16_TO_FP32), não usa tabela inicializada.
                // SAFETY: os dois buffers têm `nelements` elementos.
                unsafe {
                    sys::ggml_fp16_to_fp32_row(f16.as_ptr(), f32s.as_mut_ptr(), nelements as i64)
                };
            } else {
                inp.read_exact(as_bytes_mut(&mut f32s)).context("Modelo truncado")?;
            }
            // A saída quantizada nunca é maior que a entrada em f32.
            work.resize(nelements * 4, 0);
            let n_per_row = ne[0] as i64;
            let nrows = nelements as i64 / n_per_row;
            // SAFETY: `f32s` tem nrows*n_per_row floats e `work` comporta a saída.
            let size = unsafe {
                sys::ggml_quantize_chunk(
                    qtype,
                    f32s.as_ptr(),
                    work.as_mut_ptr().cast(),
                    0,
                    nrows,
                    n_per_row,
                    std::ptr::null(),
                )
            };
            out.write_all(&work[..size])?;
        } else {
            let bpe = if src_type == f32_type { 4 } else { 2 };
            copy_bytes(&mut inp, &mut out, nelements * bpe)?;
        }
    }
    out.into_inner().map_err(|e| e.into_error())?.sync_all()?;
    Ok(())
}

fn checked_len(a: i32, b: i32, size: usize) -> Result<usize> {
    if a < 0 || b < 0 {
        bail!("Cabeçalho do modelo inválido");
    }
    (a as usize)
        .checked_mul(b as usize)
        .and_then(|n| n.checked_mul(size))
        .context("Cabeçalho do modelo inválido")
}

/// Vista em bytes de um buffer de u16/f32 (sem padding, qualquer bit pattern é válido).
fn as_bytes_mut<T: Copy>(v: &mut [T]) -> &mut [u8] {
    // SAFETY: ver acima; o tamanho é o do slice.
    unsafe { std::slice::from_raw_parts_mut(v.as_mut_ptr().cast(), std::mem::size_of_val(v)) }
}

fn read_u32(r: &mut impl Read) -> Result<u32> {
    let mut b = [0u8; 4];
    r.read_exact(&mut b).context("Modelo truncado")?;
    Ok(u32::from_le_bytes(b))
}

fn read_i32(r: &mut impl Read) -> Result<i32> {
    Ok(read_u32(r)? as i32)
}

/// Lê um i32, ou `None` se o arquivo terminou exatamente aqui.
fn read_i32_or_eof(r: &mut impl Read) -> Result<Option<i32>> {
    let mut b = [0u8; 4];
    let mut got = 0;
    while got < 4 {
        match r.read(&mut b[got..]) {
            Ok(0) if got == 0 => return Ok(None),
            Ok(0) => bail!("Modelo truncado"),
            Ok(n) => got += n,
            Err(e) if e.kind() == std::io::ErrorKind::Interrupted => {}
            Err(e) => return Err(e.into()),
        }
    }
    Ok(Some(i32::from_le_bytes(b)))
}

fn copy_bytes(r: &mut impl Read, w: &mut impl Write, n: usize) -> Result<()> {
    let copied = std::io::copy(&mut r.take(n as u64), w)?;
    if copied != n as u64 {
        bail!("Modelo truncado");
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::mpsc;

    #[test]
    fn models_dir_ends_with_tonedial_models() {
        assert!(models_dir().ends_with("tonedial/models"));
    }

    #[test]
    fn speakrs_urls_use_pinned_base() {
        for m in OTHER_MODELS.iter().skip(1) {
            assert_eq!(m.url, format!("{SPEAKRS_BASE}{}", m.name));
        }
        assert_eq!(all_models().len(), 11);
    }

    #[test]
    fn missing_lists_absent_files() {
        let dir = tempfile::tempdir().unwrap();
        assert_eq!(missing_models(dir.path()), all_models());
        for m in all_models() {
            std::fs::write(dir.path().join(m.name), b"x").unwrap();
        }
        assert!(missing_models(dir.path()).is_empty());
    }

    #[test]
    fn part_file_is_not_a_model() {
        let dir = tempfile::tempdir().unwrap();
        for m in all_models() {
            if m.name != WHISPER_MODEL_FILE {
                std::fs::write(dir.path().join(m.name), b"x").unwrap();
            }
        }
        std::fs::write(dir.path().join(format!("{WHISPER_MODEL_FILE}.part")), b"x").unwrap();
        let missing = missing_models(dir.path());
        assert_eq!(missing.len(), 1);
        assert_eq!(missing[0].name, WHISPER_MODEL_FILE);
    }

    /// Servidor HTTP local que responde `body` a toda requisição.
    fn serve(body: Vec<u8>) -> String {
        let server = tiny_http::Server::http("127.0.0.1:0").unwrap();
        let addr = server.server_addr().to_ip().unwrap();
        std::thread::spawn(move || {
            for req in server.incoming_requests() {
                let _ = req.respond(tiny_http::Response::from_data(body.clone()));
            }
        });
        format!("http://{addr}/model.bin")
    }

    #[test]
    fn download_writes_part_then_renames() {
        let body: Vec<u8> = (0..1_048_576u32).map(|i| (i % 251) as u8).collect();
        let url = serve(body.clone());
        let dir = tempfile::tempdir().unwrap();
        let dest = dir.path().join("model.bin");
        let mut last = (0, 0);
        download(&url, &dest, &mut |d, t| last = (d, t)).unwrap();
        assert_eq!(std::fs::read(&dest).unwrap(), body);
        assert!(!dir.path().join("model.bin.part").exists());
        assert_eq!(last, (1_048_576, 1_048_576));
    }

    #[test]
    fn download_error_leaves_no_model() {
        let dir = tempfile::tempdir().unwrap();
        let dest = dir.path().join("model.bin");
        // Porta fechada: falha de conexão.
        assert!(download("http://127.0.0.1:1/x", &dest, &mut |_, _| {}).is_err());
        assert!(!dest.exists());
        assert!(!dir.path().join("model.bin.part").exists());
    }

    #[test]
    fn stalled_download_times_out_and_leaves_no_model() {
        use std::io::Write as _;
        use std::net::TcpListener;
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let addr = listener.local_addr().unwrap();
        std::thread::spawn(move || {
            if let Ok((mut s, _)) = listener.accept() {
                let mut req = [0u8; 1024];
                let _ = s.read(&mut req);
                let _ = s.write_all(
                    b"HTTP/1.1 200 OK\r\nContent-Length: 1000000\r\n\r\nalguns bytes",
                );
                let _ = s.flush();
                // Trava sem fechar a conexão.
                std::thread::sleep(Duration::from_secs(30));
            }
        });
        let dir = tempfile::tempdir().unwrap();
        let dest = dir.path().join("model.bin");
        let agent = http_agent(Duration::from_millis(500));
        let started = std::time::Instant::now();
        let r = download_with(&agent, &format!("http://{addr}/m.bin"), &dest, &mut |_, _| {});
        assert!(r.is_err());
        assert!(started.elapsed() < Duration::from_secs(10), "{:?}", started.elapsed());
        assert!(!dest.exists());
        assert!(!dir.path().join("model.bin.part").exists());
    }

    #[test]
    fn panic_during_setup_sends_failed() {
        let dir = tempfile::tempdir().unwrap();
        let (tx, rx) = mpsc::channel();
        run_setup(dir.path(), tx, |_, _| panic!("boom"));
        let msgs: Vec<_> = rx.iter().collect();
        assert_eq!(msgs, vec![SetupProgress::Failed("Erro inesperado: boom".into())]);
    }

    #[test]
    fn leftover_part_files_are_removed() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(dir.path().join("ggml-large-v3.bin.part"), b"x").unwrap();
        std::fs::write(dir.path().join(VAD_MODEL_FILE), b"x").unwrap();
        remove_part_files(dir.path());
        assert!(!dir.path().join("ggml-large-v3.bin.part").exists());
        assert!(dir.path().join(VAD_MODEL_FILE).exists());
    }

    #[test]
    fn ensure_models_with_everything_present_finishes() {
        let dir = tempfile::tempdir().unwrap();
        for m in all_models() {
            std::fs::write(dir.path().join(m.name), b"x").unwrap();
        }
        let (tx, rx) = mpsc::channel();
        ensure_models(dir.path(), tx);
        let msgs: Vec<_> = rx.iter().collect();
        assert_eq!(msgs, vec![SetupProgress::Finished]);
    }

    #[test]
    fn insufficient_space_fails_with_clear_message() {
        let err = check_free_space(Path::new("/"), u64::MAX).unwrap_err();
        assert!(err.to_string().contains("Espaço insuficiente"), "{err}");
        check_free_space(Path::new("/"), 1).unwrap();
    }

    #[test]
    fn quantize_rejects_non_ggml_file() {
        let dir = tempfile::tempdir().unwrap();
        let src = dir.path().join("x.bin");
        std::fs::write(&src, b"not a model at all").unwrap();
        assert!(quantize_whisper(&src, &dir.path().join("y.bin"), QuantType::Q8_0).is_err());
    }

    /// ggml-tiny.bin em target/test-models (o mesmo cache dos testes de asr);
    /// baixa se faltar. `None` sem rede.
    fn tiny_model() -> Option<PathBuf> {
        let dir = PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("target/test-models");
        std::fs::create_dir_all(&dir).ok()?;
        let dest = dir.join("ggml-tiny.bin");
        if !dest.exists() {
            download(
                "https://huggingface.co/ggerganov/whisper.cpp/resolve/main/ggml-tiny.bin",
                &dest,
                &mut |_, _| {},
            )
            .ok()?;
        }
        Some(dest)
    }

    #[test]
    fn quantized_tiny_loads_in_whisper() {
        let Some(tiny) = tiny_model() else {
            eprintln!("SKIPPED: sem rede, pulando quantized_tiny_loads_in_whisper");
            return;
        };
        let dir = tempfile::tempdir().unwrap();
        let out = dir.path().join("ggml-tiny-q8_0.bin");
        quantize_whisper(&tiny, &out, QuantType::Q8_0).unwrap();

        let src_len = std::fs::metadata(&tiny).unwrap().len();
        let out_len = std::fs::metadata(&out).unwrap().len();
        assert!(out_len < src_len * 3 / 4, "q8_0 {out_len} vs f16 {src_len}");

        // ftype gravado = GGML_QNT_VERSION * 1000 + GGML_FTYPE_MOSTLY_Q8_0.
        let bytes = std::fs::read(&out).unwrap();
        let ftype = i32::from_le_bytes(bytes[44..48].try_into().unwrap());
        assert_eq!(ftype, 2007);

        let mut params = whisper_rs::WhisperContextParameters::default();
        params.use_gpu(false);
        let ctx = whisper_rs::WhisperContext::new_with_params(out.to_str().unwrap(), params);
        assert!(ctx.is_ok(), "modelo quantizado não carregou");
    }
}
