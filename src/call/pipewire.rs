use anyhow::{Context, Result};
use serde_json::Value;
use std::collections::{HashMap, HashSet};
use std::process::Command;
use std::time::Duration;

#[derive(Debug, Clone, PartialEq)]
pub struct MicUser {
    pub node_id: u32,
    pub app: Option<String>,
    pub pid: Option<u32>,
}

/// pid aparece como número ou como texto, dependendo da prop.
fn as_pid(v: Option<&Value>) -> Option<u32> {
    let v = v?;
    v.as_u64()
        .or_else(|| v.as_str().and_then(|s| s.trim().parse().ok()))
        .and_then(|n| u32::try_from(n).ok())
}

fn parent_of(pid: u32) -> Option<u32> {
    let stat = std::fs::read_to_string(format!("/proc/{pid}/stat")).ok()?;
    // o nome do processo fica entre parênteses e pode ter espaços
    let rest = &stat[stat.rfind(')')? + 1..];
    rest.split_whitespace().nth(1)?.parse().ok()
}

/// O próprio processo ou um filho dele (subindo pelos pais).
fn is_own(pid: u32, own_pid: u32) -> bool {
    let mut cur = pid;
    for _ in 0..16 {
        if cur == own_pid {
            return true;
        }
        match parent_of(cur) {
            Some(p) if p > 1 => cur = p,
            _ => return false,
        }
    }
    false
}

/// Quem, além do Tone Dial, está gravando de um microfone (regras da seção 2 do spec).
pub fn mic_users(dump_json: &str, own_pid: u32) -> Result<Vec<MicUser>> {
    let dump: Value = serde_json::from_str(dump_json).context("pw-dump inválido")?;
    let objs = dump.as_array().context("pw-dump não é uma lista")?;

    let id_of = |o: &Value| o["id"].as_u64().and_then(|n| u32::try_from(n).ok());
    let props_of = |o: &Value| o["info"]["props"].clone();

    let mut sources: HashSet<u32> = HashSet::new();
    let mut clients: HashMap<u32, Value> = HashMap::new();
    for o in objs {
        let Some(id) = id_of(o) else { continue };
        match o["type"].as_str() {
            Some("PipeWire:Interface:Node") => {
                let class = o["info"]["props"]["media.class"].as_str();
                if matches!(class, Some("Audio/Source") | Some("Audio/Source/Virtual")) {
                    sources.insert(id);
                }
            }
            Some("PipeWire:Interface:Client") => {
                clients.insert(id, props_of(o));
            }
            _ => {}
        }
    }

    // nós de stream que recebem de uma fonte
    let mut linked: HashSet<u32> = HashSet::new();
    for o in objs {
        if o["type"].as_str() != Some("PipeWire:Interface:Link") {
            continue;
        }
        let out = o["info"]["output-node-id"].as_u64();
        let inp = o["info"]["input-node-id"].as_u64();
        if let (Some(out), Some(inp)) = (out, inp) {
            if sources.contains(&(out as u32)) {
                linked.insert(inp as u32);
            }
        }
    }

    let truthy = |v: &Value| v.as_bool() == Some(true) || v.as_str() == Some("true");
    let mut users = Vec::new();
    for o in objs {
        if o["type"].as_str() != Some("PipeWire:Interface:Node") {
            continue;
        }
        let Some(id) = id_of(o) else { continue };
        let p = &o["info"]["props"];
        if p["media.class"].as_str() != Some("Stream/Input/Audio")
            || truthy(&p["node.passive"])
            || truthy(&p["stream.capture.sink"])
            || !linked.contains(&id)
        {
            continue;
        }
        let client = as_pid(p.get("client.id")).and_then(|c| clients.get(&c));
        let pid = client.and_then(|c| {
            as_pid(c.get("application.process.id")).or_else(|| as_pid(c.get("pipewire.sec.pid")))
        });
        if pid.is_some_and(|pid| is_own(pid, own_pid)) {
            continue;
        }
        let app = client
            .and_then(|c| c["application.name"].as_str())
            .map(str::to_string);
        users.push(MicUser { node_id: id, app, pid });
    }
    users.sort_by_key(|u| u.node_id);
    Ok(users)
}

fn read_dump() -> Result<String> {
    let out = Command::new("pw-dump").output().context("não consegui rodar o pw-dump")?;
    anyhow::ensure!(out.status.success(), "pw-dump terminou com {}", out.status);
    Ok(String::from_utf8_lossy(&out.stdout).into_owned())
}

/// Lê o grafo a cada 2 s e avisa se alguém usa o mic. Falha do pw-dump só pula o ciclo.
pub fn spawn_watcher(own_pid: u32, on_sample: impl Fn(bool, Option<String>) + Send + 'static) {
    std::thread::spawn(move || {
        loop {
            match read_dump().and_then(|j| mic_users(&j, own_pid)) {
                Ok(users) => on_sample(!users.is_empty(), users.into_iter().next().and_then(|u| u.app)),
                Err(e) => eprintln!("detector de call: {e:#}"),
            }
            std::thread::sleep(Duration::from_secs(2));
        }
    });
}

#[cfg(test)]
mod tests {
    use super::*;

    const DUMP: &str = include_str!("testdata/dump.json");

    #[test]
    fn acha_o_navegador_usando_o_mic() {
        let u = mic_users(DUMP, 1000).unwrap();
        assert_eq!(u.len(), 1);
        assert_eq!(u[0].app.as_deref(), Some("Opera"));
        assert_eq!(u[0].pid, Some(4242));
    }

    #[test]
    fn ignora_streams_do_proprio_app() {
        // com own_pid = 4242 o Opera some; o stream do pid 1000 aparece como outro app
        let u = mic_users(DUMP, 4242).unwrap();
        assert_eq!(u.len(), 1);
        assert_eq!(u[0].app.as_deref(), Some("Tone Dial"));
        assert_eq!(u[0].pid, Some(1000));
    }

    #[test]
    fn ignora_filtro_passivo_sink_e_stream_sem_link() {
        let ids: Vec<u32> = mic_users(DUMP, 99999).unwrap().iter().map(|u| u.node_id).collect();
        assert_eq!(ids, vec![301, 303]);
    }

    #[test]
    fn aceita_pid_como_numero_ou_texto() {
        assert_eq!(as_pid(Some(&serde_json::json!(7))), Some(7));
        assert_eq!(as_pid(Some(&serde_json::json!("7"))), Some(7));
        assert_eq!(as_pid(None), None);
    }

    #[test]
    fn json_invalido_e_erro() {
        assert!(mic_users("{", 1).is_err());
    }
}
