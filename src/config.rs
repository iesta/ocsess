use std::cmp::Ordering;
use std::fs;
use std::path::PathBuf;

use serde::{Deserialize, Serialize};

use crate::db::{Session, format_cost, format_date, format_relative, format_tokens};

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ColumnId {
    State,
    Title,
    Project,
    Directory,
    Agent,
    Model,
    Cost,
    Tokens,
    Messages,
    Parent,
    Outcome,
    Created,
    Updated,
    Id,
    Archived,
}

impl ColumnId {
    pub const ALL: [ColumnId; 15] = [
        ColumnId::State,
        ColumnId::Title,
        ColumnId::Project,
        ColumnId::Directory,
        ColumnId::Agent,
        ColumnId::Model,
        ColumnId::Cost,
        ColumnId::Tokens,
        ColumnId::Messages,
        ColumnId::Parent,
        ColumnId::Outcome,
        ColumnId::Created,
        ColumnId::Updated,
        ColumnId::Id,
        ColumnId::Archived,
    ];

    pub fn label(self) -> &'static str {
        match self {
            ColumnId::State => "State",
            ColumnId::Title => "Title",
            ColumnId::Project => "Project",
            ColumnId::Directory => "Directory",
            ColumnId::Agent => "Agent",
            ColumnId::Model => "Model",
            ColumnId::Cost => "Cost",
            ColumnId::Tokens => "Tokens",
            ColumnId::Messages => "Msgs",
            ColumnId::Parent => "Parent",
            ColumnId::Outcome => "Outcome",
            ColumnId::Created => "Created",
            ColumnId::Updated => "Updated",
            ColumnId::Id => "ID",
            ColumnId::Archived => "Archived",
        }
    }

    pub fn default_width(self) -> u16 {
        match self {
            ColumnId::State => 11,
            ColumnId::Title => 32,
            ColumnId::Project => 14,
            ColumnId::Directory => 28,
            ColumnId::Agent => 8,
            ColumnId::Model => 20,
            ColumnId::Cost => 8,
            ColumnId::Tokens => 9,
            ColumnId::Messages => 6,
            ColumnId::Parent => 10,
            ColumnId::Outcome => 10,
            ColumnId::Created => 16,
            ColumnId::Updated => 11,
            ColumnId::Id => 28,
            ColumnId::Archived => 16,
        }
    }

    pub fn default_visible(self) -> bool {
        !matches!(
            self,
            ColumnId::Directory
                | ColumnId::Tokens
                | ColumnId::Parent
                | ColumnId::Outcome
                | ColumnId::Created
                | ColumnId::Id
                | ColumnId::Archived
        )
    }

    pub fn right_aligned(self) -> bool {
        matches!(
            self,
            ColumnId::Cost
                | ColumnId::Tokens
                | ColumnId::Messages
                | ColumnId::Created
                | ColumnId::Updated
        )
    }

    pub fn numeric(self) -> bool {
        matches!(self, ColumnId::Cost | ColumnId::Tokens | ColumnId::Messages)
    }

    pub fn display(self, session: &Session) -> String {
        match self {
            ColumnId::State => session.status().label().to_string(),
            ColumnId::Title => {
                if session.parent_id.is_some() {
                    format!("↳ {}", session.title)
                } else {
                    session.title.clone()
                }
            }
            ColumnId::Project => session.project.clone(),
            ColumnId::Directory => session.directory.clone(),
            ColumnId::Agent => session.agent.clone().unwrap_or_else(|| "—".into()),
            ColumnId::Model => session.model.clone().unwrap_or_else(|| "—".into()),
            ColumnId::Cost => format_cost(session.cost),
            ColumnId::Tokens => format_tokens(session.total_tokens()),
            ColumnId::Messages => session.messages.to_string(),
            ColumnId::Parent => session.parent_id.clone().unwrap_or_default(),
            ColumnId::Outcome => session.outcome.clone().unwrap_or_default(),
            ColumnId::Created => format_date(session.time_created),
            ColumnId::Updated => format_relative(session.time_updated),
            ColumnId::Id => session.id.clone(),
            ColumnId::Archived => session.time_archived.map(format_date).unwrap_or_default(),
        }
    }

    pub fn numeric_value(self, session: &Session) -> Option<f64> {
        match self {
            ColumnId::Cost => Some(session.cost),
            ColumnId::Tokens => Some(session.total_tokens() as f64),
            ColumnId::Messages => Some(session.messages as f64),
            ColumnId::Created => Some(session.time_created as f64),
            ColumnId::Updated => Some(session.time_updated as f64),
            ColumnId::Archived => session.time_archived.map(|value| value as f64),
            _ => None,
        }
    }

    pub fn compare(self, left: &Session, right: &Session) -> Ordering {
        match self {
            ColumnId::State => left.status().rank().cmp(&right.status().rank()),
            ColumnId::Cost => left
                .cost
                .partial_cmp(&right.cost)
                .unwrap_or(Ordering::Equal),
            ColumnId::Tokens => left.total_tokens().cmp(&right.total_tokens()),
            ColumnId::Messages => left.messages.cmp(&right.messages),
            ColumnId::Created => left.time_created.cmp(&right.time_created),
            ColumnId::Updated => left.time_updated.cmp(&right.time_updated),
            ColumnId::Archived => left
                .time_archived
                .unwrap_or(0)
                .cmp(&right.time_archived.unwrap_or(0)),
            _ => self
                .display(left)
                .to_lowercase()
                .cmp(&self.display(right).to_lowercase()),
        }
    }
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct Column {
    pub id: ColumnId,
    pub width: u16,
    pub visible: bool,
}

impl Column {
    fn new(id: ColumnId) -> Self {
        Self {
            id,
            width: id.default_width(),
            visible: id.default_visible(),
        }
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum FilterOp {
    Contains,
    Equals,
    Gt,
    Ge,
    Lt,
    Le,
}

impl FilterOp {
    pub fn symbol(self) -> &'static str {
        match self {
            FilterOp::Contains => "~",
            FilterOp::Equals => "=",
            FilterOp::Gt => ">",
            FilterOp::Ge => ">=",
            FilterOp::Lt => "<",
            FilterOp::Le => "<=",
        }
    }

    fn numeric(self) -> bool {
        !matches!(self, FilterOp::Contains | FilterOp::Equals)
    }
}

#[derive(Clone, Debug)]
pub struct Filter {
    pub column: ColumnId,
    pub op: FilterOp,
    pub value: String,
}

impl Filter {
    pub fn parse(column: ColumnId, raw: &str) -> Option<Self> {
        let raw = raw.trim();
        if raw.is_empty() {
            return None;
        }
        let (op, value) = if let Some(rest) = raw.strip_prefix(">=") {
            (FilterOp::Ge, rest)
        } else if let Some(rest) = raw.strip_prefix("<=") {
            (FilterOp::Le, rest)
        } else if let Some(rest) = raw.strip_prefix('>') {
            (FilterOp::Gt, rest)
        } else if let Some(rest) = raw.strip_prefix('<') {
            (FilterOp::Lt, rest)
        } else if let Some(rest) = raw.strip_prefix('=') {
            (FilterOp::Equals, rest)
        } else {
            (FilterOp::Contains, raw)
        };
        let value = value.trim().to_string();
        if value.is_empty() {
            return None;
        }
        Some(Self { column, op, value })
    }

    pub fn matches(&self, session: &Session) -> bool {
        if self.op.numeric() {
            let Some(cell) = self.column.numeric_value(session) else {
                return false;
            };
            let Ok(target) = self.value.parse::<f64>() else {
                return false;
            };
            match self.op {
                FilterOp::Gt => cell > target,
                FilterOp::Ge => cell >= target,
                FilterOp::Lt => cell < target,
                FilterOp::Le => cell <= target,
                _ => false,
            }
        } else {
            let cell = self.column.display(session);
            match self.op {
                FilterOp::Contains => cell.to_lowercase().contains(&self.value.to_lowercase()),
                FilterOp::Equals => cell.eq_ignore_ascii_case(&self.value),
                _ => false,
            }
        }
    }
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct Config {
    #[serde(default = "default_columns")]
    pub columns: Vec<Column>,
    #[serde(default = "default_sort")]
    pub sort: ColumnId,
    #[serde(default)]
    pub sort_desc: bool,
    #[serde(default)]
    pub show_archived: bool,
}

impl Default for Config {
    fn default() -> Self {
        Self {
            columns: default_columns(),
            sort: default_sort(),
            sort_desc: true,
            show_archived: false,
        }
    }
}

fn default_columns() -> Vec<Column> {
    ColumnId::ALL.iter().copied().map(Column::new).collect()
}

fn default_sort() -> ColumnId {
    ColumnId::Updated
}

impl Config {
    pub fn load() -> Self {
        let mut config: Self = fs::read_to_string(config_path())
            .ok()
            .and_then(|text| serde_json::from_str(&text).ok())
            .unwrap_or_default();
        config.normalize();
        config
    }

    pub fn save(&self) -> Result<(), String> {
        let path = config_path();
        if let Some(parent) = path.parent() {
            fs::create_dir_all(parent).map_err(|e| format!("create {}: {e}", parent.display()))?;
        }
        let text = serde_json::to_string_pretty(self).map_err(|e| e.to_string())?;
        let tmp = path.with_extension("json.tmp");
        fs::write(&tmp, text).map_err(|e| format!("write {}: {e}", tmp.display()))?;
        fs::rename(&tmp, &path).map_err(|e| format!("rename {}: {e}", path.display()))
    }

    fn normalize(&mut self) {
        let mut columns: Vec<Column> = Vec::new();
        for column in self.columns.drain(..) {
            if columns.iter().any(|existing| existing.id == column.id) {
                continue;
            }
            let mut column = column;
            column.width = column.width.clamp(4, 80);
            columns.push(column);
        }
        for id in ColumnId::ALL {
            if !columns.iter().any(|column| column.id == id) {
                columns.push(Column::new(id));
            }
        }
        if !columns.iter().any(|column| column.visible)
            && let Some(title) = columns
                .iter_mut()
                .find(|column| column.id == ColumnId::Title)
        {
            title.visible = true;
        }
        self.columns = columns;
    }
}

pub fn config_path() -> PathBuf {
    let base = std::env::var_os("XDG_CONFIG_HOME")
        .map(PathBuf::from)
        .unwrap_or_else(|| home().join(".config"));
    base.join("ocsess/config.json")
}

fn home() -> PathBuf {
    std::env::var_os("HOME")
        .map(PathBuf::from)
        .unwrap_or_else(|| PathBuf::from("."))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::db::Status;

    fn session(title: &str, cost: f64) -> Session {
        Session {
            id: "ses_test".into(),
            parent_id: None,
            project: "proj".into(),
            directory: "/tmp/proj".into(),
            title: title.into(),
            agent: Some("build".into()),
            model: Some("m".into()),
            cost,
            tokens_input: 10,
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
    fn filter_operators() {
        let s = session("Fix auth bug", 0.5);
        assert!(Filter::parse(ColumnId::Title, "auth").unwrap().matches(&s));
        assert!(
            Filter::parse(ColumnId::Title, "=fix auth bug")
                .unwrap()
                .matches(&s)
        );
        assert!(Filter::parse(ColumnId::Cost, ">0.1").unwrap().matches(&s));
        assert!(!Filter::parse(ColumnId::Cost, "<0.1").unwrap().matches(&s));
        assert!(!Filter::parse(ColumnId::State, ">1").unwrap().matches(&s));
        assert!(Filter::parse(ColumnId::State, "idle").unwrap().matches(&s));
    }

    #[test]
    fn status_display_and_sort() {
        let a = session("a", 2.0);
        let b = session("b", 1.0);
        assert_eq!(ColumnId::Cost.compare(&a, &b), Ordering::Greater);
        assert_eq!(ColumnId::State.display(&a), "idle");
    }

    #[test]
    fn config_normalizes_missing_and_duplicate_columns() {
        let mut config = Config {
            columns: vec![
                Column {
                    id: ColumnId::Title,
                    width: 999,
                    visible: false,
                },
                Column {
                    id: ColumnId::Title,
                    width: 10,
                    visible: false,
                },
            ],
            sort: ColumnId::Updated,
            sort_desc: true,
            show_archived: false,
        };
        config.normalize();
        assert_eq!(config.columns.len(), ColumnId::ALL.len());
        assert_eq!(
            config
                .columns
                .iter()
                .find(|column| column.id == ColumnId::Title)
                .unwrap()
                .width,
            80
        );
        assert!(
            !config
                .columns
                .iter()
                .find(|column| column.id == ColumnId::Title)
                .unwrap()
                .visible
        );
    }

    #[test]
    fn config_forces_one_visible_column() {
        let mut config = Config {
            columns: ColumnId::ALL
                .iter()
                .copied()
                .map(|id| Column {
                    id,
                    width: id.default_width(),
                    visible: false,
                })
                .collect(),
            sort: ColumnId::Updated,
            sort_desc: true,
            show_archived: false,
        };
        config.normalize();
        assert!(
            config
                .columns
                .iter()
                .find(|column| column.id == ColumnId::Title)
                .unwrap()
                .visible
        );
    }

    #[test]
    fn archived_status_is_displayed() {
        let mut s = session("x", 0.0);
        s.time_archived = Some(10);
        assert_eq!(ColumnId::State.display(&s), Status::Archived.label());
    }
}
