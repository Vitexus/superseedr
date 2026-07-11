// SPDX-FileCopyrightText: 2025 The superseedr Contributors
// SPDX-License-Identifier: GPL-3.0-or-later

use crate::config::UiLayoutMode;
use ratatui::layout::{Constraint, Direction, Layout, Rect};

const PANE_GAP: u16 = 1;
const STACKED_LIST_PERCENT: u32 = 35;
const STACKED_LIST_MIN_HEIGHT: u16 = 13;
const STACKED_DETAILS_MIN_HEIGHT: u16 = 14;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ConfigLayoutKind {
    Wide,
    Stacked,
    Compact,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct ConfigLayoutPlan {
    pub kind: ConfigLayoutKind,
    pub panel_outer: Rect,
    pub content_area: Rect,
    pub list_pane: Rect,
    pub details_pane: Rect,
    pub footer_area: Rect,
}

pub fn calculate_config_layout(area: Rect, layout_mode: UiLayoutMode) -> ConfigLayoutPlan {
    let framed = config_screen_area(area);
    let main = Layout::default()
        .direction(Direction::Vertical)
        .constraints([Constraint::Min(3), Constraint::Length(1)])
        .split(framed);
    let panel_outer = main[0];
    let footer_area = main[1];
    let content_area = panel_outer;

    let kind = config_layout_kind(area, content_area, layout_mode);
    let (list_pane, details_pane) = match kind {
        ConfigLayoutKind::Wide => {
            let panes = Layout::default()
                .direction(Direction::Horizontal)
                .constraints([
                    Constraint::Length(38),
                    Constraint::Length(1),
                    Constraint::Min(40),
                ])
                .split(content_area);
            (panes[0], panes[2])
        }
        ConfigLayoutKind::Stacked => {
            let (list_height, details_height) = stacked_pane_heights(content_area.height)
                .expect("stacked config layout must satisfy pane minimums");
            let panes = Layout::default()
                .direction(Direction::Vertical)
                .constraints([
                    Constraint::Length(list_height),
                    Constraint::Length(PANE_GAP),
                    Constraint::Length(details_height),
                ])
                .split(content_area);
            (panes[0], panes[2])
        }
        ConfigLayoutKind::Compact => (content_area, content_area),
    };

    ConfigLayoutPlan {
        kind,
        panel_outer,
        content_area,
        list_pane,
        details_pane,
        footer_area,
    }
}

fn config_screen_area(area: Rect) -> Rect {
    if area.width < 70 || area.height < 20 {
        return area;
    }

    Rect::new(
        area.x + 1,
        area.y + 1,
        area.width.saturating_sub(2),
        area.height.saturating_sub(2),
    )
}

fn config_layout_kind(
    area: Rect,
    content_area: Rect,
    layout_mode: UiLayoutMode,
) -> ConfigLayoutKind {
    if area.width < 60 || area.height < 18 {
        return ConfigLayoutKind::Compact;
    }

    let preferred = match layout_mode {
        UiLayoutMode::Vertical | UiLayoutMode::Square => ConfigLayoutKind::Stacked,
        UiLayoutMode::Horizontal => {
            if area.width >= 100 {
                ConfigLayoutKind::Wide
            } else {
                ConfigLayoutKind::Stacked
            }
        }
        UiLayoutMode::Auto => {
            let is_narrow = area.width < 100;
            let is_vertical_aspect = area.height as f32 > (area.width as f32 * 0.6);
            if is_narrow || is_vertical_aspect {
                ConfigLayoutKind::Stacked
            } else {
                ConfigLayoutKind::Wide
            }
        }
    };

    if preferred == ConfigLayoutKind::Stacked && stacked_pane_heights(content_area.height).is_none()
    {
        ConfigLayoutKind::Compact
    } else {
        preferred
    }
}

fn stacked_pane_heights(total_height: u16) -> Option<(u16, u16)> {
    let available = total_height.checked_sub(PANE_GAP)?;
    if available < STACKED_LIST_MIN_HEIGHT + STACKED_DETAILS_MIN_HEIGHT {
        return None;
    }

    let proportional_list = ((u32::from(available) * STACKED_LIST_PERCENT) + 50) / 100;
    let max_list_height = available.saturating_sub(STACKED_DETAILS_MIN_HEIGHT);
    let list_height = (proportional_list as u16).clamp(STACKED_LIST_MIN_HEIGHT, max_list_height);
    let details_height = available.saturating_sub(list_height);
    Some((list_height, details_height))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn forced_vertical_uses_two_stacked_panes() {
        let plan = calculate_config_layout(Rect::new(0, 0, 120, 40), UiLayoutMode::Vertical);

        assert_eq!(plan.kind, ConfigLayoutKind::Stacked);
        assert_eq!(plan.list_pane.x, plan.details_pane.x);
        assert!(plan.details_pane.y > plan.list_pane.y);
        assert_eq!(
            plan.details_pane.y,
            plan.list_pane.y + plan.list_pane.height + 1
        );
        assert_eq!(
            plan.footer_area.y,
            plan.panel_outer.y + plan.panel_outer.height
        );
    }

    #[test]
    fn wide_horizontal_uses_two_side_by_side_panes() {
        let plan = calculate_config_layout(Rect::new(0, 0, 120, 40), UiLayoutMode::Horizontal);

        assert_eq!(plan.kind, ConfigLayoutKind::Wide);
        assert_eq!(plan.content_area, plan.panel_outer);
        assert!(plan.details_pane.x > plan.list_pane.x);
        assert_eq!(
            plan.details_pane.x,
            plan.list_pane.x + plan.list_pane.width + 1
        );
        assert!(plan.list_pane.width < plan.details_pane.width);
        assert_eq!(plan.list_pane.y, plan.details_pane.y);
        assert_eq!(
            plan.footer_area.y,
            plan.panel_outer.y + plan.panel_outer.height
        );
    }

    #[test]
    fn tiny_area_uses_compact_but_keeps_footer_below_panel() {
        let plan = calculate_config_layout(Rect::new(0, 0, 50, 14), UiLayoutMode::Horizontal);

        assert_eq!(plan.kind, ConfigLayoutKind::Compact);
        assert_eq!(plan.list_pane, plan.content_area);
        assert_eq!(plan.details_pane, plan.content_area);
        assert_eq!(
            plan.footer_area.y,
            plan.panel_outer.y + plan.panel_outer.height
        );
    }

    #[test]
    fn short_stacked_layout_falls_back_to_one_full_size_pane() {
        let plan = calculate_config_layout(Rect::new(0, 0, 80, 24), UiLayoutMode::Vertical);

        assert_eq!(plan.kind, ConfigLayoutKind::Compact);
        assert_eq!(plan.list_pane, plan.content_area);
        assert_eq!(plan.details_pane, plan.content_area);
    }

    #[test]
    fn stacked_layout_keeps_both_panes_usable_before_following_target_ratio() {
        let minimum = calculate_config_layout(Rect::new(0, 0, 80, 31), UiLayoutMode::Vertical);

        assert_eq!(minimum.kind, ConfigLayoutKind::Stacked);
        assert_eq!(minimum.list_pane.height, STACKED_LIST_MIN_HEIGHT);
        assert_eq!(minimum.details_pane.height, STACKED_DETAILS_MIN_HEIGHT);
        assert_eq!(
            minimum.details_pane.y,
            minimum.list_pane.y + minimum.list_pane.height + PANE_GAP
        );

        let roomy = calculate_config_layout(Rect::new(0, 0, 80, 60), UiLayoutMode::Vertical);
        let pane_height = roomy.list_pane.height + roomy.details_pane.height;
        let ratio_delta = (i32::from(roomy.list_pane.height) * 100
            - i32::from(pane_height) * STACKED_LIST_PERCENT as i32)
            .abs();

        assert_eq!(roomy.kind, ConfigLayoutKind::Stacked);
        assert!(roomy.list_pane.height >= STACKED_LIST_MIN_HEIGHT);
        assert!(roomy.details_pane.height >= STACKED_DETAILS_MIN_HEIGHT);
        assert!(
            ratio_delta <= 100,
            "stacked split should stay within one row of 35%"
        );
    }

    #[test]
    fn wide_horizontal_stays_side_by_side_at_short_usable_height() {
        let plan = calculate_config_layout(Rect::new(0, 0, 120, 18), UiLayoutMode::Horizontal);

        assert_eq!(plan.kind, ConfigLayoutKind::Wide);
        assert_eq!(plan.list_pane.y, plan.details_pane.y);
        assert!(plan.details_pane.x > plan.list_pane.x);
    }
}
