use std::collections::HashSet;
use std::path::PathBuf;
use std::sync::Arc;
use std::sync::mpsc::{Receiver, Sender};
use std::time::{Duration, Instant};

use ratatui::crossterm::event::{
    KeyCode, KeyEvent, KeyEventKind, KeyModifiers, MouseButton, MouseEvent, MouseEventKind,
};
use ratatui::layout::Rect;
use ratatui::widgets::TableState;

use crate::api::{Api, Command, WorkerEvent};
use crate::config::{Column, ColumnId, Config, Filter, FilterOp};
use crate::db::{self, Call, Session, Status};

#[derive(Clone, Debug)]
pub struct Toast {
    pub text: String,
    pub error: bool,
    pub at: Instant,
}

#[derive(Clone, Debug, Default)]
pub struct Input {
    pub chars: Vec<char>,
    pub cursor: usize,
}

impl Input {
    pub fn from_text(text: &str) -> Self {
        let chars: Vec<char> = text.chars().collect();
        let cursor = chars.len();
        Self { chars, cursor }
    }

    pub fn text(&self) -> String {
        self.chars.iter().collect()
    }

    pub fn clear(&mut self) {
        self.chars.clear();
        self.cursor = 0;
    }

    fn insert(&mut self, character: char) {
        self.chars.insert(self.cursor, character);
        self.cursor += 1;
    }

    fn backspace(&mut self) {
        if self.cursor > 0 {
            self.cursor -= 1;
            self.chars.remove(self.cursor);
        }
    }

    fn delete(&mut self) {
        if self.cursor < self.chars.len() {
            self.chars.remove(self.cursor);
        }
    }

    fn left(&mut self) {
        self.cursor = self.cursor.saturating_sub(1);
    }

    fn right(&mut self) {
        if self.cursor < self.chars.len() {
            self.cursor += 1;
        }
    }
}

#[derive(Clone, Debug)]
pub struct Detail {
    pub session_id: String,
    pub title: String,
    pub calls: Vec<Call>,
    pub table: TableState,
    pub last_load: Instant,
    pub rows_area: Rect,
    pub table_offset: usize,
}

#[derive(Clone, Debug)]
pub enum Modal {
    None,
    Help,
    Confirm {
        prompt: String,
        command: Command,
    },
    Rename {
        input: Input,
        id: String,
    },
    Filter {
        stage: FilterStage,
        col_idx: usize,
        input: Input,
    },
    Columns {
        selected: usize,
    },
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum FilterStage {
    Column,
    Value,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum KeyAction {
    None,
    Open(usize),
    Quit,
}

pub struct App {
    pub sessions: Vec<Session>,
    pub rows: Vec<usize>,
    pub table: TableState,
    pub columns: Vec<Column>,
    pub sort: ColumnId,
    pub sort_desc: bool,
    pub filters: Vec<Filter>,
    pub search: String,
    pub search_input: Input,
    pub search_editing: bool,
    pub show_archived: bool,
    /// Whether this OpenCode version can archive sessions at all (v1 can, v2 cannot).
    pub archive_supported: bool,
    pub active_col: usize,
    pub modal: Modal,
    pub toast: Option<Toast>,
    pub quit: bool,
    pub db_path: PathBuf,
    pub header_hits: Vec<(u16, u16, usize)>,
    pub rows_area: Rect,
    pub table_offset: usize,
    pub detail: Option<Detail>,
    pub api_online: bool,
    current_session_id: Option<String>,
    active_ids: HashSet<String>,
    last_active_check: Instant,
    api: Arc<Api>,
    command_tx: Sender<Command>,
    event_rx: Receiver<WorkerEvent>,
    last_reload: Instant,
    last_error: Option<String>,
    select_after: Option<String>,
    last_click: Option<(Instant, u16, u16)>,
}

impl App {
    pub fn new(
        db_path: PathBuf,
        config: Config,
        api: Arc<Api>,
        command_tx: Sender<Command>,
        event_rx: Receiver<WorkerEvent>,
    ) -> Self {
        let active_col = visible_columns(&config.columns)
            .iter()
            .position(|column| column.id == config.sort)
            .unwrap_or(0);
        let archive_supported = api.supports_archive();
        let mut app = Self {
            sessions: Vec::new(),
            rows: Vec::new(),
            table: TableState::default(),
            columns: config.columns,
            sort: config.sort,
            sort_desc: config.sort_desc,
            filters: Vec::new(),
            search: String::new(),
            search_input: Input::default(),
            search_editing: false,
            show_archived: config.show_archived,
            archive_supported,
            active_col,
            modal: Modal::None,
            toast: None,
            quit: false,
            db_path,
            header_hits: Vec::new(),
            rows_area: Rect::default(),
            table_offset: 0,
            detail: None,
            api_online: false,
            current_session_id: std::env::var("OPENCODE_SESSION_ID").ok(),
            active_ids: HashSet::new(),
            last_active_check: Instant::now()
                .checked_sub(Duration::from_secs(10))
                .unwrap_or_else(Instant::now),
            api,
            command_tx,
            event_rx,
            last_reload: Instant::now(),
            last_error: None,
            select_after: None,
            last_click: None,
        };
        app.reload();
        app
    }

    pub fn reload(&mut self) {
        self.maybe_check_active();
        let previous = self.selected_id();
        match db::load_sessions(&self.db_path) {
            Ok(sessions) => {
                self.sessions = sessions;
                self.refresh(previous);
                self.last_error = None;
            }
            Err(error) => match self.api.list_sessions() {
                Ok(sessions) => {
                    if self.last_error.as_deref() != Some(&error) {
                        self.notify(format!("Direct DB read failed, using API · {error}"), true);
                    }
                    self.last_error = Some(error);
                    self.sessions = sessions;
                    self.refresh(previous);
                }
                Err(api_error) => {
                    let message = format!("Load failed · {error} · {api_error}");
                    if self.last_error.as_deref() != Some(&message) {
                        self.notify(message.clone(), true);
                    }
                    self.last_error = Some(message);
                }
            },
        }
        self.apply_active();
        self.last_reload = Instant::now();
        if let Some(id) = self.select_after.take()
            && let Some(position) = self
                .rows
                .iter()
                .position(|&index| self.sessions[index].id == id)
        {
            self.table.select(Some(position));
        }
    }

    fn maybe_check_active(&mut self) {
        if self.last_active_check.elapsed() < Duration::from_secs(2) {
            return;
        }
        self.last_active_check = Instant::now();
        match self.api.active_sessions() {
            Ok(ids) => {
                self.active_ids = ids;
                self.api_online = true;
            }
            Err(_) => {
                self.api_online = false;
            }
        }
    }

    fn apply_active(&mut self) {
        let online = self.api_online;
        for session in &mut self.sessions {
            session.active = self.active_ids.contains(&session.id);
            if online && !session.active {
                // The service answered, so its active list wins over the
                // heuristic busy mark derived from the idle timestamp.
                session.time_busy = None;
            }
        }
    }

    pub fn tick(&mut self) {
        while let Ok(event) = self.event_rx.try_recv() {
            if event.select.is_some() {
                self.select_after = event.select.clone();
            }
            if event.reload {
                self.reload();
            }
            self.notify(event.message, event.error);
        }
        if self.last_reload.elapsed() >= Duration::from_millis(700) {
            self.reload();
        }
        if self
            .detail
            .as_ref()
            .is_some_and(|detail| detail.last_load.elapsed() >= Duration::from_secs(2))
        {
            self.refresh_detail();
        }
        if self
            .toast
            .as_ref()
            .is_some_and(|toast| toast.at.elapsed() > Duration::from_secs(6))
        {
            self.toast = None;
        }
    }

    pub fn notify(&mut self, text: impl Into<String>, error: bool) {
        self.toast = Some(Toast {
            text: text.into(),
            error,
            at: Instant::now(),
        });
    }

    pub fn persist_now(&self) {
        let _ = self.as_config().save();
    }

    pub fn on_key(&mut self, key: KeyEvent) -> KeyAction {
        if key.kind != KeyEventKind::Press {
            return KeyAction::None;
        }
        if is_ctrl(&key, 'c') {
            return KeyAction::Quit;
        }
        if self.detail.is_some() {
            return self.on_detail_key(key);
        }
        if !matches!(self.modal, Modal::None) {
            return self.on_modal_key(key);
        }
        if self.search_editing {
            if is_ctrl(&key, 'u') {
                self.search_input.clear();
                self.search.clear();
                self.refresh(self.selected_id());
                return KeyAction::None;
            }
            if edit_input(&mut self.search_input, &key) {
                self.search = self.search_input.text();
                self.refresh(self.selected_id());
                return KeyAction::None;
            }
            match key.code {
                KeyCode::Enter => self.search_editing = false,
                KeyCode::Esc => {
                    self.search_editing = false;
                    self.search.clear();
                    self.search_input.clear();
                    self.refresh(self.selected_id());
                }
                _ => {}
            }
            return KeyAction::None;
        }

        if is_ctrl(&key, 'd') {
            self.move_selection(10);
            return KeyAction::None;
        }
        if is_ctrl(&key, 'u') {
            self.move_selection(-10);
            return KeyAction::None;
        }

        match key.code {
            KeyCode::Char('q') => return KeyAction::Quit,
            KeyCode::Char('?') => self.modal = Modal::Help,
            KeyCode::Up | KeyCode::Char('k') => self.move_selection(-1),
            KeyCode::Down | KeyCode::Char('j') => self.move_selection(1),
            KeyCode::Home | KeyCode::Char('g') => self.select_first(),
            KeyCode::End | KeyCode::Char('G') => self.select_last(),
            KeyCode::PageUp => self.move_selection(-10),
            KeyCode::PageDown => self.move_selection(10),
            KeyCode::Left | KeyCode::Char('h') => {
                self.active_col = self.active_col.saturating_sub(1);
            }
            KeyCode::Right | KeyCode::Char('l') => {
                let count = visible_columns(&self.columns).len();
                if count > 0 {
                    self.active_col = (self.active_col + 1).min(count - 1);
                }
            }
            KeyCode::Char('s') => self.sort_active(),
            KeyCode::Char('c') => self.modal = Modal::Columns { selected: 0 },
            KeyCode::Char('i') => self.open_detail(),
            KeyCode::Char('A') => {
                self.show_archived = !self.show_archived;
                self.refresh(self.selected_id());
                self.persist();
            }
            KeyCode::Char('R') => self.reload(),
            KeyCode::Char('/') => {
                self.search_editing = true;
                self.search_input = Input::from_text(&self.search);
            }
            KeyCode::Char('f') => {
                self.modal = Modal::Filter {
                    stage: FilterStage::Column,
                    col_idx: 0,
                    input: Input::default(),
                };
            }
            KeyCode::Char('F') => self.clear_filters(),
            KeyCode::Esc => {
                self.search.clear();
                self.search_input.clear();
                self.filters.clear();
                self.refresh(self.selected_id());
            }
            KeyCode::Enter => {
                if let Some(index) = self.selected_index() {
                    return KeyAction::Open(index);
                }
            }
            KeyCode::Char('d') => {
                if let Some(id) = self.selected_id() {
                    self.send(Command::Fork(id));
                }
            }
            KeyCode::Char('r') => {
                if let Some((id, title)) = self
                    .selected_session()
                    .map(|session| (session.id.clone(), session.title.clone()))
                {
                    self.modal = Modal::Rename {
                        input: Input::from_text(&title),
                        id,
                    };
                }
            }
            KeyCode::Char('a') => self.archive_selected(),
            KeyCode::Char('u') => self.unarchive_selected(),
            KeyCode::Char('D') => self.delete_selected(),
            KeyCode::Char('m') => self.compact_selected(),
            _ => {}
        }
        KeyAction::None
    }

    fn on_detail_key(&mut self, key: KeyEvent) -> KeyAction {
        if is_ctrl(&key, 'd') {
            self.move_detail(10);
            return KeyAction::None;
        }
        if is_ctrl(&key, 'u') {
            self.move_detail(-10);
            return KeyAction::None;
        }
        match key.code {
            KeyCode::Esc | KeyCode::Char('i') | KeyCode::Char('q') => self.detail = None,
            KeyCode::Up | KeyCode::Char('k') => self.move_detail(-1),
            KeyCode::Down | KeyCode::Char('j') => self.move_detail(1),
            KeyCode::PageUp => self.move_detail(-10),
            KeyCode::PageDown => self.move_detail(10),
            KeyCode::Home | KeyCode::Char('g') => self.detail_select(0),
            KeyCode::End | KeyCode::Char('G') => self.detail_select(usize::MAX),
            KeyCode::Char('R') => self.refresh_detail(),
            _ => {}
        }
        KeyAction::None
    }

    fn on_modal_key(&mut self, key: KeyEvent) -> KeyAction {
        let modal = self.modal.clone();
        match modal {
            Modal::Help => {
                if matches!(
                    key.code,
                    KeyCode::Esc | KeyCode::Enter | KeyCode::Char('q') | KeyCode::Char('?')
                ) {
                    self.modal = Modal::None;
                }
            }
            Modal::Confirm { prompt: _, command } => match key.code {
                KeyCode::Char('y') | KeyCode::Char('Y') | KeyCode::Enter => {
                    self.modal = Modal::None;
                    self.send(command);
                }
                KeyCode::Char('n') | KeyCode::Char('N') | KeyCode::Esc => {
                    self.modal = Modal::None;
                }
                _ => {}
            },
            Modal::Rename { mut input, id } => match key.code {
                KeyCode::Esc => self.modal = Modal::None,
                KeyCode::Enter => {
                    let title = input.text().trim().to_string();
                    if title.is_empty() {
                        self.notify("Title is empty", true);
                    } else {
                        self.modal = Modal::None;
                        self.send(Command::Rename { id, title });
                    }
                }
                _ => {
                    if edit_input(&mut input, &key) {
                        self.modal = Modal::Rename { input, id };
                    }
                }
            },
            Modal::Filter {
                stage,
                col_idx,
                mut input,
            } => match stage {
                FilterStage::Column => match key.code {
                    KeyCode::Esc => self.modal = Modal::None,
                    KeyCode::Up | KeyCode::Char('k') => {
                        self.modal = Modal::Filter {
                            stage,
                            col_idx: col_idx.saturating_sub(1),
                            input,
                        };
                    }
                    KeyCode::Down | KeyCode::Char('j') => {
                        self.modal = Modal::Filter {
                            stage,
                            col_idx: (col_idx + 1).min(ColumnId::ALL.len() - 1),
                            input,
                        };
                    }
                    KeyCode::Enter => {
                        self.modal = Modal::Filter {
                            stage: FilterStage::Value,
                            col_idx,
                            input,
                        };
                    }
                    _ => {}
                },
                FilterStage::Value => match key.code {
                    KeyCode::Esc => {
                        self.modal = Modal::Filter {
                            stage: FilterStage::Column,
                            col_idx,
                            input,
                        };
                    }
                    KeyCode::Enter => {
                        let column = ColumnId::ALL[col_idx];
                        match Filter::parse(column, &input.text()) {
                            Some(filter) => {
                                if matches!(
                                    filter.op,
                                    FilterOp::Gt | FilterOp::Ge | FilterOp::Lt | FilterOp::Le
                                ) && !column.numeric()
                                {
                                    self.notify("Numeric operator on a text column", true);
                                } else {
                                    self.filters.push(filter);
                                    self.modal = Modal::None;
                                    self.refresh(self.selected_id());
                                }
                            }
                            None => self.notify("Empty filter", true),
                        }
                    }
                    _ => {
                        if edit_input(&mut input, &key) {
                            self.modal = Modal::Filter {
                                stage,
                                col_idx,
                                input,
                            };
                        }
                    }
                },
            },
            Modal::Columns { mut selected } => {
                let mut close = false;
                match key.code {
                    KeyCode::Esc | KeyCode::Enter => close = true,
                    KeyCode::Up | KeyCode::Char('k') => {
                        selected = selected.saturating_sub(1);
                    }
                    KeyCode::Down | KeyCode::Char('j') => {
                        selected = (selected + 1).min(self.columns.len() - 1);
                    }
                    KeyCode::Char(' ') => {
                        if let Some(column) = self.columns.get_mut(selected) {
                            column.visible = !column.visible;
                        }
                        self.clamp_active();
                        self.refresh(self.selected_id());
                        self.persist();
                    }
                    KeyCode::Char('J') => {
                        if selected + 1 < self.columns.len() {
                            self.columns.swap(selected, selected + 1);
                            selected += 1;
                            self.persist();
                        }
                    }
                    KeyCode::Char('K') => {
                        if selected > 0 {
                            self.columns.swap(selected, selected - 1);
                            selected -= 1;
                            self.persist();
                        }
                    }
                    KeyCode::Left => {
                        if let Some(column) = self.columns.get_mut(selected) {
                            column.width = column.width.saturating_sub(2).max(4);
                        }
                        self.persist();
                    }
                    KeyCode::Right => {
                        if let Some(column) = self.columns.get_mut(selected) {
                            column.width = (column.width + 2).min(80);
                        }
                        self.persist();
                    }
                    _ => {}
                }
                if close {
                    self.modal = Modal::None;
                } else {
                    self.modal = Modal::Columns { selected };
                }
            }
            Modal::None => {}
        }
        KeyAction::None
    }

    pub fn on_mouse(&mut self, mouse: MouseEvent) -> Option<usize> {
        if self.detail.is_some() {
            match mouse.kind {
                MouseEventKind::ScrollDown => self.move_detail(3),
                MouseEventKind::ScrollUp => self.move_detail(-3),
                MouseEventKind::Down(MouseButton::Left) => {
                    if let Some(detail) = &mut self.detail {
                        let area = detail.rows_area;
                        if area.width > 0
                            && mouse.row > area.y
                            && mouse.row < area.bottom()
                            && mouse.column >= area.x
                            && mouse.column < area.right()
                        {
                            let row = detail.table_offset + (mouse.row - area.y - 1) as usize;
                            if row < detail.calls.len() {
                                detail.table.select(Some(row));
                            }
                        }
                    }
                }
                _ => {}
            }
            return None;
        }
        if !matches!(self.modal, Modal::None) {
            return None;
        }
        match mouse.kind {
            MouseEventKind::ScrollDown => self.move_selection(3),
            MouseEventKind::ScrollUp => self.move_selection(-3),
            MouseEventKind::Down(MouseButton::Left) => {
                let area = self.rows_area;
                if area.width == 0 || area.height == 0 {
                    return None;
                }
                if mouse.row == area.y {
                    for &(start, end, index) in &self.header_hits {
                        if mouse.column >= start && mouse.column < end {
                            self.active_col = index;
                            self.sort_active();
                            break;
                        }
                    }
                    return None;
                }
                if mouse.row > area.y
                    && mouse.column >= area.x
                    && mouse.column < area.right()
                    && mouse.row < area.bottom()
                {
                    let relative = (mouse.row - area.y - 1) as usize;
                    let row = self.table_offset + relative;
                    if row < self.rows.len() {
                        self.table.select(Some(row));
                        let same_cell = self.last_click.is_some_and(|(at, column, line)| {
                            at.elapsed() < Duration::from_millis(450)
                                && column == mouse.column
                                && line == mouse.row
                        });
                        self.last_click = Some((Instant::now(), mouse.column, mouse.row));
                        if same_cell {
                            return self.rows.get(row).copied();
                        }
                    }
                }
            }
            _ => {}
        }
        None
    }

    fn open_detail(&mut self) {
        let Some((id, title)) = self
            .selected_session()
            .map(|session| (session.id.clone(), session.title.clone()))
        else {
            return;
        };
        match db::load_calls(&self.db_path, &id) {
            Ok(calls) => {
                let mut table = TableState::default();
                if !calls.is_empty() {
                    table.select(Some(0));
                }
                self.detail = Some(Detail {
                    session_id: id,
                    title,
                    calls,
                    table,
                    last_load: Instant::now(),
                    rows_area: Rect::default(),
                    table_offset: 0,
                });
            }
            Err(error) => self.notify(format!("Detail load failed · {error}"), true),
        }
    }

    fn refresh_detail(&mut self) {
        let path = self.db_path.clone();
        let Some(detail) = self.detail.as_mut() else {
            return;
        };
        let session_id = detail.session_id.clone();
        let selected = detail.table.selected();
        let selected_id = selected
            .and_then(|index| detail.calls.get(index))
            .map(|call| call.id.clone());
        let follow_newest = selected == Some(0);
        match db::load_calls(&path, &session_id) {
            Ok(calls) => {
                detail.calls = calls;
                detail.last_load = Instant::now();
                if detail.calls.is_empty() {
                    detail.table.select(None);
                } else {
                    let position = if follow_newest {
                        0
                    } else {
                        selected_id
                            .and_then(|id| detail.calls.iter().position(|call| call.id == id))
                            .or_else(|| selected.map(|index| index.min(detail.calls.len() - 1)))
                            .unwrap_or(0)
                    };
                    detail.table.select(Some(position));
                }
            }
            Err(_) => detail.last_load = Instant::now(),
        }
    }

    fn move_detail(&mut self, delta: isize) {
        let Some(detail) = self.detail.as_mut() else {
            return;
        };
        if detail.calls.is_empty() {
            return;
        }
        let current = detail.table.selected().unwrap_or(0) as isize;
        let next = (current + delta).clamp(0, detail.calls.len() as isize - 1);
        detail.table.select(Some(next as usize));
    }

    fn detail_select(&mut self, index: usize) {
        let Some(detail) = self.detail.as_mut() else {
            return;
        };
        if detail.calls.is_empty() {
            return;
        }
        detail.table.select(Some(index.min(detail.calls.len() - 1)));
    }

    pub fn is_current(&self, id: &str) -> bool {
        self.current_session_id.as_deref() == Some(id)
    }

    pub fn selected_session(&self) -> Option<&Session> {
        self.selected_index()
            .and_then(|index| self.sessions.get(index))
    }

    pub fn selected_index(&self) -> Option<usize> {
        self.table
            .selected()
            .and_then(|position| self.rows.get(position).copied())
    }

    fn selected_id(&self) -> Option<String> {
        self.selected_session().map(|session| session.id.clone())
    }

    fn refresh(&mut self, previous: Option<String>) {
        let query = self.search.trim().to_lowercase();
        let mut rows: Vec<usize> = self
            .sessions
            .iter()
            .enumerate()
            .filter_map(|(index, session)| {
                if !self.show_archived && session.time_archived.is_some() {
                    return None;
                }
                if !self.filters.iter().all(|filter| filter.matches(session)) {
                    return None;
                }
                if !query.is_empty() && !matches_query(session, &query) {
                    return None;
                }
                Some(index)
            })
            .collect();

        rows.sort_by(|&left, &right| {
            let mut order = self
                .sort
                .compare(&self.sessions[left], &self.sessions[right]);
            if self.sort_desc {
                order = order.reverse();
            }
            order
                .then_with(|| {
                    self.sessions[right]
                        .time_updated
                        .cmp(&self.sessions[left].time_updated)
                })
                .then_with(|| self.sessions[left].id.cmp(&self.sessions[right].id))
        });

        let old_selection = self.table.selected();
        self.rows = rows;
        if self.rows.is_empty() {
            self.table.select(None);
        } else {
            let selected = previous
                .and_then(|id| {
                    self.rows
                        .iter()
                        .position(|&index| self.sessions[index].id == id)
                })
                .or_else(|| old_selection.map(|position| position.min(self.rows.len() - 1)))
                .unwrap_or(0);
            self.table.select(Some(selected));
        }
    }

    fn move_selection(&mut self, delta: isize) {
        if self.rows.is_empty() {
            return;
        }
        let current = self.table.selected().unwrap_or(0) as isize;
        let next = (current + delta).clamp(0, self.rows.len() as isize - 1);
        self.table.select(Some(next as usize));
    }

    fn select_first(&mut self) {
        if !self.rows.is_empty() {
            self.table.select(Some(0));
        }
    }

    fn select_last(&mut self) {
        if !self.rows.is_empty() {
            self.table.select(Some(self.rows.len() - 1));
        }
    }

    fn sort_active(&mut self) {
        let Some(column) = visible_columns(&self.columns)
            .get(self.active_col)
            .map(|column| column.id)
        else {
            return;
        };
        if self.sort == column {
            self.sort_desc = !self.sort_desc;
        } else {
            self.sort = column;
            self.sort_desc = matches!(
                column,
                ColumnId::Updated
                    | ColumnId::Created
                    | ColumnId::Archived
                    | ColumnId::Cost
                    | ColumnId::Tokens
                    | ColumnId::Messages
            );
        }
        self.refresh(self.selected_id());
        self.persist();
    }

    fn clear_filters(&mut self) {
        self.filters.clear();
        self.search.clear();
        self.search_input.clear();
        self.search_editing = false;
        self.refresh(self.selected_id());
    }

    fn archive_selected(&mut self) {
        if !self.archive_supported {
            self.notify(
                "Archiving is not available in this OpenCode version · use D to delete",
                true,
            );
            return;
        }
        let Some((id, title, status)) = self.selected_details() else {
            return;
        };
        if self.is_current(&id) {
            self.notify("Current session · action refused", true);
        } else if status == Status::Running {
            self.notify("Session is running · action refused", true);
        } else if status == Status::Archived {
            self.notify("Session already archived", true);
        } else {
            self.send(Command::Archive(id));
            self.notify(format!("Archiving \"{title}\"…"), false);
        }
    }

    fn unarchive_selected(&mut self) {
        if !self.archive_supported {
            self.notify(
                "Unarchiving is not available in this OpenCode version · use D to delete",
                true,
            );
            return;
        }
        let Some((id, _, status)) = self.selected_details() else {
            return;
        };
        if status != Status::Archived {
            self.notify("Session is not archived", true);
        } else {
            self.send(Command::Unarchive(id));
        }
    }

    fn delete_selected(&mut self) {
        let Some((id, title, status)) = self.selected_details() else {
            return;
        };
        if self.is_current(&id) {
            self.notify("Current session · delete refused", true);
            return;
        }
        if status == Status::Running {
            self.notify("Session is running · delete refused", true);
            return;
        }
        let backups = self.descendant_ids(&id);
        let children = backups.len().saturating_sub(1);
        let prompt = if children == 0 {
            format!("Delete \"{title}\"?\nGzip backup, then permanent deletion.")
        } else {
            format!(
                "Delete \"{title}\" and {children} child session(s)?\nGzip backups, then permanent deletion."
            )
        };
        self.modal = Modal::Confirm {
            prompt,
            command: Command::Delete { id, backups },
        };
    }

    fn compact_selected(&mut self) {
        let Some((id, title, status)) = self.selected_details() else {
            return;
        };
        if self.is_current(&id) {
            self.notify("Current session · compaction refused", true);
            return;
        }
        if status == Status::Running {
            self.notify("Session is running · compaction refused", true);
            return;
        }
        self.modal = Modal::Confirm {
            prompt: format!("Compact \"{title}\"?\nModel call, token cost."),
            command: Command::Compact(id),
        };
    }

    fn descendant_ids(&self, root: &str) -> Vec<String> {
        descendants(&self.sessions, root)
    }

    fn selected_details(&self) -> Option<(String, String, Status)> {
        self.selected_session()
            .map(|session| (session.id.clone(), session.title.clone(), session.status()))
    }

    fn send(&mut self, command: Command) {
        if self.command_tx.send(command).is_err() {
            self.notify("Action worker unavailable", true);
        }
    }

    fn clamp_active(&mut self) {
        let count = visible_columns(&self.columns).len();
        if count == 0 {
            self.active_col = 0;
        } else {
            self.active_col = self.active_col.min(count - 1);
        }
    }

    fn persist(&mut self) {
        if let Err(error) = self.as_config().save() {
            self.notify(format!("Config not saved · {error}"), true);
        }
    }

    fn as_config(&self) -> Config {
        Config {
            columns: self.columns.clone(),
            sort: self.sort,
            sort_desc: self.sort_desc,
            show_archived: self.show_archived,
        }
    }
}

pub fn visible_columns(columns: &[Column]) -> Vec<&Column> {
    columns.iter().filter(|column| column.visible).collect()
}

fn descendants(sessions: &[Session], root: &str) -> Vec<String> {
    let mut ids = vec![root.to_string()];
    let mut index = 0;
    while index < ids.len() {
        let parent = ids[index].clone();
        for session in sessions {
            if session.parent_id.as_deref() == Some(parent.as_str()) && !ids.contains(&session.id) {
                ids.push(session.id.clone());
            }
        }
        index += 1;
    }
    ids
}

fn matches_query(session: &Session, query: &str) -> bool {
    session.title.to_lowercase().contains(query)
        || session.project.to_lowercase().contains(query)
        || session.directory.to_lowercase().contains(query)
        || session
            .agent
            .as_deref()
            .is_some_and(|agent| agent.to_lowercase().contains(query))
        || session
            .model
            .as_deref()
            .is_some_and(|model| model.to_lowercase().contains(query))
        || session.id.to_lowercase().contains(query)
}

fn is_ctrl(key: &KeyEvent, character: char) -> bool {
    key.modifiers.contains(KeyModifiers::CONTROL)
        && matches!(key.code, KeyCode::Char(value) if value.eq_ignore_ascii_case(&character))
}

fn edit_input(input: &mut Input, key: &KeyEvent) -> bool {
    if key.modifiers.contains(KeyModifiers::CONTROL) || key.modifiers.contains(KeyModifiers::ALT) {
        return false;
    }
    match key.code {
        KeyCode::Left => input.left(),
        KeyCode::Right => input.right(),
        KeyCode::Home => input.cursor = 0,
        KeyCode::End => input.cursor = input.chars.len(),
        KeyCode::Backspace => input.backspace(),
        KeyCode::Delete => input.delete(),
        KeyCode::Char(character) => input.insert(character),
        _ => return false,
    }
    true
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn input_editing() {
        let mut input = Input::from_text("abc");
        let left = KeyEvent::new(KeyCode::Left, KeyModifiers::NONE);
        assert!(edit_input(&mut input, &left));
        input.insert('X');
        assert_eq!(input.text(), "abXc");
        input.backspace();
        assert_eq!(input.text(), "abc");
        input.delete();
        assert_eq!(input.text(), "ab");
    }

    fn sample_session(id: &str, parent: Option<&str>) -> Session {
        Session {
            id: id.into(),
            parent_id: parent.map(str::to_string),
            project: "proj".into(),
            directory: "/tmp".into(),
            title: id.into(),
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

    fn test_app() -> App {
        let (command_tx, _command_rx) = std::sync::mpsc::channel();
        let (_event_tx, event_rx) = std::sync::mpsc::channel();
        let mut table = TableState::default();
        table.select(Some(0));
        App {
            sessions: vec![
                sample_session("root", None),
                sample_session("child", Some("root")),
            ],
            rows: vec![0, 1],
            table,
            columns: Config::default().columns,
            sort: ColumnId::Updated,
            sort_desc: true,
            filters: Vec::new(),
            search: String::new(),
            search_input: Input::default(),
            search_editing: false,
            show_archived: false,
            archive_supported: true,
            active_col: 0,
            modal: Modal::None,
            toast: None,
            quit: false,
            db_path: PathBuf::from("/tmp/ocsess-test.db"),
            header_hits: Vec::new(),
            rows_area: Rect::new(0, 2, 100, 10),
            table_offset: 0,
            detail: None,
            api_online: false,
            current_session_id: None,
            active_ids: HashSet::new(),
            last_active_check: Instant::now(),
            api: Arc::new(Api::new()),
            command_tx,
            event_rx,
            last_reload: Instant::now(),
            last_error: None,
            select_after: None,
            last_click: None,
        }
    }

    #[test]
    fn header_hits_contain_rendered_labels() {
        use ratatui::Terminal;
        use ratatui::backend::TestBackend;

        let mut app = test_app();
        let mut terminal = Terminal::new(TestBackend::new(100, 20)).unwrap();
        terminal
            .draw(|frame| crate::ui::render(frame, &mut app))
            .unwrap();

        let area = app.rows_area;
        let buffer = terminal.backend().buffer();
        let mut line = String::new();
        for x in area.x..area.right() {
            line.push_str(buffer[(x, area.y)].symbol());
        }

        let mut checked = 0;
        for (index, column) in visible_columns(&app.columns).iter().enumerate() {
            let Some(label_start) = line.find(column.id.label()) else {
                continue;
            };
            let label_x = area.x + label_start as u16;
            let (start, end, hit_index) = app.header_hits[index];
            assert_eq!(hit_index, index);
            assert!(
                label_x >= start && label_x < end,
                "header {} label at {label_x} outside hit {start}..{end}",
                column.id.label()
            );
            checked += 1;
        }
        assert!(checked >= 5, "only {checked} headers rendered");
    }

    #[test]
    fn mouse_selects_then_opens_row() {
        let mut app = test_app();
        let click = MouseEvent {
            kind: MouseEventKind::Down(MouseButton::Left),
            column: 5,
            row: 4,
            modifiers: KeyModifiers::NONE,
        };
        assert_eq!(app.on_mouse(click), None);
        assert_eq!(app.table.selected(), Some(1));
        assert_eq!(app.on_mouse(click), Some(1));
    }

    #[test]
    fn archive_is_refused_when_unsupported() {
        let mut app = test_app();
        app.archive_supported = false;
        app.archive_selected();
        let toast = app.toast.as_ref().expect("expected a toast");
        assert!(toast.error);
        assert!(
            toast.text.contains("not available"),
            "unexpected toast: {}",
            toast.text
        );
    }

    #[test]
    fn descendants_include_all_levels() {
        let sessions = vec![
            sample_session("root", None),
            sample_session("child", Some("root")),
            sample_session("grandchild", Some("child")),
            sample_session("other", None),
        ];
        assert_eq!(
            descendants(&sessions, "root"),
            vec!["root", "child", "grandchild"]
        );
    }

    #[test]
    fn search_query_is_case_insensitive() {
        let session = Session {
            id: "ses_a".into(),
            parent_id: None,
            project: "Proj".into(),
            directory: "/tmp".into(),
            title: "Fix Auth Bug".into(),
            agent: Some("build".into()),
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
        };
        assert!(matches_query(&session, "auth"));
        assert!(matches_query(&session, "proj"));
        assert!(!matches_query(&session, "nope"));
    }
}
