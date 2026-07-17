// SPDX-FileCopyrightText: 2026 The superseedr Contributors
// SPDX-License-Identifier: GPL-3.0-or-later

use crate::app::{AppCommand, AppMode, AppState, JournalFilter, SearchMode};
use crate::persistence::event_journal::{
    EventCategory, EventDetails, EventJournalEntry, EventType,
};
use crate::theme::ThemeContext;
use crate::tui::action_style::{footer_key_style, ActionTone};
use crate::tui::app_command::spawn_app_command_sender;
use crate::tui::formatters::{centered_rect, sanitize_text, truncate_with_ellipsis};
use crate::tui::screen_context::ScreenContext;
use crate::tui::screens::input_panel::draw_prompt_panel;
use chrono::{DateTime, Local, Utc};
use fuzzy_matcher::skim::SkimMatcherV2;
use fuzzy_matcher::FuzzyMatcher;
use ratatui::crossterm::event::{
    Event as CrosstermEvent, KeyCode, KeyEvent, KeyEventKind, KeyModifiers,
};
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
    MovePageUp,
    MovePageDown,
    ReplaySelected,
    SearchStart,
    SearchInsert(char),
    SearchBackspace,
    SearchClear,
    SearchCommit,
    SearchCancel,
    ToggleSearchMode,
}

fn map_key_to_journal_action(key: KeyEvent, search_panel_active: bool) -> Option<JournalAction> {
    if !matches!(key.kind, KeyEventKind::Press | KeyEventKind::Repeat) {
        return None;
    }

    let has_ctrl = key.modifiers.contains(KeyModifiers::CONTROL);
    let has_alt = key.modifiers.contains(KeyModifiers::ALT);

    if search_panel_active && matches!(key.code, KeyCode::Tab) {
        return Some(JournalAction::ToggleSearchMode);
    }

    if search_panel_active {
        return match key.code {
            KeyCode::Esc => Some(JournalAction::SearchCancel),
            KeyCode::Enter => Some(JournalAction::SearchCommit),
            KeyCode::Backspace => Some(JournalAction::SearchBackspace),
            KeyCode::Char('u') if has_ctrl => Some(JournalAction::SearchClear),
            KeyCode::Up => Some(JournalAction::MoveUp),
            KeyCode::Down => Some(JournalAction::MoveDown),
            KeyCode::PageUp => Some(JournalAction::MovePageUp),
            KeyCode::PageDown => Some(JournalAction::MovePageDown),
            KeyCode::Char(c) if !has_ctrl && !has_alt => Some(JournalAction::SearchInsert(c)),
            _ => None,
        };
    }

    match key.code {
        KeyCode::Esc | KeyCode::Char('q') => Some(JournalAction::ToNormal),
        KeyCode::Tab => Some(JournalAction::FilterNext),
        KeyCode::BackTab => Some(JournalAction::FilterPrev),
        KeyCode::Up | KeyCode::Char('k') => Some(JournalAction::MoveUp),
        KeyCode::Down | KeyCode::Char('j') => Some(JournalAction::MoveDown),
        KeyCode::PageUp => Some(JournalAction::MovePageUp),
        KeyCode::PageDown => Some(JournalAction::MovePageDown),
        KeyCode::Char('Y') => Some(JournalAction::ReplaySelected),
        KeyCode::Char('/') => Some(JournalAction::SearchStart),
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

    let search_panel_active =
        app_state.ui.journal.is_searching || !app_state.ui.journal.search_query.is_empty();
    let Some(action) = map_key_to_journal_action(key, search_panel_active) else {
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
        JournalAction::MovePageUp => {
            let page_rows = journal_page_rows(app_state);
            app_state.ui.journal.selected_index = app_state
                .ui
                .journal
                .selected_index
                .saturating_sub(page_rows);
        }
        JournalAction::MovePageDown => {
            let len = journal_activities(app_state).len();
            if len > 0 {
                let page_rows = journal_page_rows(app_state);
                app_state.ui.journal.selected_index = app_state
                    .ui
                    .journal
                    .selected_index
                    .saturating_add(page_rows)
                    .min(len - 1);
            }
        }
        JournalAction::ReplaySelected => {
            replay_selected_entry(app_state, app_command_tx, shutdown_tx)
        }
        JournalAction::SearchStart => {
            app_state.ui.journal.is_searching = true;
            app_state.ui.journal.search_mode = SearchMode::Regex;
            app_state.ui.journal.selected_index = 0;
        }
        JournalAction::SearchInsert(c) => {
            app_state.ui.journal.search_query.push(c);
            app_state.ui.journal.selected_index = 0;
        }
        JournalAction::SearchBackspace => {
            app_state.ui.journal.search_query.pop();
            app_state.ui.journal.selected_index = 0;
        }
        JournalAction::SearchClear => {
            app_state.ui.journal.search_query.clear();
            app_state.ui.journal.selected_index = 0;
        }
        JournalAction::SearchCommit => {
            app_state.ui.journal.is_searching = false;
        }
        JournalAction::SearchCancel => {
            app_state.ui.journal.is_searching = false;
            app_state.ui.journal.search_query.clear();
            app_state.ui.journal.selected_index = 0;
        }
        JournalAction::ToggleSearchMode => {
            app_state.ui.journal.search_mode = match app_state.ui.journal.search_mode {
                SearchMode::Fuzzy => SearchMode::Regex,
                SearchMode::Regex => SearchMode::Fuzzy,
            };
            app_state.ui.journal.selected_index = 0;
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
}

impl<'a> JournalActivity<'a> {
    fn new(entry: &'a EventJournalEntry) -> Self {
        Self {
            entries: vec![entry],
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
    let mut pending_terminal_activities = HashMap::<ActivityKey<'_>, Vec<usize>>::new();

    for entry in app_state.event_journal_state.entries.iter().rev() {
        if !entry_matches_filter(entry, app_state.ui.journal.filter) {
            continue;
        }

        match (activity_phase(entry), activity_key(entry)) {
            (ActivityPhase::Terminal, Some(key)) => {
                let activity_index = activities.len();
                activities.push(JournalActivity::new(entry));
                pending_terminal_activities
                    .entry(key)
                    .or_default()
                    .push(activity_index);
            }
            (ActivityPhase::Queued, Some(key)) => {
                let terminal_activity = pending_terminal_activities
                    .get_mut(&key)
                    .and_then(|activity_indices| activity_indices.pop());
                if let Some(activity_index) = terminal_activity {
                    activities[activity_index].entries.insert(0, entry);
                } else {
                    activities.push(JournalActivity::new(entry));
                }
            }
            _ => activities.push(JournalActivity::new(entry)),
        }
    }

    let query = app_state.ui.journal.search_query.trim();
    if !query.is_empty() {
        match app_state.ui.journal.search_mode {
            SearchMode::Fuzzy => {
                let matcher = SkimMatcherV2::default();
                let query = query.to_lowercase();
                activities.retain(|activity| {
                    matcher
                        .fuzzy_match(&activity_search_haystack(activity).to_lowercase(), &query)
                        .is_some()
                });
            }
            SearchMode::Regex => {
                let regex = regex::RegexBuilder::new(query)
                    .case_insensitive(true)
                    .build();
                if let Ok(regex) = regex {
                    activities
                        .retain(|activity| regex.is_match(&activity_search_haystack(activity)));
                } else {
                    activities.clear();
                }
            }
        }
    }
    activities
}

fn activity_search_haystack(activity: &JournalActivity<'_>) -> String {
    activity
        .entries
        .iter()
        .flat_map(|entry| {
            [
                event_type_label(entry).to_string(),
                command_action_label(entry),
                torrent_label(entry, false),
                source_label(entry, false),
                detail_text(Some(entry), false),
                entry.host_id.clone().unwrap_or_default(),
                entry.info_hash_hex.clone().unwrap_or_default(),
            ]
        })
        .collect::<Vec<_>>()
        .join(" ")
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

fn time_since_label(ts_iso: &str, now: &DateTime<Utc>) -> String {
    let Ok(timestamp) = DateTime::parse_from_rfc3339(ts_iso) else {
        return "-".to_string();
    };
    let elapsed_seconds = now
        .signed_duration_since(timestamp.with_timezone(&Utc))
        .num_seconds();
    if elapsed_seconds <= 0 {
        return "now".to_string();
    }

    let days = elapsed_seconds / 86_400;
    let hours = (elapsed_seconds % 86_400) / 3_600;
    let minutes = (elapsed_seconds % 3_600) / 60;
    let seconds = elapsed_seconds % 60;

    if days > 0 {
        format!("{days}d {hours}h ago")
    } else if hours > 0 {
        let minute_unit = if minutes == 1 { "min" } else { "mins" };
        format!("{hours}h {minutes}{minute_unit} ago")
    } else if minutes > 0 {
        let minute_unit = if minutes == 1 { "min" } else { "mins" };
        format!("{minutes}{minute_unit} {seconds}s ago")
    } else {
        format!("{seconds}s ago")
    }
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
    TimeSince,
    Status,
    Subject,
}

fn columns_for_filter(_filter: JournalFilter) -> Vec<JournalColumn> {
    vec![
        JournalColumn::Time,
        JournalColumn::TimeSince,
        JournalColumn::Status,
        JournalColumn::Subject,
    ]
}

fn journal_table_window(len: usize, selected_index: usize, table_height: u16) -> (usize, usize) {
    let capacity = usize::from(table_height.saturating_sub(1).max(1));
    let end = if selected_index < capacity {
        capacity.min(len)
    } else {
        selected_index.saturating_add(1).min(len)
    };
    (end.saturating_sub(capacity), end)
}

fn journal_detail_height(inner_height: u16) -> u16 {
    u16::from(inner_height >= 5)
}

fn journal_page_rows(app_state: &AppState) -> usize {
    let screen_area = app_state.screen_area;
    if screen_area.width == 0 || screen_area.height == 0 {
        return 1;
    }

    let area = centered_rect(88, 94, screen_area);
    let search_panel_active =
        app_state.ui.journal.is_searching || !app_state.ui.journal.search_query.is_empty();
    let journal_area = if search_panel_active && area.height >= 7 {
        ratatui::layout::Layout::vertical([Constraint::Length(3), Constraint::Min(1)]).split(area)
            [1]
    } else {
        area
    };
    let panel_area = ratatui::layout::Layout::vertical([
        Constraint::Length(1),
        Constraint::Min(1),
        Constraint::Length(1),
    ])
    .split(journal_area)[1];
    let inner = Block::default()
        .borders(Borders::ALL)
        .padding(Padding::new(2, 2, 0, 0))
        .inner(panel_area);
    let detail_height = journal_detail_height(inner.height);
    let spacer_height = u16::from(detail_height > 0);
    let table_height = ratatui::layout::Layout::vertical([
        Constraint::Min(3),
        Constraint::Length(spacer_height),
        Constraint::Length(detail_height),
    ])
    .split(inner)[0]
        .height;

    usize::from(table_height.saturating_sub(1).max(1))
}

fn column_header(column: JournalColumn, filter: JournalFilter) -> &'static str {
    match (column, filter) {
        (JournalColumn::Subject, JournalFilter::Commands) => "Action",
        (JournalColumn::Subject, _) => "Torrent",
        (JournalColumn::Time, _) => "Time",
        (JournalColumn::TimeSince, _) => "Time Since",
        (JournalColumn::Status, _) => "Status",
    }
}

fn column_header_style(column: JournalColumn, filter: JournalFilter, ctx: &ThemeContext) -> Style {
    let color = match column {
        JournalColumn::Time => ctx.accent_peach(),
        JournalColumn::TimeSince => ctx.accent_teal(),
        JournalColumn::Status => ctx.state_warning(),
        JournalColumn::Subject if matches!(filter, JournalFilter::Commands) => {
            ctx.accent_sapphire()
        }
        JournalColumn::Subject => ctx.accent_sky(),
    };
    ctx.apply(Style::default().fg(color).bold())
}

fn column_constraint(column: JournalColumn, filter: JournalFilter) -> Constraint {
    match (filter, column) {
        (_, JournalColumn::Time) => Constraint::Length(13),
        (_, JournalColumn::TimeSince) => Constraint::Length(15),
        (_, JournalColumn::Status) => Constraint::Length(22),
        (_, JournalColumn::Subject) => Constraint::Fill(1),
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

fn activity_status_spans(activity: &JournalActivity<'_>, ctx: &ThemeContext) -> Vec<Span<'static>> {
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
    spans
}

fn activity_status_cell(activity: &JournalActivity<'_>, ctx: &ThemeContext) -> Cell<'static> {
    Cell::from(Line::from(activity_status_spans(activity, ctx)))
}

fn activity_subject_label(activity: &JournalActivity<'_>, anonymize: bool) -> String {
    let entry = activity.latest();
    if matches!(entry.category, EventCategory::Control) {
        return command_action_label(entry);
    }

    if anonymize {
        return "Torrent".to_string();
    }

    activity
        .entries
        .iter()
        .rev()
        .filter_map(|entry| entry.torrent_name.as_deref())
        .find(|name| !name.trim().is_empty())
        .map(sanitize_text)
        .or_else(|| {
            let source = activity.source_entry();
            source
                .source_path
                .as_deref()
                .or(source.source_watch_folder.as_deref())
                .and_then(Path::file_name)
                .map(|name| sanitize_text(&name.to_string_lossy()))
        })
        .filter(|label| !label.trim().is_empty())
        .unwrap_or_else(|| "Pending metadata".to_string())
}

fn column_cell(
    column: JournalColumn,
    activity: &JournalActivity<'_>,
    app_state: &AppState,
    now: &DateTime<Utc>,
    ctx: &ThemeContext,
) -> Cell<'static> {
    match column {
        JournalColumn::Time => Cell::from(pretty_timestamp(&activity.latest().ts_iso))
            .style(ctx.apply(Style::default().fg(ctx.theme.semantic.subtext0))),
        JournalColumn::TimeSince => Cell::from(time_since_label(&activity.latest().ts_iso, now))
            .style(ctx.apply(Style::default().fg(ctx.theme.semantic.subtext1))),
        JournalColumn::Status => activity_status_cell(activity, ctx),
        JournalColumn::Subject => Cell::from(activity_subject_label(
            activity,
            app_state.anonymize_torrent_names,
        ))
        .style(ctx.apply(Style::default().fg(ctx.theme.semantic.text))),
    }
}

fn activity_detail_line(
    activity: Option<&JournalActivity<'_>>,
    app_state: &AppState,
    ctx: &ThemeContext,
    width: u16,
) -> Line<'static> {
    let Some(activity) = activity else {
        return Line::from(Span::styled(
            "No journal activities yet.",
            ctx.apply(Style::default().fg(ctx.theme.semantic.subtext1)),
        ));
    };

    let latest = activity.latest();
    let timestamp = detailed_timestamp(&latest.ts_iso);
    let status_width = activity
        .entries
        .iter()
        .enumerate()
        .map(|(index, entry)| {
            usize::from(index > 0) * 3 + 2 + event_type_label(entry).chars().count()
        })
        .sum::<usize>();
    let timestamp_width = timestamp.chars().count() + 4;
    let source = (width >= 84)
        .then(|| preferred_source_text(activity.source_entry()))
        .flatten()
        .map(|source| {
            if app_state.anonymize_torrent_names {
                "/path/to/source".to_string()
            } else {
                sanitize_text(&source)
            }
        })
        .map(|source| truncate_with_ellipsis(&source, usize::from(width / 5).max(16)));
    let source_width = source
        .as_ref()
        .map(|source| 10 + source.chars().count())
        .unwrap_or(0);
    let message_width = usize::from(width)
        .saturating_sub(status_width)
        .saturating_sub(timestamp_width)
        .saturating_sub(source_width);
    let message = truncate_with_ellipsis(
        &detail_text(Some(latest), app_state.anonymize_torrent_names),
        message_width,
    );

    let mut spans = activity_status_spans(activity, ctx);
    spans.push(Span::styled(
        format!("  {timestamp}  "),
        ctx.apply(Style::default().fg(ctx.theme.semantic.subtext0)),
    ));
    spans.push(Span::styled(
        message,
        ctx.apply(Style::default().fg(ctx.theme.semantic.text)),
    ));
    if let Some(source) = source {
        spans.push(Span::styled(
            "  Source  ",
            ctx.apply(Style::default().fg(ctx.theme.semantic.subtext0)),
        ));
        spans.push(Span::styled(
            source,
            ctx.apply(Style::default().fg(ctx.accent_sapphire())),
        ));
    }

    Line::from(spans)
}

pub fn draw(f: &mut Frame, screen: &ScreenContext<'_>) {
    let app_state = screen.app.state;
    let ctx = screen.theme;
    let activities = journal_activities(app_state);
    let search_panel_active =
        app_state.ui.journal.is_searching || !app_state.ui.journal.search_query.is_empty();
    let area = centered_rect(88, 94, f.area());
    f.render_widget(Clear, area);

    let (search_area, journal_area) = if search_panel_active && area.height >= 7 {
        let chunks = ratatui::layout::Layout::vertical([Constraint::Length(3), Constraint::Min(1)])
            .split(area);
        (Some(chunks[0]), chunks[1])
    } else {
        (None, area)
    };
    if let Some(search_area) = search_area {
        draw_journal_search_panel(f, search_area, app_state, activities.len(), ctx);
    }

    let layout = ratatui::layout::Layout::vertical([
        Constraint::Length(1),
        Constraint::Min(1),
        Constraint::Length(1),
    ])
    .split(journal_area);
    let header_area = layout[0];
    let panel_area = layout[1];
    let footer_area = layout[2];

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
    let filter_spans = [
        JournalFilter::All,
        JournalFilter::Queue,
        JournalFilter::Commands,
        JournalFilter::Health,
    ]
    .iter()
    .enumerate()
    .flat_map(|(index, filter)| {
        let color = journal_filter_color(*filter, ctx);
        let style = if *filter == app_state.ui.journal.filter {
            ctx.apply(Style::default().fg(color).add_modifier(Modifier::BOLD))
        } else {
            ctx.apply(Style::default().fg(color))
        };
        let mut spans = vec![Span::styled(filter.label(), style)];
        if index < 3 {
            spans.push(Span::raw("  "));
        }
        spans
    })
    .collect::<Vec<_>>();

    f.render_widget(
        Paragraph::new(Line::from(filter_spans)).alignment(Alignment::Center),
        header_area,
    );
    f.render_widget(
        Paragraph::new(count_label)
            .alignment(Alignment::Right)
            .style(ctx.apply(Style::default().fg(ctx.theme.semantic.subtext1))),
        header_area,
    );

    let mut panel = Block::default()
        .borders(Borders::ALL)
        .border_style(ctx.apply(Style::default().fg(ctx.theme.semantic.border)))
        .padding(Padding::new(2, 2, 0, 0));
    if let Some(message) = &app_state.ui.journal.status_message {
        panel = panel.title_bottom(Span::styled(
            format!(" {} ", sanitize_text(message)),
            ctx.apply(Style::default().fg(ctx.state_warning()).bold()),
        ));
    }
    let inner = panel.inner(panel_area);
    f.render_widget(panel, panel_area);

    if inner.width == 0 || inner.height == 0 {
        return;
    }

    let selected_index = app_state
        .ui
        .journal
        .selected_index
        .min(activities.len().saturating_sub(1));
    let selected_activity = activities.get(selected_index);
    let detail_line = activity_detail_line(selected_activity, app_state, ctx, inner.width);
    let detail_height = journal_detail_height(inner.height);
    let spacer_height = u16::from(detail_height > 0);
    let rows = ratatui::layout::Layout::vertical([
        Constraint::Min(3),
        Constraint::Length(spacer_height),
        Constraint::Length(detail_height),
    ])
    .split(inner);

    let columns = columns_for_filter(app_state.ui.journal.filter);
    let (window_start, window_end) =
        journal_table_window(activities.len(), selected_index, rows[0].height);
    let now = Utc::now();
    let body_rows = activities[window_start..window_end]
        .iter()
        .map(|activity| {
            Row::new(
                columns
                    .iter()
                    .copied()
                    .map(|column| column_cell(column, activity, app_state, &now, ctx))
                    .collect::<Vec<_>>(),
            )
        })
        .collect::<Vec<_>>();

    let constraints = columns
        .iter()
        .map(|column| column_constraint(*column, app_state.ui.journal.filter))
        .collect::<Vec<_>>();
    let header_cells = columns
        .iter()
        .map(|column| {
            Cell::from(column_header(*column, app_state.ui.journal.filter)).style(
                column_header_style(*column, app_state.ui.journal.filter, ctx),
            )
        })
        .collect::<Vec<_>>();

    let table = Table::new(body_rows, constraints)
        .header(Row::new(header_cells))
        .column_spacing(1)
        .row_highlight_style(
            ctx.apply(
                Style::default()
                    .fg(ctx.state_warning())
                    .add_modifier(Modifier::BOLD),
            ),
        );

    let mut table_state = TableState::default();
    if !activities.is_empty() {
        table_state.select(Some(selected_index.saturating_sub(window_start)));
    }
    f.render_stateful_widget(table, rows[0], &mut table_state);

    if detail_height > 0 {
        f.render_widget(
            Paragraph::new(detail_line).alignment(Alignment::Left),
            rows[2],
        );
    }

    let mut footer_spans = Vec::new();
    let mut push_action = |key: &str, label: &str, tone: ActionTone| {
        if !footer_spans.is_empty() {
            footer_spans.push(Span::styled(
                " | ",
                ctx.apply(Style::default().fg(ctx.theme.semantic.overlay0)),
            ));
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
    if search_panel_active {
        push_action("type", "query", ActionTone::Edit);
        push_action("Tab", "mode", ActionTone::Mode);
        push_action("Enter", "keep", ActionTone::Confirm);
        push_action("Esc", "clear", ActionTone::Cancel);
        push_action("↑/↓ PgUp/PgDn", "nav", ActionTone::Navigate);
    } else {
        push_action("Esc", "back", ActionTone::Cancel);
        push_action("Tab", "filter", ActionTone::Mode);
        push_action("/", "search", ActionTone::Search);
        push_action("↑/↓ PgUp/PgDn", "nav", ActionTone::Navigate);
        push_action("Shift+Y", "replay", ActionTone::Replay);
    }
    let footer_hint = Paragraph::new(Line::from(footer_spans)).alignment(Alignment::Center);
    f.render_widget(footer_hint, footer_area);
}

fn journal_filter_color(filter: JournalFilter, ctx: &ThemeContext) -> ratatui::style::Color {
    match filter {
        JournalFilter::All => ctx.state_selected(),
        JournalFilter::Queue => ctx.state_warning(),
        JournalFilter::Commands => ctx.accent_sapphire(),
        JournalFilter::Health => ctx.state_success(),
    }
}

fn draw_journal_search_panel(
    f: &mut Frame,
    area: ratatui::prelude::Rect,
    app_state: &AppState,
    visible_count: usize,
    ctx: &ThemeContext,
) {
    let mut trailing_spans = journal_search_mode_spans(app_state, ctx);
    trailing_spans.push(Span::styled(
        format!("  {visible_count} matches"),
        ctx.apply(Style::default().fg(ctx.theme.semantic.subtext0)),
    ));
    draw_prompt_panel(
        f,
        area,
        " Journal Search ".to_string(),
        sanitize_text(&app_state.ui.journal.search_query),
        trailing_spans,
        ctx,
    );
}

fn journal_search_mode_spans(app_state: &AppState, ctx: &ThemeContext) -> Vec<Span<'static>> {
    let (fuzzy_style, regex_style) = match app_state.ui.journal.search_mode {
        SearchMode::Fuzzy => (
            ctx.apply(Style::default().fg(ctx.state_selected()).bold()),
            ctx.apply(Style::default().fg(ctx.theme.semantic.overlay0)),
        ),
        SearchMode::Regex => (
            ctx.apply(Style::default().fg(ctx.theme.semantic.overlay0)),
            ctx.apply(Style::default().fg(ctx.state_selected()).bold()),
        ),
    };
    vec![
        Span::raw("  "),
        Span::styled("Fuzzy", fuzzy_style),
        Span::raw(" / "),
        Span::styled("Regex", regex_style),
    ]
}

#[cfg(test)]
mod tests {
    use super::*;
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
    fn journal_search_filters_activities_and_toggles_mode() {
        let mut app_state = base_state();
        let (tx, _rx) = mpsc::channel(1);

        handle_event(
            CrosstermEvent::Key(KeyEvent::new(KeyCode::Char('/'), KeyModifiers::NONE)),
            &mut app_state,
            &tx,
        );
        for character in "sample beta".chars() {
            handle_event(
                CrosstermEvent::Key(KeyEvent::new(KeyCode::Char(character), KeyModifiers::NONE)),
                &mut app_state,
                &tx,
            );
        }

        assert!(app_state.ui.journal.is_searching);
        assert_eq!(app_state.ui.journal.search_mode, SearchMode::Regex);
        assert_eq!(journal_activities(&app_state).len(), 1);
        assert_eq!(journal_activities(&app_state)[0].latest().id, 2);

        handle_event(
            CrosstermEvent::Key(KeyEvent::new(KeyCode::Tab, KeyModifiers::NONE)),
            &mut app_state,
            &tx,
        );
        assert_eq!(app_state.ui.journal.search_mode, SearchMode::Fuzzy);
    }

    #[test]
    fn journal_escape_clears_search_before_closing() {
        let mut app_state = base_state();
        let (tx, _rx) = mpsc::channel(1);
        app_state.ui.journal.is_searching = true;
        app_state.ui.journal.search_query = "sample".to_string();

        handle_event(
            CrosstermEvent::Key(KeyEvent::new(KeyCode::Esc, KeyModifiers::NONE)),
            &mut app_state,
            &tx,
        );

        assert!(matches!(app_state.mode, AppMode::Journal));
        assert!(!app_state.ui.journal.is_searching);
        assert!(app_state.ui.journal.search_query.is_empty());
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
    fn queued_activity_without_metadata_uses_source_filename_as_subject() {
        let mut app_state = base_state();
        app_state.ui.journal.filter = JournalFilter::Queue;
        app_state.event_journal_state.entries = vec![EventJournalEntry {
            id: 12,
            category: EventCategory::Ingest,
            event_type: EventType::IngestQueued,
            source_path: Some(Path::new("/watch/pending-item.magnet").to_path_buf()),
            correlation_id: Some("pending-source".to_string()),
            ..Default::default()
        }];

        let activities = journal_activities(&app_state);

        assert_eq!(activities.len(), 1);
        assert_eq!(
            activity_subject_label(&activities[0], false),
            "pending-item.magnet"
        );
    }

    #[test]
    fn activity_without_name_or_source_uses_pending_metadata_label() {
        let mut app_state = base_state();
        app_state.ui.journal.filter = JournalFilter::Queue;
        app_state.event_journal_state.entries = vec![EventJournalEntry {
            id: 13,
            category: EventCategory::Ingest,
            event_type: EventType::IngestQueued,
            ..Default::default()
        }];

        let activities = journal_activities(&app_state);

        assert_eq!(activities.len(), 1);
        assert_eq!(
            activity_subject_label(&activities[0], false),
            "Pending metadata"
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
    fn table_window_builds_only_the_visible_rows_around_selection() {
        assert_eq!(journal_table_window(100, 0, 6), (0, 5));
        assert_eq!(journal_table_window(100, 4, 6), (0, 5));
        assert_eq!(journal_table_window(100, 5, 6), (1, 6));
        assert_eq!(journal_table_window(100, 73, 6), (69, 74));
        assert_eq!(journal_table_window(3, 2, 6), (0, 3));
    }

    #[test]
    fn detail_region_keeps_a_stable_height() {
        assert_eq!(journal_detail_height(30), 1);
        assert_eq!(journal_detail_height(8), 1);
        assert_eq!(journal_detail_height(5), 1);
        assert_eq!(journal_detail_height(4), 0);
    }

    #[test]
    fn page_keys_move_by_the_visible_table_capacity() {
        let mut app_state = base_state();
        app_state.screen_area = ratatui::prelude::Rect::new(0, 0, 120, 30);
        app_state.event_journal_state.entries = (0..40)
            .map(|id| EventJournalEntry {
                id,
                category: EventCategory::Ingest,
                event_type: EventType::IngestAdded,
                torrent_name: Some(format!("Sample Item {id}")),
                ..Default::default()
            })
            .collect();
        let page_rows = journal_page_rows(&app_state);
        let (tx, _rx) = mpsc::channel(1);

        handle_event(
            CrosstermEvent::Key(KeyEvent::new(KeyCode::PageDown, KeyModifiers::NONE)),
            &mut app_state,
            &tx,
        );
        assert_eq!(app_state.ui.journal.selected_index, page_rows);

        handle_event(
            CrosstermEvent::Key(KeyEvent::new(KeyCode::PageUp, KeyModifiers::NONE)),
            &mut app_state,
            &tx,
        );
        assert_eq!(app_state.ui.journal.selected_index, 0);
    }

    #[test]
    fn grouped_activity_uses_one_line_with_latest_detail() {
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
        assert!(rendered.contains("Added ingest item"), "{rendered}");
        assert!(!rendered.contains("Queued ingest item"), "{rendered}");
        assert!(rendered.contains("/watch/sample"), "{rendered}");
    }

    #[test]
    fn narrow_render_keeps_latest_detail_on_one_line() {
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

        assert!(rendered.contains("Result detail"), "{rendered}");
        assert!(!rendered.contains("Queue detail"), "{rendered}");
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
    fn time_since_label_uses_two_live_time_units() {
        let now = DateTime::parse_from_rfc3339("2026-03-15T15:00:00Z")
            .expect("valid now timestamp")
            .with_timezone(&Utc);

        assert_eq!(time_since_label("2026-03-15T15:00:00Z", &now), "now");
        assert_eq!(time_since_label("2026-03-15T14:59:40Z", &now), "20s ago");
        assert_eq!(
            time_since_label("2026-03-15T14:29:40Z", &now),
            "30mins 20s ago"
        );
        assert_eq!(
            time_since_label("2026-03-15T12:55:00Z", &now),
            "2h 5mins ago"
        );
        assert_eq!(time_since_label("invalid", &now), "-");
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
            "Time Since"
        );
        assert_eq!(
            column_header(
                columns_for_filter(JournalFilter::Commands)[2],
                JournalFilter::Commands
            ),
            "Status"
        );
        assert_eq!(
            column_header(
                columns_for_filter(JournalFilter::Commands)[3],
                JournalFilter::Commands
            ),
            "Action"
        );
        assert_eq!(command_action_label(&entry), "pause");
    }

    #[test]
    fn every_filter_uses_the_same_four_core_columns() {
        for filter in [
            JournalFilter::All,
            JournalFilter::Queue,
            JournalFilter::Commands,
            JournalFilter::Health,
        ] {
            assert_eq!(columns_for_filter(filter).len(), 4);
        }
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
