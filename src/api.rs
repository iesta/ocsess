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

use crate::db::{Session, derive_busy};

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
        let value = self.request_quiet("GET", "/api/session?limit=1000&order=desc", None)?;
        let Some(items) = value.get("data").and_then(Value::as_array) else {
            return Ok(Vec::new());
        };
        Ok(items
            .iter()
            .filter_map(session_from_api)
            .collect::<Vec<Session>>())
    }

    pub fn active_sessions(&self) -> Result<HashSet<String>, String> {
        let value = self.request_quiet("GET", "/api/session/active", None)?;
        let Some(data) = value.get("data").and_then(Value::as_object) else {
            return Ok(HashSet::new());
        };
        Ok(data.keys().cloned().collect())
    }

    /// OpenCode v2 has no write path for archiving: there is no endpoint, and
    /// `PATCH /api/session/{id}` accepts `archived` then ignores it. Ask the
    /// server for its version once so the UI can hide keys that cannot work
    /// instead of offering them and failing. Unknown (service offline) is
    /// treated as supported, and `set_archived` still verifies the result.
    pub fn supports_archive(&self) -> bool {
        let Ok(value) = self.request_quiet("GET", "/api/info", None) else {
            return true;
        };
        value
            .get("version")
            .and_then(Value::as_str)
            .and_then(major_version)
            .is_none_or(|major| major <= 1)
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
        // Some builds accept the field and silently ignore it, so confirm the
        // change took effect before telling the user it worked.
        let value = self.request_quiet("GET", &format!("/api/session/{id}"), None)?;
        let stored = value
            .get("data")
            .and_then(|data| data.get("time"))
            .and_then(|time| time.get("archived"))
            .is_some_and(|archived| !archived.is_null());
        if stored != archived {
            return Err(if archived {
                "this OpenCode version ignores session archiving · use delete instead".to_string()
            } else {
                "this OpenCode version ignores session archiving".to_string()
            });
        }
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
        match read_service() {
            Ok(service) => match self.request_with(&self.agent, &service, method, path, body) {
                Ok(value) => Ok(value),
                Err(ReqErr::Transport(_)) => self.retry_after_start(method, path, body),
                Err(error) => Err(error.to_string()),
            },
            // A missing or unreadable service file usually means the service is
            // not running yet, not that the request is invalid. Start it and
            // retry once; the second read_service() reports the real error.
            Err(_) => self.retry_after_start(method, path, body),
        }
    }

    fn retry_after_start(
        &self,
        method: &str,
        path: &str,
        body: Option<&str>,
    ) -> Result<Value, String> {
        self.start_service();
        let service = read_service().map_err(|error| error.to_string())?;
        self.request_with(&self.agent, &service, method, path, body)
            .map_err(|error| error.to_string())
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

fn major_version(version: &str) -> Option<u32> {
    version
        .trim()
        .trim_start_matches('v')
        .split('.')
        .next()
        .and_then(|major| major.parse().ok())
}

fn session_from_api(item: &Value) -> Option<Session> {
    let id = item.get("id")?.as_str()?.to_string();
    let directory = item
        .get("location")
        .and_then(|location| location.get("directory"))
        .or_else(|| item.get("directory"))
        .or_else(|| item.get("path").and_then(|path| path.get("cwd")))
        .and_then(Value::as_str)
        .unwrap_or_default()
        .to_string();
    let time = item.get("time").cloned().unwrap_or(Value::Null);
    let tokens = item.get("tokens").cloned().unwrap_or(Value::Null);
    let cache = tokens.get("cache").cloned().unwrap_or(Value::Null);
    let model = item
        .get("model")
        .and_then(|model| {
            model
                .get("id")
                .or_else(|| model.get("modelID"))
                .and_then(Value::as_str)
                .or_else(|| model.as_str())
        })
        .map(str::to_string);
    let updated = time.get("updated").and_then(Value::as_i64).unwrap_or(0);
    let idle = time.get("idle").and_then(Value::as_i64);
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
        model,
        cost: item.get("cost").and_then(Value::as_f64).unwrap_or(0.0),
        tokens_input: tokens.get("input").and_then(Value::as_i64).unwrap_or(0),
        tokens_output: tokens.get("output").and_then(Value::as_i64).unwrap_or(0),
        tokens_reasoning: tokens.get("reasoning").and_then(Value::as_i64).unwrap_or(0),
        tokens_cache_read: cache.get("read").and_then(Value::as_i64).unwrap_or(0),
        tokens_cache_write: cache.get("write").and_then(Value::as_i64).unwrap_or(0),
        messages: 0,
        active: false,
        time_created: time.get("created").and_then(Value::as_i64).unwrap_or(0),
        time_updated: updated,
        time_archived: time.get("archived").and_then(Value::as_i64),
        time_busy: time
            .get("busy")
            .and_then(Value::as_i64)
            .or_else(|| derive_busy(updated, idle)),
        time_idle: idle,
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

#[cfg(test)]
mod tests {
    use super::*;
    use crate::db::Status;

    // Shape returned by the OpenCode v2 service: no `time.busy`, directory
    // under `location`, model as an object.
    fn v2_payload(updated: i64, idle: i64) -> serde_json::Value {
        json!({
            "id": "ses_v2",
            "projectID": "proj",
            "agent": "build",
            "model": {"id": "deepseek/deepseek-v4.1", "providerID": "openrouter", "variant": "high"},
            "cost": 0.25,
            "tokens": {"input": 100, "output": 20, "reasoning": 5, "cache": {"read": 300, "write": 7}},
            "outcome": "succeeded",
            "time": {"created": 1, "updated": updated, "idle": idle, "viewed": idle},
            "title": "Directory review",
            "location": {"directory": "/home/jm/dev/ocsess"}
        })
    }

    // Shape returned by older services: flat directory, model as a plain
    // string, archived sessions flagged with `time.archived`.
    const V1: &str = r#"{
        "id": "ses_v1",
        "directory": "/home/jm/dev/old",
        "model": "~deepseek/deepseek-v4-flash-latest",
        "time": {"created": 1, "updated": 2, "archived": 3},
        "title": "Old session"
    }"#;

    fn parse(item: &serde_json::Value) -> Session {
        session_from_api(item).unwrap()
    }

    #[test]
    fn parses_v2_session() {
        let now = crate::db::now_ms();
        let session = parse(&v2_payload(now - 1_000, now - 2_000));
        assert_eq!(session.id, "ses_v2");
        assert_eq!(session.directory, "/home/jm/dev/ocsess");
        assert_eq!(session.project, "ocsess");
        assert_eq!(session.model.as_deref(), Some("deepseek/deepseek-v4.1"));
        assert_eq!(session.tokens_cache_read, 300);
        assert_eq!(session.tokens_cache_write, 7);
        assert_eq!(session.time_archived, None);
        // v2 has no time.busy, so it comes from updated > idle.
        assert_eq!(session.time_busy, Some(now - 1_000));
        assert_eq!(session.status(), Status::Running);
        assert_eq!(session.outcome.as_deref(), Some("succeeded"));
    }

    #[test]
    fn old_update_is_not_running() {
        let now = crate::db::now_ms();
        let session = parse(&v2_payload(now - 900_000, now - 1_000_000));
        assert_eq!(session.time_busy, None);
        assert_eq!(session.status(), Status::Idle);
    }

    #[test]
    fn parses_legacy_session() {
        let session = parse(&serde_json::from_str(V1).unwrap());
        assert_eq!(session.directory, "/home/jm/dev/old");
        assert_eq!(session.project, "old");
        assert_eq!(
            session.model.as_deref(),
            Some("~deepseek/deepseek-v4-flash-latest")
        );
        assert_eq!(session.time_archived, Some(3));
        assert_eq!(session.status(), Status::Archived);
        assert_eq!(session.time_busy, None);
    }

    #[test]
    fn version_major_parses() {
        assert_eq!(major_version("2.0.19"), Some(2));
        assert_eq!(major_version("v1.14.2"), Some(1));
        assert_eq!(major_version(" 3.1 "), Some(3));
        assert_eq!(major_version("dev"), None);
    }
}
