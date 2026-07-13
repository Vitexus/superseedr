// SPDX-FileCopyrightText: 2026 The superseedr Contributors
// SPDX-License-Identifier: GPL-3.0-or-later

use crate::app::{AppCommand, AppMode, AppState, JournalFilter};
use crate::persistence::event_journal::{
    EventCategory, EventDetails, EventJournalEntry, EventType,
};
use crate::theme::ThemeContext;
use crate::tui::action_style::{footer_key_style, ActionTone};
use crate::tui::app_command::spawn_app_command_sender;
use crate::tui::formatters::{sanitize_text, truncate_with_ellipsis};
use crate::tui::screen_context::ScreenContext;
use chrono::{DateTime, Local};
use ratatui::crossterm::event::{Event as CrosstermEvent, KeyCode, KeyEventKind};
use ratatui::prelude::{Alignment, Constraint, Frame, Line, Modifier, Span, Style};
use ratatui::widgets::{Block, Borders, Cell, Clear, Padding, Paragraph, Row, Table, TableState};
use std::collections::HashMap;
use std::path::{Component, Path};
use tokio::sync::{broadcast, mpsc};

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum JournalAction {
    ToNormal,
    FilterNext,
    FilterPrev,
    MoveUp,
    MoveDown,
    ReplaySelected,
}

fn map_key_to_journal_action(key_code: KeyCode, key_kind: KeyEventKind) -> Option<JournalAction> {
    if !matches!(key_kind, KeyEventKind::Press | KeyEventKind::Repeat) {
        return None;
    }

    match key_code {
        KeyCode::Esc | KeyCode::Char('q') => Some(JournalAction::ToNormal),
        KeyCode::Tab => Some(JournalAction::FilterNext),
        KeyCode::BackTab => Some(JournalAction::FilterPrev),
        KeyCode::Up | KeyCode::Char('k') => Some(JournalAction::MoveUp),
        KeyCode::Down | KeyCode::Char('j') => Some(JournalAction::MoveDown),
        KeyCode::Char('Y') => Some(JournalAction::ReplaySelected),
        _ => None,
    }
}

pub fn handle_event_with_shutdown(
    event: CrosstermEvent,
    app_state: &mut AppState,
    app_command_tx: &mpsc::Sender<AppCommand>,
    shutdown_tx: &broadcast::Sender<()>,
) {
    handle_event_inner(event, app_state, app_command_tx, Some(shutdown_tx));
}

fn handle_event_inner(
    event: CrosstermEvent,
    app_state: &mut AppState,
    app_command_tx: &mpsc::Sender<AppCommand>,
    shutdown_tx: Option<&broadcast::Sender<()>>,
) {
    if !matches!(app_state.mode, AppMode::Journal) {
        return;
    }

    let CrosstermEvent::Key(key) = event else {
        return;
    };

    let Some(action) = map_key_to_journal_action(key.code, key.kind) else {
        return;
    };

    app_state.ui.journal.status_message = None;

    match action {
        JournalAction::ToNormal => app_state.mode = AppMode::Normal,
        JournalAction::FilterNext => {
            app_state.ui.journal.filter = app_state.ui.journal.filter.next();
            app_state.ui.journal.selected_index = 0;
        }
        JournalAction::FilterPrev => {
            app_state.ui.journal.filter = app_state.ui.journal.filter.prev();
            app_state.ui.journal.selected_index = 0;
        }
        JournalAction::MoveUp => {
            app_state.ui.journal.selected_index =
                app_state.ui.journal.selected_index.saturating_sub(1);
        }
        JournalAction::MoveDown => {
            let len = journal_activities(app_state).len();
            if len > 0 {
                app_state.ui.journal.selected_index =
                    (app_state.ui.journal.selected_index + 1).min(len - 1);
            }
        }
        JournalAction::ReplaySelected => {
            replay_selected_entry(app_state, app_command_tx, shutdown_tx)
        }
    }
}

fn entry_matches_filter(entry: &EventJournalEntry, filter: JournalFilter) -> bool {
    match filter {
        JournalFilter::All => true,
        JournalFilter::Queue => matches!(entry.category, EventCategory::Ingest),
        JournalFilter::Commands => matches!(entry.category, EventCategory::Control),
        JournalFilter::Health => matches!(entry.category, EventCategory::DataHealth),
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum ActivityPhase {
    Queued,
    Terminal,
    Standalone,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
struct ActivityKey<'a> {
    category: u8,
    scope: u8,
    host_id: Option<&'a str>,
    correlation_id: &'a str,
}

#[derive(Debug)]
struct JournalActivity<'a> {
    entries: Vec<&'a EventJournalEntry>,
    latest_position: usize,
}

impl<'a> JournalActivity<'a> {
    fn new(entry: &'a EventJournalEntry, position: usize) -> Self {
        Self {
            entries: vec![entry],
            latest_position: position,
        }
    }

    fn latest(&self) -> &'a EventJournalEntry {
        self.entries
            .last()
            .copied()
            .expect("journal activities always contain an entry")
    }

    fn source_entry(&self) -> &'a EventJournalEntry {
        self.entries
            .iter()
            .rev()
            .find(|entry| entry.source_path.is_some() || entry.source_watch_folder.is_some())
            .copied()
            .unwrap_or_else(|| self.latest())
    }

    fn torrent_entry(&self) -> &'a EventJournalEntry {
        self.entries
            .iter()
            .rev()
            .find(|entry| entry.torrent_name.is_some() || entry.info_hash_hex.is_some())
            .copied()
            .unwrap_or_else(|| self.latest())
    }
}

fn activity_phase(entry: &EventJournalEntry) -> ActivityPhase {
    match (entry.category, entry.event_type) {
        (EventCategory::Ingest, EventType::IngestQueued)
        | (EventCategory::Control, EventType::ControlQueued) => ActivityPhase::Queued,
        (
            EventCategory::Ingest,
            EventType::IngestAdded
            | EventType::IngestDuplicate
            | EventType::IngestInvalid
            | EventType::IngestFailed,
        )
        | (EventCategory::Control, EventType::ControlApplied | EventType::ControlFailed) => {
            ActivityPhase::Terminal
        }
        _ => ActivityPhase::Standalone,
    }
}

fn activity_key(entry: &EventJournalEntry) -> Option<ActivityKey<'_>> {
    let category = match entry.category {
        EventCategory::Ingest => 0,
        EventCategory::Control => 1,
        _ => return None,
    };
    let scope = match entry.scope {
        crate::persistence::event_journal::EventScope::Host => 0,
        crate::persistence::event_journal::EventScope::Shared => 1,
    };

    Some(ActivityKey {
        category,
        scope,
        host_id: entry.host_id.as_deref(),
        correlation_id: entry.correlation_id.as_deref()?,
    })
}

fn journal_activities(app_state: &AppState) -> Vec<JournalActivity<'_>> {
    let mut activities = Vec::<JournalActivity<'_>>::new();
    let mut open_activities = HashMap::<ActivityKey<'_>, Vec<usize>>::new();

    for (position, entry) in app_state.event_journal_state.entries.iter().enumerate() {
        if !entry_matches_filter(entry, app_state.ui.journal.filter) {
            continue;
        }

        match (activity_phase(entry), activity_key(entry)) {
            (ActivityPhase::Queued, Some(key)) => {
                let activity_index = activities.len();
                activities.push(JournalActivity::new(entry, position));
                open_activities.entry(key).or_default().push(activity_index);
            }
            (ActivityPhase::Terminal, Some(key)) => {
                let open_activity = open_activities
                    .get_mut(&key)
                    .and_then(|activity_indices| activity_indices.pop());
                if let Some(activity_index) = open_activity {
                    activities[activity_index].entries.push(entry);
                    activities[activity_index].latest_position = position;
                } else {
                    activities.push(JournalActivity::new(entry, position));
                }
            }
            _ => activities.push(JournalActivity::new(entry, position)),
        }
    }

    activities.sort_by_key(|activity| std::cmp::Reverse(activity.latest_position));
    activities
}

fn event_type_label(entry: &EventJournalEntry) -> &'static str {
    match entry.event_type {
        EventType::IngestQueued => "Queued",
        EventType::IngestAdded => "Added",
        EventType::IngestDuplicate => "Duplicate",
        EventType::IngestInvalid => "Invalid",
        EventType::IngestFailed => "Failed",
        EventType::TorrentCompleted => "Complete",
        EventType::DataUnavailable => "Missing",
        EventType::DataRecovered => "Found",
        EventType::ControlQueued => "Queued",
        EventType::ControlApplied => "Applied",
        EventType::ControlFailed => "Error",
    }
}

fn command_action_label(entry: &EventJournalEntry) -> String {
    match &entry.details {
        EventDetails::Control { action, .. } => sanitize_text(action),
        _ => event_type_label(entry).to_string(),
    }
}

fn source_label(entry: &EventJournalEntry, anonymize: bool) -> String {
    if anonymize {
        return "/path/to/source".to_string();
    }

    entry
        .source_path
        .as_ref()
        .map(|path| compact_path_label(path, 1))
        .or_else(|| {
            entry
                .source_watch_folder
                .as_ref()
                .map(|path| compact_path_label(path, 1))
        })
        .unwrap_or_else(|| "-".to_string())
}

fn torrent_label(entry: &EventJournalEntry, anonymize: bool) -> String {
    if anonymize {
        return "Torrent".to_string();
    }

    entry
        .torrent_name
        .as_ref()
        .map(|name| sanitize_text(name))
        .unwrap_or_else(|| "-".to_string())
}

fn live_completion_percent(entry: &EventJournalEntry, app_state: &AppState) -> Option<f64> {
    if let Some(info_hash_hex) = entry.info_hash_hex.as_deref() {
        if let Some(display) = app_state
            .torrents
            .iter()
            .find(|(info_hash, _)| hex::encode(info_hash.as_slice()) == info_hash_hex)
            .map(|(_, display)| display)
        {
            return Some(crate::app::torrent_completion_percent(
                &display.latest_state,
            ));
        }
    }

    entry.torrent_name.as_ref().and_then(|torrent_name| {
        app_state
            .torrents
            .values()
            .filter(|display| display.latest_state.torrent_name == *torrent_name)
            .map(|display| crate::app::torrent_completion_percent(&display.latest_state))
            .max_by(|left, right| left.total_cmp(right))
    })
}

fn progress_label(entry: &EventJournalEntry, app_state: &AppState) -> String {
    live_completion_percent(entry, app_state)
        .map(|pct| format!("{pct:.0}%"))
        .unwrap_or_else(|| "-".to_string())
}

fn preferred_source_text(entry: &EventJournalEntry) -> Option<String> {
    entry
        .source_path
        .as_ref()
        .map(|path| path.display().to_string())
        .or_else(|| {
            entry
                .source_watch_folder
                .as_ref()
                .map(|path| path.display().to_string())
        })
}

fn pretty_timestamp(ts_iso: &str) -> String {
    DateTime::parse_from_rfc3339(ts_iso)
        .map(|dt| dt.with_timezone(&Local).format("%b %d %H:%M").to_string())
        .unwrap_or_else(|_| ts_iso.to_string())
}

fn detailed_timestamp(ts_iso: &str) -> String {
    DateTime::parse_from_rfc3339(ts_iso)
        .map(|dt| {
            dt.with_timezone(&Local)
                .format("%b %d %I:%M:%S %p")
                .to_string()
        })
        .unwrap_or_else(|_| ts_iso.to_string())
}

fn compact_path_label(path: &Path, depth: usize) -> String {
    let components = path
        .components()
        .filter_map(|component| match component {
            Component::Normal(segment) => Some(segment.to_string_lossy().into_owned()),
            Component::Prefix(prefix) => Some(prefix.as_os_str().to_string_lossy().into_owned()),
            _ => None,
        })
        .collect::<Vec<_>>();

    if components.is_empty() {
        return sanitize_text(&path.display().to_string());
    }

    if components.len() <= depth {
        return sanitize_text(&components.join("/"));
    }

    sanitize_text(&format!(
        ".../{}",
        components[components.len() - depth..].join("/")
    ))
}

fn detail_text(entry: Option<&EventJournalEntry>, anonymize: bool) -> String {
    let Some(entry) = entry else {
        return "No journal entries yet.".to_string();
    };

    let mut text = entry
        .message
        .clone()
        .unwrap_or_else(|| "No details recorded.".to_string());

    if anonymize {
        if let Some(torrent_name) = &entry.torrent_name {
            text = text.replace(torrent_name, "Torrent");
        }
        if let Some(source_path) = &entry.source_path {
            text = text.replace(&source_path.display().to_string(), "/path/to/source");
        }
        if let Some(source_watch_folder) = &entry.source_watch_folder {
            text = text.replace(
                &source_watch_folder.display().to_string(),
                "/path/to/source",
            );
        }
    }

    sanitize_text(&text)
}

fn replay_command_for_path(path: &Path) -> Option<AppCommand> {
    match path.extension().and_then(|ext| ext.to_str()) {
        Some(ext) if ext.eq_ignore_ascii_case("torrent") => {
            Some(AppCommand::AddTorrentFromFile(path.to_path_buf()))
        }
        Some(ext) if ext.eq_ignore_ascii_case("magnet") => {
            Some(AppCommand::AddMagnetFromFile(path.to_path_buf()))
        }
        Some(ext) if ext.eq_ignore_ascii_case("path") => {
            Some(AppCommand::AddTorrentFromPathFile(path.to_path_buf()))
        }
        _ => None,
    }
}

fn replay_selected_entry(
    app_state: &mut AppState,
    app_command_tx: &mpsc::Sender<AppCommand>,
    shutdown_tx: Option<&broadcast::Sender<()>>,
) {
    let activities = journal_activities(app_state);
    let selected_index = app_state
        .ui
        .journal
        .selected_index
        .min(activities.len().saturating_sub(1));
    let Some(activity) = activities.get(selected_index) else {
        app_state.ui.journal.status_message = Some("No journal entry selected".to_string());
        return;
    };

    let Some(source_path) = activity
        .entries
        .iter()
        .rev()
        .find_map(|entry| entry.source_path.as_ref())
    else {
        app_state.ui.journal.status_message =
            Some("Selected entry has no replayable source file".to_string());
        return;
    };

    let Some(command) = replay_command_for_path(source_path) else {
        app_state.ui.journal.status_message =
            Some("Selected entry does not point to a replayable source file".to_string());
        return;
    };

    if !source_path.exists() {
        app_state.ui.journal.status_message =
            Some("Replay source file is no longer available".to_string());
        return;
    }

    if let Some(shutdown_tx) = shutdown_tx {
        spawn_app_command_sender(app_command_tx.clone(), shutdown_tx.subscribe(), command);
        app_state.ui.journal.status_message =
            Some(format!("Replayed {}", compact_path_label(source_path, 2)));
    } else {
        match app_command_tx.try_send(command) {
            Ok(()) => {
                app_state.ui.journal.status_message =
                    Some(format!("Replayed {}", compact_path_label(source_path, 2)));
            }
            Err(_) => {
                app_state.ui.journal.status_message =
                    Some("Replay request queue is busy".to_string());
            }
        }
    }
}

#[derive(Clone, Copy)]
enum JournalColumn {
    Time,
    Status,
    Subject,
    Done,
    Source,
}

fn columns_for_filter(filter: JournalFilter) -> Vec<JournalColumn> {
    match filter {
        JournalFilter::All => vec![
            JournalColumn::Time,
            JournalColumn::Status,
            JournalColumn::Subject,
            JournalColumn::Done,
            JournalColumn::Source,
        ],
        JournalFilter::Queue => vec![
            JournalColumn::Time,
            JournalColumn::Status,
            JournalColumn::Subject,
            JournalColumn::Done,
            JournalColumn::Source,
        ],
        JournalFilter::Commands => {
            vec![
                JournalColumn::Time,
                JournalColumn::Status,
                JournalColumn::Subject,
                JournalColumn::Source,
            ]
        }
        JournalFilter::Health => vec![
            JournalColumn::Time,
            JournalColumn::Status,
            JournalColumn::Subject,
        ],
    }
}

fn column_header(column: JournalColumn, filter: JournalFilter) -> &'static str {
    match (column, filter) {
        (JournalColumn::Subject, JournalFilter::Commands) => "Action",
        (JournalColumn::Subject, _) => "Torrent",
        (JournalColumn::Time, _) => "Time",
        (JournalColumn::Status, _) => "Status",
        (JournalColumn::Done, _) => "Done",
        (JournalColumn::Source, _) => "Source",
    }
}

fn visible_columns(filter: JournalFilter, width: u16) -> Vec<JournalColumn> {
    columns_for_filter(filter)
        .into_iter()
        .filter(|column| match column {
            JournalColumn::Source => width >= 96,
            JournalColumn::Done => width >= 76,
            _ => true,
        })
        .collect()
}

fn column_header_style(column: JournalColumn, ctx: &ThemeContext) -> Style {
    let color = match column {
        JournalColumn::Time => ctx.theme.semantic.subtext0,
        JournalColumn::Status => ctx.state_warning(),
        JournalColumn::Subject => ctx.accent_sky(),
        JournalColumn::Done => ctx.state_success(),
        JournalColumn::Source => ctx.accent_sapphire(),
    };
    ctx.apply(Style::default().fg(color).bold())
}

fn column_constraint(column: JournalColumn, filter: JournalFilter) -> Constraint {
    match (filter, column) {
        (_, JournalColumn::Time) => Constraint::Length(13),
        (_, JournalColumn::Status) => Constraint::Length(22),
        (_, JournalColumn::Subject) => Constraint::Fill(1),
        (_, JournalColumn::Done) => Constraint::Length(7),
        (_, JournalColumn::Source) => Constraint::Length(22),
    }
}

fn event_status_color(entry: &EventJournalEntry, ctx: &ThemeContext) -> ratatui::style::Color {
    match entry.event_type {
        EventType::IngestQueued | EventType::ControlQueued => ctx.state_warning(),
        EventType::IngestAdded | EventType::DataRecovered | EventType::ControlApplied => {
            ctx.state_success()
        }
        EventType::TorrentCompleted => ctx.state_complete(),
        EventType::IngestDuplicate => ctx.state_info(),
        EventType::IngestInvalid
        | EventType::IngestFailed
        | EventType::DataUnavailable
        | EventType::ControlFailed => ctx.state_error(),
    }
}

fn event_status_style(entry: &EventJournalEntry, ctx: &ThemeContext) -> Style {
    let style = Style::default().fg(event_status_color(entry, ctx));
    if matches!(activity_phase(entry), ActivityPhase::Terminal)
        || matches!(
            entry.event_type,
            EventType::TorrentCompleted | EventType::DataUnavailable | EventType::DataRecovered
        )
    {
        ctx.apply(style.bold())
    } else {
        ctx.apply(style)
    }
}

fn activity_status_cell(activity: &JournalActivity<'_>, ctx: &ThemeContext) -> Cell<'static> {
    let mut spans = Vec::new();
    for (index, entry) in activity.entries.iter().enumerate() {
        if index > 0 {
            spans.push(Span::styled(
                " → ",
                ctx.apply(Style::default().fg(ctx.theme.semantic.overlay0)),
            ));
        }
        spans.push(Span::styled("● ", event_status_style(entry, ctx)));
        spans.push(Span::styled(
            event_type_label(entry).to_string(),
            event_status_style(entry, ctx),
        ));
    }
    Cell::from(Line::from(spans))
}

fn activity_subject_label(activity: &JournalActivity<'_>, anonymize: bool) -> String {
    let entry = activity.latest();
    if matches!(entry.category, EventCategory::Control) {
        command_action_label(entry)
    } else {
        torrent_label(activity.torrent_entry(), anonymize)
    }
}

fn activity_progress_label(activity: &JournalActivity<'_>, app_state: &AppState) -> String {
    activity
        .entries
        .iter()
        .rev()
        .find(|entry| live_completion_percent(entry, app_state).is_some())
        .map(|entry| progress_label(entry, app_state))
        .unwrap_or_else(|| "-".to_string())
}

fn column_cell(
    column: JournalColumn,
    activity: &JournalActivity<'_>,
    app_state: &AppState,
    ctx: &ThemeContext,
) -> Cell<'static> {
    match column {
        JournalColumn::Time => Cell::from(pretty_timestamp(&activity.latest().ts_iso))
            .style(ctx.apply(Style::default().fg(ctx.theme.semantic.subtext0))),
        JournalColumn::Status => activity_status_cell(activity, ctx),
        JournalColumn::Subject => Cell::from(activity_subject_label(
            activity,
            app_state.anonymize_torrent_names,
        ))
        .style(ctx.apply(Style::default().fg(ctx.theme.semantic.text))),
        JournalColumn::Done => Cell::from(activity_progress_label(activity, app_state))
            .style(ctx.apply(Style::default().fg(ctx.state_success()).bold())),
        JournalColumn::Source => Cell::from(source_label(
            activity.source_entry(),
            app_state.anonymize_torrent_names,
        ))
        .style(ctx.apply(Style::default().fg(ctx.theme.semantic.subtext1))),
    }
}

fn activity_detail_lines(
    activity: Option<&JournalActivity<'_>>,
    app_state: &AppState,
    ctx: &ThemeContext,
    width: u16,
) -> Vec<Line<'static>> {
    let Some(activity) = activity else {
        return vec![Line::from(Span::styled(
            "No journal activities yet.",
            ctx.apply(Style::default().fg(ctx.theme.semantic.subtext1)),
        ))];
    };

    let record_message_width = usize::from(width.saturating_sub(35));
    let mut lines = activity
        .entries
        .iter()
        .map(|entry| {
            let message = detail_text(Some(entry), app_state.anonymize_torrent_names);
            Line::from(vec![
                Span::styled("● ", event_status_style(entry, ctx)),
                Span::styled(
                    format!("{:<10}", event_type_label(entry)),
                    event_status_style(entry, ctx),
                ),
                Span::styled(
                    format!("{}  ", detailed_timestamp(&entry.ts_iso)),
                    ctx.apply(Style::default().fg(ctx.theme.semantic.subtext0)),
                ),
                Span::styled(
                    truncate_with_ellipsis(&message, record_message_width),
                    ctx.apply(Style::default().fg(ctx.theme.semantic.text)),
                ),
            ])
        })
        .collect::<Vec<_>>();

    if let Some(source) = preferred_source_text(activity.source_entry()) {
        let source = if app_state.anonymize_torrent_names {
            "/path/to/source".to_string()
        } else {
            sanitize_text(&source)
        };
        let source = truncate_with_ellipsis(&source, usize::from(width.saturating_sub(10)));
        lines.push(Line::from(vec![
            Span::styled(
                "  Source  ",
                ctx.apply(Style::default().fg(ctx.theme.semantic.subtext0)),
            ),
            Span::styled(
                source,
                ctx.apply(Style::default().fg(ctx.accent_sapphire())),
            ),
        ]));
    }

    lines
}

pub fn draw(f: &mut Frame, screen: &ScreenContext<'_>) {
    let app_state = screen.app.state;
    let ctx = screen.theme;
    let area = f.area();
    let popup = crate::tui::formatters::centered_rect(94, 94, area);
    let popup_layout = ratatui::layout::Layout::vertical([
        Constraint::Length(1),
        Constraint::Min(0),
        Constraint::Length(1),
    ])
    .split(popup);
    let filter_area = popup_layout[0];
    let panel_area = popup_layout[1];
    let footer_area = popup_layout[2];
    f.render_widget(Clear, popup);

    let activities = journal_activities(app_state);
    let entry_count = activities
        .iter()
        .map(|activity| activity.entries.len())
        .sum::<usize>();
    let activity_label = if activities.len() == 1 {
        "1 activity".to_string()
    } else {
        format!("{} activities", activities.len())
    };
    let count_label = if activities.len() == entry_count {
        activity_label
    } else {
        format!("{activity_label} · {entry_count} records")
    };
    let count_width = u16::try_from(count_label.chars().count()).unwrap_or(u16::MAX);
    let header_areas =
        ratatui::layout::Layout::horizontal([Constraint::Min(0), Constraint::Length(count_width)])
            .split(filter_area);

    let filter_spans = [
        JournalFilter::All,
        JournalFilter::Queue,
        JournalFilter::Commands,
        JournalFilter::Health,
    ]
    .iter()
    .enumerate()
    .flat_map(|(index, filter)| {
        let style = if *filter == app_state.ui.journal.filter {
            ctx.apply(
                Style::default()
                    .fg(ctx.state_warning())
                    .add_modifier(Modifier::BOLD | Modifier::UNDERLINED),
            )
        } else {
            ctx.apply(Style::default().fg(ctx.theme.semantic.subtext0))
        };
        let mut spans = vec![Span::styled(filter.label(), style)];
        if index < 3 {
            spans.push(Span::raw("   "));
        }
        spans
    })
    .collect::<Vec<_>>();
    f.render_widget(
        Paragraph::new(Line::from(filter_spans)).alignment(Alignment::Center),
        header_areas[0],
    );
    f.render_widget(
        Paragraph::new(count_label)
            .alignment(Alignment::Right)
            .style(ctx.apply(Style::default().fg(ctx.theme.semantic.subtext1))),
        header_areas[1],
    );

    let panel = Block::default()
        .title(Span::styled(
            " Event Journal ",
            ctx.apply(Style::default().fg(ctx.state_selected()).bold()),
        ))
        .borders(Borders::ALL)
        .border_style(ctx.apply(Style::default().fg(ctx.theme.semantic.border)))
        .padding(Padding::new(2, 2, 0, 0));
    let inner = panel.inner(panel_area);
    f.render_widget(panel, panel_area);

    if inner.width == 0 || inner.height == 0 {
        return;
    }

    let rows = ratatui::layout::Layout::vertical([
        Constraint::Length(1),
        Constraint::Min(5),
        Constraint::Length(1),
        Constraint::Length(4),
    ])
    .split(inner);

    if let Some(message) = &app_state.ui.journal.status_message {
        f.render_widget(
            Paragraph::new(sanitize_text(message))
                .style(ctx.apply(Style::default().fg(ctx.state_warning()))),
            rows[0],
        );
    }

    let columns = visible_columns(app_state.ui.journal.filter, rows[1].width);
    let body_rows = activities
        .iter()
        .map(|activity| {
            Row::new(
                columns
                    .iter()
                    .copied()
                    .map(|column| column_cell(column, activity, app_state, ctx))
                    .collect::<Vec<_>>(),
            )
            .bottom_margin(1)
        })
        .collect::<Vec<_>>();

    let constraints = columns
        .iter()
        .map(|column| column_constraint(*column, app_state.ui.journal.filter))
        .collect::<Vec<_>>();
    let header_cells = columns
        .iter()
        .map(|column| {
            Cell::from(column_header(*column, app_state.ui.journal.filter))
                .style(column_header_style(*column, ctx))
        })
        .collect::<Vec<_>>();

    let table = Table::new(body_rows, constraints)
        .header(Row::new(header_cells).bottom_margin(1))
        .column_spacing(2)
        .row_highlight_style(
            ctx.apply(
                Style::default()
                    .fg(ctx.state_warning())
                    .add_modifier(Modifier::BOLD),
            ),
        )
        .highlight_symbol("▌ ");

    let selected_index = app_state
        .ui
        .journal
        .selected_index
        .min(activities.len().saturating_sub(1));
    let mut table_state = TableState::default();
    if !activities.is_empty() {
        table_state.select(Some(selected_index));
    }
    f.render_stateful_widget(table, rows[1], &mut table_state);

    let selected_activity = activities.get(selected_index);
    f.render_widget(
        Paragraph::new(activity_detail_lines(
            selected_activity,
            app_state,
            ctx,
            rows[3].width,
        ))
        .alignment(Alignment::Left),
        rows[3],
    );

    let mut footer_spans = Vec::new();
    let mut push_action = |key: &str, label: &str, tone: ActionTone| {
        if !footer_spans.is_empty() {
            footer_spans.push(Span::raw("   "));
        }
        footer_spans.push(Span::styled(
            format!("[{key}]"),
            footer_key_style(ctx, tone),
        ));
        footer_spans.push(Span::styled(
            format!(" {label}"),
            ctx.apply(Style::default().fg(ctx.theme.semantic.subtext0)),
        ));
    };
    push_action("↑/↓", "nav", ActionTone::Navigate);
    push_action("Tab", "filter", ActionTone::Mode);
    push_action("Shift+Y", "replay", ActionTone::Replay);
    push_action("Esc", "back", ActionTone::Cancel);
    let footer_hint = Paragraph::new(Line::from(footer_spans)).alignment(Alignment::Center);
    f.render_widget(footer_hint, footer_area);
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::app::{TorrentDisplayState, TorrentMetrics};
    use crate::config::Settings;
    use crate::dht_service::{DhtStatus, DhtWaveTelemetry};
    use crate::persistence::event_journal::{EventCategory, EventJournalState, EventScope};
    use crate::theme::ThemeContext;
    use crate::tui::screen_context::ScreenContext;
    use ratatui::backend::TestBackend;
    use ratatui::crossterm::event::{KeyEvent, KeyModifiers};
    use ratatui::Terminal;
    use std::fs;
    use std::path::Path;
    use tokio::sync::mpsc;

    fn base_state() -> AppState {
        let mut state = AppState {
            mode: AppMode::Journal,
            ..Default::default()
        };
        state.event_journal_state = EventJournalState {
            next_id: 4,
            entries: vec![
                EventJournalEntry {
                    id: 1,
                    category: EventCategory::Ingest,
                    event_type: EventType::IngestAdded,
                    torrent_name: Some("Sample Alpha".to_string()),
                    ..Default::default()
                },
                EventJournalEntry {
                    id: 2,
                    category: EventCategory::Control,
                    event_type: EventType::ControlApplied,
                    torrent_name: Some("Sample Beta".to_string()),
                    ..Default::default()
                },
                EventJournalEntry {
                    id: 3,
                    category: EventCategory::DataHealth,
                    event_type: EventType::DataUnavailable,
                    torrent_name: Some("Sample Gamma".to_string()),
                    ..Default::default()
                },
            ],
        };
        state
    }

    fn handle_event(
        event: CrosstermEvent,
        app_state: &mut AppState,
        app_command_tx: &mpsc::Sender<AppCommand>,
    ) {
        handle_event_inner(event, app_state, app_command_tx, None);
    }

    fn render_journal_text(app_state: &AppState, width: u16, height: u16) -> String {
        let backend = TestBackend::new(width, height);
        let mut terminal = Terminal::new(backend).expect("create journal test terminal");
        let dht_status = DhtStatus::default();
        let dht_wave_telemetry = DhtWaveTelemetry::default();
        let settings = Settings::default();
        let theme = ThemeContext::new(app_state.theme, 0.0);

        terminal
            .draw(|frame| {
                let screen = ScreenContext::new(
                    app_state,
                    &dht_status,
                    &dht_wave_telemetry,
                    &settings,
                    &theme,
                );
                draw(frame, &screen);
            })
            .expect("draw journal test frame");

        let buffer = terminal.backend().buffer();
        (0..height)
            .map(|y| {
                (0..width)
                    .filter_map(|x| buffer.cell((x, y)).map(|cell| cell.symbol()))
                    .collect::<String>()
                    .trim_end()
                    .to_string()
            })
            .collect::<Vec<_>>()
            .join("\n")
    }

    #[test]
    fn tab_cycles_filters() {
        let mut app_state = base_state();
        let (tx, _rx) = mpsc::channel(1);

        handle_event(
            CrosstermEvent::Key(KeyEvent::new(KeyCode::Tab, KeyModifiers::NONE)),
            &mut app_state,
            &tx,
        );
        assert_eq!(app_state.ui.journal.filter, JournalFilter::Queue);

        handle_event(
            CrosstermEvent::Key(KeyEvent::new(KeyCode::Tab, KeyModifiers::NONE)),
            &mut app_state,
            &tx,
        );
        assert_eq!(app_state.ui.journal.filter, JournalFilter::Commands);
    }

    #[test]
    fn filter_selection_matches_requested_groups() {
        let mut app_state = base_state();

        app_state.ui.journal.filter = JournalFilter::Queue;
        let added = journal_activities(&app_state);
        assert_eq!(added.len(), 1);
        assert_eq!(added[0].latest().event_type, EventType::IngestAdded);

        app_state.ui.journal.filter = JournalFilter::Commands;
        let commands = journal_activities(&app_state);
        assert_eq!(commands.len(), 1);
        assert_eq!(commands[0].latest().event_type, EventType::ControlApplied);

        app_state.ui.journal.filter = JournalFilter::Health;
        let health = journal_activities(&app_state);
        assert_eq!(health.len(), 1);
        assert_eq!(health[0].latest().event_type, EventType::DataUnavailable);
    }

    #[test]
    fn correlated_ingest_stages_form_one_activity() {
        let mut app_state = base_state();
        app_state.ui.journal.filter = JournalFilter::Queue;
        app_state.event_journal_state.entries = vec![
            EventJournalEntry {
                id: 10,
                ts_iso: "2026-03-15T14:26:27Z".to_string(),
                category: EventCategory::Ingest,
                event_type: EventType::IngestQueued,
                correlation_id: Some("activity-alpha".to_string()),
                message: Some("Queued ingest item".to_string()),
                ..Default::default()
            },
            EventJournalEntry {
                id: 11,
                ts_iso: "2026-03-15T14:26:28Z".to_string(),
                category: EventCategory::Ingest,
                event_type: EventType::IngestAdded,
                correlation_id: Some("activity-alpha".to_string()),
                torrent_name: Some("Sample Delta".to_string()),
                message: Some("Added ingest item".to_string()),
                ..Default::default()
            },
        ];

        let activities = journal_activities(&app_state);

        assert_eq!(activities.len(), 1);
        assert_eq!(activities[0].entries.len(), 2);
        assert_eq!(activities[0].entries[0].id, 10);
        assert_eq!(activities[0].latest().id, 11);
        assert_eq!(
            activity_subject_label(&activities[0], false),
            "Sample Delta"
        );
    }

    #[test]
    fn reused_correlation_starts_a_new_activity() {
        let mut app_state = base_state();
        app_state.ui.journal.filter = JournalFilter::Queue;
        app_state.event_journal_state.entries = [
            (20, EventType::IngestQueued),
            (21, EventType::IngestAdded),
            (22, EventType::IngestQueued),
            (23, EventType::IngestFailed),
        ]
        .into_iter()
        .map(|(id, event_type)| EventJournalEntry {
            id,
            category: EventCategory::Ingest,
            event_type,
            correlation_id: Some("reused-path".to_string()),
            ..Default::default()
        })
        .collect();

        let activities = journal_activities(&app_state);

        assert_eq!(activities.len(), 2);
        assert_eq!(activities[0].entries.len(), 2);
        assert_eq!(activities[0].latest().id, 23);
        assert_eq!(activities[1].entries.len(), 2);
        assert_eq!(activities[1].latest().id, 21);
    }

    #[test]
    fn reused_correlation_terminal_pairs_with_newest_open_queue() {
        let mut app_state = base_state();
        app_state.ui.journal.filter = JournalFilter::Queue;
        app_state.event_journal_state.entries = [
            (24, EventType::IngestQueued),
            (25, EventType::IngestQueued),
            (26, EventType::IngestAdded),
        ]
        .into_iter()
        .map(|(id, event_type)| EventJournalEntry {
            id,
            category: EventCategory::Ingest,
            event_type,
            correlation_id: Some("reused-open-path".to_string()),
            ..Default::default()
        })
        .collect();

        let activities = journal_activities(&app_state);

        assert_eq!(activities.len(), 2);
        assert_eq!(activities[0].entries[0].id, 25);
        assert_eq!(activities[0].latest().id, 26);
        assert_eq!(activities[1].entries.len(), 1);
        assert_eq!(activities[1].latest().id, 24);
    }

    #[test]
    fn identical_correlations_do_not_cross_host_or_scope_boundaries() {
        let mut app_state = base_state();
        app_state.ui.journal.filter = JournalFilter::Queue;
        app_state.event_journal_state.entries = vec![
            EventJournalEntry {
                id: 27,
                category: EventCategory::Ingest,
                event_type: EventType::IngestQueued,
                host_id: Some("host-alpha".to_string()),
                correlation_id: Some("shared-correlation".to_string()),
                ..Default::default()
            },
            EventJournalEntry {
                id: 28,
                category: EventCategory::Ingest,
                event_type: EventType::IngestQueued,
                host_id: Some("host-beta".to_string()),
                correlation_id: Some("shared-correlation".to_string()),
                ..Default::default()
            },
            EventJournalEntry {
                id: 29,
                scope: EventScope::Shared,
                category: EventCategory::Ingest,
                event_type: EventType::IngestAdded,
                host_id: Some("host-alpha".to_string()),
                correlation_id: Some("shared-correlation".to_string()),
                ..Default::default()
            },
            EventJournalEntry {
                id: 30,
                category: EventCategory::Ingest,
                event_type: EventType::IngestAdded,
                host_id: Some("host-beta".to_string()),
                correlation_id: Some("shared-correlation".to_string()),
                ..Default::default()
            },
        ];

        let activities = journal_activities(&app_state);

        assert_eq!(activities.len(), 3);
        assert_eq!(activities[0].entries.len(), 2);
        assert_eq!(
            activities[0].entries[0].host_id.as_deref(),
            Some("host-beta")
        );
        assert_eq!(activities[0].latest().id, 30);
        assert!(activities[1..]
            .iter()
            .all(|activity| activity.entries.len() == 1));
    }

    #[test]
    fn interleaved_correlations_pair_with_their_own_terminal_stage() {
        let mut app_state = base_state();
        app_state.ui.journal.filter = JournalFilter::Queue;
        app_state.event_journal_state.entries = vec![
            EventJournalEntry {
                id: 30,
                category: EventCategory::Ingest,
                event_type: EventType::IngestQueued,
                correlation_id: Some("activity-alpha".to_string()),
                ..Default::default()
            },
            EventJournalEntry {
                id: 31,
                category: EventCategory::Ingest,
                event_type: EventType::IngestQueued,
                correlation_id: Some("activity-beta".to_string()),
                ..Default::default()
            },
            EventJournalEntry {
                id: 32,
                category: EventCategory::Ingest,
                event_type: EventType::IngestAdded,
                correlation_id: Some("activity-alpha".to_string()),
                ..Default::default()
            },
            EventJournalEntry {
                id: 33,
                category: EventCategory::Ingest,
                event_type: EventType::IngestFailed,
                correlation_id: Some("activity-beta".to_string()),
                ..Default::default()
            },
        ];

        let activities = journal_activities(&app_state);

        assert_eq!(activities.len(), 2);
        assert_eq!(activities[0].entries[0].id, 31);
        assert_eq!(activities[0].latest().id, 33);
        assert_eq!(activities[1].entries[0].id, 30);
        assert_eq!(activities[1].latest().id, 32);
    }

    #[test]
    fn navigation_clamps_to_grouped_activity_count() {
        let mut app_state = base_state();
        app_state.ui.journal.filter = JournalFilter::Queue;
        app_state.event_journal_state.entries = [
            (40, EventType::IngestQueued, Some("activity-alpha")),
            (41, EventType::IngestAdded, Some("activity-alpha")),
            (42, EventType::IngestAdded, None),
        ]
        .into_iter()
        .map(|(id, event_type, correlation_id)| EventJournalEntry {
            id,
            category: EventCategory::Ingest,
            event_type,
            correlation_id: correlation_id.map(str::to_string),
            ..Default::default()
        })
        .collect();
        let (tx, _rx) = mpsc::channel(1);

        for _ in 0..4 {
            handle_event(
                CrosstermEvent::Key(KeyEvent::new(KeyCode::Down, KeyModifiers::NONE)),
                &mut app_state,
                &tx,
            );
        }

        assert_eq!(journal_activities(&app_state).len(), 2);
        assert_eq!(app_state.ui.journal.selected_index, 1);
    }

    #[test]
    fn grouped_activity_renders_once_and_keeps_both_stage_details() {
        let mut app_state = base_state();
        app_state.ui.journal.filter = JournalFilter::Queue;
        app_state.ui.journal.selected_index = 99;
        app_state.event_journal_state.entries = vec![
            EventJournalEntry {
                id: 50,
                ts_iso: "2026-03-15T14:26:27Z".to_string(),
                category: EventCategory::Ingest,
                event_type: EventType::IngestQueued,
                correlation_id: Some("activity-render".to_string()),
                source_path: Some(Path::new("/watch/sample-delta.magnet").to_path_buf()),
                message: Some("Queued ingest item".to_string()),
                ..Default::default()
            },
            EventJournalEntry {
                id: 51,
                ts_iso: "2026-03-15T14:26:28Z".to_string(),
                category: EventCategory::Ingest,
                event_type: EventType::IngestAdded,
                correlation_id: Some("activity-render".to_string()),
                torrent_name: Some("Sample Delta".to_string()),
                source_path: Some(Path::new("/watch/sample-delta.magnet").to_path_buf()),
                message: Some("Added ingest item".to_string()),
                ..Default::default()
            },
        ];

        let rendered = render_journal_text(&app_state, 120, 40);

        assert!(rendered.contains("1 activity"), "{rendered}");
        assert!(rendered.contains("2 records"), "{rendered}");
        assert_eq!(rendered.matches("Sample Delta").count(), 1, "{rendered}");
        assert!(rendered.contains("Queued ingest item"), "{rendered}");
        assert!(rendered.contains("Added ingest item"), "{rendered}");
        assert!(
            rendered.contains("/watch/sample-delta.magnet"),
            "{rendered}"
        );
    }

    #[test]
    fn narrow_render_keeps_each_stage_and_source_on_separate_rows() {
        let mut app_state = base_state();
        app_state.ui.journal.filter = JournalFilter::Queue;
        app_state.event_journal_state.entries = vec![
            EventJournalEntry {
                id: 52,
                ts_iso: "2026-03-15T14:26:27Z".to_string(),
                category: EventCategory::Ingest,
                event_type: EventType::IngestQueued,
                correlation_id: Some("activity-narrow".to_string()),
                source_path: Some(Path::new("/watch/sample-epsilon.magnet").to_path_buf()),
                message: Some("Queue detail ".repeat(20)),
                ..Default::default()
            },
            EventJournalEntry {
                id: 53,
                ts_iso: "2026-03-15T14:26:28Z".to_string(),
                category: EventCategory::Ingest,
                event_type: EventType::IngestAdded,
                correlation_id: Some("activity-narrow".to_string()),
                torrent_name: Some("Sample Epsilon".to_string()),
                source_path: Some(Path::new("/watch/sample-epsilon.magnet").to_path_buf()),
                message: Some("Result detail ".repeat(20)),
                ..Default::default()
            },
        ];

        let rendered = render_journal_text(&app_state, 80, 24);

        assert!(rendered.contains("Queue detail"), "{rendered}");
        assert!(rendered.contains("Result detail"), "{rendered}");
        assert!(
            rendered.contains("/watch/sample-epsilon.magnet"),
            "{rendered}"
        );
    }

    #[test]
    fn compact_path_label_keeps_tail_components() {
        let label = compact_path_label(Path::new("/alpha/beta/watch_files"), 2);
        assert_eq!(label, ".../beta/watch_files");
    }

    #[test]
    fn pretty_timestamp_formats_rfc3339_values() {
        let label = pretty_timestamp("2026-03-15T14:26:28Z");
        assert!(label.contains("Mar"));
    }

    #[test]
    fn progress_label_uses_live_torrent_metrics_when_info_hash_matches() {
        let mut app_state = base_state();
        let info_hash = vec![0x11; 20];
        app_state.event_journal_state.entries[0].info_hash_hex = Some(hex::encode(&info_hash));
        app_state.torrents.insert(
            info_hash,
            TorrentDisplayState {
                latest_state: TorrentMetrics {
                    number_of_pieces_total: 10,
                    number_of_pieces_completed: 4,
                    ..Default::default()
                },
                ..Default::default()
            },
        );

        assert_eq!(
            progress_label(&app_state.event_journal_state.entries[0], &app_state),
            "40%"
        );
    }

    #[test]
    fn anonymized_journal_hides_torrent_names_and_paths() {
        let entry = EventJournalEntry {
            torrent_name: Some("Sample Alpha".to_string()),
            source_path: Some(Path::new("/alpha/beta/watch_files/sample.torrent").to_path_buf()),
            message: Some(
                "Added Sample Alpha from /alpha/beta/watch_files/sample.torrent".to_string(),
            ),
            ..Default::default()
        };

        assert_eq!(torrent_label(&entry, true), "Torrent");
        assert_eq!(source_label(&entry, true), "/path/to/source");

        let details = detail_text(Some(&entry), true);
        assert!(!details.contains("Sample Alpha"));
        assert!(!details.contains("/alpha/beta/watch_files/sample.torrent"));
        assert!(details.contains("Torrent"));
        assert!(details.contains("/path/to/source"));
    }

    #[test]
    fn command_filter_uses_action_label_and_reduced_columns() {
        let entry = EventJournalEntry {
            details: EventDetails::Control {
                origin: crate::persistence::event_journal::ControlOrigin::CliOnline,
                action: "pause".to_string(),
                target_info_hash_hex: None,
                file_index: None,
                file_path: None,
                priority: None,
            },
            ..Default::default()
        };

        assert_eq!(command_action_label(&entry), "pause");
        assert_eq!(columns_for_filter(JournalFilter::Commands).len(), 4);
        assert_eq!(
            column_header(
                columns_for_filter(JournalFilter::Commands)[1],
                JournalFilter::Commands
            ),
            "Status"
        );
        assert_eq!(
            column_header(
                columns_for_filter(JournalFilter::Commands)[2],
                JournalFilter::Commands
            ),
            "Action"
        );
        assert_eq!(
            column_header(
                columns_for_filter(JournalFilter::Commands)[3],
                JournalFilter::Commands
            ),
            "Source"
        );
        assert_eq!(command_action_label(&entry), "pause");
    }

    #[test]
    fn health_filter_hides_source_column() {
        let columns = columns_for_filter(JournalFilter::Health);
        assert_eq!(columns.len(), 3);
        assert!(columns
            .iter()
            .all(|column| !matches!(column, JournalColumn::Source)));
    }

    #[test]
    fn shift_y_replays_selected_magnet_source() {
        let mut app_state = base_state();
        app_state.ui.journal.filter = JournalFilter::Queue;
        app_state.ui.journal.selected_index = 99;
        let replay_path = std::env::temp_dir().join(format!(
            "superseedr-journal-replay-{}.magnet",
            std::process::id()
        ));
        fs::write(
            &replay_path,
            "magnet:?xt=urn:btih:4444444444444444444444444444444444444444",
        )
        .expect("write replay file");
        app_state.event_journal_state.entries.insert(
            0,
            EventJournalEntry {
                id: 0,
                category: EventCategory::Ingest,
                event_type: EventType::IngestQueued,
                correlation_id: Some("activity-replay".to_string()),
                source_path: Some(replay_path.clone()),
                ..Default::default()
            },
        );
        app_state.event_journal_state.entries[1].correlation_id =
            Some("activity-replay".to_string());
        app_state.event_journal_state.entries[1].source_path = Some(replay_path.clone());
        let (tx, mut rx) = mpsc::channel(1);

        handle_event(
            CrosstermEvent::Key(KeyEvent::new(KeyCode::Char('Y'), KeyModifiers::SHIFT)),
            &mut app_state,
            &tx,
        );

        match rx.try_recv() {
            Ok(AppCommand::AddMagnetFromFile(path)) => assert_eq!(path, replay_path),
            Ok(_) => panic!("expected replayed magnet command"),
            Err(error) => panic!("expected replay command, got {error:?}"),
        }

        fs::remove_file(&replay_path).ok();
    }

    #[test]
    fn shift_y_reports_missing_replay_source() {
        let mut app_state = base_state();
        let (tx, _rx) = mpsc::channel(1);

        handle_event(
            CrosstermEvent::Key(KeyEvent::new(KeyCode::Char('Y'), KeyModifiers::SHIFT)),
            &mut app_state,
            &tx,
        );

        assert_eq!(
            app_state.ui.journal.status_message.as_deref(),
            Some("Selected entry has no replayable source file")
        );
    }
}
