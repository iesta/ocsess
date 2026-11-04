use ratatui::Frame;
use ratatui::layout::{Alignment, Constraint, Flex, Layout, Rect};
use ratatui::style::{Color, Modifier, Style};
use ratatui::text::{Line, Span, Text};
use ratatui::widgets::{Block, Cell, Clear, Paragraph, Row, Table, Wrap};

use crate::app::{App, Detail, FilterStage, Input, Modal, visible_columns};
use crate::config::{ColumnId, Filter};
use crate::db::{
    Call, Session, Status, format_cost, format_date, format_duration, format_tokens, now_ms,
    summarize,
};

pub fn render(frame: &mut Frame, app: &mut App) {
    let area = frame.area();
    let chunks = Layout::vertical([
        Constraint::Length(1),
        Constraint::Min(5),
        Constraint::Length(1),
        Constraint::Length(1),
    ])
    .split(area);

    render_header(frame, app, chunks[0]);
    render_table(frame, app, chunks[1]);
    render_footer(frame, app, chunks[2]);
    render_status(frame, app, chunks[3]);
    render_modal(frame, app, area);
    if app.detail.is_some() {
        render_detail(frame, app, area);
    }
}

fn render_header(frame: &mut Frame, app: &App, area: Rect) {
    let columns = Layout::horizontal([Constraint::Min(0), Constraint::Length(38)]).split(area);
    let left = Line::from(vec![
        Span::styled(
            " ocsess ",
            Style::default()
                .fg(Color::Cyan)
                .add_modifier(Modifier::BOLD),
        ),
        Span::styled("· ", dim()),
        Span::styled(
            format!("{} / {} sessions", app.rows.len(), app.sessions.len()),
            Style::default().fg(Color::Gray),
        ),
        Span::styled(
            if app.show_archived {
                "  · archived shown"
            } else {
                "  · archived hidden"
            },
            dim(),
        ),
    ]);
    frame.render_widget(Paragraph::new(left), columns[0]);

    let sort = format!(
        "{} · sort {} {} ",
        if app.api_online { "api ●" } else { "api ○" },
        app.sort.label(),
        if app.sort_desc { "▼" } else { "▲" }
    );
    frame.render_widget(
        Paragraph::new(Line::from(Span::styled(sort, dim()))).alignment(Alignment::Right),
        columns[1],
    );
}

fn render_table(frame: &mut Frame, app: &mut App, area: Rect) {
    let visible = visible_columns(&app.columns);
    let widths: Vec<Constraint> = visible
        .iter()
        .map(|column| Constraint::Length(column.width.max(4)))
        .collect();

    let header_cells = visible.iter().enumerate().map(|(index, column)| {
        let sorted = column.id == app.sort;
        let active = index == app.active_col;
        let style = if active {
            Style::default()
                .fg(Color::Black)
                .bg(Color::Cyan)
                .add_modifier(Modifier::BOLD)
        } else if sorted {
            Style::default()
                .fg(Color::Gray)
                .add_modifier(Modifier::BOLD)
        } else {
            Style::default()
                .fg(Color::DarkGray)
                .add_modifier(Modifier::BOLD)
        };
        let mut spans = vec![Span::styled(column.id.label(), style)];
        if sorted {
            let arrow_style = if active {
                Style::default().fg(Color::Black)
            } else {
                Style::default()
                    .fg(Color::Yellow)
                    .add_modifier(Modifier::BOLD)
            };
            spans.push(Span::styled(
                if app.sort_desc { " ▼" } else { " ▲" },
                arrow_style,
            ));
        }
        let mut content = Text::from(Line::from(spans));
        if column.id.right_aligned() {
            content = content.alignment(Alignment::Right);
        }
        Cell::from(content).style(style)
    });
    let header = Row::new(header_cells).height(1);

    let rows: Vec<Row> = app
        .rows
        .iter()
        .filter_map(|&index| app.sessions.get(index))
        .map(|session| {
            let current = app.is_current(&session.id);
            let cells = visible
                .iter()
                .map(|column| make_cell(column.id, session, current));
            let mut row = Row::new(cells).height(1);
            if session.time_archived.is_some() {
                row = row.style(Style::default().fg(Color::DarkGray));
            }
            row
        })
        .collect();

    let block = Block::bordered()
        .border_style(dim())
        .title(Line::from(vec![Span::styled(" sessions ", dim())]));
    let inner = block.inner(area);
    let table = Table::new(rows, widths.clone())
        .header(header)
        .block(block)
        .column_spacing(1)
        .row_highlight_style(
            Style::default()
                .bg(Color::Indexed(237))
                .add_modifier(Modifier::BOLD),
        );

    let column_rects = Layout::horizontal(widths.iter().copied())
        .flex(Flex::Start)
        .spacing(1)
        .split(inner);
    frame.render_stateful_widget(table, area, &mut app.table);

    app.rows_area = inner;
    app.table_offset = app.table.offset();
    app.header_hits = column_rects
        .iter()
        .enumerate()
        .filter_map(|(index, rect)| {
            let start = rect.x;
            let end = rect.right().min(inner.right());
            (start < end).then_some((start, end, index))
        })
        .collect();

    if app.rows.is_empty() {
        let message = if visible.is_empty() {
            "No columns visible · press c to show some"
        } else {
            "No sessions · adjust filters or press A to show archived"
        };
        let rect = Rect::new(
            inner.x,
            inner.y.saturating_add(1),
            inner.width,
            inner.height.saturating_sub(1),
        );
        frame.render_widget(
            Paragraph::new(message)
                .style(dim())
                .alignment(Alignment::Center),
            rect,
        );
    }
}

fn make_cell(column: ColumnId, session: &Session, current: bool) -> Cell<'static> {
    let status = session.status();
    let mut content = if column == ColumnId::State {
        Text::from(format!("{} {}", status.symbol(), status.label()))
    } else if column == ColumnId::Title && current {
        Text::from(Line::from(vec![
            Span::styled("◆ ", accent()),
            Span::raw(column.display(session)),
        ]))
    } else {
        Text::from(column.display(session))
    };

    let mut style = match column {
        ColumnId::State => status_style(status),
        ColumnId::Title => Style::default().add_modifier(Modifier::BOLD),
        ColumnId::Agent
        | ColumnId::Model
        | ColumnId::Directory
        | ColumnId::Tokens
        | ColumnId::Parent
        | ColumnId::Outcome
        | ColumnId::Created
        | ColumnId::Id
        | ColumnId::Archived => Style::default().fg(Color::Gray),
        ColumnId::Cost => cost_style(session.cost),
        ColumnId::Updated => age_style(session.time_updated),
        ColumnId::Project | ColumnId::Messages => Style::default(),
    };

    if session.time_archived.is_some() && !matches!(column, ColumnId::State | ColumnId::Title) {
        style = style.fg(Color::DarkGray);
    }
    if column.right_aligned() {
        content = content.alignment(Alignment::Right);
    }
    Cell::from(content).style(style)
}

fn render_footer(frame: &mut Frame, app: &App, area: Rect) {
    let mut spans = Vec::new();
    if app.search_editing {
        spans.push(Span::styled(" / ", accent()));
        spans.extend(input_line(&app.search_input).spans);
    } else if !app.search.is_empty() {
        spans.push(Span::styled(format!(" / {} ", app.search), accent()));
    } else {
        spans.push(Span::styled(
            " / search · f filter · c columns · ? help",
            dim(),
        ));
    }

    for filter in &app.filters {
        spans.push(Span::styled(" ", dim()));
        spans.push(Span::styled(
            filter_chip(filter),
            Style::default().fg(Color::Black).bg(Color::Indexed(110)),
        ));
    }
    if !app.filters.is_empty() {
        spans.push(Span::styled("  F clear", dim()));
    }

    frame.render_widget(Paragraph::new(Line::from(spans)), area);
}

fn render_status(frame: &mut Frame, app: &App, area: Rect) {
    let columns = Layout::horizontal([Constraint::Min(0), Constraint::Length(28)]).split(area);
    let (text, style) = if let Some(toast) = &app.toast {
        (
            toast.text.clone(),
            if toast.error {
                Style::default().fg(Color::LightRed)
            } else {
                Style::default().fg(Color::Green)
            },
        )
    } else {
        let actions = if app.archive_supported {
            "Enter open · i detail · d duplicate · r rename · a archive · D delete · m compact"
        } else {
            "Enter open · i detail · d duplicate · r rename · D delete · m compact"
        };
        (actions.to_string(), dim())
    };
    frame.render_widget(
        Paragraph::new(Line::from(Span::styled(text, style))),
        columns[0],
    );
    frame.render_widget(
        Paragraph::new(Line::from(Span::styled(
            format!(
                "{} columns · {} rows ",
                visible_columns(&app.columns).len(),
                app.rows.len()
            ),
            dim(),
        )))
        .alignment(Alignment::Right),
        columns[1],
    );
}

fn render_modal(frame: &mut Frame, app: &App, area: Rect) {
    match &app.modal {
        Modal::None => {}
        Modal::Help => {
            let mut actions = vec![
                Line::from(Span::styled("Actions", accent())),
                Line::from("  Enter          open in opencode"),
                Line::from("  i              call detail"),
                Line::from("  d              duplicate (fork)"),
                Line::from("  r              rename"),
            ];
            if app.archive_supported {
                actions.push(Line::from("  a / u          archive / unarchive"));
            }
            actions.extend([
                Line::from("  D              delete (gzip backup)"),
                Line::from("  m              compact"),
                Line::from("  R              reload · q quit"),
            ]);
            let lines = vec![
                Line::from(Span::styled("Navigation", accent())),
                Line::from("  ↑↓ / j k       row"),
                Line::from("  gg / G         top / bottom"),
                Line::from("  Ctrl+d / u     page"),
                Line::from("  ←→ / h l       active column"),
                Line::from("  s              sort active column"),
                Line::from(""),
                Line::from(Span::styled("Filters", accent())),
                Line::from("  /              global search"),
                Line::from("  f              per-column filter"),
                Line::from("  F / Esc        clear search and filters"),
                Line::from("  A              show / hide archived"),
                Line::from("  c              columns: space, ←→ width, J/K order"),
                Line::from(""),
            ]
            .into_iter()
            .chain(actions)
            .chain([
                Line::from(""),
                Line::from(Span::styled("Colors", accent())),
                Line::from("  green running · blue suspended · gray idle · magenta archived"),
                Line::from("  cost: green <$0.05 · yellow <$0.5 · orange <$2 · red >=$2"),
                Line::from("  ◆ current session · yellow sort arrow"),
                Line::from(""),
                Line::from(Span::styled(
                    "Mouse: click header to sort, click row to select, double-click to open, wheel scrolls",
                    dim(),
                )),
            ])
            .collect::<Vec<_>>();
            draw_modal(frame, area, "Help", lines, 82, 33);
        }
        Modal::Confirm { prompt, .. } => {
            let mut lines: Vec<Line> = prompt
                .lines()
                .map(|line| Line::from(line.to_string()))
                .collect();
            lines.push(Line::from(""));
            lines.push(Line::from(Span::styled(
                "[y] yes    [n] no",
                Style::default().fg(Color::Yellow),
            )));
            draw_modal(frame, area, "Confirm", lines, 64, 9);
        }
        Modal::Rename { input, .. } => {
            let lines = vec![
                Line::from("New title:"),
                input_line(input),
                Line::from(""),
                Line::from(Span::styled("Enter save · Esc cancel", dim())),
            ];
            draw_modal(frame, area, "Rename", lines, 64, 7);
        }
        Modal::Filter {
            stage: FilterStage::Column,
            col_idx,
            ..
        } => {
            let lines = ColumnId::ALL
                .iter()
                .enumerate()
                .map(|(index, column)| {
                    let text = format!("  {}", column.label());
                    if index == *col_idx {
                        Line::from(Span::styled(
                            text,
                            Style::default()
                                .fg(Color::Black)
                                .bg(Color::Cyan)
                                .add_modifier(Modifier::BOLD),
                        ))
                    } else {
                        Line::from(text)
                    }
                })
                .chain(std::iter::once(Line::from("")))
                .chain(std::iter::once(Line::from(Span::styled(
                    "↑↓ choose · Enter select · Esc close",
                    dim(),
                ))))
                .collect::<Vec<_>>();
            draw_modal(
                frame,
                area,
                "Filter · column",
                lines,
                48,
                ColumnId::ALL.len() as u16 + 5,
            );
        }
        Modal::Filter {
            stage: FilterStage::Value,
            col_idx,
            input,
        } => {
            let column = ColumnId::ALL[*col_idx];
            let lines = vec![
                Line::from(format!("Column: {}", column.label())),
                input_line(input),
                Line::from(""),
                Line::from(Span::styled(
                    "text = contains · =exact · >N >=N <N <=N numeric",
                    dim(),
                )),
                Line::from(Span::styled("Enter add · Esc back", dim())),
            ];
            draw_modal(frame, area, "Filter · value", lines, 64, 8);
        }
        Modal::Columns { selected } => {
            let mut lines: Vec<Line> = app
                .columns
                .iter()
                .enumerate()
                .map(|(index, column)| {
                    let check = if column.visible { "[x]" } else { "[ ]" };
                    let text = format!(" {check} {:<16} {:>3}", column.id.label(), column.width);
                    if index == *selected {
                        Line::from(Span::styled(
                            text,
                            Style::default()
                                .fg(Color::Black)
                                .bg(Color::Cyan)
                                .add_modifier(Modifier::BOLD),
                        ))
                    } else {
                        Line::from(text)
                    }
                })
                .collect();
            lines.push(Line::from(""));
            lines.push(Line::from(Span::styled(
                "Space toggle · ←→ width · J/K order · Enter close",
                dim(),
            )));
            draw_modal(
                frame,
                area,
                "Columns",
                lines,
                64,
                app.columns.len() as u16 + 5,
            );
        }
    }
}

fn render_detail(frame: &mut Frame, app: &mut App, area: Rect) {
    let Some(detail) = app.detail.as_mut() else {
        return;
    };
    let popup = area;
    if popup.width == 0 || popup.height == 0 {
        return;
    }
    frame.render_widget(Clear, popup);
    let block = Block::bordered()
        .title(format!(" Detail · {} ", detail.title))
        .border_style(Style::default().fg(Color::Cyan));
    let inner = block.inner(popup);
    frame.render_widget(block, popup);

    let chunks = Layout::vertical([
        Constraint::Length(3),
        Constraint::Min(4),
        Constraint::Length(1),
    ])
    .split(inner);

    render_detail_summary(frame, detail, chunks[0]);
    render_detail_table(frame, detail, chunks[1]);
    frame.render_widget(
        Paragraph::new(Line::from(Span::styled(
            "↑↓ scroll · g/G newest/oldest · R reload · i/Esc close",
            dim(),
        ))),
        chunks[2],
    );
}

fn render_detail_summary(frame: &mut Frame, detail: &Detail, area: Rect) {
    let summary = summarize(&detail.calls);
    let models = model_summary(&detail.calls);
    let lines = vec![
        Line::from(vec![
            Span::styled(format!("{} calls", summary.calls), accent()),
            Span::raw(format!(
                " · {} · avg {} · {} tools · {} tool errors",
                zero_cost(summary.cost),
                zero_cost(summary.avg_cost()),
                summary.tools,
                summary.errors
            )),
        ]),
        Line::from(vec![
            Span::styled("tokens ", dim()),
            Span::raw(format!(
                "in {} · out {} · reasoning {} · cache {}/{}",
                token_count(summary.tokens_input),
                token_count(summary.tokens_output),
                token_count(summary.tokens_reasoning),
                token_count(summary.cache_read),
                token_count(summary.cache_write)
            )),
        ]),
        Line::from(vec![Span::styled("models ", dim()), Span::raw(models)]),
    ];
    frame.render_widget(Paragraph::new(lines), area);
}

fn render_detail_table(frame: &mut Frame, detail: &mut Detail, area: Rect) {
    let widths = vec![
        Constraint::Length(16),
        Constraint::Length(26),
        Constraint::Length(9),
        Constraint::Length(8),
        Constraint::Length(7),
        Constraint::Length(8),
        Constraint::Length(7),
        Constraint::Length(10),
        Constraint::Length(30),
    ];

    let header = Row::new(vec![
        aligned_cell("Time (UTC)", Style::default().fg(Color::DarkGray), false),
        aligned_cell("Model", Style::default().fg(Color::DarkGray), false),
        aligned_cell("Cost", Style::default().fg(Color::DarkGray), true),
        aligned_cell("In", Style::default().fg(Color::DarkGray), true),
        aligned_cell("Out", Style::default().fg(Color::DarkGray), true),
        aligned_cell("Cache", Style::default().fg(Color::DarkGray), true),
        aligned_cell("Dur", Style::default().fg(Color::DarkGray), true),
        aligned_cell("Finish", Style::default().fg(Color::DarkGray), false),
        aligned_cell("Tools", Style::default().fg(Color::DarkGray), false),
    ])
    .height(1);

    let rows: Vec<Row> = detail
        .calls
        .iter()
        .map(|call| {
            let finish = call.finish.clone().unwrap_or_else(|| "—".to_string());
            let tools = call.tools_label();
            let tools = if tools.is_empty() {
                "—".to_string()
            } else {
                tools
            };
            Row::new(vec![
                aligned_cell(
                    format_date(call.created),
                    Style::default().fg(Color::Gray),
                    false,
                ),
                aligned_cell(
                    call.model.clone().unwrap_or_else(|| "—".to_string()),
                    Style::default().fg(Color::Gray),
                    false,
                ),
                aligned_cell(
                    call_cost(call.cost),
                    cost_style(call.cost.unwrap_or(0.0)),
                    true,
                ),
                aligned_cell(
                    token_count(call.tokens_input),
                    Style::default().fg(Color::Gray),
                    true,
                ),
                aligned_cell(
                    token_count(call.tokens_output),
                    Style::default().fg(Color::Gray),
                    true,
                ),
                aligned_cell(
                    token_count(call.tokens_cache_read),
                    Style::default().fg(Color::Gray),
                    true,
                ),
                aligned_cell(
                    format_duration(call.duration_ms()),
                    Style::default().fg(Color::Gray),
                    true,
                ),
                aligned_cell(finish.clone(), finish_style(&finish), false),
                aligned_cell(tools, Style::default().fg(Color::Gray), false),
            ])
            .height(1)
        })
        .collect();

    let table = Table::new(rows, widths.clone())
        .header(header)
        .column_spacing(1)
        .row_highlight_style(
            Style::default()
                .bg(Color::Indexed(237))
                .add_modifier(Modifier::BOLD),
        );
    frame.render_stateful_widget(table, area, &mut detail.table);
    detail.rows_area = area;
    detail.table_offset = detail.table.offset();
}

fn aligned_cell(text: impl Into<String>, style: Style, right: bool) -> Cell<'static> {
    let mut content = Text::from(text.into());
    if right {
        content = content.alignment(Alignment::Right);
    }
    Cell::from(content).style(style)
}

fn token_count(tokens: i64) -> String {
    if tokens <= 0 {
        "0".to_string()
    } else {
        format_tokens(tokens)
    }
}

fn call_cost(cost: Option<f64>) -> String {
    match cost {
        None => "—".to_string(),
        Some(cost) if cost <= 0.0 => "$0.00".to_string(),
        Some(cost) => format_cost(cost),
    }
}

fn zero_cost(cost: f64) -> String {
    if cost <= 0.0 {
        "$0.00".to_string()
    } else {
        format_cost(cost)
    }
}

fn model_summary(calls: &[Call]) -> String {
    let mut models: Vec<String> = Vec::new();
    for call in calls {
        if let Some(model) = &call.model
            && !models.contains(model)
        {
            models.push(model.clone());
        }
    }
    match models.len() {
        0 => "—".to_string(),
        1 | 2 => models.join(", "),
        _ => format!("{}, +{} more", models[0], models.len() - 1),
    }
}

fn finish_style(finish: &str) -> Style {
    match finish {
        "stop" => Style::default().fg(Color::Green),
        "tool-calls" => Style::default().fg(Color::Gray),
        "error" => Style::default().fg(Color::LightRed),
        "unknown" => Style::default().fg(Color::Yellow),
        _ => dim(),
    }
}

fn draw_modal(
    frame: &mut Frame,
    area: Rect,
    title: &str,
    lines: Vec<Line<'static>>,
    width: u16,
    height: u16,
) {
    let popup = popup_area(area, width, height);
    frame.render_widget(Clear, popup);
    let block = Block::bordered()
        .title(format!(" {title} "))
        .border_style(Style::default().fg(Color::Cyan));
    let inner = block.inner(popup);
    frame.render_widget(block, popup);
    frame.render_widget(Paragraph::new(lines).wrap(Wrap { trim: false }), inner);
}

fn popup_area(area: Rect, width: u16, height: u16) -> Rect {
    let width = width.min(area.width).max(1);
    let height = height.min(area.height).max(1);
    Rect::new(
        area.x + area.width.saturating_sub(width) / 2,
        area.y + area.height.saturating_sub(height) / 2,
        width,
        height,
    )
}

fn input_line(input: &Input) -> Line<'static> {
    let before: String = input.chars[..input.cursor.min(input.chars.len())]
        .iter()
        .collect();
    let mut spans = vec![Span::raw(before)];
    if let Some(character) = input.chars.get(input.cursor) {
        spans.push(Span::styled(
            character.to_string(),
            Style::default().fg(Color::Black).bg(Color::Cyan),
        ));
        let after: String = input.chars[input.cursor + 1..].iter().collect();
        if !after.is_empty() {
            spans.push(Span::raw(after));
        }
    } else {
        spans.push(Span::styled(
            " ",
            Style::default().fg(Color::Black).bg(Color::Cyan),
        ));
    }
    Line::from(spans)
}

fn filter_chip(filter: &Filter) -> String {
    format!(
        " {} {} {} ",
        filter.column.label(),
        filter.op.symbol(),
        filter.value
    )
}

fn status_style(status: Status) -> Style {
    match status {
        Status::Running => Style::default().fg(Color::Green),
        Status::Suspended => Style::default().fg(Color::LightBlue),
        Status::Idle => Style::default().fg(Color::Gray),
        Status::New => Style::default().fg(Color::DarkGray),
        Status::Archived => Style::default().fg(Color::Magenta),
    }
}

fn cost_style(cost: f64) -> Style {
    if cost <= 0.0 {
        dim()
    } else if cost < 0.05 {
        Style::default().fg(Color::Green)
    } else if cost < 0.5 {
        Style::default().fg(Color::Yellow)
    } else if cost < 2.0 {
        Style::default().fg(Color::Indexed(208))
    } else {
        Style::default().fg(Color::LightRed)
    }
}

fn age_style(updated: i64) -> Style {
    let age = (now_ms() - updated).max(0);
    if age < 60_000 {
        Style::default().fg(Color::Cyan)
    } else if age < 7 * 86_400_000 {
        Style::default().fg(Color::Gray)
    } else {
        dim()
    }
}

fn dim() -> Style {
    Style::default().fg(Color::DarkGray)
}

fn accent() -> Style {
    Style::default().fg(Color::Cyan)
}
