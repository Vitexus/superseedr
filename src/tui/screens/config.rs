// SPDX-FileCopyrightText: 2025 The superseedr Contributors
// SPDX-License-Identifier: GPL-3.0-or-later

use std::sync::Arc;

use crate::app::{AppCommand, AppMode, ConfigItem, ConfigPane, FileBrowserMode};
use crate::config::Settings;
use crate::token_bucket::{rate_limit_bps_to_bucket_bytes_per_sec, TokenBucket};
use crate::tui::action_style::{footer_key_style, ActionTone};
use crate::tui::app_command::spawn_app_command_sender;
use crate::tui::formatters::{format_limit_bps, path_to_string};
use crate::tui::layout::config::{calculate_config_layout, ConfigLayoutKind};
use crate::tui::screen_context::ScreenContext;
use directories::UserDirs;
use ratatui::crossterm::event::{Event as CrosstermEvent, KeyCode, KeyEventKind};
use ratatui::layout::{Alignment, Constraint, Direction, Layout};
use ratatui::prelude::{Frame, Line, Span, Style};
use ratatui::widgets::{Block, Borders, Clear, Paragraph};
use tokio::sync::{broadcast, mpsc};

const RATE_LIMIT_STEP_BPS: u64 = 10_000 * 8;
const UNLIMITED_RATE_LIMIT_BPS: u64 = crate::config::UNLIMITED_RATE_LIMIT_BPS;

#[derive(Clone, Debug, PartialEq)]
pub enum ConfigAction {
    SaveAndExit,
    StartEditOrBrowse,
    ToggleSelectedBool,
    SetSelectedBool(bool),
    MoveUp,
    MoveDown,
    ToggleFocus,
    ResetSelected,
    IncreaseSelected,
    DecreaseSelected,
    EditInsert(char),
    EditBackspace,
    EditCancel,
    EditCommit,
}

pub enum ConfigEffect {
    AppCommand(Box<AppCommand>),
    SetDownloadRate(u64),
    SetUploadRate(u64),
    ToNormal,
}

pub struct ConfigHandleContext<'a> {
    pub mode: &'a mut AppMode,
    pub settings_edit: &'a mut Box<Settings>,
    pub selected_index: &'a mut usize,
    pub items: &'a mut [ConfigItem],
    pub active_pane: &'a mut ConfigPane,
    pub editing: &'a mut Option<(ConfigItem, String)>,
    pub app_command_tx: &'a mpsc::Sender<AppCommand>,
    pub shutdown_tx: &'a broadcast::Sender<()>,
    pub file_browser_generation: &'a mut u64,
    pub global_dl_bucket: &'a Arc<TokenBucket>,
    pub global_ul_bucket: &'a Arc<TokenBucket>,
}

#[derive(Default)]
pub struct ConfigReduceResult {
    pub consumed: bool,
    pub effects: Vec<ConfigEffect>,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum ConfigCategory {
    Paths,
    Downloads,
    Network,
    Ui,
}

impl ConfigCategory {
    fn label(self) -> &'static str {
        match self {
            Self::Paths => "Paths",
            Self::Downloads => "Downloads",
            Self::Network => "Network",
            Self::Ui => "UI",
        }
    }

    fn all() -> &'static [Self] {
        &[Self::Network, Self::Paths, Self::Downloads, Self::Ui]
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum ConfigControlKind {
    Bool,
    Enum,
    Number,
    RateLimit,
    Path,
}

#[derive(Clone, Copy, Debug)]
struct ConfigSettingDescriptor {
    item: ConfigItem,
    category: ConfigCategory,
    label: &'static str,
    control: ConfigControlKind,
}

fn config_setting_descriptors() -> &'static [ConfigSettingDescriptor] {
    &[
        ConfigSettingDescriptor {
            item: ConfigItem::ClientPort,
            category: ConfigCategory::Network,
            label: "Listen Port",
            control: ConfigControlKind::Number,
        },
        ConfigSettingDescriptor {
            item: ConfigItem::DefaultDownloadFolder,
            category: ConfigCategory::Paths,
            label: "Default Download Folder",
            control: ConfigControlKind::Path,
        },
        ConfigSettingDescriptor {
            item: ConfigItem::WatchFolder,
            category: ConfigCategory::Paths,
            label: "Torrent Watch Folder",
            control: ConfigControlKind::Path,
        },
        ConfigSettingDescriptor {
            item: ConfigItem::UiLayoutMode,
            category: ConfigCategory::Ui,
            label: "Layout",
            control: ConfigControlKind::Enum,
        },
        ConfigSettingDescriptor {
            item: ConfigItem::AlwaysShowAddLocationPrompt,
            category: ConfigCategory::Downloads,
            label: "Confirm Add Priority And Location",
            control: ConfigControlKind::Bool,
        },
        ConfigSettingDescriptor {
            item: ConfigItem::GlobalDownloadLimit,
            category: ConfigCategory::Downloads,
            label: "Global Download Limit",
            control: ConfigControlKind::RateLimit,
        },
        ConfigSettingDescriptor {
            item: ConfigItem::GlobalUploadLimit,
            category: ConfigCategory::Downloads,
            label: "Global Upload Limit",
            control: ConfigControlKind::RateLimit,
        },
    ]
}

fn descriptor_for_item(item: ConfigItem) -> &'static ConfigSettingDescriptor {
    config_setting_descriptors()
        .iter()
        .find(|descriptor| descriptor.item == item)
        .expect("all config items must have descriptors")
}

fn config_category_for_item(item: ConfigItem) -> ConfigCategory {
    descriptor_for_item(item).category
}

fn selected_item(items: &[ConfigItem], selected_index: usize) -> ConfigItem {
    items
        .get(selected_index)
        .copied()
        .unwrap_or(ConfigItem::ClientPort)
}

fn value_for_item(item: ConfigItem, settings: &Settings) -> String {
    match item {
        ConfigItem::ClientPort => settings.client_port.to_string(),
        ConfigItem::DefaultDownloadFolder => {
            path_to_string(settings.default_download_folder.as_deref())
        }
        ConfigItem::WatchFolder => path_to_string(settings.watch_folder.as_deref()),
        ConfigItem::AlwaysShowAddLocationPrompt => {
            if settings.always_show_add_location_prompt {
                "Enabled".to_string()
            } else {
                "Disabled".to_string()
            }
        }
        ConfigItem::UiLayoutMode => settings.ui_layout_mode.label().to_string(),
        ConfigItem::GlobalDownloadLimit => format_limit_bps(settings.global_download_limit_bps),
        ConfigItem::GlobalUploadLimit => format_limit_bps(settings.global_upload_limit_bps),
    }
}

fn toggle_config_pane(active_pane: &mut ConfigPane) {
    *active_pane = match *active_pane {
        ConfigPane::Settings => ConfigPane::Details,
        ConfigPane::Details => ConfigPane::Settings,
    };
}

fn shared_path_is_manual(item: ConfigItem) -> bool {
    crate::config::is_shared_config_mode() && item == ConfigItem::DefaultDownloadFolder
}

fn increase_rate_limit_bps(current: u64) -> u64 {
    match current {
        0 => UNLIMITED_RATE_LIMIT_BPS,
        UNLIMITED_RATE_LIMIT_BPS => RATE_LIMIT_STEP_BPS,
        _ => current.saturating_add(RATE_LIMIT_STEP_BPS),
    }
}

fn decrease_rate_limit_bps(current: u64) -> u64 {
    match current {
        0 => 0,
        UNLIMITED_RATE_LIMIT_BPS => 0,
        _ => current
            .checked_sub(RATE_LIMIT_STEP_BPS)
            .filter(|new_rate| *new_rate > 0)
            .unwrap_or(UNLIMITED_RATE_LIMIT_BPS),
    }
}

fn map_key_to_config_action(
    key_code: KeyCode,
    editing: &Option<(ConfigItem, String)>,
) -> Option<ConfigAction> {
    if editing.is_some() {
        return match key_code {
            KeyCode::Char(c) if c.is_ascii_digit() => Some(ConfigAction::EditInsert(c)),
            KeyCode::Backspace => Some(ConfigAction::EditBackspace),
            KeyCode::Esc => Some(ConfigAction::EditCancel),
            KeyCode::Enter => Some(ConfigAction::EditCommit),
            _ => None,
        };
    }

    match key_code {
        KeyCode::Esc | KeyCode::Char('Q') => Some(ConfigAction::SaveAndExit),
        KeyCode::Enter => Some(ConfigAction::StartEditOrBrowse),
        KeyCode::Char(' ') => Some(ConfigAction::ToggleSelectedBool),
        KeyCode::Char('t') => Some(ConfigAction::SetSelectedBool(true)),
        KeyCode::Char('f') => Some(ConfigAction::SetSelectedBool(false)),
        KeyCode::Up | KeyCode::Char('k') => Some(ConfigAction::MoveUp),
        KeyCode::Down | KeyCode::Char('j') => Some(ConfigAction::MoveDown),
        KeyCode::Tab | KeyCode::BackTab => Some(ConfigAction::ToggleFocus),
        KeyCode::Char('r') => Some(ConfigAction::ResetSelected),
        KeyCode::Right | KeyCode::Char('l') => Some(ConfigAction::IncreaseSelected),
        KeyCode::Left | KeyCode::Char('h') => Some(ConfigAction::DecreaseSelected),
        _ => None,
    }
}

pub fn reduce_config_action(
    action: ConfigAction,
    settings_edit: &mut Box<Settings>,
    selected_index: &mut usize,
    items: &mut [ConfigItem],
    editing: &mut Option<(ConfigItem, String)>,
) -> ConfigReduceResult {
    let mut result = ConfigReduceResult::default();
    match action {
        ConfigAction::SaveAndExit => {
            result.consumed = true;
            result.effects.push(ConfigEffect::AppCommand(Box::new(
                AppCommand::UpdateConfig(*settings_edit.clone()),
            )));
            result.effects.push(ConfigEffect::ToNormal);
        }
        ConfigAction::StartEditOrBrowse => {
            result.consumed = true;
            let selected_item = items[*selected_index];
            match selected_item {
                ConfigItem::GlobalDownloadLimit
                | ConfigItem::GlobalUploadLimit
                | ConfigItem::ClientPort => {
                    *editing = Some((selected_item, String::new()));
                }
                ConfigItem::AlwaysShowAddLocationPrompt => {
                    settings_edit.always_show_add_location_prompt =
                        !settings_edit.always_show_add_location_prompt;
                }
                ConfigItem::UiLayoutMode => {
                    settings_edit.ui_layout_mode = settings_edit.ui_layout_mode.next();
                }
                ConfigItem::DefaultDownloadFolder | ConfigItem::WatchFolder => {
                    if shared_path_is_manual(selected_item) {
                        return result;
                    }
                    let initial_path = if selected_item == ConfigItem::WatchFolder {
                        settings_edit.watch_folder.clone()
                    } else {
                        settings_edit.default_download_folder.clone()
                    }
                    .unwrap_or_else(|| {
                        UserDirs::new()
                            .and_then(|ud| ud.download_dir().map(|p| p.to_path_buf()))
                            .unwrap_or_else(|| std::path::PathBuf::from("."))
                    });

                    result.effects.push(ConfigEffect::AppCommand(Box::new(
                        AppCommand::FetchFileTree {
                            browser_generation: 0,
                            path: initial_path,
                            browser_mode: FileBrowserMode::ConfigPathSelection {
                                target_item: selected_item,
                                current_settings: settings_edit.clone(),
                                selected_index: *selected_index,
                                items: items.to_vec(),
                            },
                            preserve_browser_mode: false,
                            highlight_path: None,
                        },
                    )));
                }
            }
        }
        ConfigAction::ToggleSelectedBool => {
            result.consumed = true;
            if items[*selected_index] == ConfigItem::AlwaysShowAddLocationPrompt {
                settings_edit.always_show_add_location_prompt =
                    !settings_edit.always_show_add_location_prompt;
            }
        }
        ConfigAction::SetSelectedBool(value) => {
            result.consumed = true;
            if items[*selected_index] == ConfigItem::AlwaysShowAddLocationPrompt {
                settings_edit.always_show_add_location_prompt = value;
            }
        }
        ConfigAction::MoveUp => {
            result.consumed = true;
            *selected_index = previous_visible_setting_index(items, *selected_index);
        }
        ConfigAction::MoveDown => {
            result.consumed = true;
            *selected_index = next_visible_setting_index(items, *selected_index);
        }
        ConfigAction::ToggleFocus => {
            result.consumed = true;
        }
        ConfigAction::ResetSelected => {
            result.consumed = true;
            let default_settings = Settings::default();
            let selected_item = items[*selected_index];
            match selected_item {
                ConfigItem::ClientPort => {
                    settings_edit.client_port = default_settings.client_port;
                }
                ConfigItem::DefaultDownloadFolder => {
                    if !shared_path_is_manual(selected_item) {
                        settings_edit.default_download_folder =
                            default_settings.default_download_folder;
                    }
                }
                ConfigItem::WatchFolder => {
                    settings_edit.watch_folder = default_settings.watch_folder;
                }
                ConfigItem::AlwaysShowAddLocationPrompt => {
                    settings_edit.always_show_add_location_prompt =
                        default_settings.always_show_add_location_prompt;
                }
                ConfigItem::UiLayoutMode => {
                    settings_edit.ui_layout_mode = default_settings.ui_layout_mode;
                }
                ConfigItem::GlobalDownloadLimit => {
                    settings_edit.global_download_limit_bps =
                        default_settings.global_download_limit_bps;
                }
                ConfigItem::GlobalUploadLimit => {
                    settings_edit.global_upload_limit_bps =
                        default_settings.global_upload_limit_bps;
                }
            }
        }
        ConfigAction::IncreaseSelected => {
            result.consumed = true;
            let item = items[*selected_index];
            match item {
                ConfigItem::GlobalDownloadLimit => {
                    let new_rate = increase_rate_limit_bps(settings_edit.global_download_limit_bps);
                    settings_edit.global_download_limit_bps = new_rate;
                    result.effects.push(ConfigEffect::SetDownloadRate(new_rate));
                }
                ConfigItem::GlobalUploadLimit => {
                    let new_rate = increase_rate_limit_bps(settings_edit.global_upload_limit_bps);
                    settings_edit.global_upload_limit_bps = new_rate;
                    result.effects.push(ConfigEffect::SetUploadRate(new_rate));
                }
                ConfigItem::UiLayoutMode => {
                    settings_edit.ui_layout_mode = settings_edit.ui_layout_mode.next();
                }
                _ => {}
            }
        }
        ConfigAction::DecreaseSelected => {
            result.consumed = true;
            let item = items[*selected_index];
            match item {
                ConfigItem::GlobalDownloadLimit => {
                    let new_rate = decrease_rate_limit_bps(settings_edit.global_download_limit_bps);
                    settings_edit.global_download_limit_bps = new_rate;
                    result.effects.push(ConfigEffect::SetDownloadRate(new_rate));
                }
                ConfigItem::GlobalUploadLimit => {
                    let new_rate = decrease_rate_limit_bps(settings_edit.global_upload_limit_bps);
                    settings_edit.global_upload_limit_bps = new_rate;
                    result.effects.push(ConfigEffect::SetUploadRate(new_rate));
                }
                ConfigItem::UiLayoutMode => {
                    settings_edit.ui_layout_mode = settings_edit.ui_layout_mode.previous();
                }
                _ => {}
            }
        }
        ConfigAction::EditInsert(c) => {
            result.consumed = true;
            if let Some((_item, buffer)) = editing {
                buffer.push(c);
            }
        }
        ConfigAction::EditBackspace => {
            result.consumed = true;
            if let Some((_item, buffer)) = editing {
                buffer.pop();
            }
        }
        ConfigAction::EditCancel => {
            result.consumed = true;
            *editing = None;
        }
        ConfigAction::EditCommit => {
            result.consumed = true;
            if let Some((item, buffer)) = editing {
                let mut committed = false;
                match item {
                    ConfigItem::ClientPort => {
                        if let Ok(new_port) = buffer.parse::<u16>() {
                            if new_port > 0 {
                                settings_edit.client_port = new_port;
                                committed = true;
                            }
                        }
                    }
                    ConfigItem::GlobalDownloadLimit => {
                        if let Ok(new_rate) = buffer.parse::<u64>() {
                            settings_edit.global_download_limit_bps = new_rate;
                            result.effects.push(ConfigEffect::SetDownloadRate(new_rate));
                            committed = true;
                        }
                    }
                    ConfigItem::GlobalUploadLimit => {
                        if let Ok(new_rate) = buffer.parse::<u64>() {
                            settings_edit.global_upload_limit_bps = new_rate;
                            result.effects.push(ConfigEffect::SetUploadRate(new_rate));
                            committed = true;
                        }
                    }
                    _ => {
                        committed = true;
                    }
                }
                if committed {
                    *editing = None;
                }
            }
        }
    }
    result
}

#[derive(Clone, Copy, Debug, PartialEq)]
enum ConfigListRow {
    Category(ConfigCategory),
    Setting {
        global_index: usize,
        item: ConfigItem,
    },
}

fn config_list_rows(items: &[ConfigItem]) -> Vec<ConfigListRow> {
    let mut rows = Vec::new();
    for category in ConfigCategory::all() {
        let category_items = items
            .iter()
            .copied()
            .enumerate()
            .filter(|(_, item)| config_category_for_item(*item) == *category)
            .collect::<Vec<_>>();
        if category_items.is_empty() {
            continue;
        }
        rows.push(ConfigListRow::Category(*category));
        rows.extend(
            category_items
                .into_iter()
                .map(|(global_index, item)| ConfigListRow::Setting { global_index, item }),
        );
    }
    rows
}

fn visible_setting_indices(items: &[ConfigItem]) -> Vec<usize> {
    config_list_rows(items)
        .into_iter()
        .filter_map(|row| match row {
            ConfigListRow::Setting { global_index, .. } => Some(global_index),
            ConfigListRow::Category(_) => None,
        })
        .collect()
}

fn next_visible_setting_index(items: &[ConfigItem], selected_index: usize) -> usize {
    let visible_indices = visible_setting_indices(items);
    visible_indices
        .iter()
        .position(|index| *index == selected_index)
        .and_then(|position| visible_indices.get(position + 1).copied())
        .unwrap_or(selected_index)
}

fn previous_visible_setting_index(items: &[ConfigItem], selected_index: usize) -> usize {
    let visible_indices = visible_setting_indices(items);
    visible_indices
        .iter()
        .position(|index| *index == selected_index)
        .and_then(|position| position.checked_sub(1))
        .and_then(|position| visible_indices.get(position).copied())
        .unwrap_or(selected_index)
}

fn settings_focus_moves_to_details(action: &ConfigAction) -> bool {
    matches!(
        action,
        ConfigAction::StartEditOrBrowse
            | ConfigAction::ToggleSelectedBool
            | ConfigAction::SetSelectedBool(_)
            | ConfigAction::ResetSelected
    )
}

fn settings_pane_ignores_action(action: &ConfigAction) -> bool {
    matches!(
        action,
        ConfigAction::IncreaseSelected | ConfigAction::DecreaseSelected
    )
}

fn controls_pane_ignores_action(action: &ConfigAction) -> bool {
    matches!(action, ConfigAction::MoveUp | ConfigAction::MoveDown)
}

fn pane_content_margin(area: ratatui::layout::Rect) -> ratatui::layout::Margin {
    ratatui::layout::Margin {
        horizontal: if area.width >= 70 {
            3
        } else if area.width >= 30 {
            2
        } else {
            1
        },
        vertical: if area.height >= 18 { 2 } else { 1 },
    }
}

fn settings_pane_content_area(area: ratatui::layout::Rect) -> ratatui::layout::Rect {
    let margin = pane_content_margin(area);
    area.inner(ratatui::layout::Margin {
        horizontal: margin.horizontal,
        vertical: 1,
    })
}

struct ConfigRenderContext<'a, 'b> {
    screen: &'a ScreenContext<'b>,
    settings: &'a Settings,
    editing: &'a Option<(ConfigItem, String)>,
    layout_kind: ConfigLayoutKind,
}

pub fn draw(
    f: &mut Frame,
    screen: &ScreenContext<'_>,
    settings: &Settings,
    selected_index: usize,
    items: &[ConfigItem],
    active_pane: ConfigPane,
    editing: &Option<(ConfigItem, String)>,
) {
    let ctx = screen.theme;
    let plan = calculate_config_layout(f.area(), settings.ui_layout_mode);
    f.render_widget(Clear, f.area());

    let active_item = selected_item(items, selected_index);
    let active_descriptor = descriptor_for_item(active_item);

    let render_ctx = ConfigRenderContext {
        screen,
        settings,
        editing,
        layout_kind: plan.kind,
    };
    render_settings_pane(
        f,
        &render_ctx,
        items,
        selected_index,
        plan.list_pane,
        active_pane == ConfigPane::Settings,
    );
    render_details_pane(
        f,
        &render_ctx,
        active_item,
        active_descriptor,
        plan.details_pane,
        active_pane == ConfigPane::Details,
    );

    let help_text = if editing.is_some() {
        Line::from(vec![
            Span::styled("[Enter]", footer_key_style(ctx, ActionTone::Confirm)),
            Span::raw(" to confirm, "),
            Span::styled("[Esc]", footer_key_style(ctx, ActionTone::Cancel)),
            Span::raw(" to cancel."),
        ])
    } else if active_pane == ConfigPane::Settings {
        Line::from(vec![
            Span::styled("↑/↓/k/j", footer_key_style(ctx, ActionTone::Navigate)),
            Span::raw(" select, "),
            Span::styled("[Enter]|[Tab]", footer_key_style(ctx, ActionTone::Navigate)),
            Span::raw(" controls, "),
            Span::styled("[Esc]|[Q]", footer_key_style(ctx, ActionTone::Confirm)),
            Span::raw(" Save & Exit."),
        ])
    } else {
        Line::from(vec![
            Span::styled("[Tab]", footer_key_style(ctx, ActionTone::Navigate)),
            Span::raw(" settings, "),
            Span::styled("[Enter]", footer_key_style(ctx, ActionTone::Edit)),
            Span::raw(" edit/open, "),
            Span::styled("←/→", footer_key_style(ctx, ActionTone::Toggle)),
            Span::raw(" adjust, "),
            Span::styled("[r]", footer_key_style(ctx, ActionTone::Clear)),
            Span::raw("eset, "),
            Span::styled("[Esc]|[Q]", footer_key_style(ctx, ActionTone::Confirm)),
            Span::raw(" Save & Exit."),
        ])
    };

    let footer_paragraph = Paragraph::new(help_text)
        .alignment(Alignment::Center)
        .style(ctx.apply(Style::default().fg(ctx.theme.semantic.subtext1)));
    f.render_widget(footer_paragraph, plan.footer_area);
}

fn render_settings_pane(
    f: &mut Frame,
    render_ctx: &ConfigRenderContext<'_, '_>,
    items: &[ConfigItem],
    selected_index: usize,
    area: ratatui::layout::Rect,
    focused: bool,
) {
    let ctx = render_ctx.screen.theme;
    let editing = render_ctx.editing;
    let layout_kind = render_ctx.layout_kind;
    let border_style = if focused {
        ctx.apply(Style::default().fg(ctx.state_selected()))
    } else {
        ctx.apply(Style::default().fg(ctx.theme.semantic.border))
    };
    let inner = settings_pane_content_area(area);
    let block = Block::default()
        .borders(Borders::ALL)
        .border_style(border_style);
    f.render_widget(block, area);

    let rows_model = config_list_rows(items);
    let constraints = rows_model
        .iter()
        .map(|row| match row {
            ConfigListRow::Category(_) => Constraint::Length(1),
            ConfigListRow::Setting { .. } if layout_kind == ConfigLayoutKind::Compact => {
                Constraint::Length(2)
            }
            ConfigListRow::Setting { .. } => Constraint::Length(1),
        })
        .collect::<Vec<_>>();
    let rows = Layout::default()
        .direction(Direction::Vertical)
        .constraints(constraints)
        .split(inner);

    for (row_model, row_area) in rows_model.iter().zip(rows.iter()) {
        match row_model {
            ConfigListRow::Category(category) => {
                let line = Line::from(vec![Span::styled(
                    category.label(),
                    ctx.apply(Style::default().fg(ctx.state_selected())),
                )]);
                f.render_widget(Paragraph::new(line), *row_area);
            }
            ConfigListRow::Setting { global_index, item } => {
                let descriptor = descriptor_for_item(*item);
                let is_highlighted = if let Some((edited_item, _)) = editing {
                    *edited_item == *item
                } else {
                    *global_index == selected_index
                };
                let row_style = if is_highlighted {
                    ctx.apply(Style::default().fg(ctx.state_warning()))
                } else {
                    ctx.apply(Style::default().fg(ctx.theme.semantic.text))
                };
                let marker = if is_highlighted { "▶" } else { " " };
                let line = if layout_kind == ConfigLayoutKind::Compact {
                    format!("{marker} {}\n", descriptor.label)
                } else {
                    format!("{marker} {}", descriptor.label)
                };
                f.render_widget(Paragraph::new(line).style(row_style), *row_area);
            }
        }
    }
}

fn render_details_pane(
    f: &mut Frame,
    render_ctx: &ConfigRenderContext<'_, '_>,
    active_item: ConfigItem,
    active_descriptor: &ConfigSettingDescriptor,
    area: ratatui::layout::Rect,
    focused: bool,
) {
    let ctx = render_ctx.screen.theme;
    let settings = render_ctx.settings;
    let editing = render_ctx.editing;
    let border_style = if focused {
        ctx.apply(Style::default().fg(ctx.state_selected()))
    } else {
        ctx.apply(Style::default().fg(ctx.theme.semantic.border))
    };
    let shared_path_notice =
        crate::config::is_shared_config_mode() && active_item == ConfigItem::DefaultDownloadFolder;
    let value = if let Some((edited_item, buffer)) = editing {
        if *edited_item == active_item {
            format!("[{buffer}]")
        } else {
            value_for_item(active_item, settings)
        }
    } else {
        value_for_item(active_item, settings)
    };
    let inner = area.inner(ratatui::layout::Margin {
        horizontal: 2,
        vertical: 1,
    });

    let block = Block::default()
        .borders(Borders::ALL)
        .border_style(border_style);
    f.render_widget(block, area);

    let mut lines = vec![Line::from(vec![
        Span::raw("Value: "),
        Span::styled(value, ctx.apply(Style::default().fg(ctx.state_warning()))),
    ])];

    if let Some((edited_item, buffer)) = editing {
        if *edited_item == active_item {
            lines.push(Line::from(format!("Editing: {buffer}")));
        }
    }

    if shared_path_notice {
        let settings_label = crate::config::shared_settings_path()
            .map(|path| path.to_string_lossy().to_string())
            .unwrap_or_else(|| "settings.toml".to_string());
        lines.push(Line::from(""));
        lines.push(Line::from(vec![
            Span::raw("Shared mode: edit this value in "),
            Span::styled(
                settings_label,
                ctx.apply(Style::default().fg(ctx.state_warning())),
            ),
        ]));
    }

    lines.push(Line::from(""));
    lines.push(Line::from(control_hint(
        active_descriptor.control,
        active_item,
    )));

    let input =
        Paragraph::new(lines).style(ctx.apply(Style::default().fg(ctx.theme.semantic.text)));
    f.render_widget(input, inner);

    if let Some((edited_item, buffer)) = editing {
        if *edited_item == active_item {
            let cursor_x = inner
                .x
                .saturating_add(8)
                .saturating_add(buffer.len() as u16);
            f.set_cursor_position((
                cursor_x.min(inner.x + inner.width.saturating_sub(1)),
                inner.y.saturating_add(1),
            ));
        }
    }
}

fn control_hint(control: ConfigControlKind, item: ConfigItem) -> &'static str {
    match control {
        ConfigControlKind::Bool => "Space/Enter toggles. t enables. f disables.",
        ConfigControlKind::Enum => "←/→ cycles choices. Enter advances to the next choice.",
        ConfigControlKind::Number => "Enter edits exact numeric value. r resets to default.",
        ConfigControlKind::RateLimit => {
            "←/→ changes in steps. Enter edits exact bytes/sec. r resets."
        }
        ConfigControlKind::Path if shared_path_is_manual(item) => {
            "Managed by shared config. Host-local settings still save here."
        }
        ConfigControlKind::Path => "Enter opens the directory picker. r resets to default.",
    }
}

pub fn handle_event(event: CrosstermEvent, ctx: ConfigHandleContext<'_>) -> bool {
    if let CrosstermEvent::Key(key) = event {
        if key.kind != KeyEventKind::Press {
            return false;
        }
        if let Some(action) = map_key_to_config_action(key.code, ctx.editing) {
            if action == ConfigAction::ToggleFocus {
                toggle_config_pane(ctx.active_pane);
                return true;
            }
            if *ctx.active_pane == ConfigPane::Details && controls_pane_ignores_action(&action) {
                return true;
            }
            if *ctx.active_pane == ConfigPane::Settings && settings_pane_ignores_action(&action) {
                return true;
            }
            if *ctx.active_pane == ConfigPane::Settings && settings_focus_moves_to_details(&action)
            {
                *ctx.active_pane = ConfigPane::Details;
                return true;
            }
            let reduced = reduce_config_action(
                action,
                ctx.settings_edit,
                ctx.selected_index,
                ctx.items,
                ctx.editing,
            );
            for effect in reduced.effects {
                match effect {
                    ConfigEffect::AppCommand(command) => {
                        let mut command = *command;
                        if let AppCommand::FetchFileTree {
                            browser_generation, ..
                        } = &mut command
                        {
                            *ctx.file_browser_generation =
                                ctx.file_browser_generation.wrapping_add(1);
                            *browser_generation = *ctx.file_browser_generation;
                        }
                        spawn_app_command_sender(
                            ctx.app_command_tx.clone(),
                            ctx.shutdown_tx.subscribe(),
                            command,
                        );
                    }
                    ConfigEffect::SetDownloadRate(new_rate) => {
                        let bucket = ctx.global_dl_bucket.clone();
                        tokio::spawn(async move {
                            bucket.set_rate(rate_limit_bps_to_bucket_bytes_per_sec(new_rate));
                        });
                    }
                    ConfigEffect::SetUploadRate(new_rate) => {
                        let bucket = ctx.global_ul_bucket.clone();
                        tokio::spawn(async move {
                            bucket.set_rate(rate_limit_bps_to_bucket_bytes_per_sec(new_rate));
                        });
                    }
                    ConfigEffect::ToNormal => {
                        *ctx.file_browser_generation = ctx.file_browser_generation.wrapping_add(1);
                        *ctx.mode = AppMode::Normal;
                    }
                }
            }
            return reduced.consumed;
        }
    }

    false
}

#[cfg(test)]
mod tests {
    use super::*;
    use strum::IntoEnumIterator;

    fn config_items() -> Vec<ConfigItem> {
        vec![
            ConfigItem::ClientPort,
            ConfigItem::DefaultDownloadFolder,
            ConfigItem::WatchFolder,
            ConfigItem::UiLayoutMode,
            ConfigItem::AlwaysShowAddLocationPrompt,
            ConfigItem::GlobalDownloadLimit,
            ConfigItem::GlobalUploadLimit,
        ]
    }

    #[test]
    fn descriptors_cover_existing_config_items_by_category() {
        let described_items = config_setting_descriptors()
            .iter()
            .map(|descriptor| descriptor.item)
            .collect::<Vec<_>>();

        assert_eq!(described_items.len(), ConfigItem::iter().count());
        for item in ConfigItem::iter() {
            assert!(
                described_items.contains(&item),
                "missing config descriptor for {item:?}"
            );
        }

        assert_eq!(
            config_category_for_item(ConfigItem::DefaultDownloadFolder),
            ConfigCategory::Paths
        );
        assert_eq!(
            config_category_for_item(ConfigItem::GlobalDownloadLimit),
            ConfigCategory::Downloads
        );
        assert_eq!(
            config_category_for_item(ConfigItem::ClientPort),
            ConfigCategory::Network
        );
        assert_eq!(
            config_category_for_item(ConfigItem::UiLayoutMode),
            ConfigCategory::Ui
        );
    }

    #[test]
    fn config_list_rows_group_settings_under_category_headers() {
        let rows = config_list_rows(&config_items());

        assert_eq!(rows[0], ConfigListRow::Category(ConfigCategory::Network));
        assert_eq!(
            rows[1],
            ConfigListRow::Setting {
                global_index: 0,
                item: ConfigItem::ClientPort,
            }
        );
        assert!(rows.contains(&ConfigListRow::Category(ConfigCategory::Paths)));
        assert!(rows.contains(&ConfigListRow::Category(ConfigCategory::Downloads)));
        assert!(rows.contains(&ConfigListRow::Category(ConfigCategory::Ui)));
    }

    #[test]
    fn visible_navigation_follows_grouped_category_order() {
        let items = config_items();

        assert_eq!(next_visible_setting_index(&items, 0), 1);
        assert_eq!(next_visible_setting_index(&items, 2), 4);
        assert_eq!(next_visible_setting_index(&items, 6), 3);
        assert_eq!(next_visible_setting_index(&items, 3), 3);
        assert_eq!(previous_visible_setting_index(&items, 3), 6);
        assert_eq!(previous_visible_setting_index(&items, 4), 2);
        assert_eq!(previous_visible_setting_index(&items, 0), 0);
    }

    #[test]
    fn pane_content_margin_adds_adaptive_padding() {
        let narrow = pane_content_margin(ratatui::layout::Rect::new(0, 0, 24, 10));
        assert_eq!(narrow.horizontal, 1);
        assert_eq!(narrow.vertical, 1);

        let medium = pane_content_margin(ratatui::layout::Rect::new(0, 0, 38, 16));
        assert_eq!(medium.horizontal, 2);
        assert_eq!(medium.vertical, 1);

        let roomy = pane_content_margin(ratatui::layout::Rect::new(0, 0, 80, 24));
        assert_eq!(roomy.horizontal, 3);
        assert_eq!(roomy.vertical, 2);
    }

    #[test]
    fn settings_pane_content_area_reserves_border_but_no_extra_top_padding() {
        let area = ratatui::layout::Rect::new(4, 5, 38, 16);
        let content = settings_pane_content_area(area);

        assert_eq!(content.x, area.x + 2);
        assert_eq!(content.y, area.y + 1);
        assert_eq!(content.height, area.height - 2);
    }

    #[test]
    fn settings_pane_action_controls_move_focus_to_details() {
        assert!(settings_focus_moves_to_details(
            &ConfigAction::StartEditOrBrowse
        ));
        assert!(!settings_focus_moves_to_details(
            &ConfigAction::IncreaseSelected
        ));
        assert!(!settings_focus_moves_to_details(&ConfigAction::MoveDown));
        assert!(!settings_focus_moves_to_details(&ConfigAction::SaveAndExit));
    }

    #[test]
    fn settings_pane_ignores_left_right_adjustment_actions() {
        assert!(settings_pane_ignores_action(
            &ConfigAction::IncreaseSelected
        ));
        assert!(settings_pane_ignores_action(
            &ConfigAction::DecreaseSelected
        ));
        assert!(!settings_pane_ignores_action(
            &ConfigAction::StartEditOrBrowse
        ));
        assert!(!settings_pane_ignores_action(&ConfigAction::MoveDown));
    }

    #[test]
    fn controls_pane_ignores_menu_navigation_actions() {
        assert!(controls_pane_ignores_action(&ConfigAction::MoveUp));
        assert!(controls_pane_ignores_action(&ConfigAction::MoveDown));
        assert!(!controls_pane_ignores_action(
            &ConfigAction::IncreaseSelected
        ));
        assert!(!controls_pane_ignores_action(&ConfigAction::SaveAndExit));
    }

    #[test]
    fn reducer_move_down_is_clamped() {
        let mut settings = Box::new(Settings::default());
        let mut idx = 0usize;
        let mut items = config_items();
        let mut editing = None;

        for _ in 0..10 {
            let _ = reduce_config_action(
                ConfigAction::MoveDown,
                &mut settings,
                &mut idx,
                items.as_mut_slice(),
                &mut editing,
            );
        }

        assert_eq!(idx, 3);
    }

    #[test]
    fn reducer_edit_commit_updates_download_limit_and_emits_effect() {
        let mut settings = Box::new(Settings::default());
        let mut idx = 5usize;
        let mut items = config_items();
        let mut editing = Some((ConfigItem::GlobalDownloadLimit, "123".to_string()));

        let out = reduce_config_action(
            ConfigAction::EditCommit,
            &mut settings,
            &mut idx,
            items.as_mut_slice(),
            &mut editing,
        );

        assert_eq!(settings.global_download_limit_bps, 123);
        assert_eq!(editing, None);
        assert_eq!(out.effects.len(), 1);
        assert!(matches!(out.effects[0], ConfigEffect::SetDownloadRate(123)));
    }

    #[test]
    fn reducer_rate_limit_arrows_keep_unlimited_as_sentinel() {
        let mut settings = Box::new(Settings::default());
        let mut idx = 5usize;
        let mut items = config_items();
        let mut editing = None;

        let out = reduce_config_action(
            ConfigAction::IncreaseSelected,
            &mut settings,
            &mut idx,
            items.as_mut_slice(),
            &mut editing,
        );
        assert_eq!(settings.global_download_limit_bps, RATE_LIMIT_STEP_BPS);
        assert!(matches!(
            out.effects.as_slice(),
            [ConfigEffect::SetDownloadRate(RATE_LIMIT_STEP_BPS)]
        ));

        let out = reduce_config_action(
            ConfigAction::DecreaseSelected,
            &mut settings,
            &mut idx,
            items.as_mut_slice(),
            &mut editing,
        );
        assert_eq!(settings.global_download_limit_bps, UNLIMITED_RATE_LIMIT_BPS);
        assert!(matches!(
            out.effects.as_slice(),
            [ConfigEffect::SetDownloadRate(UNLIMITED_RATE_LIMIT_BPS)]
        ));

        let out = reduce_config_action(
            ConfigAction::DecreaseSelected,
            &mut settings,
            &mut idx,
            items.as_mut_slice(),
            &mut editing,
        );
        assert_eq!(settings.global_download_limit_bps, 0);
        assert!(matches!(
            out.effects.as_slice(),
            [ConfigEffect::SetDownloadRate(0)]
        ));
    }

    #[test]
    fn reducer_upload_rate_decrease_from_small_cap_returns_to_unlimited() {
        let mut settings = Box::new(Settings::default());
        settings.global_upload_limit_bps = RATE_LIMIT_STEP_BPS / 2;
        let mut idx = 6usize;
        let mut items = config_items();
        let mut editing = None;

        let out = reduce_config_action(
            ConfigAction::DecreaseSelected,
            &mut settings,
            &mut idx,
            items.as_mut_slice(),
            &mut editing,
        );

        assert_eq!(settings.global_upload_limit_bps, UNLIMITED_RATE_LIMIT_BPS);
        assert!(matches!(
            out.effects.as_slice(),
            [ConfigEffect::SetUploadRate(UNLIMITED_RATE_LIMIT_BPS)]
        ));
    }

    #[test]
    fn reducer_boolean_row_accepts_toggle_true_and_false() {
        let mut settings = Box::new(Settings::default());
        let mut idx = 4usize;
        let mut items = config_items();
        let mut editing = None;

        let out = reduce_config_action(
            ConfigAction::ToggleSelectedBool,
            &mut settings,
            &mut idx,
            items.as_mut_slice(),
            &mut editing,
        );
        assert!(out.consumed);
        assert!(settings.always_show_add_location_prompt);

        let out = reduce_config_action(
            ConfigAction::SetSelectedBool(false),
            &mut settings,
            &mut idx,
            items.as_mut_slice(),
            &mut editing,
        );
        assert!(out.consumed);
        assert!(!settings.always_show_add_location_prompt);

        let out = reduce_config_action(
            ConfigAction::SetSelectedBool(true),
            &mut settings,
            &mut idx,
            items.as_mut_slice(),
            &mut editing,
        );
        assert!(out.consumed);
        assert!(settings.always_show_add_location_prompt);
    }

    #[test]
    fn tab_toggles_active_config_pane() {
        let mut active_pane = ConfigPane::Settings;

        toggle_config_pane(&mut active_pane);
        assert_eq!(active_pane, ConfigPane::Details);

        toggle_config_pane(&mut active_pane);
        assert_eq!(active_pane, ConfigPane::Settings);
    }

    #[test]
    fn reducer_invalid_port_edit_stays_in_edit_mode() {
        let mut settings = Box::new(Settings::default());
        let original_port = settings.client_port;
        let mut idx = 0usize;
        let mut items = config_items();
        let mut editing = Some((ConfigItem::ClientPort, "0".to_string()));

        let out = reduce_config_action(
            ConfigAction::EditCommit,
            &mut settings,
            &mut idx,
            items.as_mut_slice(),
            &mut editing,
        );

        assert!(out.consumed);
        assert_eq!(settings.client_port, original_port);
        assert_eq!(editing, Some((ConfigItem::ClientPort, "0".to_string())));
    }

    #[test]
    fn reducer_save_and_exit_emits_update_config_command() {
        let mut settings = Box::new(Settings::default());
        let mut idx = 0usize;
        let mut items = config_items();
        let mut editing = None;

        let out = reduce_config_action(
            ConfigAction::SaveAndExit,
            &mut settings,
            &mut idx,
            items.as_mut_slice(),
            &mut editing,
        );

        assert_eq!(out.effects.len(), 2);
        assert!(matches!(out.effects[0], ConfigEffect::AppCommand(_)));
        assert!(matches!(out.effects[1], ConfigEffect::ToNormal));
    }
}
