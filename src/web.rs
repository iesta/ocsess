use std::collections::HashSet;
use std::io::{Read, Write};
use std::net::{TcpListener, TcpStream};
use std::path::{Path, PathBuf};
use std::sync::{Arc, RwLock};
use std::thread;
use std::time::{Duration, Instant};

use serde_json::{Value, json};

use crate::api::Api;
use crate::db::{
    self, Call, Session, format_cost, format_date, format_duration, format_relative, format_tokens,
    now_ms,
};

const PAGE: &str = include_str!("web/index.html");
const REFRESH_MS: u64 = 1_000;
const ACTIVE_MS: u64 = 2_000;
const MAX_REQUEST: usize = 8 * 1024;

struct Snapshot {
    json: String,
    sessions: usize,
    online: bool,
    updated_at: i64,
    error: Option<String>,
}

impl Snapshot {
    fn empty() -> Self {
        Self {
            json: json!({
                "updated_at": now_ms(),
                "api_online": false,
                "error": "starting",
                "sessions": []
            })
            .to_string(),
            sessions: 0,
            online: false,
            updated_at: now_ms(),
            error: Some("starting".to_string()),
        }
    }
}

pub fn serve(db_path: PathBuf, port: u16) -> Result<(), String> {
    let listener = TcpListener::bind(("127.0.0.1", port))
        .map_err(|error| format!("bind 127.0.0.1:{port}: {error}"))?;
    let actual = listener
        .local_addr()
        .map_err(|error| format!("local address: {error}"))?
        .port();
    let cache = start_cache(db_path.clone());
    println!("ocsess web · http://127.0.0.1:{actual} · read-only · Ctrl+C to stop");
    let _ = std::io::stdout().flush();

    for stream in listener.incoming() {
        match stream {
            Ok(stream) => {
                let cache = Arc::clone(&cache);
                let db_path = db_path.clone();
                thread::spawn(move || handle(stream, &cache, &db_path));
            }
            Err(error) => eprintln!("ocsess web: accept failed: {error}"),
        }
    }
    Ok(())
}

fn start_cache(db_path: PathBuf) -> Arc<RwLock<Snapshot>> {
    let cache = Arc::new(RwLock::new(Snapshot::empty()));
    let shared = Arc::clone(&cache);
    thread::spawn(move || {
        let api = Api::new();
        let mut active: HashSet<String> = HashSet::new();
        let mut online = false;
        let mut last_active = Instant::now()
            .checked_sub(Duration::from_secs(10))
            .unwrap_or_else(Instant::now);
        let mut last_sessions: Vec<Session> = Vec::new();
        loop {
            if last_active.elapsed() >= Duration::from_millis(ACTIVE_MS) {
                match api.active_sessions() {
                    Ok(ids) => {
                        active = ids;
                        online = true;
                    }
                    Err(_) => {
                        active.clear();
                        online = false;
                    }
                }
                last_active = Instant::now();
            }
            let snapshot = build_snapshot(&db_path, &api, &active, online, &mut last_sessions);
            if let Ok(mut guard) = shared.write() {
                *guard = snapshot;
            }
            thread::sleep(Duration::from_millis(REFRESH_MS));
        }
    });
    cache
}

fn build_snapshot(
    db_path: &Path,
    api: &Api,
    active: &HashSet<String>,
    online: bool,
    last_sessions: &mut Vec<Session>,
) -> Snapshot {
    let mut error = None;
    let mut sessions = match db::load_sessions(db_path) {
        Ok(sessions) => sessions,
        Err(db_error) => match api.list_sessions() {
            Ok(sessions) => {
                error = Some(format!("Direct DB read failed, using API · {db_error}"));
                sessions
            }
            Err(api_error) => {
                error = Some(format!("Load failed · {db_error} · {api_error}"));
                last_sessions.clone()
            }
        },
    };
    db::apply_active(&mut sessions, active, online);
    sessions.sort_by(|left, right| {
        right
            .time_updated
            .cmp(&left.time_updated)
            .then_with(|| left.id.cmp(&right.id))
    });
    *last_sessions = sessions.clone();
    Snapshot {
        json: sessions_json(&sessions, online, error.as_deref()),
        sessions: sessions.len(),
        online,
        updated_at: now_ms(),
        error,
    }
}

fn handle(mut stream: TcpStream, cache: &Arc<RwLock<Snapshot>>, db_path: &Path) {
    let _ = stream.set_read_timeout(Some(Duration::from_secs(2)));
    let _ = stream.set_write_timeout(Some(Duration::from_secs(2)));
    let _ = stream.set_nodelay(true);
    let request = match read_request(&mut stream) {
        Ok(request) => request,
        Err(_) => return,
    };
    let head = request.method == "HEAD";
    if request.method != "GET" && request.method != "HEAD" {
        respond(
            &mut stream,
            405,
            "text/plain; charset=utf-8",
            "method not allowed\n",
            false,
        );
        return;
    }
    match route(&request.path) {
        Route::Page => respond(&mut stream, 200, "text/html; charset=utf-8", PAGE, head),
        Route::Favicon => {
            let _ = write!(
                stream,
                "HTTP/1.1 204 No Content\r\n\
                 Cache-Control: public, max-age=86400\r\n\
                 Connection: close\r\n\
                 \r\n"
            );
            let _ = stream.flush();
        }
        Route::Sessions => {
            let body = cache
                .read()
                .map(|snapshot| snapshot.json.clone())
                .unwrap_or_else(|_| json!({"error": "cache unavailable"}).to_string());
            respond(
                &mut stream,
                200,
                "application/json; charset=utf-8",
                &body,
                head,
            );
        }
        Route::Health => {
            let body = cache
                .read()
                .map(|snapshot| health_json(&snapshot))
                .unwrap_or_else(|_| json!({"ok": false, "error": "cache unavailable"}).to_string());
            respond(
                &mut stream,
                200,
                "application/json; charset=utf-8",
                &body,
                head,
            );
        }
        Route::Calls(id) => match db::session_exists(db_path, id) {
            Ok(false) => {
                let body = json!({"error": "unknown session", "id": id}).to_string();
                respond(
                    &mut stream,
                    404,
                    "application/json; charset=utf-8",
                    &body,
                    head,
                );
            }
            Ok(true) => match calls_json(db_path, id) {
                Ok(body) => respond(
                    &mut stream,
                    200,
                    "application/json; charset=utf-8",
                    &body,
                    head,
                ),
                Err(error) => {
                    let body = json!({"error": error}).to_string();
                    respond(
                        &mut stream,
                        500,
                        "application/json; charset=utf-8",
                        &body,
                        head,
                    );
                }
            },
            Err(error) => {
                let body = json!({"error": error}).to_string();
                respond(
                    &mut stream,
                    500,
                    "application/json; charset=utf-8",
                    &body,
                    head,
                );
            }
        },
        Route::NotFound => respond(
            &mut stream,
            404,
            "text/plain; charset=utf-8",
            "not found\n",
            head,
        ),
    }
}

struct Request {
    method: String,
    path: String,
}

fn read_request(stream: &mut TcpStream) -> Result<Request, String> {
    let mut buffer = [0u8; 1024];
    let mut raw = Vec::new();
    loop {
        let read = stream
            .read(&mut buffer)
            .map_err(|error| error.to_string())?;
        if read == 0 {
            break;
        }
        raw.extend_from_slice(&buffer[..read]);
        if raw.windows(4).any(|window| window == b"\r\n\r\n")
            || raw.windows(2).any(|window| window == b"\n\n")
        {
            break;
        }
        if raw.len() >= MAX_REQUEST {
            return Err("request too large".to_string());
        }
    }
    if raw.is_empty() {
        return Err("empty request".to_string());
    }
    let text = String::from_utf8_lossy(&raw);
    let line = text.lines().next().ok_or("empty request line")?;
    let mut parts = line.split_whitespace();
    let method = parts.next().ok_or("missing method")?.to_string();
    let target = parts.next().ok_or("missing target")?.to_string();
    let path = target.split('?').next().unwrap_or("/").to_string();
    Ok(Request { method, path })
}

#[derive(Debug, PartialEq, Eq)]
enum Route<'a> {
    Page,
    Favicon,
    Sessions,
    Health,
    Calls(&'a str),
    NotFound,
}

fn route(path: &str) -> Route<'_> {
    match path {
        "/" | "/index.html" => Route::Page,
        // Browsers request this unconditionally; answering 204 keeps the
        // console clean without shipping an icon.
        "/favicon.ico" => Route::Favicon,
        "/api/sessions" => Route::Sessions,
        "/api/health" => Route::Health,
        _ => match path
            .strip_prefix("/api/sessions/")
            .and_then(|rest| rest.strip_suffix("/calls"))
        {
            Some(id) if valid_id(id) => Route::Calls(id),
            _ => Route::NotFound,
        },
    }
}

fn valid_id(id: &str) -> bool {
    !id.is_empty()
        && id.chars().all(|character| {
            character.is_ascii_alphanumeric() || character == '_' || character == '-'
        })
}

fn sessions_json(sessions: &[Session], online: bool, error: Option<&str>) -> String {
    let items: Vec<Value> = sessions
        .iter()
        .map(|session| {
            json!({
                "id": session.id,
                "parent_id": session.parent_id,
                "title": session.title,
                "project": session.project,
                "directory": session.directory,
                "agent": session.agent,
                "model": session.model,
                "cost": session.cost,
                "cost_display": format_cost(session.cost),
                "tokens": session.total_tokens(),
                "tokens_display": format_tokens(session.total_tokens()),
                "messages": session.messages,
                "state": session.status().label(),
                "state_symbol": session.status().symbol(),
                "state_rank": session.status().rank(),
                "active": session.active,
                "time_created": session.time_created,
                "created_display": format_date(session.time_created),
                "time_updated": session.time_updated,
                "updated_display": format_relative(session.time_updated),
                "time_archived": session.time_archived,
                "archived_display": session.time_archived.map(format_date).unwrap_or_default(),
                "outcome": session.outcome,
            })
        })
        .collect();
    json!({
        "updated_at": now_ms(),
        "api_online": online,
        "error": error,
        "sessions": items
    })
    .to_string()
}

fn calls_json(db_path: &Path, id: &str) -> Result<String, String> {
    let calls = db::load_calls(db_path, id)?;
    Ok(calls_json_from(id, &calls))
}

fn calls_json_from(id: &str, calls: &[Call]) -> String {
    let summary = db::summarize(calls);
    let items: Vec<Value> = calls
        .iter()
        .map(|call| {
            json!({
                "id": call.id,
                "created": call.created,
                "created_display": format_date(call.created),
                "model": call.model,
                "cost": call.cost,
                "cost_display": call.cost.map(format_cost).unwrap_or_else(|| "—".to_string()),
                "tokens_input": call.tokens_input,
                "tokens_output": call.tokens_output,
                "tokens_reasoning": call.tokens_reasoning,
                "tokens_cache_read": call.tokens_cache_read,
                "tokens_cache_write": call.tokens_cache_write,
                "duration_ms": call.duration_ms(),
                "duration_display": format_duration(call.duration_ms()),
                "finish": call.finish,
                "tools": call.tools.iter().map(|tool| json!({
                    "name": tool.name,
                    "status": tool.status,
                })).collect::<Vec<Value>>(),
                "tools_label": call.tools_label(),
            })
        })
        .collect();
    json!({
        "session_id": id,
        "updated_at": now_ms(),
        "summary": {
            "calls": summary.calls,
            "cost": summary.cost,
            "cost_display": format_cost(summary.cost),
            "avg_cost": summary.avg_cost(),
            "avg_cost_display": format_cost(summary.avg_cost()),
            "tools": summary.tools,
            "errors": summary.errors,
            "tokens_input": summary.tokens_input,
            "tokens_output": summary.tokens_output,
            "tokens_reasoning": summary.tokens_reasoning,
            "cache_read": summary.cache_read,
            "cache_write": summary.cache_write,
        },
        "calls": items
    })
    .to_string()
}

fn health_json(snapshot: &Snapshot) -> String {
    json!({
        "ok": snapshot.error.is_none(),
        "sessions": snapshot.sessions,
        "api_online": snapshot.online,
        "updated_at": snapshot.updated_at,
        "error": snapshot.error,
    })
    .to_string()
}

fn respond(stream: &mut TcpStream, status: u16, content_type: &str, body: &str, head: bool) {
    let reason = match status {
        200 => "OK",
        400 => "Bad Request",
        404 => "Not Found",
        405 => "Method Not Allowed",
        500 => "Internal Server Error",
        _ => "OK",
    };
    let _ = write!(
        stream,
        "HTTP/1.1 {status} {reason}\r\n\
         Content-Type: {content_type}\r\n\
         Content-Length: {}\r\n\
         Cache-Control: no-store\r\n\
         X-Content-Type-Options: nosniff\r\n\
         Referrer-Policy: no-referrer\r\n\
         Content-Security-Policy: default-src 'self'; base-uri 'none'; frame-ancestors 'none'; style-src 'unsafe-inline'; script-src 'unsafe-inline'\r\n\
         Connection: close\r\n\
         \r\n",
        body.len()
    );
    if !head {
        let _ = stream.write_all(body.as_bytes());
    }
    let _ = stream.flush();
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::db::ToolUse;

    fn session() -> Session {
        Session {
            id: "ses_test".into(),
            parent_id: None,
            project: "proj".into(),
            directory: "/tmp/proj".into(),
            title: "Web <test>".into(),
            agent: Some("build".into()),
            model: Some("m".into()),
            cost: 0.0,
            tokens_input: 0,
            tokens_output: 0,
            tokens_reasoning: 0,
            tokens_cache_read: 0,
            tokens_cache_write: 0,
            messages: 2,
            active: false,
            time_created: 1,
            time_updated: 2,
            time_archived: None,
            time_busy: None,
            time_idle: Some(3),
            time_suspended: None,
            outcome: Some("succeeded".into()),
        }
    }

    #[test]
    fn routes_match_paths() {
        assert_eq!(route("/"), Route::Page);
        assert_eq!(route("/index.html"), Route::Page);
        assert_eq!(route("/favicon.ico"), Route::Favicon);
        assert_eq!(route("/api/sessions"), Route::Sessions);
        assert_eq!(route("/api/health"), Route::Health);
        assert_eq!(route("/api/sessions/ses_a/calls"), Route::Calls("ses_a"));
        assert_eq!(route("/api/sessions/../calls"), Route::NotFound);
        assert_eq!(route("/nope"), Route::NotFound);
    }

    #[test]
    fn sessions_json_contains_display_fields() {
        let body = sessions_json(&[session()], true, None);
        let value: Value = serde_json::from_str(&body).unwrap();
        assert_eq!(value["api_online"], true);
        assert_eq!(value["sessions"][0]["id"], "ses_test");
        assert_eq!(value["sessions"][0]["state"], "idle");
        assert_eq!(value["sessions"][0]["tokens_display"], "—");
        assert_eq!(value["sessions"][0]["title"], "Web <test>");
    }

    #[test]
    fn calls_json_has_summary_and_tools() {
        let calls = vec![Call {
            id: "msg".into(),
            created: 1_000,
            streamed: Some(2_000),
            model: Some("model".into()),
            cost: Some(0.5),
            tokens_input: 100,
            tokens_output: 20,
            tokens_reasoning: 5,
            tokens_cache_read: 300,
            tokens_cache_write: 0,
            finish: Some("tool-calls".into()),
            tools: vec![ToolUse {
                name: Some("shell".into()),
                status: Some("completed".into()),
            }],
        }];
        let value: Value = serde_json::from_str(&calls_json_from("ses_test", &calls)).unwrap();
        assert_eq!(value["summary"]["calls"], 1);
        assert_eq!(value["summary"]["cost"], 0.5);
        assert_eq!(value["calls"][0]["tools_label"], "shell");
    }
}
