use std::collections::HashSet;
use std::path::{Path, PathBuf};
use std::process::Command;
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use rusqlite::{Connection, OpenFlags};
use serde::Deserialize;
use serde_json::Value;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Status {
    Running,
    Suspended,
    Idle,
    New,
    Archived,
}

impl Status {
    pub fn label(self) -> &'static str {
        match self {
            Status::Running => "running",
            Status::Suspended => "suspended",
            Status::Idle => "idle",
            Status::New => "new",
            Status::Archived => "archived",
        }
    }

    pub fn symbol(self) -> &'static str {
        match self {
            Status::Running => "●",
            Status::Suspended => "◐",
            Status::Idle => "○",
            Status::New => "·",
            Status::Archived => "▣",
        }
    }

    pub fn rank(self) -> u8 {
        match self {
            Status::Running => 0,
            Status::Suspended => 1,
            Status::Idle => 2,
            Status::New => 3,
            Status::Archived => 4,
        }
    }
}

#[derive(Clone, Debug)]
pub struct Session {
    pub id: String,
    pub parent_id: Option<String>,
    pub project: String,
    pub directory: String,
    pub title: String,
    pub agent: Option<String>,
    pub model: Option<String>,
    pub cost: f64,
    pub tokens_input: i64,
    pub tokens_output: i64,
    pub tokens_reasoning: i64,
    pub tokens_cache_read: i64,
    pub tokens_cache_write: i64,
    pub messages: i64,
    pub active: bool,
    pub time_created: i64,
    pub time_updated: i64,
    pub time_archived: Option<i64>,
    pub time_busy: Option<i64>,
    pub time_idle: Option<i64>,
    pub time_suspended: Option<i64>,
    pub outcome: Option<String>,
}

impl Session {
    pub fn status(&self) -> Status {
        if self.time_archived.is_some() {
            return Status::Archived;
        }
        if self.active {
            return Status::Running;
        }
        if let Some(suspended) = self.time_suspended
            && self.time_idle.is_none_or(|idle| suspended >= idle)
        {
            return Status::Suspended;
        }
        if let Some(busy) = self.time_busy
            && self.time_idle.is_none_or(|idle| busy > idle)
        {
            return Status::Running;
        }
        if self.time_idle.is_some() {
            Status::Idle
        } else {
            Status::New
        }
    }

    pub fn total_tokens(&self) -> i64 {
        self.tokens_input
            + self.tokens_output
            + self.tokens_reasoning
            + self.tokens_cache_read
            + self.tokens_cache_write
    }
}

pub fn now_ms() -> i64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_millis() as i64
}

/// Merge the API's active set into database sessions. When the service is
/// unreachable the last answer is not evidence, so the database state wins.
pub fn apply_active(sessions: &mut [Session], active_ids: &HashSet<String>, online: bool) {
    for session in sessions {
        session.active = online && active_ids.contains(&session.id);
        if online && !session.active {
            // The service answered, so its active list wins over the heuristic
            // busy mark derived from the idle timestamp.
            session.time_busy = None;
        }
    }
}

/// A session that has updated after its idle mark is working again. Bounded in
/// time because a session killed mid-run never writes a newer idle mark and
/// would otherwise look busy forever.
pub const BUSY_WINDOW_MS: i64 = 15 * 60 * 1000;

/// Some v2 databases omit `time.busy` from both the database and the API, so
/// derive it from the idle mark instead.
pub fn derive_busy(updated: i64, idle: Option<i64>) -> Option<i64> {
    let idle = idle?;
    (updated > idle && updated > now_ms() - BUSY_WINDOW_MS).then_some(updated)
}

pub fn format_relative(ms: i64) -> String {
    format_relative_at(ms, now_ms())
}

pub fn format_relative_at(ms: i64, now: i64) -> String {
    if ms <= 0 {
        return "—".to_string();
    }
    let diff = (now - ms).max(0);
    const MINUTE: i64 = 60_000;
    const HOUR: i64 = 60 * MINUTE;
    const DAY: i64 = 24 * HOUR;
    if diff < MINUTE {
        "now".to_string()
    } else if diff < HOUR {
        format!("{}m", diff / MINUTE)
    } else if diff < DAY {
        format!("{}h", diff / HOUR)
    } else if diff < 30 * DAY {
        format!("{}d", diff / DAY)
    } else if diff < 365 * DAY {
        format!("{}mo", diff / (30 * DAY))
    } else {
        format!("{}y", diff / (365 * DAY))
    }
}

pub fn format_date(ms: i64) -> String {
    if ms <= 0 {
        return "—".to_string();
    }
    let secs = ms.div_euclid(1000);
    let days = secs.div_euclid(86_400);
    let rem = secs.rem_euclid(86_400);
    let (year, month, day) = civil_from_days(days);
    format!(
        "{year:04}-{month:02}-{day:02} {:02}:{:02}",
        rem / 3600,
        (rem % 3600) / 60
    )
}

fn civil_from_days(days: i64) -> (i64, u32, u32) {
    let z = days + 719_468;
    let era = if z >= 0 { z } else { z - 146_096 } / 146_097;
    let doe = z - era * 146_097;
    let yoe = (doe - doe / 1_460 + doe / 36_524 - doe / 146_096) / 365;
    let year = yoe + era * 400;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let day = doy - (153 * mp + 2) / 5 + 1;
    let month = if mp < 10 { mp + 3 } else { mp - 9 };
    (year + i64::from(month <= 2), month as u32, day as u32)
}

pub fn format_cost(cost: f64) -> String {
    if cost <= 0.0 {
        "—".to_string()
    } else if cost < 0.01 {
        format!("${cost:.4}")
    } else if cost < 1.0 {
        format!("${cost:.3}")
    } else {
        format!("${cost:.2}")
    }
}

pub fn format_tokens(tokens: i64) -> String {
    if tokens <= 0 {
        "—".to_string()
    } else if tokens >= 1_000_000 {
        format!("{:.1}M", tokens as f64 / 1_000_000.0)
    } else if tokens >= 1_000 {
        let thousands = tokens as f64 / 1_000.0;
        // 999_999 must round up to 1.0M, not print "1000.0k".
        if thousands >= 999.95 {
            "1.0M".to_string()
        } else {
            format!("{thousands:.1}k")
        }
    } else {
        tokens.to_string()
    }
}

pub fn default_db_path() -> PathBuf {
    if let Some(path) = std::env::var_os("OCSESS_DB") {
        return PathBuf::from(path);
    }
    let data = std::env::var_os("XDG_DATA_HOME")
        .map(PathBuf::from)
        .unwrap_or_else(|| home().join(".local/share"));
    let candidate = data.join("opencode/opencode.db");
    if candidate.exists() {
        return candidate;
    }
    opencode_debug_db().unwrap_or(candidate)
}

fn home() -> PathBuf {
    std::env::var_os("HOME")
        .map(PathBuf::from)
        .unwrap_or_else(|| PathBuf::from("."))
}

fn opencode_debug_db() -> Option<PathBuf> {
    let out = Command::new("opencode")
        .args(["debug", "paths"])
        .output()
        .ok()?;
    let text = String::from_utf8_lossy(&out.stdout);
    text.lines().find_map(|line| {
        let mut parts = line.split_whitespace();
        if parts.next() == Some("db") {
            parts.next().map(PathBuf::from)
        } else {
            None
        }
    })
}

fn open_db(path: &Path) -> Result<Connection, String> {
    let conn = Connection::open_with_flags(path, OpenFlags::SQLITE_OPEN_READ_ONLY)
        .map_err(|e| format!("open DB {}: {e}", path.display()))?;
    conn.busy_timeout(Duration::from_millis(1_500))
        .map_err(|e| format!("busy_timeout: {e}"))?;
    let _ = conn.pragma_update(None, "query_only", "ON");
    Ok(conn)
}

/// OpenCode v1 keeps sessions in `session` with messages in `message`/`part`;
/// v2 moved them to `session_v2` with `session_message`. Detect what this
/// database has instead of assuming one layout, so both versions load.
struct Schema {
    sessions: &'static str,
    messages: &'static str,
    session_columns: Vec<String>,
    parts: bool,
    /// Whether the `project` table can be joined, i.e. both it and the
    /// `project_id` column exist.
    project: bool,
}

impl Schema {
    fn detect(conn: &Connection) -> Result<Self, String> {
        let objects = object_names(conn)?;
        let pick = |candidates: [&'static str; 2]| {
            candidates
                .into_iter()
                .find(|name| objects.iter().any(|object| object == name))
        };
        let sessions = pick(["session_v2", "session"]).ok_or_else(|| {
            "no session table in this database (expected session_v2 or session)".to_string()
        })?;
        let messages = pick(["session_message", "message"]).ok_or_else(|| {
            "no message table in this database (expected session_message or message)".to_string()
        })?;
        let session_columns = columns(conn, sessions)?;
        let has_project = objects.iter().any(|object| object == "project")
            && session_columns.iter().any(|column| column == "project_id");
        Ok(Self {
            sessions,
            messages,
            session_columns,
            parts: objects.iter().any(|object| object == "part"),
            project: has_project,
        })
    }

    fn has(&self, column: &str) -> bool {
        self.session_columns.iter().any(|name| name == column)
    }

    /// Column reference, or `NULL` when this OpenCode schema does not have it.
    fn col(&self, column: &str) -> String {
        if self.has(column) {
            format!("s.{column}")
        } else {
            "NULL".to_string()
        }
    }
}

fn object_names(conn: &Connection) -> Result<Vec<String>, String> {
    let mut stmt = conn
        .prepare("SELECT name FROM sqlite_master WHERE type IN ('table', 'view')")
        .map_err(|e| format!("read schema: {e}"))?;
    let rows = stmt
        .query_map([], |row| row.get::<_, String>(0))
        .map_err(|e| format!("read schema: {e}"))?;
    rows.collect::<Result<Vec<_>, _>>()
        .map_err(|e| format!("read schema: {e}"))
}

fn columns(conn: &Connection, table: &str) -> Result<Vec<String>, String> {
    let mut stmt = conn
        .prepare(&format!("PRAGMA table_info({table})"))
        .map_err(|e| format!("read {table} columns: {e}"))?;
    let rows = stmt
        .query_map([], |row| row.get::<_, String>(1))
        .map_err(|e| format!("read {table} columns: {e}"))?;
    rows.collect::<Result<Vec<_>, _>>()
        .map_err(|e| format!("read {table} columns: {e}"))
}

/// v2 databases may keep a nullable `time_busy` that is only set while a run is
/// live. Fall back to the idle mark so a recent update still reads as working,
/// and keep the API's active list as the final word when the service is up.
fn busy_expr(schema: &Schema) -> String {
    let derived = format!(
        "CASE WHEN s.time_updated > s.time_idle AND s.time_updated > {} \
         THEN s.time_updated END",
        now_ms() - BUSY_WINDOW_MS
    );
    match (schema.has("time_busy"), schema.has("time_idle")) {
        (true, true) => format!("COALESCE(s.time_busy, {derived})"),
        (true, false) => "s.time_busy".to_string(),
        (false, true) => derived,
        (false, false) => "NULL".to_string(),
    }
}

pub fn load_sessions(path: &Path) -> Result<Vec<Session>, String> {
    let conn = open_db(path)?;
    query_sessions(&conn)
}

fn query_sessions(conn: &Connection) -> Result<Vec<Session>, String> {
    let schema = Schema::detect(conn)?;
    let selects = [
        "s.id".to_string(),
        schema.col("parent_id"),
        if schema.project {
            "COALESCE(p.name, '')".to_string()
        } else {
            "''".to_string()
        },
        schema.col("directory"),
        schema.col("title"),
        schema.col("agent"),
        schema.col("model"),
        schema.col("cost"),
        schema.col("tokens_input"),
        schema.col("tokens_output"),
        schema.col("tokens_reasoning"),
        schema.col("tokens_cache_read"),
        schema.col("tokens_cache_write"),
        schema.col("time_created"),
        schema.col("time_updated"),
        schema.col("time_archived"),
        busy_expr(&schema),
        schema.col("time_idle"),
        schema.col("time_suspended"),
        schema.col("idle_outcome"),
        format!(
            "(SELECT COUNT(*) FROM {} m WHERE m.session_id = s.id)",
            schema.messages
        ),
    ];
    let join = if schema.project {
        "LEFT JOIN project p ON p.id = s.project_id"
    } else {
        ""
    };
    let sql = format!(
        "SELECT {}\n        FROM {} s\n        {join}",
        selects.join(", "),
        schema.sessions
    );
    let mut stmt = conn
        .prepare(&sql)
        .map_err(|e| format!("query sessions: {e}"))?;
    let rows = stmt
        .query_map([], |row| {
            let directory: String = row.get::<_, Option<String>>(3)?.unwrap_or_default();
            let project: String = row.get(2)?;
            let raw_model: Option<String> = row.get(6)?;
            Ok(Session {
                id: row.get(0)?,
                parent_id: row.get(1)?,
                project: if project.trim().is_empty() {
                    Path::new(&directory)
                        .file_name()
                        .map(|name| name.to_string_lossy().into_owned())
                        .unwrap_or_else(|| directory.clone())
                } else {
                    project
                },
                directory,
                title: row
                    .get::<_, Option<String>>(4)?
                    .filter(|title| !title.trim().is_empty())
                    .unwrap_or_else(|| "(untitled)".into()),
                agent: row.get(5)?,
                model: parse_model(raw_model),
                cost: row.get::<_, Option<f64>>(7)?.unwrap_or(0.0),
                tokens_input: row.get::<_, Option<i64>>(8)?.unwrap_or(0),
                tokens_output: row.get::<_, Option<i64>>(9)?.unwrap_or(0),
                tokens_reasoning: row.get::<_, Option<i64>>(10)?.unwrap_or(0),
                tokens_cache_read: row.get::<_, Option<i64>>(11)?.unwrap_or(0),
                tokens_cache_write: row.get::<_, Option<i64>>(12)?.unwrap_or(0),
                time_created: row.get::<_, Option<i64>>(13)?.unwrap_or(0),
                time_updated: row.get::<_, Option<i64>>(14)?.unwrap_or(0),
                time_archived: row.get(15)?,
                time_busy: row.get(16)?,
                time_idle: row.get(17)?,
                time_suspended: row.get(18)?,
                outcome: row.get(19)?,
                messages: row.get(20)?,
                active: false,
            })
        })
        .map_err(|e| format!("read sessions: {e}"))?;

    rows.collect::<Result<Vec<_>, _>>()
        .map_err(|e| format!("mapping sessions: {e}"))
}

fn parse_model(raw: Option<String>) -> Option<String> {
    let raw = raw?;
    if raw.trim().is_empty() {
        return None;
    }
    let value: Value = serde_json::from_str(&raw).unwrap_or(Value::Null);
    value
        .get("id")
        .or_else(|| value.get("modelID"))
        .and_then(Value::as_str)
        .map(str::to_string)
        .or(Some(raw))
}

#[derive(Clone, Debug, Deserialize)]
pub struct ToolUse {
    pub name: Option<String>,
    pub status: Option<String>,
}

#[derive(Clone, Debug)]
pub struct Call {
    pub id: String,
    pub created: i64,
    pub streamed: Option<i64>,
    pub model: Option<String>,
    pub cost: Option<f64>,
    pub tokens_input: i64,
    pub tokens_output: i64,
    pub tokens_reasoning: i64,
    pub tokens_cache_read: i64,
    pub tokens_cache_write: i64,
    pub finish: Option<String>,
    pub tools: Vec<ToolUse>,
}

impl Call {
    pub fn duration_ms(&self) -> Option<i64> {
        self.streamed
            .map(|streamed| (streamed - self.created).max(0))
    }

    pub fn tools_label(&self) -> String {
        let mut counts: std::collections::BTreeMap<&str, (usize, usize)> =
            std::collections::BTreeMap::new();
        for tool in &self.tools {
            let name = tool.name.as_deref().unwrap_or("?");
            let entry = counts.entry(name).or_insert((0, 0));
            entry.0 += 1;
            if tool.status.as_deref() == Some("error") {
                entry.1 += 1;
            }
        }
        counts
            .into_iter()
            .map(|(name, (count, errors))| {
                let mut label = if count > 1 {
                    format!("{name}×{count}")
                } else {
                    name.to_string()
                };
                if errors > 0 {
                    label.push('!');
                }
                label
            })
            .collect::<Vec<_>>()
            .join(", ")
    }
}

#[derive(Clone, Debug, Default)]
pub struct CallSummary {
    pub calls: usize,
    pub cost: f64,
    pub tools: usize,
    pub errors: usize,
    pub tokens_input: i64,
    pub tokens_output: i64,
    pub tokens_reasoning: i64,
    pub cache_read: i64,
    pub cache_write: i64,
}

impl CallSummary {
    pub fn avg_cost(&self) -> f64 {
        if self.calls == 0 {
            0.0
        } else {
            self.cost / self.calls as f64
        }
    }
}

pub fn summarize(calls: &[Call]) -> CallSummary {
    let mut summary = CallSummary {
        calls: calls.len(),
        ..CallSummary::default()
    };
    for call in calls {
        summary.cost += call.cost.unwrap_or(0.0);
        summary.tools += call.tools.len();
        summary.errors += call
            .tools
            .iter()
            .filter(|tool| tool.status.as_deref() == Some("error"))
            .count();
        summary.tokens_input += call.tokens_input;
        summary.tokens_output += call.tokens_output;
        summary.tokens_reasoning += call.tokens_reasoning;
        summary.cache_read += call.tokens_cache_read;
        summary.cache_write += call.tokens_cache_write;
    }
    summary
}

pub fn load_calls(path: &Path, session_id: &str) -> Result<Vec<Call>, String> {
    let conn = open_db(path)?;
    query_calls(&conn, session_id)
}

/// Whether this database knows the session. Lets the web view answer 404 for a
/// typo instead of an empty call list.
pub fn session_exists(path: &Path, session_id: &str) -> Result<bool, String> {
    let conn = open_db(path)?;
    let schema = Schema::detect(&conn)?;
    let sql = format!("SELECT 1 FROM {} WHERE id = ?1 LIMIT 1", schema.sessions);
    let mut stmt = conn
        .prepare(&sql)
        .map_err(|e| format!("query session: {e}"))?;
    stmt.exists([session_id])
        .map_err(|e| format!("read session: {e}"))
}

fn query_calls(conn: &Connection, session_id: &str) -> Result<Vec<Call>, String> {
    let schema = Schema::detect(conn)?;
    let modern = schema.messages == "session_message";
    let role_filter = if modern {
        "m.type = 'assistant'"
    } else {
        "json_extract(m.data, '$.role') = 'assistant'"
    };
    let tools_expr = if modern {
        "COALESCE((
             SELECT json_group_array(json_object(
                 'name', json_extract(p.value, '$.name'),
                 'status', json_extract(p.value, '$.state.status')
             ))
             FROM json_each(m.data, '$.content') p
             WHERE json_extract(p.value, '$.type') = 'tool'
         ), '[]')"
            .to_string()
    } else if schema.parts {
        "COALESCE((
             SELECT json_group_array(json_object(
                 'name', json_extract(pt.data, '$.tool'),
                 'status', json_extract(pt.data, '$.state.status')
             ))
             FROM part pt
             WHERE pt.message_id = m.id AND json_extract(pt.data, '$.type') = 'tool'
         ), '[]')"
            .to_string()
    } else {
        "'[]'".to_string()
    };
    let sql = format!(
        "SELECT m.id, m.time_created,
               COALESCE(json_extract(m.data, '$.time.streamed'),
                        json_extract(m.data, '$.time.completed')),
               COALESCE(json_extract(m.data, '$.model.id'),
                        json_extract(m.data, '$.model.modelID'),
                        json_extract(m.data, '$.modelID')),
               json_extract(m.data, '$.cost'),
               json_extract(m.data, '$.tokens.input'),
               json_extract(m.data, '$.tokens.output'),
               json_extract(m.data, '$.tokens.reasoning'),
               json_extract(m.data, '$.tokens.cache.read'),
               json_extract(m.data, '$.tokens.cache.write'),
               json_extract(m.data, '$.finish'),
               {tools_expr}
        FROM {} m
        WHERE m.session_id = ?1 AND {role_filter}
        ORDER BY m.time_created DESC, m.id DESC",
        schema.messages
    );
    let mut stmt = conn
        .prepare(&sql)
        .map_err(|e| format!("query calls: {e}"))?;
    let rows = stmt
        .query_map([session_id], |row| {
            let tools_json: String = row.get(11)?;
            let tools: Vec<ToolUse> = serde_json::from_str(&tools_json).unwrap_or_default();
            Ok(Call {
                id: row.get(0)?,
                created: row.get(1)?,
                streamed: row.get(2)?,
                model: row.get(3)?,
                cost: row.get(4)?,
                tokens_input: row.get::<_, Option<i64>>(5)?.unwrap_or(0),
                tokens_output: row.get::<_, Option<i64>>(6)?.unwrap_or(0),
                tokens_reasoning: row.get::<_, Option<i64>>(7)?.unwrap_or(0),
                tokens_cache_read: row.get::<_, Option<i64>>(8)?.unwrap_or(0),
                tokens_cache_write: row.get::<_, Option<i64>>(9)?.unwrap_or(0),
                finish: row.get(10)?,
                tools,
            })
        })
        .map_err(|e| format!("read calls: {e}"))?;
    rows.collect::<Result<Vec<_>, _>>()
        .map_err(|e| format!("mapping calls: {e}"))
}

pub fn format_duration(ms: Option<i64>) -> String {
    match ms {
        None => "—".to_string(),
        Some(ms) if ms < 1_000 => format!("{ms}ms"),
        Some(ms) if ms < 60_000 => format!("{:.1}s", ms as f64 / 1_000.0),
        Some(ms) => format!("{}m{:02}s", ms / 60_000, (ms % 60_000) / 1_000),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn session() -> Session {
        Session {
            id: "ses_test".into(),
            parent_id: None,
            project: "proj".into(),
            directory: "/tmp".into(),
            title: "t".into(),
            agent: None,
            model: None,
            cost: 0.0,
            tokens_input: 0,
            tokens_output: 0,
            tokens_reasoning: 0,
            tokens_cache_read: 0,
            tokens_cache_write: 0,
            messages: 0,
            active: false,
            time_created: 1,
            time_updated: 2,
            time_archived: None,
            time_busy: None,
            time_idle: None,
            time_suspended: None,
            outcome: None,
        }
    }

    #[test]
    fn status_running_beats_idle() {
        let mut s = session();
        s.time_busy = Some(20);
        s.time_idle = Some(10);
        assert_eq!(s.status(), Status::Running);
    }

    #[test]
    fn status_suspended_beats_busy() {
        let mut s = session();
        s.time_busy = Some(20);
        s.time_idle = Some(10);
        s.time_suspended = Some(21);
        assert_eq!(s.status(), Status::Suspended);
    }

    #[test]
    fn active_overrides_database_state() {
        let mut s = session();
        s.active = true;
        s.time_suspended = Some(21);
        assert_eq!(s.status(), Status::Running);
        s.time_archived = Some(30);
        assert_eq!(s.status(), Status::Archived);
    }

    #[test]
    fn status_idle_and_archived() {
        let mut s = session();
        s.time_idle = Some(10);
        assert_eq!(s.status(), Status::Idle);
        s.time_archived = Some(11);
        assert_eq!(s.status(), Status::Archived);
    }

    #[test]
    fn dates_are_utc() {
        assert_eq!(format_date(0), "—");
        assert_eq!(format_date(1_000), "1970-01-01 00:00");
        assert_eq!(format_date(1_700_000_000_000), "2023-11-14 22:13");
    }

    fn call(cost: f64) -> Call {
        Call {
            id: "msg".into(),
            created: 1_000,
            streamed: Some(2_000),
            model: Some("model".into()),
            cost: Some(cost),
            tokens_input: 100,
            tokens_output: 20,
            tokens_reasoning: 5,
            tokens_cache_read: 300,
            tokens_cache_write: 0,
            finish: Some("tool-calls".into()),
            tools: vec![
                ToolUse {
                    name: Some("shell".into()),
                    status: Some("completed".into()),
                },
                ToolUse {
                    name: Some("shell".into()),
                    status: Some("error".into()),
                },
                ToolUse {
                    name: Some("read".into()),
                    status: Some("completed".into()),
                },
            ],
        }
    }

    #[test]
    fn call_tools_and_summary() {
        let call = call(0.5);
        assert_eq!(call.duration_ms(), Some(1_000));
        assert_eq!(call.tools_label(), "read, shell×2!");
        let summary = summarize(&[call]);
        assert_eq!(summary.calls, 1);
        assert_eq!(summary.tools, 3);
        assert_eq!(summary.errors, 1);
        assert_eq!(summary.cost, 0.5);
        assert_eq!(summary.avg_cost(), 0.5);
        assert_eq!(summary.tokens_input, 100);
    }

    #[test]
    fn duration_formats() {
        assert_eq!(format_duration(None), "—");
        assert_eq!(format_duration(Some(350)), "350ms");
        assert_eq!(format_duration(Some(1_200)), "1.2s");
        assert_eq!(format_duration(Some(90_000)), "1m30s");
    }

    #[test]
    fn relative_formats() {
        let now = 1_000_000_000;
        assert_eq!(format_relative_at(now - 45_000, now), "now");
        assert_eq!(format_relative_at(now - 59_999, now), "now");
        assert_eq!(format_relative_at(now - 60_000, now), "1m");
        assert_eq!(format_relative_at(now - 3_600_000, now), "1h");
        assert_eq!(format_relative_at(now - 86_400_000, now), "1d");
        assert_eq!(format_relative_at(0, now), "—");
    }

    #[test]
    fn compact_formats() {
        assert_eq!(format_tokens(999), "999");
        assert_eq!(format_tokens(1_500), "1.5k");
        assert_eq!(format_tokens(999_999), "1.0M");
        assert_eq!(format_cost(0.0), "—");
        assert_eq!(format_cost(0.0026), "$0.0026");
    }

    fn create_v2(conn: &Connection) {
        conn.execute_batch(
            "CREATE TABLE project (id text primary key, name text);
             CREATE TABLE session_v2 (
                 id text primary key, project_id text not null, parent_id text,
                 slug text not null, directory text not null, title text,
                 version text not null, cost real not null default 0,
                 tokens_input integer not null default 0,
                 tokens_output integer not null default 0,
                 tokens_reasoning integer not null default 0,
                 tokens_cache_read integer not null default 0,
                 tokens_cache_write integer not null default 0,
                 agent text, model text,
                 time_created integer not null, time_updated integer not null,
                 time_archived integer, time_suspended integer, time_idle integer,
                 idle_outcome text);
             CREATE TABLE session_message (
                 id text primary key, session_id text not null, type text not null,
                 seq integer not null, time_created integer not null,
                 time_updated integer not null, data text not null);",
        )
        .unwrap();
    }

    fn create_v1(conn: &Connection) {
        conn.execute_batch(
            "CREATE TABLE project (id text primary key, name text);
             CREATE TABLE session (
                 id text primary key, project_id text not null, parent_id text,
                 slug text not null, directory text not null, title text not null,
                 version text not null, cost real not null default 0,
                 tokens_input integer not null default 0,
                 tokens_output integer not null default 0,
                 tokens_reasoning integer not null default 0,
                 tokens_cache_read integer not null default 0,
                 tokens_cache_write integer not null default 0,
                 agent text, model text,
                 time_created integer not null, time_updated integer not null,
                 time_archived integer);
             CREATE TABLE message (
                 id text primary key, session_id text not null,
                 time_created integer not null, time_updated integer not null,
                 data text not null);
             CREATE TABLE part (
                 id text primary key, message_id text not null, session_id text not null,
                 time_created integer not null, time_updated integer not null,
                 data text not null);",
        )
        .unwrap();
    }

    fn insert_session(conn: &Connection, sql: &str) {
        conn.execute(sql, []).unwrap();
    }

    #[test]
    fn reads_v2_database() {
        let conn = Connection::open_in_memory().unwrap();
        create_v2(&conn);
        let now = now_ms();
        insert_session(
            &conn,
            "INSERT INTO project (id, name) VALUES ('p1', 'proj')",
        );
        insert_session(
            &conn,
            &format!(
                "INSERT INTO session_v2
                    (id, project_id, slug, directory, title, version, cost, model,
                     time_created, time_updated, time_idle)
                 VALUES ('ses_busy', 'p1', 'slug', '/tmp/busy', 'Busy', '2', 0.5,
                         '{{\"id\":\"m1\"}}', {a}, {b}, {c})",
                a = now - 3_000,
                b = now - 1_000,
                c = now - 2_000
            ),
        );
        insert_session(
            &conn,
            &format!(
                "INSERT INTO session_v2
                    (id, project_id, slug, directory, title, version, time_created,
                     time_updated, time_idle, idle_outcome)
                 VALUES ('ses_idle', 'p1', 'slug', '/tmp/idle', 'Idle', '2',
                         {a}, {b}, {c}, 'succeeded')",
                a = now - 5_000,
                b = now - 4_000,
                c = now - 1_000
            ),
        );
        insert_session(
            &conn,
            &format!(
                "INSERT INTO session_v2
                    (id, project_id, slug, directory, title, version, time_created,
                     time_updated, time_idle, time_archived)
                 VALUES ('ses_arch', 'p1', 'slug', '/tmp/arch', 'Arch', '2',
                         {a}, {b}, {c}, {d})",
                a = now - 5_000,
                b = now - 4_000,
                c = now - 1_000,
                d = now - 500
            ),
        );
        // A session killed mid-run: newer update, but far outside the busy window.
        insert_session(
            &conn,
            &format!(
                "INSERT INTO session_v2
                    (id, project_id, slug, directory, title, version, time_created,
                     time_updated, time_idle)
                 VALUES ('ses_stale', 'p1', 'slug', '/tmp/stale', 'Stale', '2',
                         {a}, {b}, {c})",
                a = now - 1_500_000,
                b = now - 1_200_000,
                c = now - 1_300_000
            ),
        );
        insert_session(
            &conn,
            r#"INSERT INTO session_message (id, session_id, type, seq, time_created, time_updated, data)
               VALUES ('msg_1', 'ses_busy', 'assistant', 1, 1000, 2000,
                       '{"time":{"streamed":4000},"model":{"id":"m1"},"cost":0.25,
                         "tokens":{"input":10,"output":5,"reasoning":2,"cache":{"read":7,"write":0}},
                         "finish":"tool-calls",
                         "content":[{"type":"tool","name":"shell","state":{"status":"error"}}]}')"#,
        );

        let sessions = query_sessions(&conn).unwrap();
        assert_eq!(sessions.len(), 4);
        let busy = sessions.iter().find(|s| s.id == "ses_busy").unwrap();
        assert_eq!(
            busy.status(),
            Status::Running,
            "updated past idle means busy"
        );
        assert_eq!(busy.messages, 1);
        assert_eq!(busy.project, "proj");
        assert_eq!(busy.model.as_deref(), Some("m1"));
        let idle = sessions.iter().find(|s| s.id == "ses_idle").unwrap();
        assert_eq!(idle.status(), Status::Idle);
        assert_eq!(idle.outcome.as_deref(), Some("succeeded"));
        assert_eq!(
            sessions
                .iter()
                .find(|s| s.id == "ses_arch")
                .unwrap()
                .status(),
            Status::Archived
        );
        assert_eq!(
            sessions
                .iter()
                .find(|s| s.id == "ses_stale")
                .unwrap()
                .status(),
            Status::Idle,
            "an old update past idle is not running any more"
        );

        let calls = query_calls(&conn, "ses_busy").unwrap();
        assert_eq!(calls.len(), 1);
        assert_eq!(calls[0].duration_ms(), Some(3_000));
        assert_eq!(calls[0].model.as_deref(), Some("m1"));
        assert_eq!(calls[0].tools_label(), "shell!");
        assert_eq!(calls[0].tokens_cache_read, 7);
    }

    #[test]
    fn reads_v1_database() {
        let conn = Connection::open_in_memory().unwrap();
        create_v1(&conn);
        insert_session(
            &conn,
            "INSERT INTO project (id, name) VALUES ('p1', 'proj')",
        );
        insert_session(
            &conn,
            "INSERT INTO session
                (id, project_id, slug, directory, title, version, cost, time_created, time_updated)
             VALUES ('ses_old', 'p1', 'slug', '/tmp/old', 'Old', '1', 1.5, 10, 20)",
        );
        insert_session(
            &conn,
            r#"INSERT INTO message (id, session_id, time_created, time_updated, data)
               VALUES ('msg_1', 'ses_old', 1000, 2000,
                       '{"role":"assistant","modelID":"m2","cost":0.5,
                         "tokens":{"input":1,"output":2,"reasoning":3,"cache":{"read":4,"write":5}},
                         "time":{"created":1000,"completed":3500},"finish":"stop"}')"#,
        );
        insert_session(
            &conn,
            r#"INSERT INTO part (id, message_id, session_id, time_created, time_updated, data)
               VALUES ('prt_1', 'msg_1', 'ses_old', 1000, 2000,
                       '{"type":"tool","tool":"bash","state":{"status":"completed"}}')"#,
        );

        let sessions = query_sessions(&conn).unwrap();
        assert_eq!(sessions.len(), 1);
        assert_eq!(sessions[0].status(), Status::New);
        assert_eq!(sessions[0].messages, 1);
        assert_eq!(sessions[0].cost, 1.5);

        let calls = query_calls(&conn, "ses_old").unwrap();
        assert_eq!(calls.len(), 1);
        assert_eq!(calls[0].model.as_deref(), Some("m2"));
        assert_eq!(calls[0].duration_ms(), Some(2_500));
        assert_eq!(calls[0].tools_label(), "bash");
        assert_eq!(calls[0].tokens_cache_write, 5);
    }

    #[test]
    fn reads_schema_without_directory_or_project() {
        let conn = Connection::open_in_memory().unwrap();
        conn.execute_batch(
            "CREATE TABLE session_v2 (
                 id text primary key, title text,
                 time_created integer not null, time_updated integer not null);
             CREATE TABLE session_message (
                 id text primary key, session_id text not null, type text not null,
                 time_created integer not null, data text not null);",
        )
        .unwrap();
        conn.execute(
            "INSERT INTO session_v2 (id, title, time_created, time_updated)
             VALUES ('ses_min', 'Minimal', 10, 20)",
            [],
        )
        .unwrap();

        let sessions = query_sessions(&conn).unwrap();
        assert_eq!(sessions.len(), 1);
        assert_eq!(sessions[0].directory, "");
        assert_eq!(sessions[0].project, "");
        assert_eq!(sessions[0].title, "Minimal");
        assert_eq!(sessions[0].messages, 0);
        // No time_idle column, so the session has never been marked idle.
        assert_eq!(sessions[0].status(), Status::New);
    }

    #[test]
    fn rejects_database_without_sessions() {
        let conn = Connection::open_in_memory().unwrap();
        let error = query_sessions(&conn).unwrap_err();
        assert!(error.contains("no session table"), "unexpected: {error}");
    }
}
