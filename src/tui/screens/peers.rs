// SPDX-FileCopyrightText: 2026 The superseedr Contributors
// SPDX-License-Identifier: GPL-3.0-or-later

use crate::app::{AppMode, AppState, PeerManagementFilter, SearchMode};
use crate::config::SortDirection;
use crate::peer_manager::{PeerManagerTrackedPeer, PeerRestriction, PeerRestrictionReason};
use crate::theme::ThemeContext;
use crate::tui::action_style::{footer_key_style, ActionTone};
use crate::tui::formatters::{anonymize_preserving_shape, format_bytes, sanitize_text};
use crate::tui::layout::common::{compute_smart_table_layout, SmartCol};
use crate::tui::layout::peers::{calculate_peer_screen_layout, PeerBodyLayout};
use crate::tui::screen_context::ScreenContext;
use crate::tui::screens::input_panel::draw_prompt_panel;
use fuzzy_matcher::skim::SkimMatcherV2;
use fuzzy_matcher::FuzzyMatcher;
use ratatui::crossterm::event::{Event as CrosstermEvent, KeyCode, KeyEventKind};
use ratatui::layout::{Alignment, Constraint, Rect};
use ratatui::prelude::{Color, Frame, Line, Modifier, Span, Style};
use ratatui::widgets::{
    Block, Borders, Cell, Clear, Padding, Paragraph, Row, Table, TableState, Wrap,
};
use regex::RegexBuilder;
use std::cmp::Ordering;
use std::collections::{BTreeMap, BTreeSet};
use std::net::IpAddr;
use std::time::{Duration, SystemTime};

#[cfg(test)]
std::thread_local! {
    static PEER_ROW_BUILD_COUNT: std::cell::Cell<usize> = const { std::cell::Cell::new(0) };
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum PeerManagementAction {
    ToNormal,
    MoveUp,
    MoveDown,
    MovePageUp,
    MovePageDown,
    MoveFirst,
    MoveLast,
    MoveColumnLeft,
    MoveColumnRight,
    SortBySelectedColumn,
    FilterNext,
    FilterPrev,
    StartSearch,
    SearchInsert(char),
    SearchBackspace,
    SearchCommit,
    SearchCancel,
    ToggleSearchMode,
    TogglePrivacy,
    ToggleDetails,
    CloseDetails,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum PeerManagementEffect {
    ToNormal,
}

#[derive(Default)]
pub struct PeerManagementReduceResult {
    pub redraw: bool,
    pub effects: Vec<PeerManagementEffect>,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum PeerColumnId {
    State,
    Address,
    Torrents,
    Evidence,
    LastSeen,
    Restriction,
}

#[derive(Clone, Debug)]
struct PeerColumnDefinition {
    id: PeerColumnId,
    header: &'static str,
    min_width: u16,
    priority: u8,
    constraint: Constraint,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord)]
enum EvidenceKind {
    Upload,
    Download,
    Reconnect,
    Manual,
}

#[derive(Clone, Debug, PartialEq, Eq)]
struct PeerEvidence {
    kind: EvidenceKind,
    observed: u64,
    threshold: u64,
    from_policy: bool,
}

impl PeerEvidence {
    fn compact_label(&self) -> String {
        match self.kind {
            EvidenceKind::Upload => format!("UL {:.0}%", self.percent()),
            EvidenceKind::Download => format!("DL {:.0}%", self.percent()),
            EvidenceKind::Reconnect => {
                format!("Reconnect {}/{}", self.observed, self.threshold)
            }
            EvidenceKind::Manual => "MANUAL".to_string(),
        }
    }

    fn percent(&self) -> f64 {
        if self.threshold == 0 {
            0.0
        } else {
            self.observed as f64 * 100.0 / self.threshold as f64
        }
    }
}

#[derive(Clone, Debug)]
struct PeerRowModel {
    ip: IpAddr,
    tracked: Vec<PeerManagerTrackedPeer>,
    restriction: Option<PeerRestriction>,
    torrent_count: usize,
}

impl PeerRowModel {
    fn is_active(&self) -> bool {
        self.tracked.iter().any(|peer| peer.is_active)
    }

    fn is_restricted(&self) -> bool {
        self.restriction.is_some()
    }

    fn state_label(&self) -> &'static str {
        if self.is_restricted() {
            "BLOCKED"
        } else if self.is_active() {
            "ACTIVE"
        } else {
            "RECENT"
        }
    }

    fn last_seen(&self) -> Option<SystemTime> {
        self.tracked.iter().filter_map(|peer| peer.last_seen).max()
    }

    fn torrent_hashes(&self) -> BTreeSet<Vec<u8>> {
        let mut hashes = self
            .tracked
            .iter()
            .map(|peer| peer.torrent_info_hash.clone())
            .collect::<BTreeSet<_>>();
        if let Some(hash) = self
            .restriction
            .as_ref()
            .and_then(|restriction| restriction.torrent_info_hash.clone())
        {
            hashes.insert(hash);
        }
        hashes
    }

    fn endpoint_count(&self) -> usize {
        self.tracked
            .iter()
            .flat_map(|peer| peer.endpoints.iter().map(|endpoint| &endpoint.address))
            .collect::<BTreeSet<_>>()
            .len()
    }

    fn strongest_evidence(&self) -> PeerEvidence {
        let mut candidates = self
            .tracked
            .iter()
            .flat_map(tracked_peer_evidence)
            .collect::<Vec<_>>();
        if let Some(restriction) = &self.restriction {
            candidates.push(restriction_evidence(&restriction.reason));
        }
        candidates
            .into_iter()
            .max_by(compare_evidence)
            .unwrap_or(PeerEvidence {
                kind: EvidenceKind::Reconnect,
                observed: 0,
                threshold: 0,
                from_policy: false,
            })
    }
}

pub fn handle_event(event: CrosstermEvent, app_state: &mut AppState) {
    if !matches!(app_state.mode, AppMode::PeerManagement) {
        return;
    }

    let CrosstermEvent::Key(key) = event else {
        return;
    };
    if !matches!(key.kind, KeyEventKind::Press | KeyEventKind::Repeat) {
        return;
    }

    let Some(action) = map_key_to_peer_management_action(key.code, app_state) else {
        return;
    };
    let result = reduce_peer_management_action(app_state, action);
    if result.redraw {
        app_state.ui.needs_redraw = true;
    }
    execute_peer_management_effects(app_state, result.effects);
}

fn map_key_to_peer_management_action(
    key_code: KeyCode,
    app_state: &AppState,
) -> Option<PeerManagementAction> {
    if app_state.ui.peer_management.is_searching {
        return match key_code {
            KeyCode::Esc => Some(PeerManagementAction::SearchCancel),
            KeyCode::Enter => Some(PeerManagementAction::SearchCommit),
            KeyCode::Tab => Some(PeerManagementAction::ToggleSearchMode),
            KeyCode::Backspace => Some(PeerManagementAction::SearchBackspace),
            KeyCode::Char(c) => Some(PeerManagementAction::SearchInsert(c)),
            _ => None,
        };
    }

    if peer_search_panel_active(app_state) && matches!(key_code, KeyCode::Esc) {
        return Some(PeerManagementAction::SearchCancel);
    }

    if peer_search_panel_active(app_state) && matches!(key_code, KeyCode::Tab) {
        return Some(PeerManagementAction::ToggleSearchMode);
    }

    match key_code {
        KeyCode::Char('q') => Some(PeerManagementAction::ToNormal),
        KeyCode::Esc if peer_details_overlay_active(app_state) => {
            Some(PeerManagementAction::CloseDetails)
        }
        KeyCode::Esc => Some(PeerManagementAction::ToNormal),
        KeyCode::Up | KeyCode::Char('k') => Some(PeerManagementAction::MoveUp),
        KeyCode::Down | KeyCode::Char('j') => Some(PeerManagementAction::MoveDown),
        KeyCode::PageUp => Some(PeerManagementAction::MovePageUp),
        KeyCode::PageDown => Some(PeerManagementAction::MovePageDown),
        KeyCode::Home => Some(PeerManagementAction::MoveFirst),
        KeyCode::End => Some(PeerManagementAction::MoveLast),
        KeyCode::Left | KeyCode::Char('h') => Some(PeerManagementAction::MoveColumnLeft),
        KeyCode::Right | KeyCode::Char('l') => Some(PeerManagementAction::MoveColumnRight),
        KeyCode::Char('s') => Some(PeerManagementAction::SortBySelectedColumn),
        KeyCode::Tab => Some(PeerManagementAction::FilterNext),
        KeyCode::BackTab => Some(PeerManagementAction::FilterPrev),
        KeyCode::Char('/') => Some(PeerManagementAction::StartSearch),
        KeyCode::Char('x') => Some(PeerManagementAction::TogglePrivacy),
        KeyCode::Enter if peer_uses_details_overlay(app_state) => {
            Some(PeerManagementAction::ToggleDetails)
        }
        _ => None,
    }
}

pub fn reduce_peer_management_action(
    app_state: &mut AppState,
    action: PeerManagementAction,
) -> PeerManagementReduceResult {
    let now = SystemTime::now();
    app_state.ui.peer_management.status_message = None;
    let mut result = PeerManagementReduceResult {
        redraw: true,
        effects: Vec::new(),
    };

    match action {
        PeerManagementAction::ToNormal => {
            app_state.ui.peer_management.is_searching = false;
            app_state.ui.peer_management.search_query.clear();
            app_state.ui.peer_management.show_details = false;
            result.effects.push(PeerManagementEffect::ToNormal);
        }
        PeerManagementAction::MoveUp => {
            let rows = build_peer_rows_at(app_state, now);
            move_peer_selection(app_state, &rows, -1);
        }
        PeerManagementAction::MoveDown => {
            let rows = build_peer_rows_at(app_state, now);
            move_peer_selection(app_state, &rows, 1);
        }
        PeerManagementAction::MovePageUp => {
            let rows = build_peer_rows_at(app_state, now);
            move_peer_selection(app_state, &rows, -(peer_page_rows(app_state) as isize));
        }
        PeerManagementAction::MovePageDown => {
            let rows = build_peer_rows_at(app_state, now);
            move_peer_selection(app_state, &rows, peer_page_rows(app_state) as isize);
        }
        PeerManagementAction::MoveFirst => {
            let rows = build_peer_rows_at(app_state, now);
            select_peer_index(app_state, &rows, 0);
        }
        PeerManagementAction::MoveLast => {
            let rows = build_peer_rows_at(app_state, now);
            select_peer_index(app_state, &rows, rows.len().saturating_sub(1));
        }
        PeerManagementAction::MoveColumnLeft => move_peer_column(app_state, -1),
        PeerManagementAction::MoveColumnRight => move_peer_column(app_state, 1),
        PeerManagementAction::SortBySelectedColumn => {
            let selected = normalized_selected_peer_column_index(app_state);
            app_state.ui.peer_management.selected_column_index = selected;
            if app_state.ui.peer_management.sort_column_index == Some(selected) {
                app_state.ui.peer_management.sort_direction =
                    reverse_sort_direction(app_state.ui.peer_management.sort_direction);
            } else {
                app_state.ui.peer_management.sort_column_index = Some(selected);
                app_state.ui.peer_management.sort_direction =
                    peer_column_default_direction(peer_columns()[selected].id);
            }
        }
        PeerManagementAction::FilterNext => {
            app_state.ui.peer_management.filter = app_state.ui.peer_management.filter.next();
            reset_peer_selection(app_state);
        }
        PeerManagementAction::FilterPrev => {
            app_state.ui.peer_management.filter = app_state.ui.peer_management.filter.prev();
            reset_peer_selection(app_state);
        }
        PeerManagementAction::StartSearch => {
            app_state.ui.peer_management.is_searching = true;
        }
        PeerManagementAction::SearchInsert(c) => {
            app_state.ui.peer_management.search_query.push(c);
            reset_peer_selection(app_state);
        }
        PeerManagementAction::SearchBackspace => {
            app_state.ui.peer_management.search_query.pop();
            reset_peer_selection(app_state);
        }
        PeerManagementAction::SearchCommit => {
            app_state.ui.peer_management.is_searching = false;
        }
        PeerManagementAction::SearchCancel => {
            app_state.ui.peer_management.is_searching = false;
            app_state.ui.peer_management.search_query.clear();
            reset_peer_selection(app_state);
        }
        PeerManagementAction::ToggleSearchMode => {
            app_state.ui.peer_management.search_mode =
                match app_state.ui.peer_management.search_mode {
                    SearchMode::Fuzzy => SearchMode::Regex,
                    SearchMode::Regex => SearchMode::Fuzzy,
                };
            reset_peer_selection(app_state);
        }
        PeerManagementAction::TogglePrivacy => {
            let enabling_privacy = !app_state.anonymize_torrent_names;
            app_state.anonymize_torrent_names = enabling_privacy;
            if enabling_privacy {
                app_state.ui.peer_management.is_searching = false;
                app_state.ui.peer_management.search_query.clear();
                reset_peer_selection(app_state);
            }
        }
        PeerManagementAction::ToggleDetails => {
            app_state.ui.peer_management.show_details = !app_state.ui.peer_management.show_details;
        }
        PeerManagementAction::CloseDetails => {
            app_state.ui.peer_management.show_details = false;
        }
    }

    clamp_peer_column_state(app_state);
    result
}

fn execute_peer_management_effects(app_state: &mut AppState, effects: Vec<PeerManagementEffect>) {
    for effect in effects {
        match effect {
            PeerManagementEffect::ToNormal => app_state.mode = AppMode::Normal,
        }
    }
}

pub fn draw(f: &mut Frame, screen: &ScreenContext<'_>) {
    let app_state = screen.app.state;
    let ctx = screen.theme;
    let area = f.area();
    let now = SystemTime::now();
    let search_visible = peer_search_panel_active(app_state);
    let layout = calculate_peer_screen_layout(
        area,
        search_visible,
        app_state.ui.peer_management.show_details,
    );
    let rows = build_peer_rows_at(app_state, now);

    f.render_widget(Clear, area);
    if let Some(search_area) = layout.search {
        draw_peer_search_panel(f, app_state, search_area, ctx);
    }
    draw_peer_summary(f, app_state, layout.summary, rows.len(), now, ctx);

    match layout.body {
        PeerBodyLayout::Wide { table, details } | PeerBodyLayout::Stacked { table, details } => {
            draw_peer_table(f, app_state, &rows, table, now, ctx);
            draw_peer_details(
                f,
                app_state,
                selected_peer_row(app_state, &rows),
                details,
                now,
                ctx,
            );
        }
        PeerBodyLayout::TableOnly { table } => {
            draw_peer_table(f, app_state, &rows, table, now, ctx);
        }
        PeerBodyLayout::DetailsOnly { details } => {
            draw_peer_details(
                f,
                app_state,
                selected_peer_row(app_state, &rows),
                details,
                now,
                ctx,
            );
        }
    }
    draw_peer_footer(f, app_state, layout.footer, ctx);
}

fn build_peer_rows_at(app_state: &AppState, now: SystemTime) -> Vec<PeerRowModel> {
    #[cfg(test)]
    PEER_ROW_BUILD_COUNT.with(|count| count.set(count.get().saturating_add(1)));

    let mut by_ip = BTreeMap::<IpAddr, PeerRowModel>::new();
    for tracked in &app_state.peer_manager_view.tracked_peers {
        let ip = normalize_peer_ip(tracked.ip);
        by_ip
            .entry(ip)
            .or_insert_with(|| PeerRowModel {
                ip,
                tracked: Vec::new(),
                restriction: None,
                torrent_count: 0,
            })
            .tracked
            .push(tracked.clone());
    }
    for (policy_ip, restriction) in &app_state.peer_policy.restrictions {
        if restriction.blocked_until <= now {
            continue;
        }
        let ip = normalize_peer_ip(*policy_ip);
        let row = by_ip.entry(ip).or_insert_with(|| PeerRowModel {
            ip,
            tracked: Vec::new(),
            restriction: None,
            torrent_count: 0,
        });
        if row
            .restriction
            .as_ref()
            .is_none_or(|current| restriction.blocked_until > current.blocked_until)
        {
            row.restriction = Some(restriction.clone());
        }
    }

    let mut rows = by_ip.into_values().collect::<Vec<_>>();
    for row in &mut rows {
        row.tracked.sort_by(|left, right| {
            left.torrent_name
                .cmp(&right.torrent_name)
                .then_with(|| left.torrent_info_hash.cmp(&right.torrent_info_hash))
        });
        row.torrent_count = row.torrent_hashes().len();
    }
    rows.retain(|row| peer_matches_filter(row, app_state.ui.peer_management.filter));
    let search = PeerSearchMatcher::new(app_state);
    if !matches!(&search, PeerSearchMatcher::MatchAll) {
        rows.retain(|row| search.matches(&peer_search_text(row, app_state)));
    }
    sort_peer_rows(app_state, &mut rows);
    rows
}

fn normalize_peer_ip(ip: IpAddr) -> IpAddr {
    match ip {
        IpAddr::V6(ipv6) => ipv6.to_ipv4_mapped().map_or(IpAddr::V6(ipv6), IpAddr::V4),
        IpAddr::V4(_) => ip,
    }
}

fn peer_matches_filter(row: &PeerRowModel, filter: PeerManagementFilter) -> bool {
    match filter {
        PeerManagementFilter::All => true,
        PeerManagementFilter::Active => row.is_active() && !row.is_restricted(),
        PeerManagementFilter::Recent => !row.is_active() && !row.is_restricted(),
        PeerManagementFilter::Restricted => row.is_restricted(),
    }
}

enum PeerSearchMatcher {
    MatchAll,
    Fuzzy {
        query: String,
        matcher: Box<SkimMatcherV2>,
    },
    Regex(regex::Regex),
    Invalid,
}

impl PeerSearchMatcher {
    fn new(app_state: &AppState) -> Self {
        let query = app_state.ui.peer_management.search_query.trim();
        if query.is_empty() {
            return Self::MatchAll;
        }
        match app_state.ui.peer_management.search_mode {
            SearchMode::Fuzzy => Self::Fuzzy {
                query: query.to_string(),
                matcher: Box::new(SkimMatcherV2::default().ignore_case()),
            },
            SearchMode::Regex => RegexBuilder::new(query)
                .case_insensitive(true)
                .build()
                .map_or(Self::Invalid, Self::Regex),
        }
    }

    fn matches(&self, haystack: &str) -> bool {
        match self {
            Self::MatchAll => true,
            Self::Fuzzy { query, matcher } => matcher.fuzzy_match(haystack, query).is_some(),
            Self::Regex(regex) => regex.is_match(haystack),
            Self::Invalid => false,
        }
    }
}

fn peer_search_text(row: &PeerRowModel, app_state: &AppState) -> String {
    let mut fields = vec![row.ip.to_string(), row.state_label().to_string()];
    for tracked in &row.tracked {
        fields.push(tracked.torrent_name.clone());
        fields.push(hex::encode(&tracked.torrent_info_hash));
        fields.extend(
            tracked
                .endpoints
                .iter()
                .map(|endpoint| endpoint.address.clone()),
        );
    }
    if let Some(restriction) = &row.restriction {
        fields.push(restriction_reason_search_text(&restriction.reason).to_string());
        if let Some(hash) = &restriction.torrent_info_hash {
            fields.push(hex::encode(hash));
            if let Some(torrent) = app_state.torrents.get(hash) {
                fields.push(torrent.latest_state.torrent_name.clone());
            }
        }
    }
    fields.join("\n")
}

fn restriction_reason_search_text(reason: &PeerRestrictionReason) -> &'static str {
    match reason {
        PeerRestrictionReason::ExcessiveUpload { .. } => "excessive upload",
        PeerRestrictionReason::ExcessiveDownload { .. } => "excessive download",
        PeerRestrictionReason::ReconnectChurn { .. } => "reconnect churn",
        PeerRestrictionReason::Manual => "manual",
    }
}

fn tracked_peer_evidence(peer: &PeerManagerTrackedPeer) -> [PeerEvidence; 3] {
    [
        PeerEvidence {
            kind: EvidenceKind::Upload,
            observed: peer.uploaded_evidence_bytes,
            threshold: peer.transfer_threshold_bytes,
            from_policy: false,
        },
        PeerEvidence {
            kind: EvidenceKind::Download,
            observed: peer.downloaded_evidence_bytes,
            threshold: peer.transfer_threshold_bytes,
            from_policy: false,
        },
        PeerEvidence {
            kind: EvidenceKind::Reconnect,
            observed: u64::from(peer.reconnect_count),
            threshold: u64::from(peer.reconnect_limit),
            from_policy: false,
        },
    ]
}

fn restriction_evidence(reason: &PeerRestrictionReason) -> PeerEvidence {
    match reason {
        PeerRestrictionReason::ExcessiveUpload {
            uploaded_bytes,
            threshold_bytes,
        } => PeerEvidence {
            kind: EvidenceKind::Upload,
            observed: *uploaded_bytes,
            threshold: *threshold_bytes,
            from_policy: true,
        },
        PeerRestrictionReason::ExcessiveDownload {
            downloaded_bytes,
            threshold_bytes,
        } => PeerEvidence {
            kind: EvidenceKind::Download,
            observed: *downloaded_bytes,
            threshold: *threshold_bytes,
            from_policy: true,
        },
        PeerRestrictionReason::ReconnectChurn {
            reconnects,
            threshold,
            ..
        } => PeerEvidence {
            kind: EvidenceKind::Reconnect,
            observed: u64::from(*reconnects),
            threshold: u64::from(*threshold),
            from_policy: true,
        },
        PeerRestrictionReason::Manual => PeerEvidence {
            kind: EvidenceKind::Manual,
            observed: 1,
            threshold: 1,
            from_policy: true,
        },
    }
}

fn compare_evidence(left: &PeerEvidence, right: &PeerEvidence) -> Ordering {
    compare_evidence_ratio(left, right)
        .then_with(|| left.from_policy.cmp(&right.from_policy))
        .then_with(|| left.kind.cmp(&right.kind))
}

fn compare_evidence_ratio(left: &PeerEvidence, right: &PeerEvidence) -> Ordering {
    let (left_numerator, left_denominator) = evidence_ratio_parts(left);
    let (right_numerator, right_denominator) = evidence_ratio_parts(right);
    left_numerator
        .saturating_mul(right_denominator)
        .cmp(&right_numerator.saturating_mul(left_denominator))
}

fn evidence_ratio_parts(evidence: &PeerEvidence) -> (u128, u128) {
    if evidence.threshold == 0 {
        (0, 1)
    } else {
        (
            u128::from(evidence.observed),
            u128::from(evidence.threshold),
        )
    }
}

fn peer_columns() -> Vec<PeerColumnDefinition> {
    vec![
        PeerColumnDefinition {
            id: PeerColumnId::State,
            header: "State",
            min_width: 10,
            priority: 0,
            constraint: Constraint::Length(10),
        },
        PeerColumnDefinition {
            id: PeerColumnId::Address,
            header: "Address",
            min_width: 20,
            priority: 0,
            constraint: Constraint::Fill(2),
        },
        PeerColumnDefinition {
            id: PeerColumnId::Torrents,
            header: "Torrents",
            min_width: 10,
            priority: 2,
            constraint: Constraint::Length(10),
        },
        PeerColumnDefinition {
            id: PeerColumnId::Evidence,
            header: "Evidence",
            min_width: 15,
            priority: 0,
            constraint: Constraint::Length(15),
        },
        PeerColumnDefinition {
            id: PeerColumnId::LastSeen,
            header: "Last Seen",
            min_width: 12,
            priority: 1,
            constraint: Constraint::Length(12),
        },
        PeerColumnDefinition {
            id: PeerColumnId::Restriction,
            header: "Restricted",
            min_width: 12,
            priority: 1,
            constraint: Constraint::Length(12),
        },
    ]
}

fn compute_visible_peer_management_columns(available_width: u16) -> (Vec<Constraint>, Vec<usize>) {
    let columns = peer_columns();
    let smart = columns
        .iter()
        .map(|column| SmartCol {
            min_width: column.min_width,
            priority: column.priority,
            constraint: column.constraint,
        })
        .collect::<Vec<_>>();
    compute_smart_table_layout(&smart, available_width.saturating_sub(4), 1)
}

fn peer_table_width_for_state(app_state: &AppState) -> u16 {
    let area = if app_state.screen_area.width == 0 {
        Rect::new(0, 0, 140, 36)
    } else {
        app_state.screen_area
    };
    let layout = calculate_peer_screen_layout(
        area,
        peer_search_panel_active(app_state),
        app_state.ui.peer_management.show_details,
    );
    match layout.body {
        PeerBodyLayout::Wide { table, .. }
        | PeerBodyLayout::Stacked { table, .. }
        | PeerBodyLayout::TableOnly { table } => table.width,
        PeerBodyLayout::DetailsOnly { details } => details.width,
    }
}

fn peer_uses_details_overlay(app_state: &AppState) -> bool {
    let area = if app_state.screen_area.width == 0 {
        Rect::new(0, 0, 140, 36)
    } else {
        app_state.screen_area
    };
    matches!(
        calculate_peer_screen_layout(
            area,
            peer_search_panel_active(app_state),
            app_state.ui.peer_management.show_details,
        )
        .body,
        PeerBodyLayout::TableOnly { .. } | PeerBodyLayout::DetailsOnly { .. }
    )
}

fn peer_details_overlay_active(app_state: &AppState) -> bool {
    app_state.ui.peer_management.show_details && peer_uses_details_overlay(app_state)
}

fn visible_peer_column_indices_for_state(app_state: &AppState) -> Vec<usize> {
    compute_visible_peer_management_columns(peer_table_width_for_state(app_state)).1
}

fn normalized_selected_column_from_visible(selected: usize, visible: &[usize]) -> usize {
    if visible.is_empty() {
        return 0;
    }
    if visible.contains(&selected) {
        return selected;
    }
    visible
        .iter()
        .copied()
        .rfind(|index| *index <= selected)
        .or_else(|| visible.first().copied())
        .unwrap_or(0)
}

fn normalized_selected_peer_column_index(app_state: &AppState) -> usize {
    normalized_selected_column_from_visible(
        app_state.ui.peer_management.selected_column_index,
        &visible_peer_column_indices_for_state(app_state),
    )
}

fn move_peer_column(app_state: &mut AppState, direction: isize) {
    let visible = visible_peer_column_indices_for_state(app_state);
    if visible.is_empty() {
        return;
    }
    let current = normalized_selected_column_from_visible(
        app_state.ui.peer_management.selected_column_index,
        &visible,
    );
    let position = visible
        .iter()
        .position(|index| *index == current)
        .unwrap_or(0);
    let next = if direction < 0 {
        position.saturating_sub(1)
    } else {
        (position + 1).min(visible.len() - 1)
    };
    app_state.ui.peer_management.selected_column_index = visible[next];
}

fn clamp_peer_column_state(app_state: &mut AppState) {
    let columns_len = peer_columns().len();
    if columns_len == 0 {
        return;
    }
    app_state.ui.peer_management.selected_column_index = app_state
        .ui
        .peer_management
        .selected_column_index
        .min(columns_len - 1);
    if app_state
        .ui
        .peer_management
        .sort_column_index
        .is_some_and(|index| index >= columns_len)
    {
        app_state.ui.peer_management.sort_column_index = None;
    }
}

fn reverse_sort_direction(direction: SortDirection) -> SortDirection {
    match direction {
        SortDirection::Ascending => SortDirection::Descending,
        SortDirection::Descending => SortDirection::Ascending,
    }
}

fn peer_column_default_direction(column: PeerColumnId) -> SortDirection {
    match column {
        PeerColumnId::Address => SortDirection::Ascending,
        PeerColumnId::State
        | PeerColumnId::Torrents
        | PeerColumnId::Evidence
        | PeerColumnId::LastSeen
        | PeerColumnId::Restriction => SortDirection::Descending,
    }
}

fn peer_sort_column(app_state: &AppState) -> Option<PeerColumnId> {
    app_state
        .ui
        .peer_management
        .sort_column_index
        .and_then(|index| peer_columns().get(index).map(|column| column.id))
}

fn sort_peer_rows(app_state: &AppState, rows: &mut [PeerRowModel]) {
    rows.sort_by(|left, right| compare_peer_rows(app_state, left, right));
}

fn compare_peer_rows(app_state: &AppState, left: &PeerRowModel, right: &PeerRowModel) -> Ordering {
    let Some(column) = peer_sort_column(app_state) else {
        return default_peer_order(left, right);
    };
    let ordering = match column {
        PeerColumnId::State => default_peer_order_values(left, right),
        PeerColumnId::Address => left.ip.cmp(&right.ip),
        PeerColumnId::Torrents => left.torrent_count.cmp(&right.torrent_count),
        PeerColumnId::Evidence => {
            compare_evidence_ratio(&left.strongest_evidence(), &right.strongest_evidence())
        }
        PeerColumnId::LastSeen => left.last_seen().cmp(&right.last_seen()),
        PeerColumnId::Restriction => left
            .restriction
            .as_ref()
            .map(|restriction| restriction.blocked_until)
            .cmp(
                &right
                    .restriction
                    .as_ref()
                    .map(|restriction| restriction.blocked_until),
            ),
    };
    apply_sort_direction(ordering, app_state.ui.peer_management.sort_direction)
        .then_with(|| left.ip.cmp(&right.ip))
}

fn default_peer_order(left: &PeerRowModel, right: &PeerRowModel) -> Ordering {
    default_peer_order_values(left, right)
        .reverse()
        .then_with(|| left.ip.cmp(&right.ip))
}

fn default_peer_order_values(left: &PeerRowModel, right: &PeerRowModel) -> Ordering {
    left.is_restricted()
        .cmp(&right.is_restricted())
        .then_with(|| {
            compare_evidence_ratio(&left.strongest_evidence(), &right.strongest_evidence())
        })
        .then_with(|| left.is_active().cmp(&right.is_active()))
        .then_with(|| left.last_seen().cmp(&right.last_seen()))
}

fn apply_sort_direction(ordering: Ordering, direction: SortDirection) -> Ordering {
    match direction {
        SortDirection::Ascending => ordering,
        SortDirection::Descending => ordering.reverse(),
    }
}

fn reset_peer_selection(app_state: &mut AppState) {
    app_state.ui.peer_management.selected_index = 0;
    app_state.ui.peer_management.show_details = false;
}

fn reconcile_peer_selection(app_state: &mut AppState, rows: &[PeerRowModel]) {
    if rows.is_empty() {
        app_state.ui.peer_management.selected_index = 0;
        app_state.ui.peer_management.show_details = false;
        return;
    }
    app_state.ui.peer_management.selected_index = app_state
        .ui
        .peer_management
        .selected_index
        .min(rows.len() - 1);
}

fn move_peer_selection(app_state: &mut AppState, rows: &[PeerRowModel], delta: isize) {
    if rows.is_empty() {
        reconcile_peer_selection(app_state, rows);
        return;
    }
    reconcile_peer_selection(app_state, rows);
    let current = selected_peer_index(app_state, rows).unwrap_or(0);
    let next = current
        .saturating_add_signed(delta)
        .min(rows.len().saturating_sub(1));
    app_state.ui.peer_management.selected_index = next;
}

fn select_peer_index(app_state: &mut AppState, rows: &[PeerRowModel], index: usize) {
    if rows.is_empty() {
        reconcile_peer_selection(app_state, rows);
        return;
    }
    app_state.ui.peer_management.selected_index = index.min(rows.len() - 1);
}

fn selected_peer_index(app_state: &AppState, rows: &[PeerRowModel]) -> Option<usize> {
    if rows.is_empty() {
        return None;
    }
    Some(
        app_state
            .ui
            .peer_management
            .selected_index
            .min(rows.len() - 1),
    )
}

fn selected_peer_row<'a>(
    app_state: &AppState,
    rows: &'a [PeerRowModel],
) -> Option<&'a PeerRowModel> {
    selected_peer_index(app_state, rows).and_then(|index| rows.get(index))
}

fn peer_page_rows(app_state: &AppState) -> usize {
    let area = if app_state.screen_area.height == 0 {
        Rect::new(0, 0, 140, 36)
    } else {
        app_state.screen_area
    };
    let layout = calculate_peer_screen_layout(
        area,
        peer_search_panel_active(app_state),
        app_state.ui.peer_management.show_details,
    );
    let table_height = match layout.body {
        PeerBodyLayout::Wide { table, .. }
        | PeerBodyLayout::Stacked { table, .. }
        | PeerBodyLayout::TableOnly { table } => table.height,
        PeerBodyLayout::DetailsOnly { details } => details.height,
    };
    table_height.saturating_sub(3).max(1) as usize
}

fn peer_search_panel_active(app_state: &AppState) -> bool {
    app_state.ui.peer_management.is_searching
        || !app_state.ui.peer_management.search_query.is_empty()
}

fn draw_peer_search_panel(f: &mut Frame, app_state: &AppState, area: Rect, ctx: &ThemeContext) {
    draw_prompt_panel(
        f,
        area,
        " Peer Search ".to_string(),
        sanitize_text(&app_state.ui.peer_management.search_query),
        peer_search_mode_spans(app_state.ui.peer_management.search_mode, ctx),
        ctx,
    );
}

fn peer_search_mode_spans(mode: SearchMode, ctx: &ThemeContext) -> Vec<Span<'static>> {
    let selected = ctx.apply(Style::default().fg(ctx.state_selected()).bold());
    let idle = ctx.apply(Style::default().fg(ctx.theme.semantic.overlay0));
    let (fuzzy, regex) = match mode {
        SearchMode::Fuzzy => (selected, idle),
        SearchMode::Regex => (idle, selected),
    };
    vec![
        Span::raw("  "),
        Span::styled("Fuzzy", fuzzy),
        Span::raw(" / "),
        Span::styled("Regex", regex),
    ]
}

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
struct PeerSummaryCounts {
    tracked: usize,
    active: usize,
    restricted: usize,
}

fn peer_summary_counts_at(app_state: &AppState, now: SystemTime) -> PeerSummaryCounts {
    let tracked = app_state
        .peer_manager_view
        .tracked_peers
        .iter()
        .map(|peer| normalize_peer_ip(peer.ip))
        .collect::<BTreeSet<_>>();
    let active = app_state
        .peer_manager_view
        .tracked_peers
        .iter()
        .filter(|peer| peer.is_active)
        .map(|peer| normalize_peer_ip(peer.ip))
        .collect::<BTreeSet<_>>();
    let restricted = app_state
        .peer_policy
        .restrictions
        .iter()
        .filter(|(_, restriction)| restriction.blocked_until > now)
        .map(|(ip, _)| normalize_peer_ip(*ip))
        .collect::<BTreeSet<_>>();
    PeerSummaryCounts {
        tracked: tracked.len(),
        active: active.len(),
        restricted: restricted.len(),
    }
}

fn draw_peer_summary(
    f: &mut Frame,
    app_state: &AppState,
    area: Rect,
    visible_count: usize,
    now: SystemTime,
    ctx: &ThemeContext,
) {
    if area.height == 0 {
        return;
    }
    let counts = peer_summary_counts_at(app_state, now);
    let mut count_spans = vec![
        Span::styled(
            "Tracked ",
            ctx.apply(Style::default().fg(ctx.theme.semantic.subtext1)),
        ),
        Span::styled(
            counts.tracked.to_string(),
            ctx.apply(Style::default().fg(ctx.accent_sky()).bold()),
        ),
        Span::styled(
            "  Live ",
            ctx.apply(Style::default().fg(ctx.theme.semantic.subtext1)),
        ),
        Span::styled(
            counts.active.to_string(),
            ctx.apply(Style::default().fg(ctx.state_success()).bold()),
        ),
        Span::styled(
            "  Restricted ",
            ctx.apply(Style::default().fg(ctx.theme.semantic.subtext1)),
        ),
        Span::styled(
            counts.restricted.to_string(),
            ctx.apply(Style::default().fg(ctx.state_error()).bold()),
        ),
        Span::styled(
            format!(
                "  Torrents {}  Showing {}",
                app_state.peer_manager_view.registered_torrents, visible_count
            ),
            ctx.apply(Style::default().fg(ctx.theme.semantic.subtext1)),
        ),
    ];
    if app_state.anonymize_torrent_names {
        count_spans.push(Span::styled(
            "  PRIVACY",
            ctx.apply(Style::default().fg(ctx.state_warning()).bold()),
        ));
    }
    if let Some(message) =
        peer_search_error(app_state).or_else(|| app_state.ui.peer_management.status_message.clone())
    {
        count_spans.push(Span::styled(
            format!("  |  {}", sanitize_text(&message)),
            ctx.apply(Style::default().fg(ctx.state_warning())),
        ));
    }

    let filters = [
        PeerManagementFilter::All,
        PeerManagementFilter::Active,
        PeerManagementFilter::Recent,
        PeerManagementFilter::Restricted,
    ];
    let filter_spans = filters
        .iter()
        .enumerate()
        .flat_map(|(index, filter)| {
            let style = if *filter == app_state.ui.peer_management.filter {
                ctx.apply(Style::default().fg(ctx.state_selected()).bold())
            } else {
                ctx.apply(Style::default().fg(ctx.theme.semantic.subtext1))
            };
            let mut spans = vec![Span::styled(filter.label(), style)];
            if index + 1 < filters.len() {
                spans.push(Span::styled(
                    "  ",
                    ctx.apply(Style::default().fg(ctx.theme.semantic.surface2)),
                ));
            }
            spans
        })
        .collect::<Vec<_>>();

    f.render_widget(
        Paragraph::new(vec![Line::from(count_spans), Line::from(filter_spans)]),
        area,
    );
}

fn peer_search_error(app_state: &AppState) -> Option<String> {
    let query = app_state.ui.peer_management.search_query.trim();
    if query.is_empty() || !matches!(app_state.ui.peer_management.search_mode, SearchMode::Regex) {
        return None;
    }
    RegexBuilder::new(query)
        .case_insensitive(true)
        .build()
        .err()
        .map(|_| "Invalid regular expression".to_string())
}

fn draw_peer_table(
    f: &mut Frame,
    app_state: &AppState,
    rows: &[PeerRowModel],
    area: Rect,
    now: SystemTime,
    ctx: &ThemeContext,
) {
    let columns = peer_columns();
    let (constraints, visible) = compute_visible_peer_management_columns(area.width);
    let selected_column = normalized_selected_column_from_visible(
        app_state.ui.peer_management.selected_column_index,
        &visible,
    );
    let header = Row::new(
        visible
            .iter()
            .map(|index| {
                let column = &columns[*index];
                let is_selected = *index == selected_column;
                let is_sorting = app_state.ui.peer_management.sort_column_index == Some(*index);
                let mut style =
                    ctx.apply(Style::default().fg(peer_column_header_color(column.id, ctx)));
                if is_sorting {
                    style = style.bold();
                }
                if is_selected {
                    style = ctx.apply(style.add_modifier(Modifier::BOLD | Modifier::UNDERLINED));
                }
                let mut spans = vec![Span::styled(column.header, style)];
                if is_sorting {
                    spans.push(Span::styled(
                        peer_sort_arrow(app_state.ui.peer_management.sort_direction),
                        style,
                    ));
                }
                Cell::from(Line::from(spans))
            })
            .collect::<Vec<_>>(),
    );
    let table_rows = rows
        .iter()
        .map(|row| peer_table_row(app_state, row, &visible, now, ctx))
        .collect::<Vec<_>>();
    let table = Table::new(table_rows, constraints)
        .header(header)
        .row_highlight_style(
            ctx.apply(
                Style::default()
                    .fg(ctx.theme.semantic.text)
                    .bg(ctx.theme.semantic.surface0)
                    .bold(),
            ),
        )
        .block(
            Block::default()
                .title(Span::styled(
                    " Peer Manager ",
                    ctx.apply(Style::default().fg(ctx.state_selected()).bold()),
                ))
                .borders(Borders::ALL)
                .border_style(ctx.apply(Style::default().fg(ctx.theme.semantic.border)))
                .padding(Padding::horizontal(1)),
        );
    let mut table_state = TableState::default();
    table_state.select(selected_peer_index(app_state, rows));
    f.render_stateful_widget(table, area, &mut table_state);

    if rows.is_empty() {
        let message = if peer_search_error(app_state).is_some() {
            "Fix the search expression to see peers"
        } else if app_state.ui.peer_management.search_query.is_empty()
            && matches!(
                app_state.ui.peer_management.filter,
                PeerManagementFilter::All
            )
        {
            "No peers tracked yet"
        } else {
            "No peers match this view"
        };
        f.render_widget(
            Paragraph::new(message)
                .alignment(Alignment::Center)
                .style(ctx.apply(Style::default().fg(ctx.theme.semantic.surface2))),
            centered_line_rect(inner_rect(area)),
        );
    }
}

fn peer_table_row<'a>(
    app_state: &AppState,
    row: &PeerRowModel,
    visible: &[usize],
    now: SystemTime,
    ctx: &ThemeContext,
) -> Row<'a> {
    let columns = peer_columns();
    let cells = visible
        .iter()
        .map(|index| match columns[*index].id {
            PeerColumnId::State => Cell::from(row.state_label()).style(peer_state_style(row, ctx)),
            PeerColumnId::Address => {
                Cell::from(display_ip(row.ip, app_state.anonymize_torrent_names))
            }
            PeerColumnId::Torrents => Cell::from(peer_torrents_label(row)),
            PeerColumnId::Evidence => Cell::from(row.strongest_evidence().compact_label()),
            PeerColumnId::LastSeen => Cell::from(
                row.last_seen()
                    .map(|last_seen| format_elapsed(now, last_seen))
                    .unwrap_or_else(|| "policy only".to_string()),
            ),
            PeerColumnId::Restriction => Cell::from(
                row.restriction
                    .as_ref()
                    .map(|restriction| format_remaining(now, restriction.blocked_until))
                    .unwrap_or_else(|| "-".to_string()),
            ),
        })
        .collect::<Vec<_>>();
    Row::new(cells).style(if row.is_restricted() {
        ctx.apply(Style::default().fg(ctx.state_error()))
    } else if row.is_active() {
        ctx.apply(Style::default().fg(ctx.theme.semantic.text))
    } else {
        ctx.apply(Style::default().fg(ctx.theme.semantic.subtext1))
    })
}

fn peer_state_style(row: &PeerRowModel, ctx: &ThemeContext) -> Style {
    if row.is_restricted() {
        ctx.apply(Style::default().fg(ctx.state_error()).bold())
    } else if row.is_active() {
        ctx.apply(Style::default().fg(ctx.state_success()).bold())
    } else {
        ctx.apply(Style::default().fg(ctx.theme.semantic.subtext1))
    }
}

fn peer_column_header_color(column: PeerColumnId, ctx: &ThemeContext) -> Color {
    match column {
        PeerColumnId::State => ctx.state_success(),
        PeerColumnId::Address => ctx.accent_sky(),
        PeerColumnId::Torrents => ctx.accent_teal(),
        PeerColumnId::Evidence => ctx.state_warning(),
        PeerColumnId::LastSeen => ctx.state_info(),
        PeerColumnId::Restriction => ctx.state_error(),
    }
}

fn peer_sort_arrow(direction: SortDirection) -> &'static str {
    match direction {
        SortDirection::Ascending => " ▲",
        SortDirection::Descending => " ▼",
    }
}

fn peer_torrents_label(row: &PeerRowModel) -> String {
    row.torrent_count.to_string()
}

fn torrent_name_for_hash<'a>(
    app_state: &'a AppState,
    row: &'a PeerRowModel,
    hash: &[u8],
) -> Option<&'a str> {
    row.tracked
        .iter()
        .find(|tracked| tracked.torrent_info_hash == hash)
        .map(|tracked| tracked.torrent_name.as_str())
        .or_else(|| {
            app_state
                .torrents
                .get(hash)
                .map(|torrent| torrent.latest_state.torrent_name.as_str())
        })
}

fn torrent_label_for_hash(app_state: &AppState, row: &PeerRowModel, hash: &[u8]) -> String {
    let name = torrent_name_for_hash(app_state, row, hash);
    let Some(name) = name else {
        return short_info_hash(hash, app_state.anonymize_torrent_names);
    };
    display_torrent_name(name, app_state.anonymize_torrent_names)
}

fn draw_peer_details(
    f: &mut Frame,
    app_state: &AppState,
    row: Option<&PeerRowModel>,
    area: Rect,
    now: SystemTime,
    ctx: &ThemeContext,
) {
    if area.width == 0 || area.height == 0 {
        return;
    }
    f.render_widget(Clear, area);
    let block = Block::default()
        .title(Span::styled(
            " Peer Details ",
            ctx.apply(Style::default().fg(ctx.accent_sapphire()).bold()),
        ))
        .borders(Borders::ALL)
        .border_style(ctx.apply(Style::default().fg(ctx.theme.semantic.border)))
        .padding(Padding::new(1, 1, 0, 0));
    let inner = block.inner(area);
    f.render_widget(block, area);

    let Some(row) = row else {
        f.render_widget(
            Paragraph::new("No peer selected")
                .alignment(Alignment::Center)
                .style(ctx.apply(Style::default().fg(ctx.theme.semantic.surface2))),
            centered_line_rect(inner),
        );
        return;
    };

    let privacy = app_state.anonymize_torrent_names;
    let mut lines = vec![Line::from(vec![
        Span::styled(
            display_ip(row.ip, privacy),
            ctx.apply(Style::default().fg(ctx.accent_sky()).bold()),
        ),
        Span::styled(
            format!(
                "  {}  {} torrent{}  {} endpoint{}",
                row.state_label(),
                row.torrent_count,
                plural_suffix(row.torrent_count),
                row.endpoint_count(),
                plural_suffix(row.endpoint_count())
            ),
            peer_state_style(row, ctx),
        ),
    ])];

    if let Some(restriction) = &row.restriction {
        lines.push(Line::from(""));
        lines.push(section_line("Policy", ctx));
        lines.push(key_value_line(
            "Reason",
            restriction_reason_detail(&restriction.reason),
            ctx,
        ));
        lines.push(key_value_line(
            "Detected",
            format_elapsed(now, restriction.detected_at),
            ctx,
        ));
        lines.push(key_value_line(
            "Expires",
            format_remaining(now, restriction.blocked_until),
            ctx,
        ));
        let origin = restriction
            .torrent_info_hash
            .as_ref()
            .map(|hash| torrent_label_for_hash(app_state, row, hash))
            .unwrap_or_else(|| "global/manual".to_string());
        lines.push(key_value_line("Origin", origin, ctx));
    } else {
        lines.push(Line::from(""));
        lines.push(Line::from(Span::styled(
            "No active restriction",
            ctx.apply(Style::default().fg(ctx.state_success())),
        )));
    }

    if row.tracked.is_empty() {
        lines.push(Line::from(""));
        lines.push(Line::from(Span::styled(
            "Tracking history is unavailable for this restored policy entry.",
            ctx.apply(Style::default().fg(ctx.theme.semantic.subtext1).italic()),
        )));
    } else {
        lines.push(Line::from(""));
        lines.push(section_line("Per-torrent evidence", ctx));
        for tracked in &row.tracked {
            lines.extend(tracked_peer_detail_lines(tracked, privacy, ctx));
        }
    }

    f.render_widget(
        Paragraph::new(lines)
            .wrap(Wrap { trim: true })
            .style(ctx.apply(Style::default().fg(ctx.theme.semantic.text))),
        inner,
    );
}

fn tracked_peer_detail_lines(
    peer: &PeerManagerTrackedPeer,
    privacy: bool,
    ctx: &ThemeContext,
) -> Vec<Line<'static>> {
    let torrent_name = display_torrent_name(&peer.torrent_name, privacy);
    let hash = short_info_hash(&peer.torrent_info_hash, privacy);
    let mut lines = vec![Line::from(vec![
        Span::styled(
            format!("{} ", sanitize_text(&torrent_name)),
            ctx.apply(Style::default().fg(ctx.accent_teal()).bold()),
        ),
        Span::styled(
            "— ",
            ctx.apply(Style::default().fg(ctx.theme.semantic.overlay0)),
        ),
        Span::styled(
            hash,
            ctx.apply(Style::default().fg(ctx.theme.semantic.overlay0)),
        ),
    ])];
    lines.push(Line::from(format!(
        "  UL {} / {} ({:.0}%)  DL {} / {} ({:.0}%)",
        format_bytes(peer.uploaded_evidence_bytes),
        format_bytes(peer.transfer_threshold_bytes),
        evidence_percent(peer.uploaded_evidence_bytes, peer.transfer_threshold_bytes),
        format_bytes(peer.downloaded_evidence_bytes),
        format_bytes(peer.transfer_threshold_bytes),
        evidence_percent(
            peer.downloaded_evidence_bytes,
            peer.transfer_threshold_bytes
        ),
    )));
    lines.push(Line::from(format!(
        "  Reconnects: {} observed, {} limit ({} window)",
        peer.reconnect_count,
        peer.reconnect_limit,
        compact_duration(Duration::from_secs(peer.reconnect_window_secs))
    )));
    if peer.endpoints.is_empty() {
        lines.push(Line::from(Span::styled(
            "  Current endpoints: none",
            ctx.apply(Style::default().fg(ctx.theme.semantic.subtext1)),
        )));
    } else {
        for endpoint in &peer.endpoints {
            lines.push(Line::from(format!(
                "  {}  DL {}  UL {}",
                display_endpoint(&endpoint.address, privacy),
                format_bytes(endpoint.total_downloaded),
                format_bytes(endpoint.total_uploaded)
            )));
        }
    }
    lines
}

fn section_line(label: &str, ctx: &ThemeContext) -> Line<'static> {
    Line::from(Span::styled(
        label.to_string(),
        ctx.apply(Style::default().fg(ctx.state_selected()).bold()),
    ))
}

fn key_value_line(label: &str, value: String, ctx: &ThemeContext) -> Line<'static> {
    Line::from(vec![
        Span::styled(
            format!("{label}: "),
            ctx.apply(Style::default().fg(ctx.theme.semantic.subtext1)),
        ),
        Span::raw(sanitize_text(&value)),
    ])
}

fn restriction_reason_detail(reason: &PeerRestrictionReason) -> String {
    match reason {
        PeerRestrictionReason::ExcessiveUpload {
            uploaded_bytes,
            threshold_bytes,
        } => format!(
            "Excessive upload: {} / {}",
            format_bytes(*uploaded_bytes),
            format_bytes(*threshold_bytes)
        ),
        PeerRestrictionReason::ExcessiveDownload {
            downloaded_bytes,
            threshold_bytes,
        } => format!(
            "Excessive download: {} / {}",
            format_bytes(*downloaded_bytes),
            format_bytes(*threshold_bytes)
        ),
        PeerRestrictionReason::ReconnectChurn {
            reconnects,
            threshold,
            window_secs,
        } => format!(
            "Reconnect churn: {reconnects}/{threshold} in {}",
            compact_duration(Duration::from_secs(*window_secs))
        ),
        PeerRestrictionReason::Manual => "Manual restriction".to_string(),
    }
}

fn draw_peer_footer(f: &mut Frame, app_state: &AppState, area: Rect, ctx: &ThemeContext) {
    if area.height == 0 {
        return;
    }
    let mut spans = Vec::new();
    let mut push = |key: &str, action: &str, tone: ActionTone| {
        spans.push(Span::styled(
            format!("[{key}]"),
            footer_key_style(ctx, tone),
        ));
        spans.push(Span::styled(
            action.to_string(),
            ctx.apply(Style::default().fg(ctx.theme.semantic.subtext0)),
        ));
        spans.push(Span::styled(
            " | ",
            ctx.apply(Style::default().fg(ctx.theme.semantic.overlay0)),
        ));
    };

    if app_state.ui.peer_management.is_searching {
        push("Enter", "apply", ActionTone::Confirm);
        push("Tab", "mode", ActionTone::Mode);
        push("Esc", "clear", ActionTone::Cancel);
    } else if peer_details_overlay_active(app_state) {
        push("Enter/Esc", "table", ActionTone::Navigate);
        push("q", "back", ActionTone::Cancel);
    } else {
        push("arrows", "nav", ActionTone::Navigate);
        push("h/l", "column", ActionTone::Navigate);
        push("s", "ort", ActionTone::Sort);
        if peer_search_panel_active(app_state) {
            push("Tab", "mode", ActionTone::Mode);
            push("Shift+Tab", "filter", ActionTone::Mode);
        } else {
            push("Tab", "filter", ActionTone::Mode);
        }
        push("/", "search", ActionTone::Search);
        if peer_uses_details_overlay(app_state) {
            push("Enter", "details", ActionTone::Info);
        }
        push("x", "privacy", ActionTone::Toggle);
        if peer_search_panel_active(app_state) {
            push("Esc", "clear", ActionTone::Clear);
            push("q", "back", ActionTone::Cancel);
        } else {
            push("Esc", "back", ActionTone::Cancel);
        }
    }
    if !spans.is_empty() {
        spans.pop();
    }
    f.render_widget(
        Paragraph::new(Line::from(spans)).alignment(Alignment::Center),
        area,
    );
}

fn display_torrent_name(name: &str, privacy: bool) -> String {
    if !privacy {
        return sanitize_text(name);
    }
    let masked = anonymize_preserving_shape(name);
    if masked.is_empty() {
        "Torrent".to_string()
    } else {
        sanitize_text(&masked)
    }
}

fn display_ip(ip: IpAddr, privacy: bool) -> String {
    if !privacy {
        return ip.to_string();
    }
    format!("peer-{:04x}", stable_mask_id(&ip.to_string()))
}

fn display_endpoint(address: &str, privacy: bool) -> String {
    if !privacy {
        return sanitize_text(address);
    }
    format!("endpoint-{:04x}", stable_mask_id(address))
}

fn stable_mask_id(value: &str) -> u16 {
    let mut hash = 0xcbf29ce484222325u64;
    for byte in value.bytes() {
        hash ^= u64::from(byte);
        hash = hash.wrapping_mul(0x100000001b3);
    }
    (hash ^ (hash >> 32)) as u16
}

fn short_info_hash(hash: &[u8], privacy: bool) -> String {
    if privacy {
        return "hidden-hash".to_string();
    }
    let encoded = hex::encode(hash);
    encoded.chars().take(12).collect()
}

fn evidence_percent(observed: u64, threshold: u64) -> f64 {
    if threshold == 0 {
        0.0
    } else {
        observed as f64 * 100.0 / threshold as f64
    }
}

fn format_elapsed(now: SystemTime, time: SystemTime) -> String {
    let elapsed = now.duration_since(time).unwrap_or_default();
    if elapsed < Duration::from_secs(5) {
        "now".to_string()
    } else {
        format!("{} ago", compact_duration(elapsed))
    }
}

fn format_remaining(now: SystemTime, deadline: SystemTime) -> String {
    let remaining = deadline.duration_since(now).unwrap_or_default();
    if remaining.is_zero() {
        "expired".to_string()
    } else {
        format!("{} left", compact_duration(remaining))
    }
}

fn compact_duration(duration: Duration) -> String {
    let mut seconds = duration.as_secs();
    let days = seconds / 86_400;
    seconds %= 86_400;
    let hours = seconds / 3_600;
    seconds %= 3_600;
    let minutes = seconds / 60;
    let seconds = seconds % 60;
    if days > 0 {
        format!("{days}d {hours}h")
    } else if hours > 0 {
        format!("{hours}h {minutes}m")
    } else if minutes > 0 {
        format!("{minutes}m")
    } else {
        format!("{seconds}s")
    }
}

fn plural_suffix(count: usize) -> &'static str {
    if count == 1 {
        ""
    } else {
        "s"
    }
}

fn inner_rect(area: Rect) -> Rect {
    Rect::new(
        area.x.saturating_add(1),
        area.y.saturating_add(1),
        area.width.saturating_sub(2),
        area.height.saturating_sub(2),
    )
}

fn centered_line_rect(area: Rect) -> Rect {
    Rect::new(
        area.x,
        area.y + area.height.saturating_sub(1) / 2,
        area.width,
        1,
    )
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::peer_manager::{PeerManagerEndpointView, PeerManagerView, PeerPolicy};
    use std::collections::HashMap;
    use std::sync::Arc;

    fn test_now() -> SystemTime {
        SystemTime::UNIX_EPOCH + Duration::from_secs(10_000)
    }

    fn tracked_peer(ip: &str, name: &str, hash_seed: u8) -> PeerManagerTrackedPeer {
        PeerManagerTrackedPeer {
            torrent_info_hash: vec![hash_seed; 20],
            torrent_name: name.to_string(),
            ip: ip.parse().unwrap(),
            is_active: false,
            endpoints: Vec::new(),
            downloaded_evidence_bytes: 0,
            uploaded_evidence_bytes: 0,
            transfer_threshold_bytes: 100,
            reconnect_count: 0,
            reconnect_limit: 6,
            reconnect_window_secs: 300,
            last_seen: Some(test_now() - Duration::from_secs(30)),
        }
    }

    fn state_with_peers(peers: Vec<PeerManagerTrackedPeer>) -> AppState {
        let mut state = AppState {
            mode: AppMode::PeerManagement,
            ..Default::default()
        };
        state.peer_manager_view = Arc::new(PeerManagerView {
            registered_torrents: peers
                .iter()
                .map(|peer| peer.torrent_info_hash.clone())
                .collect::<BTreeSet<_>>()
                .len(),
            metrics_updates: 1,
            tracked_peers: peers,
        });
        state
    }

    fn restriction(blocked_until: SystemTime, reason: PeerRestrictionReason) -> PeerRestriction {
        PeerRestriction {
            detected_at: test_now() - Duration::from_secs(60),
            blocked_until,
            torrent_info_hash: None,
            reason,
        }
    }

    fn line_text(line: &Line<'_>) -> String {
        line.spans
            .iter()
            .map(|span| span.content.as_ref())
            .collect::<Vec<_>>()
            .concat()
    }

    fn take_peer_row_build_count() -> usize {
        PEER_ROW_BUILD_COUNT.with(|count| count.replace(0))
    }

    #[test]
    fn normalized_ip_rows_keep_per_torrent_evidence_separate() {
        let mut first = tracked_peer("192.0.2.10", "Quartz Archive", 1);
        first.uploaded_evidence_bytes = 60;
        let mut second = tracked_peer("::ffff:192.0.2.10", "Cinder Atlas", 2);
        second.downloaded_evidence_bytes = 55;
        let state = state_with_peers(vec![first, second]);

        let rows = build_peer_rows_at(&state, test_now());

        assert_eq!(rows.len(), 1);
        assert_eq!(rows[0].tracked.len(), 2);
        assert_eq!(rows[0].strongest_evidence().observed, 60);
        assert_eq!(rows[0].strongest_evidence().threshold, 100);
        assert_eq!(rows[0].strongest_evidence().compact_label(), "UL 60%");
    }

    #[test]
    fn policy_only_rows_include_only_live_restrictions() {
        let live_ip: IpAddr = "198.51.100.20".parse().unwrap();
        let expired_ip: IpAddr = "203.0.113.30".parse().unwrap();
        let mut state = state_with_peers(Vec::new());
        state.peer_policy = Arc::new(PeerPolicy {
            restrictions: HashMap::from([
                (
                    live_ip,
                    restriction(
                        test_now() + Duration::from_secs(300),
                        PeerRestrictionReason::Manual,
                    ),
                ),
                (
                    expired_ip,
                    restriction(test_now(), PeerRestrictionReason::Manual),
                ),
            ]),
        });

        let rows = build_peer_rows_at(&state, test_now());

        assert_eq!(rows.len(), 1);
        assert_eq!(rows[0].ip, live_ip);
        assert!(rows[0].tracked.is_empty());
        assert!(rows[0].is_restricted());
    }

    #[test]
    fn restriction_reason_preserves_trigger_after_counters_reset() {
        let ip: IpAddr = "192.0.2.44".parse().unwrap();
        let peer = tracked_peer("192.0.2.44", "Mica Field Notes", 3);
        let mut state = state_with_peers(vec![peer]);
        state.peer_policy = Arc::new(PeerPolicy {
            restrictions: HashMap::from([(
                ip,
                restriction(
                    test_now() + Duration::from_secs(600),
                    PeerRestrictionReason::ExcessiveDownload {
                        downloaded_bytes: 125,
                        threshold_bytes: 100,
                    },
                ),
            )]),
        });

        let evidence = build_peer_rows_at(&state, test_now())[0].strongest_evidence();

        assert_eq!(evidence.kind, EvidenceKind::Download);
        assert_eq!(evidence.observed, 125);
        assert!(evidence.from_policy);
    }

    #[test]
    fn state_filters_keep_restricted_peers_out_of_active() {
        let ip: IpAddr = "192.0.2.45".parse().unwrap();
        let mut peer = tracked_peer("192.0.2.45", "Ember Field Notes", 8);
        peer.is_active = true;
        let mut state = state_with_peers(vec![peer]);
        state.peer_policy = Arc::new(PeerPolicy {
            restrictions: HashMap::from([(
                ip,
                restriction(
                    test_now() + Duration::from_secs(600),
                    PeerRestrictionReason::Manual,
                ),
            )]),
        });

        state.ui.peer_management.filter = PeerManagementFilter::Active;
        assert!(build_peer_rows_at(&state, test_now()).is_empty());
        state.ui.peer_management.filter = PeerManagementFilter::Restricted;
        assert_eq!(build_peer_rows_at(&state, test_now()).len(), 1);
    }

    #[test]
    fn filters_and_both_search_modes_use_peer_manager_fields() {
        let mut active = tracked_peer("192.0.2.1", "Quartz Archive", 4);
        active.is_active = true;
        active.endpoints.push(PeerManagerEndpointView {
            address: "192.0.2.1:6881".to_string(),
            total_downloaded: 10,
            total_uploaded: 20,
        });
        let recent = tracked_peer("198.51.100.2", "Cinder Atlas", 5);
        let mut state = state_with_peers(vec![active, recent]);

        state.ui.peer_management.filter = PeerManagementFilter::Active;
        assert_eq!(build_peer_rows_at(&state, test_now()).len(), 1);

        state.ui.peer_management.filter = PeerManagementFilter::All;
        state.ui.peer_management.search_mode = SearchMode::Regex;
        state.ui.peer_management.search_query = "quartz|6881".to_string();
        assert_eq!(build_peer_rows_at(&state, test_now()).len(), 1);

        state.ui.peer_management.search_mode = SearchMode::Fuzzy;
        state.ui.peer_management.search_query = "CNDRATLS".to_string();
        assert_eq!(build_peer_rows_at(&state, test_now()).len(), 1);

        state.ui.peer_management.search_mode = SearchMode::Regex;
        state.ui.peer_management.search_query = "[".to_string();
        assert!(build_peer_rows_at(&state, test_now()).is_empty());
        assert_eq!(
            peer_search_error(&state).as_deref(),
            Some("Invalid regular expression")
        );
    }

    #[test]
    fn cursor_index_stays_fixed_when_sorting_reorders_rows() {
        let mut lower = tracked_peer("192.0.2.11", "Opal Ledger", 6);
        lower.uploaded_evidence_bytes = 10;
        let mut higher = tracked_peer("192.0.2.12", "Sable Ledger", 7);
        higher.uploaded_evidence_bytes = 90;
        let mut state = state_with_peers(vec![lower, higher]);
        state.ui.peer_management.selected_index = 1;
        state.ui.peer_management.selected_column_index = 3;
        state.ui.peer_management.sort_column_index = None;

        reduce_peer_management_action(&mut state, PeerManagementAction::SortBySelectedColumn);

        assert_eq!(state.ui.peer_management.selected_index, 1);
        assert_eq!(state.ui.peer_management.sort_column_index, Some(3));
        let rows = build_peer_rows_at(&state, test_now());
        assert_eq!(rows[1].ip, "192.0.2.11".parse::<IpAddr>().unwrap());
    }

    #[test]
    fn cursor_index_stays_fixed_when_live_peers_sort_before_it() {
        let first = tracked_peer("192.0.2.11", "Opal Ledger", 6);
        let second = tracked_peer("192.0.2.12", "Sable Ledger", 7);
        let mut state = state_with_peers(vec![first, second]);
        state.ui.peer_management.selected_index = 1;

        Arc::make_mut(&mut state.peer_manager_view)
            .tracked_peers
            .push(tracked_peer("192.0.2.1", "Cinder Ledger", 8));
        let rows = build_peer_rows_at(&state, test_now());

        assert_eq!(state.ui.peer_management.selected_index, 1);
        assert_eq!(selected_peer_index(&state, &rows), Some(1));
        assert_eq!(
            selected_peer_row(&state, &rows).unwrap().ip,
            "192.0.2.11".parse::<IpAddr>().unwrap()
        );
    }

    #[test]
    fn cursor_index_clamps_when_live_rows_shrink() {
        let first = tracked_peer("192.0.2.11", "Opal Ledger", 6);
        let second = tracked_peer("192.0.2.12", "Sable Ledger", 7);
        let mut state = state_with_peers(vec![first, second]);
        state.ui.peer_management.selected_index = 1;
        Arc::make_mut(&mut state.peer_manager_view)
            .tracked_peers
            .truncate(1);
        let rows = build_peer_rows_at(&state, test_now());

        assert_eq!(selected_peer_index(&state, &rows), Some(0));
    }

    #[test]
    fn navigation_derives_visible_rows_only_once_per_keypress() {
        let first = tracked_peer("192.0.2.11", "Cinder Atlas", 6);
        let second = tracked_peer("192.0.2.12", "Quartz Archive", 7);
        let mut state = state_with_peers(vec![first, second]);
        take_peer_row_build_count();

        reduce_peer_management_action(&mut state, PeerManagementAction::MoveDown);

        assert_eq!(take_peer_row_build_count(), 1);
        assert_eq!(state.ui.peer_management.selected_index, 1);
    }

    #[test]
    fn torrents_are_numeric_and_sort_by_count_by_default() {
        let two_first = tracked_peer("192.0.2.21", "Zephyr Notebook", 9);
        let one = tracked_peer("192.0.2.22", "Amber Notebook", 10);
        let two_second = tracked_peer("192.0.2.21", "Cinder Notebook", 11);
        let state = state_with_peers(vec![two_first, one, two_second]);

        assert_eq!(state.ui.peer_management.selected_column_index, 2);
        assert_eq!(state.ui.peer_management.sort_column_index, Some(2));
        assert_eq!(
            state.ui.peer_management.sort_direction,
            SortDirection::Descending
        );
        let rows = build_peer_rows_at(&state, test_now());
        assert_eq!(rows[0].torrent_count, 2);
        assert_eq!(rows[1].torrent_count, 1);
        assert_eq!(peer_torrents_label(&rows[0]), "2");
        assert_eq!(peer_torrents_label(&rows[1]), "1");
    }

    #[test]
    fn peer_details_separate_titles_and_explain_reconnect_evidence() {
        let mut peer = tracked_peer("192.0.2.23", "Copper Notebook", 11);
        peer.reconnect_count = 3;
        let state = state_with_peers(vec![peer.clone()]);
        let ctx = ThemeContext::new(state.theme, 0.0);

        let lines = tracked_peer_detail_lines(&peer, false, &ctx);
        assert!(line_text(&lines[0]).contains("Copper Notebook — "));
        assert_eq!(
            line_text(&lines[2]),
            "  Reconnects: 3 observed, 6 limit (5m window)"
        );

        let evidence = PeerEvidence {
            kind: EvidenceKind::Reconnect,
            observed: 3,
            threshold: 6,
            from_policy: false,
        };
        assert_eq!(evidence.compact_label(), "Reconnect 3/6");
    }

    #[test]
    fn search_panel_keys_take_precedence_while_visible() {
        let mut state = state_with_peers(Vec::new());
        assert_eq!(
            map_key_to_peer_management_action(KeyCode::Tab, &state),
            Some(PeerManagementAction::FilterNext)
        );

        state.ui.peer_management.is_searching = true;
        assert_eq!(
            map_key_to_peer_management_action(KeyCode::Tab, &state),
            Some(PeerManagementAction::ToggleSearchMode)
        );
        assert_eq!(
            map_key_to_peer_management_action(KeyCode::Esc, &state),
            Some(PeerManagementAction::SearchCancel)
        );

        state.ui.peer_management.is_searching = false;
        state.ui.peer_management.search_query = "quartz".to_string();
        assert_eq!(
            map_key_to_peer_management_action(KeyCode::Tab, &state),
            Some(PeerManagementAction::ToggleSearchMode)
        );
    }

    #[test]
    fn privacy_masks_numeric_peer_identity_and_endpoints() {
        let ip: IpAddr = "203.0.113.55".parse().unwrap();
        let address = "203.0.113.55:51413";

        let masked_ip = display_ip(ip, true);
        let masked_endpoint = display_endpoint(address, true);

        assert!(!masked_ip.contains("203.0.113.55"));
        assert!(!masked_endpoint.contains("203.0.113.55"));
        assert_ne!(masked_ip, display_ip(ip, false));
        assert_ne!(masked_endpoint, display_endpoint(address, false));

        let mut state = state_with_peers(Vec::new());
        state.ui.peer_management.search_query = address.to_string();
        reduce_peer_management_action(&mut state, PeerManagementAction::TogglePrivacy);
        assert!(state.anonymize_torrent_names);
        assert!(state.ui.peer_management.search_query.is_empty());
        assert!(!state.ui.peer_management.is_searching);
    }

    #[test]
    fn evidence_ratios_are_compared_without_truncation() {
        let left = PeerEvidence {
            kind: EvidenceKind::Upload,
            observed: 1_000_001,
            threshold: 1_000_000,
            from_policy: false,
        };
        let right = PeerEvidence {
            kind: EvidenceKind::Download,
            observed: 1_000_000,
            threshold: 999_999,
            from_policy: false,
        };

        assert_eq!(compare_evidence_ratio(&left, &right), Ordering::Less);
    }

    #[test]
    fn narrow_table_keeps_state_address_and_evidence() {
        let (_, visible) = compute_visible_peer_management_columns(64);
        let columns = peer_columns();
        let ids = visible
            .into_iter()
            .map(|index| columns[index].id)
            .collect::<Vec<_>>();

        assert!(ids.contains(&PeerColumnId::State));
        assert!(ids.contains(&PeerColumnId::Address));
        assert!(ids.contains(&PeerColumnId::Evidence));
        assert!(!ids.contains(&PeerColumnId::Torrents));
    }
}
