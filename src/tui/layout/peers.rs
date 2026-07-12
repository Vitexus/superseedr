// SPDX-FileCopyrightText: 2026 The superseedr Contributors
// SPDX-License-Identifier: GPL-3.0-or-later

use ratatui::layout::{Constraint, Layout, Rect};

pub const WIDE_PEER_SCREEN_MIN_WIDTH: u16 = 120;
pub const STACKED_PEER_SCREEN_MIN_WIDTH: u16 = 80;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum PeerBodyLayout {
    Wide { table: Rect, details: Rect },
    Stacked { table: Rect, details: Rect },
    TableOnly { table: Rect },
    DetailsOnly { details: Rect },
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct PeerScreenLayout {
    pub search: Option<Rect>,
    pub summary: Rect,
    pub body: PeerBodyLayout,
    pub footer: Rect,
}

pub fn calculate_peer_screen_layout(
    area: Rect,
    search_visible: bool,
    show_details: bool,
) -> PeerScreenLayout {
    let content = peer_content_area(area);
    let search_height = u16::from(search_visible).saturating_mul(3);
    let vertical = Layout::vertical([
        Constraint::Length(search_height),
        Constraint::Length(2),
        Constraint::Min(5),
        Constraint::Length(1),
    ])
    .split(content);

    let search = search_visible.then_some(vertical[0]);
    let summary = vertical[1];
    let body_area = vertical[2];
    let footer = vertical[3];

    let body = if content.width >= WIDE_PEER_SCREEN_MIN_WIDTH && body_area.height >= 8 {
        let columns = Layout::horizontal([Constraint::Percentage(65), Constraint::Percentage(35)])
            .split(body_area);
        PeerBodyLayout::Wide {
            table: columns[0],
            details: columns[1],
        }
    } else if content.width >= STACKED_PEER_SCREEN_MIN_WIDTH && body_area.height >= 16 {
        let details_height = (body_area.height / 3)
            .clamp(7, 12)
            .min(body_area.height.saturating_sub(5));
        let rows = Layout::vertical([Constraint::Min(5), Constraint::Length(details_height)])
            .split(body_area);
        PeerBodyLayout::Stacked {
            table: rows[0],
            details: rows[1],
        }
    } else if show_details {
        PeerBodyLayout::DetailsOnly { details: body_area }
    } else {
        PeerBodyLayout::TableOnly { table: body_area }
    };

    PeerScreenLayout {
        search,
        summary,
        body,
        footer,
    }
}

fn peer_content_area(area: Rect) -> Rect {
    if area.width < 90 || area.height < 18 {
        return area;
    }

    Rect::new(
        area.x.saturating_add(1),
        area.y.saturating_add(1),
        area.width.saturating_sub(2),
        area.height.saturating_sub(2),
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn wide_layout_keeps_table_and_details_side_by_side() {
        let layout = calculate_peer_screen_layout(Rect::new(0, 0, 150, 40), false, false);

        let PeerBodyLayout::Wide { table, details } = layout.body else {
            panic!("expected wide layout");
        };
        assert!(table.width > details.width);
        assert_eq!(table.height, details.height);
    }

    #[test]
    fn medium_layout_stacks_details_under_table() {
        let layout = calculate_peer_screen_layout(Rect::new(0, 0, 100, 32), false, false);

        let PeerBodyLayout::Stacked { table, details } = layout.body else {
            panic!("expected stacked layout");
        };
        assert!(table.height >= 5);
        assert!(details.y >= table.y + table.height);
    }

    #[test]
    fn narrow_details_replace_the_table_only_when_requested() {
        let table = calculate_peer_screen_layout(Rect::new(0, 0, 70, 24), false, false);
        assert!(matches!(table.body, PeerBodyLayout::TableOnly { .. }));

        let details = calculate_peer_screen_layout(Rect::new(0, 0, 70, 24), false, true);
        assert!(matches!(details.body, PeerBodyLayout::DetailsOnly { .. }));
    }

    #[test]
    fn search_reserves_a_standard_prompt_row() {
        let without_search = calculate_peer_screen_layout(Rect::new(0, 0, 100, 30), false, false);
        let with_search = calculate_peer_screen_layout(Rect::new(0, 0, 100, 30), true, false);

        assert!(with_search.search.is_some());
        assert_eq!(with_search.search.unwrap().height, 3);
        let without_table_height = match without_search.body {
            PeerBodyLayout::Stacked { table, .. } => table.height,
            _ => panic!("expected stacked layout"),
        };
        let with_table_height = match with_search.body {
            PeerBodyLayout::Stacked { table, .. } => table.height,
            _ => panic!("expected stacked layout"),
        };
        assert!(with_table_height < without_table_height);
    }
}
