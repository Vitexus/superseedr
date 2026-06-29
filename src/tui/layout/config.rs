// SPDX-FileCopyrightText: 2025 The superseedr Contributors
// SPDX-License-Identifier: GPL-3.0-or-later

use crate::config::UiLayoutMode;
use ratatui::layout::{Constraint, Direction, Layout, Rect};

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

    let kind = config_layout_kind(area, layout_mode);
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
            let panes = Layout::default()
                .direction(Direction::Vertical)
                .constraints([
                    Constraint::Percentage(45),
                    Constraint::Length(1),
                    Constraint::Percentage(55),
                ])
                .split(content_area);
            (panes[0], panes[2])
        }
        ConfigLayoutKind::Compact => {
            let panes = Layout::default()
                .direction(Direction::Vertical)
                .constraints([Constraint::Length(4), Constraint::Min(3)])
                .split(content_area);
            (panes[0], panes[1])
        }
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

fn config_layout_kind(area: Rect, layout_mode: UiLayoutMode) -> ConfigLayoutKind {
    if area.width < 60 || area.height < 18 {
        return ConfigLayoutKind::Compact;
    }

    match layout_mode {
        UiLayoutMode::Vertical | UiLayoutMode::Square => ConfigLayoutKind::Stacked,
        UiLayoutMode::Horizontal => {
            if area.width >= 100 && area.height >= 24 {
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
    }
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
        assert_eq!(
            plan.footer_area.y,
            plan.panel_outer.y + plan.panel_outer.height
        );
    }
}
