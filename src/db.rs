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

pub fn format_relative(ms: i64) -> String {
    if ms <= 0 {
        return "—".to_string();
    }
    let diff = (now_ms() - ms).max(0);
    const MINUTE: i64 = 60_000;
    const HOUR: i64 = 60 * MINUTE;
    const DAY: i64 = 24 * HOUR;
    if diff < 45_000 {
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
        format!("{:.1}k", tokens as f64 / 1_000.0)
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

pub fn load_sessions(path: &Path) -> Result<Vec<Session>, String> {
    let conn = open_db(path)?;

    let sql = "
        SELECT s.id, s.parent_id, COALESCE(p.name, ''),
               s.directory, s.title, s.agent, s.model, s.cost,
               s.tokens_input, s.tokens_output, s.tokens_reasoning,
               s.tokens_cache_read, s.tokens_cache_write,
               s.time_created, s.time_updated, s.time_archived,
               s.time_busy, s.time_idle, s.time_suspended, s.idle_outcome,
               (SELECT COUNT(*) FROM session_message m WHERE m.session_id = s.id)
        FROM session_v2 s
        LEFT JOIN project p ON p.id = s.project_id
    ";
    let mut stmt = conn
        .prepare(sql)
        .map_err(|e| format!("query sessions: {e}"))?;
    let rows = stmt
        .query_map([], |row| {
            let directory: String = row.get(3)?;
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
                cost: row.get(7)?,
                tokens_input: row.get(8)?,
                tokens_output: row.get(9)?,
                tokens_reasoning: row.get(10)?,
                tokens_cache_read: row.get(11)?,
                tokens_cache_write: row.get(12)?,
                time_created: row.get(13)?,
                time_updated: row.get(14)?,
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
    let sql = "
        SELECT m.id, m.time_created,
               json_extract(m.data, '$.time.streamed'),
               json_extract(m.data, '$.model.id'),
               json_extract(m.data, '$.cost'),
               json_extract(m.data, '$.tokens.input'),
               json_extract(m.data, '$.tokens.output'),
               json_extract(m.data, '$.tokens.reasoning'),
               json_extract(m.data, '$.tokens.cache.read'),
               json_extract(m.data, '$.tokens.cache.write'),
               json_extract(m.data, '$.finish'),
               COALESCE((
                   SELECT json_group_array(json_object(
                       'name', json_extract(p.value, '$.name'),
                       'status', json_extract(p.value, '$.state.status')
                   ))
                   FROM json_each(m.data, '$.content') p
                   WHERE json_extract(p.value, '$.type') = 'tool'
               ), '[]')
        FROM session_message m
        WHERE m.session_id = ?1 AND m.type = 'assistant'
        ORDER BY m.time_created DESC, m.id DESC
    ";
    let mut stmt = conn.prepare(sql).map_err(|e| format!("query calls: {e}"))?;
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
    fn compact_formats() {
        assert_eq!(format_tokens(999), "999");
        assert_eq!(format_tokens(1_500), "1.5k");
        assert_eq!(format_cost(0.0), "—");
        assert_eq!(format_cost(0.0026), "$0.0026");
    }
}
