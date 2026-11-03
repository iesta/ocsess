use std::collections::HashSet;
use std::fs::{self, File};
use std::io;
use std::path::PathBuf;
use std::process::{Command as ProcessCommand, Stdio};
use std::sync::Arc;
use std::sync::mpsc::{self, Receiver, Sender};
use std::thread;
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use base64::Engine;
use base64::engine::general_purpose::STANDARD;
use flate2::Compression;
use flate2::write::GzEncoder;
use serde::Deserialize;
use serde_json::{Value, json};

use crate::db::{Session, now_ms};

const SERVICE_USER: &str = "opencode";

#[derive(Deserialize)]
struct ServiceFile {
    url: String,
    password: String,
}

struct Service {
    url: String,
    password: String,
}

#[derive(Debug)]
enum ReqErr {
    Transport(String),
    Http(u16, String),
    Parse(String),
    Service(String),
}

impl std::fmt::Display for ReqErr {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            ReqErr::Transport(message) => write!(f, "service unreachable · {message}"),
            ReqErr::Http(status, body) => {
                let body = body.trim();
                if body.is_empty() {
                    write!(f, "API HTTP {status}")
                } else {
                    write!(f, "API HTTP {status} · {body}")
                }
            }
            ReqErr::Parse(message) => write!(f, "invalid response · {message}"),
            ReqErr::Service(message) => write!(f, "{message}"),
        }
    }
}

pub struct Api {
    agent: ureq::Agent,
    quick_agent: ureq::Agent,
}

impl Api {
    pub fn new() -> Self {
        Self {
            agent: build_agent(Duration::from_secs(6)),
            quick_agent: build_agent(Duration::from_millis(600)),
        }
    }

    pub fn list_sessions(&self) -> Result<Vec<Session>, String> {
        let mut sessions = Vec::new();
        for archived in [false, true] {
            let path = format!("/api/session?limit=1000&order=desc&archived={archived}");
            let value = self.request_quiet("GET", &path, None)?;
            let Some(items) = value.get("data").and_then(Value::as_array) else {
                continue;
            };
            for item in items {
                if let Some(session) = session_from_api(item, archived) {
                    sessions.push(session);
                }
            }
        }
        sessions.sort_by_key(|session| session.time_updated);
        sessions.dedup_by(|left, right| left.id == right.id);
        Ok(sessions)
    }

    pub fn active_sessions(&self) -> Result<HashSet<String>, String> {
        let value = self.request_quiet("GET", "/api/session/active", None)?;
        let Some(data) = value.get("data").and_then(Value::as_object) else {
            return Ok(HashSet::new());
        };
        Ok(data.keys().cloned().collect())
    }

    pub fn fork(&self, id: &str) -> Result<String, String> {
        let value = self.request(
            "POST",
            &format!("/api/session/{id}/fork"),
            Some(r#"{"before":null}"#),
        )?;
        value
            .get("data")
            .and_then(|data| data.get("id"))
            .and_then(Value::as_str)
            .map(str::to_string)
            .ok_or_else(|| "fork response without id".to_string())
    }

    pub fn rename(&self, id: &str, title: &str) -> Result<(), String> {
        let body = json!({ "title": title }).to_string();
        self.request("PATCH", &format!("/api/session/{id}"), Some(&body))?;
        Ok(())
    }

    pub fn set_archived(&self, id: &str, archived: bool) -> Result<(), String> {
        let body = json!({ "archived": archived }).to_string();
        self.request("PATCH", &format!("/api/session/{id}"), Some(&body))?;
        Ok(())
    }

    pub fn delete(&self, id: &str) -> Result<(), String> {
        self.request("DELETE", &format!("/api/session/{id}"), None)?;
        Ok(())
    }

    pub fn compact(&self, id: &str) -> Result<(), String> {
        self.request("POST", &format!("/api/session/{id}/compact"), Some("{}"))?;
        Ok(())
    }

    fn request_quiet(&self, method: &str, path: &str, body: Option<&str>) -> Result<Value, String> {
        let service = read_service().map_err(|error| error.to_string())?;
        self.request_with(&self.quick_agent, &service, method, path, body)
            .map_err(|error| error.to_string())
    }

    fn request(&self, method: &str, path: &str, body: Option<&str>) -> Result<Value, String> {
        match read_service()
            .and_then(|service| self.request_with(&self.agent, &service, method, path, body))
        {
            Ok(value) => Ok(value),
            Err(ReqErr::Transport(_)) => {
                self.start_service();
                let service = read_service().map_err(|error| error.to_string())?;
                self.request_with(&self.agent, &service, method, path, body)
                    .map_err(|error| error.to_string())
            }
            Err(error) => Err(error.to_string()),
        }
    }

    fn request_with(
        &self,
        agent: &ureq::Agent,
        service: &Service,
        method: &str,
        path: &str,
        body: Option<&str>,
    ) -> Result<Value, ReqErr> {
        let url = format!("{}{}", service.url.trim_end_matches('/'), path);
        let auth = format!(
            "Basic {}",
            STANDARD.encode(format!("{SERVICE_USER}:{}", service.password))
        );
        let mut response = match method {
            "GET" => agent.get(&url).header("Authorization", &auth).call(),
            "DELETE" => agent.delete(&url).header("Authorization", &auth).call(),
            "POST" => agent
                .post(&url)
                .header("Authorization", &auth)
                .header("Content-Type", "application/json")
                .send(body.unwrap_or("{}")),
            "PATCH" => agent
                .patch(&url)
                .header("Authorization", &auth)
                .header("Content-Type", "application/json")
                .send(body.unwrap_or("{}")),
            other => return Err(ReqErr::Service(format!("unknown method: {other}"))),
        }
        .map_err(|error| ReqErr::Transport(error.to_string()))?;

        let status = response.status().as_u16();
        let text = response
            .body_mut()
            .read_to_string()
            .map_err(|error| ReqErr::Parse(error.to_string()))?;
        if !(200..300).contains(&status) {
            return Err(ReqErr::Http(status, text));
        }
        if text.trim().is_empty() {
            return Ok(Value::Null);
        }
        serde_json::from_str(&text).map_err(|error| ReqErr::Parse(error.to_string()))
    }

    fn start_service(&self) {
        let _ = ProcessCommand::new("opencode")
            .args(["service", "start"])
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .status();
        thread::sleep(Duration::from_millis(500));
    }
}

fn build_agent(timeout: Duration) -> ureq::Agent {
    ureq::Agent::config_builder()
        .timeout_global(Some(timeout))
        .http_status_as_error(false)
        .build()
        .into()
}

fn read_service() -> Result<Service, ReqErr> {
    let path = state_dir().join("opencode/service.json");
    let text = fs::read_to_string(&path)
        .map_err(|error| ReqErr::Service(format!("{} not found ({error})", path.display())))?;
    let file: ServiceFile = serde_json::from_str(&text)
        .map_err(|error| ReqErr::Parse(format!("{}: {error}", path.display())))?;
    Ok(Service {
        url: file.url,
        password: file.password,
    })
}

fn session_from_api(item: &Value, archived: bool) -> Option<Session> {
    let id = item.get("id")?.as_str()?.to_string();
    let directory = item
        .get("location")
        .and_then(|location| location.get("directory"))
        .and_then(Value::as_str)
        .unwrap_or_default()
        .to_string();
    let time = item.get("time").cloned().unwrap_or(Value::Null);
    let tokens = item.get("tokens").cloned().unwrap_or(Value::Null);
    let cache = tokens.get("cache").cloned().unwrap_or(Value::Null);
    let archived_at = time.get("archived").and_then(Value::as_i64).or_else(|| {
        if archived {
            Some(
                time.get("updated")
                    .and_then(Value::as_i64)
                    .unwrap_or_else(now_ms),
            )
        } else {
            None
        }
    });
    Some(Session {
        id,
        parent_id: item
            .get("parentID")
            .and_then(Value::as_str)
            .map(str::to_string),
        project: std::path::Path::new(&directory)
            .file_name()
            .map(|name| name.to_string_lossy().into_owned())
            .unwrap_or_else(|| directory.clone()),
        directory,
        title: item
            .get("title")
            .and_then(Value::as_str)
            .unwrap_or("(untitled)")
            .to_string(),
        agent: item
            .get("agent")
            .and_then(Value::as_str)
            .map(str::to_string),
        model: item
            .get("model")
            .and_then(|model| model.get("id"))
            .and_then(Value::as_str)
            .map(str::to_string),
        cost: item.get("cost").and_then(Value::as_f64).unwrap_or(0.0),
        tokens_input: tokens.get("input").and_then(Value::as_i64).unwrap_or(0),
        tokens_output: tokens.get("output").and_then(Value::as_i64).unwrap_or(0),
        tokens_reasoning: tokens.get("reasoning").and_then(Value::as_i64).unwrap_or(0),
        tokens_cache_read: cache.get("read").and_then(Value::as_i64).unwrap_or(0),
        tokens_cache_write: cache.get("write").and_then(Value::as_i64).unwrap_or(0),
        messages: 0,
        active: false,
        time_created: time.get("created").and_then(Value::as_i64).unwrap_or(0),
        time_updated: time.get("updated").and_then(Value::as_i64).unwrap_or(0),
        time_archived: archived_at,
        time_busy: time.get("busy").and_then(Value::as_i64),
        time_idle: time.get("idle").and_then(Value::as_i64),
        time_suspended: time.get("suspended").and_then(Value::as_i64),
        outcome: item
            .get("outcome")
            .and_then(Value::as_str)
            .map(str::to_string),
    })
}

pub fn backup_session(id: &str) -> Result<PathBuf, String> {
    let dir = state_dir().join("ocsess/backups");
    fs::create_dir_all(&dir).map_err(|error| format!("create {}: {error}", dir.display()))?;
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        let _ = fs::set_permissions(&dir, fs::Permissions::from_mode(0o700));
    }
    let stamp = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_millis();
    let final_path = dir.join(format!("{id}-{stamp}.json.gz"));
    let part_path = dir.join(format!("{id}-{stamp}.json.gz.part"));

    let mut child = ProcessCommand::new("opencode")
        .args(["session", "export", id])
        .stdout(Stdio::piped())
        .stderr(Stdio::null())
        .spawn()
        .map_err(|error| format!("spawn opencode export: {error}"))?;
    let mut stdout = child
        .stdout
        .take()
        .ok_or_else(|| "export stdout unavailable".to_string())?;
    let file = File::create(&part_path)
        .map_err(|error| format!("create {}: {error}", part_path.display()))?;
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        let _ = fs::set_permissions(&part_path, fs::Permissions::from_mode(0o600));
    }
    let mut encoder = GzEncoder::new(file, Compression::default());
    if let Err(error) = io::copy(&mut stdout, &mut encoder) {
        let _ = child.kill();
        let _ = fs::remove_file(&part_path);
        return Err(format!("copy export: {error}"));
    }
    encoder
        .finish()
        .map_err(|error| format!("finish gzip: {error}"))?;
    let status = match child.wait() {
        Ok(status) => status,
        Err(error) => {
            let _ = child.kill();
            let _ = fs::remove_file(&part_path);
            return Err(format!("wait export: {error}"));
        }
    };
    if !status.success() {
        let _ = fs::remove_file(&part_path);
        return Err(format!("export failed ({status})"));
    }
    fs::rename(&part_path, &final_path).map_err(|error| format!("rename backup: {error}"))?;
    Ok(final_path)
}

pub fn state_dir() -> PathBuf {
    std::env::var_os("XDG_STATE_HOME")
        .map(PathBuf::from)
        .unwrap_or_else(|| home().join(".local/state"))
}

fn home() -> PathBuf {
    std::env::var_os("HOME")
        .map(PathBuf::from)
        .unwrap_or_else(|| PathBuf::from("."))
}

#[derive(Clone, Debug)]
pub enum Command {
    Fork(String),
    Rename { id: String, title: String },
    Archive(String),
    Unarchive(String),
    Delete { id: String, backups: Vec<String> },
    Compact(String),
}

#[derive(Debug)]
pub struct WorkerEvent {
    pub message: String,
    pub error: bool,
    pub reload: bool,
    pub select: Option<String>,
}

pub fn spawn_worker(api: Arc<Api>) -> (Sender<Command>, Receiver<WorkerEvent>) {
    let (command_tx, command_rx) = mpsc::channel();
    let (event_tx, event_rx) = mpsc::channel();
    thread::spawn(move || {
        while let Ok(command) = command_rx.recv() {
            let event = run_command(&api, command);
            if event_tx.send(event).is_err() {
                break;
            }
        }
    });
    (command_tx, event_rx)
}

fn run_command(api: &Api, command: Command) -> WorkerEvent {
    match command {
        Command::Fork(id) => match api.fork(&id) {
            Ok(new_id) => WorkerEvent {
                message: format!("Session duplicated · {new_id}"),
                error: false,
                reload: true,
                select: Some(new_id),
            },
            Err(error) => failure(error),
        },
        Command::Rename { id, title } => match api.rename(&id, &title) {
            Ok(()) => WorkerEvent {
                message: "Session renamed".into(),
                error: false,
                reload: true,
                select: None,
            },
            Err(error) => failure(error),
        },
        Command::Archive(id) => match backup_session(&id) {
            Ok(path) => match api.set_archived(&id, true) {
                Ok(()) => WorkerEvent {
                    message: format!(
                        "Archived · backup {}",
                        path.file_name().unwrap_or_default().to_string_lossy()
                    ),
                    error: false,
                    reload: true,
                    select: None,
                },
                Err(error) => WorkerEvent {
                    message: format!("Archive failed · backup kept {} · {error}", path.display()),
                    error: true,
                    reload: false,
                    select: None,
                },
            },
            Err(error) => failure(error),
        },
        Command::Unarchive(id) => match api.set_archived(&id, false) {
            Ok(()) => WorkerEvent {
                message: "Session unarchived".into(),
                error: false,
                reload: true,
                select: None,
            },
            Err(error) => failure(error),
        },
        Command::Delete { id, backups } => {
            let mut paths = Vec::new();
            for backup_id in &backups {
                match backup_session(backup_id) {
                    Ok(path) => paths.push(path),
                    Err(error) => {
                        return WorkerEvent {
                            message: format!(
                                "Delete cancelled · backup {backup_id} failed · {error}"
                            ),
                            error: true,
                            reload: false,
                            select: None,
                        };
                    }
                }
            }
            match api.delete(&id) {
                Ok(()) => WorkerEvent {
                    message: format!("Deleted · {} gzip backup(s)", paths.len()),
                    error: false,
                    reload: true,
                    select: None,
                },
                Err(error) => WorkerEvent {
                    message: format!("Delete failed · {} backup(s) kept · {error}", paths.len()),
                    error: true,
                    reload: true,
                    select: None,
                },
            }
        }
        Command::Compact(id) => match api.compact(&id) {
            Ok(()) => WorkerEvent {
                message: "Compaction started".into(),
                error: false,
                reload: true,
                select: None,
            },
            Err(error) => failure(error),
        },
    }
}

fn failure(error: String) -> WorkerEvent {
    WorkerEvent {
        message: format!("Failed · {error}"),
        error: true,
        reload: false,
        select: None,
    }
}
