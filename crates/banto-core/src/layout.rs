//! The emporium's screen geometry: where the sidebar, the pane area, each
//! tile and each draggable boundary sit, as a pure function of the terminal
//! area, the stage and the layout preferences.
//!
//! Everything that needs a rectangle — drawing, hit-testing the mouse, and
//! sizing each hosted PTY — goes through [`geometry`] (via
//! `EmporiumState::geometry`), so they cannot disagree about where a tile is.
//!
//! The sizes the operator has chosen are *preferences*: a column count, a
//! permille share, row weights. They are clamped here, at use, and never
//! rewritten because the terminal happens to be small, so widening the
//! terminal again brings the preferred split back.

use std::collections::HashMap;

use ratatui_core::layout::{Constraint, Layout, Rect};
use unicode_width::UnicodeWidthStr;

use crate::engine::{SessionKey, Stage};
use crate::model::{BrigadeId, MemberToken};

/// The sidebar's default width in columns (its preference starts here).
pub const SIDEBAR_WIDTH: u16 = 36;
/// Details panel height: one border row plus its content rows.
pub const SUMMARY_HEIGHT: u16 = 5;
/// Below this left-column height the details panel is dropped so the list keeps
/// the room.
pub const MIN_HEIGHT_FOR_SUMMARY: u16 = 12;

/// Narrowest the sidebar can be dragged to.
pub const MIN_SIDEBAR_WIDTH: u16 = 16;
/// Narrowest the pane area can be dragged to, so a drag cannot squeeze the
/// panes out of sight.
pub const MIN_PANE_AREA_WIDTH: u16 = 20;
/// Narrowest the master tile can be dragged to.
pub const MIN_MASTER_WIDTH: u16 = 20;
/// Narrowest the stack of Worker tiles can be dragged to.
pub const MIN_STACK_WIDTH: u16 = 20;
/// Shortest a stack tile can be dragged to: border, title row's content,
/// and two rows of the child — below that a Claude or Codex pane has no room
/// for its own prompt block.
pub const MIN_STACK_TILE_HEIGHT: u16 = 4;
/// The master tile's default share of the pane area's width, in permille.
pub const DEFAULT_MASTER_PERMILLE: u16 = 500;

/// Layout preferences for one brigade, kept per [`BrigadeId`] and keyed by
/// member rather than by pane or position, so they survive the brigade going
/// off-stage, a pane being re-keyed, and a member being relaunched.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct BrigadeLayout {
    /// The master tile's share of the pane area's width, in permille.
    pub master_permille: u16,
    /// Relative heights of the stack's tiles, by member token.
    pub row_weights: HashMap<MemberToken, u16>,
    /// The operator's preferred order of the stack's tiles, top to bottom,
    /// by member token. `Stage::Brigade::panes` is what is on screen; this is
    /// what puts it back after something rebuilds `panes` (staging the
    /// brigade again, a Worker arriving). Empty until a tile has been moved.
    pub stack_order: Vec<MemberToken>,
}

impl Default for BrigadeLayout {
    fn default() -> Self {
        Self {
            master_permille: DEFAULT_MASTER_PERMILLE,
            row_weights: HashMap::new(),
            stack_order: Vec::new(),
        }
    }
}

/// The regions of the emporium layout.
#[derive(Clone, Copy)]
pub struct Areas {
    pub sidebar: Rect,
    pub summary: Rect,
    pub pane: Rect,
    pub status: Rect,
}

/// Compute the layout: a bottom status bar, and above it a left column
/// (sidebar list + details panel) beside the session pane.
///
/// `sidebar_width` is the operator's preference. With room for both
/// [`MIN_SIDEBAR_WIDTH`] and [`MIN_PANE_AREA_WIDTH`] it is clamped between
/// them; without, the solver squeezes it as it always has, so a tiny terminal
/// behaves as it did before the width could be changed.
pub fn layout(area: Rect, sidebar_width: u16) -> Areas {
    let [body, status] = Layout::vertical([Constraint::Min(1), Constraint::Length(1)]).areas(area);
    let width = if body.width >= MIN_SIDEBAR_WIDTH + MIN_PANE_AREA_WIDTH {
        sidebar_width.clamp(MIN_SIDEBAR_WIDTH, body.width - MIN_PANE_AREA_WIDTH)
    } else {
        sidebar_width
    };
    let [left, pane] =
        Layout::horizontal([Constraint::Length(width), Constraint::Min(1)]).areas(body);
    let summary_h = if left.height < MIN_HEIGHT_FOR_SUMMARY {
        0
    } else {
        SUMMARY_HEIGHT
    };
    let [sidebar, summary] =
        Layout::vertical([Constraint::Min(1), Constraint::Length(summary_h)]).areas(left);
    Areas {
        sidebar,
        summary,
        pane,
        status,
    }
}

/// The inner content rect of the right pane (inside its border).
pub fn pane_content(pane_area: Rect) -> Rect {
    Rect {
        x: pane_area.x + 1,
        y: pane_area.y + 1,
        width: pane_area.width.saturating_sub(2).max(1),
        height: pane_area.height.saturating_sub(2).max(1),
    }
}

/// The master tile's width for a pane area `area_width` columns wide, from
/// its permille share.
///
/// Rounds half up, which is what the solver's `Percentage(50)` pair gave for
/// an odd width (the master takes the odd column), so the default split is
/// unchanged. Clamped to leave [`MIN_MASTER_WIDTH`] and [`MIN_STACK_WIDTH`]
/// when both fit; in a pane area too narrow for both it only keeps one column
/// each, which is as far as the old fixed split ever went.
pub fn master_width(area_width: u16, permille: u16) -> u16 {
    let share = (u32::from(area_width) * u32::from(permille.min(1000)) + 500) / 1000;
    let share = share as u16;
    if area_width >= MIN_MASTER_WIDTH + MIN_STACK_WIDTH {
        share.clamp(MIN_MASTER_WIDTH, area_width - MIN_STACK_WIDTH)
    } else if area_width >= 2 {
        share.clamp(1, area_width - 1)
    } else {
        share.min(area_width)
    }
}

/// The permille share a master tile of `master` columns in a pane area of
/// `area_width` columns corresponds to (the inverse of [`master_width`] at
/// that width).
pub fn master_permille_for(master: u16, area_width: u16) -> u16 {
    if area_width == 0 {
        return DEFAULT_MASTER_PERMILLE;
    }
    let w = u32::from(area_width);
    ((u32::from(master) * 1000 + w / 2) / w).min(1000) as u16
}

/// Split `total` rows among `weights.len()` stack tiles in proportion to
/// `weights`, never leaving a tile under `min_each` rows while the total
/// allows it, with the heights summing to exactly `total`.
///
/// Heights come from rounding the cumulative proportional boundaries half up,
/// not from giving each tile its floor and handing the leftover rows out in
/// some order: with equal weights that reproduces, row for row, the split the
/// layout solver's `Ratio(1, n)` produced before the heights could be chosen
/// (its extra rows land in a pattern — `[3, 4, 3]`, `[3, 2, 3, 2]` — that no
/// first-come order gives), so the default layout does not move.
///
/// A tile whose proportional share would be under `min_each` is pinned at
/// `min_each` and the rest share what remains, repeating until none is
/// short. When `total` cannot give every tile `min_each` the minimum is
/// dropped and the plain proportional split is used.
pub fn stack_heights(total: u16, weights: &[u16], min_each: u16) -> Vec<u16> {
    let n = weights.len();
    if n == 0 {
        return Vec::new();
    }
    let weights: Vec<u64> = weights.iter().map(|w| u64::from((*w).max(1))).collect();
    let total = u64::from(total);
    let min_each = u64::from(min_each);
    let mut pinned = vec![false; n];
    if total >= min_each * n as u64 {
        loop {
            let pinned_count = pinned.iter().filter(|p| **p).count() as u64;
            let free_space = total - pinned_count * min_each;
            let free_weight: u64 = weights
                .iter()
                .zip(&pinned)
                .filter(|(_, p)| !**p)
                .map(|(w, _)| *w)
                .sum();
            if free_weight == 0 {
                break;
            }
            let mut changed = false;
            for i in 0..n {
                if !pinned[i] && weights[i] * free_space < min_each * free_weight {
                    pinned[i] = true;
                    changed = true;
                }
            }
            if !changed {
                break;
            }
        }
    }
    let pinned_count = pinned.iter().filter(|p| **p).count() as u64;
    let free_space = total - pinned_count * min_each;
    let free_weight: u64 = weights
        .iter()
        .zip(&pinned)
        .filter(|(_, p)| !**p)
        .map(|(w, _)| *w)
        .sum();
    let mut heights = vec![0u16; n];
    let mut cumulative_weight = 0u64;
    let mut previous_boundary = 0u64;
    for i in 0..n {
        if pinned[i] {
            heights[i] = min_each as u16;
            continue;
        }
        cumulative_weight += weights[i];
        let boundary = (2 * free_space * cumulative_weight + free_weight) / (2 * free_weight);
        heights[i] = (boundary - previous_boundary) as u16;
        previous_boundary = boundary;
    }
    heights
}

/// The outer (bordered) tile rects for the currently-staged sessions, each
/// paired with its key. A solo session fills the whole pane; a brigade puts
/// its first pane on the left as the master tile and stacks the rest down
/// the right — geometry only, keyed by position in `panes`
/// (`Stage::Brigade::panes`'s own doc), independent of which pane actually
/// holds the Director.
///
/// `master_permille` and `stack_weights` (one per stack tile, in `panes`
/// order; a missing one counts as 1) are the operator's preferences, resolved
/// by the caller.
pub fn stage_tiles(
    pane_area: Rect,
    stage: &Stage,
    master_permille: u16,
    stack_weights: &[u16],
) -> Vec<(SessionKey, Rect)> {
    match stage {
        Stage::Empty => Vec::new(),
        Stage::Solo(key) => vec![(key.clone(), pane_area)],
        Stage::Brigade { panes, .. } => match panes.split_first() {
            None => Vec::new(),
            Some((director, [])) => vec![(director.clone(), pane_area)],
            Some((director, workers)) => {
                let master_w = master_width(pane_area.width, master_permille);
                let master = Rect {
                    width: master_w,
                    ..pane_area
                };
                let stack = Rect {
                    x: pane_area.x + master_w,
                    width: pane_area.width - master_w,
                    ..pane_area
                };
                let weights: Vec<u16> = (0..workers.len())
                    .map(|i| stack_weights.get(i).copied().unwrap_or(1))
                    .collect();
                let heights = stack_heights(stack.height, &weights, MIN_STACK_TILE_HEIGHT);
                let mut tiles = vec![(director.clone(), master)];
                let mut y = stack.y;
                for (worker, height) in workers.iter().zip(heights) {
                    tiles.push((worker.clone(), Rect { y, height, ..stack }));
                    y += height;
                }
                tiles
            }
        },
    }
}

/// Which boundary a drag moves. Carries enough of the stage's identity that
/// a boundary which stops existing (a pane closed, another brigade staged)
/// stops comparing equal to every boundary the stage now has.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Boundary {
    /// Between the left column and the pane area.
    Sidebar,
    /// Between the master tile and the stack.
    Master { brigade: BrigadeId },
    /// Between two vertically adjacent tiles of the stack.
    Stack {
        brigade: BrigadeId,
        above: SessionKey,
        below: SessionKey,
    },
}

/// The direction a boundary's line runs.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Axis {
    /// A vertical line, moved left and right (its position is a column).
    Vertical,
    /// A horizontal line, moved up and down (its position is a row).
    Horizontal,
}

/// One draggable boundary, as the current geometry has it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Handle {
    pub boundary: Boundary,
    pub axis: Axis,
    /// The cells where a press starts a drag. More than one rectangle for the
    /// stack row boundary, whose lower row skips the tile's title label.
    hits: Vec<Rect>,
    /// Where the boundary is now: the first column (vertical) or row
    /// (horizontal) of whatever lies after it.
    pub position: u16,
    /// The range `position` may be dragged to, inclusive. `min > max` when
    /// the terminal is too small to give both neighbours their minimum, in
    /// which case the boundary does not move.
    pub min: u16,
    pub max: u16,
    /// The extent of the line along the other axis, as `[start, end)`.
    span: (u16, u16),
}

impl Handle {
    /// The cells where a press picks this boundary up.
    pub fn hits(&self) -> &[Rect] {
        &self.hits
    }

    /// `position`, clamped into the range this boundary may take; unchanged
    /// when that range is empty.
    pub fn clamp(&self, position: i32) -> u16 {
        if self.min > self.max {
            return self.position;
        }
        position.clamp(i32::from(self.min), i32::from(self.max)) as u16
    }

    /// The cells of the line drawn for a drag previewed at `position`: on the
    /// last column or row before the boundary — the left tile's right border,
    /// or the upper tile's bottom border.
    pub fn line_at(&self, position: u16) -> Rect {
        let line = position.saturating_sub(1);
        match self.axis {
            Axis::Vertical => Rect::new(line, self.span.0, 1, self.span.1 - self.span.0),
            Axis::Horizontal => Rect::new(self.span.0, line, self.span.1 - self.span.0, 1),
        }
    }
}

/// Everything the layout decides for one terminal area.
pub struct Geometry {
    pub areas: Areas,
    pub tiles: Vec<(SessionKey, Rect)>,
    handles: Vec<Handle>,
    /// The title-label cells of every stack tile of a stack with two or more
    /// tiles — what a press picks a tile up by. Just the label: the rest of
    /// a title row is not assumed to be inert.
    labels: Vec<(SessionKey, Rect)>,
}

impl Geometry {
    /// Every draggable boundary, vertical ones first.
    pub fn handles(&self) -> &[Handle] {
        &self.handles
    }

    /// The boundary a press at (`column`, `row`) would pick up. A vertical
    /// handle wins over a horizontal one where they meet.
    pub fn handle_at(&self, column: u16, row: u16) -> Option<&Handle> {
        let at = ratatui_core::layout::Position::new(column, row);
        self.handles
            .iter()
            .find(|handle| handle.hits.iter().any(|hit| hit.contains(at)))
    }

    /// The stack tile whose title label a press at (`column`, `row`) lands on.
    pub fn label_at(&self, column: u16, row: u16) -> Option<&SessionKey> {
        let at = ratatui_core::layout::Position::new(column, row);
        self.labels
            .iter()
            .find(|(_, cells)| cells.contains(at))
            .map(|(key, _)| key)
    }

    /// The slot (an index into `tiles`, so into `panes`; always a stack tile)
    /// that a pointer on `row` is over while moving a tile: the tile the row
    /// falls in, the first stack slot above the stack, the last below it.
    /// `None` when there is no stack to drop into.
    pub fn drop_slot(&self, row: u16) -> Option<usize> {
        let stack = self.tiles.get(1..).filter(|stack| stack.len() >= 2)?;
        let last = stack.len();
        let top = stack[0].1.y;
        if row < top {
            return Some(1);
        }
        let index = stack
            .iter()
            .position(|(_, rect)| row < rect.y + rect.height)
            .map_or(last, |at| at + 1);
        Some(index)
    }

    /// The line that previews moving the tile at `from` to the slot `to`: the
    /// top edge of the target tile when moving up, its bottom edge when
    /// moving down, across the stack's width. `None` when `to` is `from`.
    pub fn move_line(&self, from: usize, to: usize) -> Option<Rect> {
        let target = self.tiles.get(to)?.1;
        let row = match to.cmp(&from) {
            std::cmp::Ordering::Equal => return None,
            std::cmp::Ordering::Less => target.y,
            std::cmp::Ordering::Greater => target.y + target.height.saturating_sub(1),
        };
        Some(Rect::new(target.x, row, target.width, 1))
    }

    pub fn handle_for(&self, boundary: &Boundary) -> Option<&Handle> {
        self.handles
            .iter()
            .find(|handle| &handle.boundary == boundary)
    }
}

/// Put the stack in the preferred order: `panes[1..]` stably sorted by each
/// member's rank in `order`, members with no rank after the ranked ones in
/// their current relative order. `panes[0]` (the master) is never touched,
/// and `focused` keeps pointing at the same key. Returns whether anything
/// moved.
///
/// The one function that applies the preference; the engine calls it once,
/// at the end of every `update`, so no branch that rebuilds `panes` has to
/// remember to.
pub fn sort_stack<'a>(
    panes: &mut [SessionKey],
    focused: &mut usize,
    order: &[MemberToken],
    token_of: impl Fn(&SessionKey) -> Option<&'a str>,
) -> bool {
    let Some((_, stack)) = panes.split_first_mut() else {
        return false;
    };
    let rank = |key: &SessionKey| {
        token_of(key)
            .and_then(|token| order.iter().position(|ranked| ranked == token))
            .unwrap_or(usize::MAX)
    };
    if stack.is_sorted_by_key(rank) {
        return false;
    }
    let focused_key = panes.get(*focused).cloned();
    panes[1..].sort_by_key(rank);
    if let Some(key) = focused_key
        && let Some(index) = panes.iter().position(|pane| *pane == key)
    {
        *focused = index;
    }
    true
}

/// The text drawn on a tile's top border for the tile labelled `label`: the
/// label with a space on each side. The shell draws exactly this, and
/// [`title_columns`] measures exactly this, so the drawn text and the cells
/// treated as the label cannot disagree.
pub fn title_text(label: &str) -> String {
    format!(" {label} ")
}

/// The columns `[start, end)` of `tile`'s top border row that the drawn
/// title occupies: from the cell after the left corner, as wide as
/// [`title_text`] is, clipped to the tile's inner width the way ratatui's
/// `Block` clips a title (by display width, between the two corners).
pub fn title_columns(tile: Rect, label: &str) -> (u16, u16) {
    let room = usize::from(tile.width.saturating_sub(2));
    let width = UnicodeWidthStr::width(title_text(label).as_str()).min(room) as u16;
    let start = tile.x.saturating_add(1);
    (start, start.saturating_add(width))
}

/// Compute the whole geometry for `area`.
///
/// `stack_weights` has one entry per stack tile of the staged brigade, in
/// `panes` order (see [`stage_tiles`]). `label_of` is a tile's title label
/// (the shell draws it with [`title_text`]); the stack row handle keeps off
/// the lower tile's label, which is the tile's own to click.
pub fn geometry(
    area: Rect,
    stage: &Stage,
    sidebar_width: u16,
    master_permille: u16,
    stack_weights: &[u16],
    label_of: &dyn Fn(&SessionKey) -> String,
) -> Geometry {
    let areas = layout(area, sidebar_width);
    let tiles = stage_tiles(areas.pane, stage, master_permille, stack_weights);
    let body_height = areas.pane.height;
    let mut handles = Vec::new();

    // Sidebar | pane area. The sidebar's right border column and the pane
    // area's left border column both belong to it.
    if areas.pane.x > 0 {
        let width = areas.pane.x + areas.pane.width;
        let room = width >= MIN_SIDEBAR_WIDTH + MIN_PANE_AREA_WIDTH;
        handles.push(Handle {
            boundary: Boundary::Sidebar,
            axis: Axis::Vertical,
            hits: vec![Rect::new(areas.pane.x - 1, areas.pane.y, 2, body_height)],
            position: areas.pane.x,
            min: if room {
                MIN_SIDEBAR_WIDTH
            } else {
                areas.pane.x
            },
            max: if room {
                width - MIN_PANE_AREA_WIDTH
            } else {
                areas.pane.x
            },
            span: (areas.pane.y, areas.pane.y + body_height),
        });
    }

    if let Stage::Brigade { id, .. } = stage
        && tiles.len() >= 2
    {
        let master = tiles[0].1;
        let stack_x = tiles[1].1.x;
        let pane = areas.pane;
        let room = pane.width >= MIN_MASTER_WIDTH + MIN_STACK_WIDTH;
        if stack_x > pane.x {
            handles.push(Handle {
                boundary: Boundary::Master { brigade: *id },
                axis: Axis::Vertical,
                hits: vec![Rect::new(stack_x - 1, pane.y, 2, pane.height)],
                position: stack_x,
                min: if room {
                    pane.x + MIN_MASTER_WIDTH
                } else {
                    stack_x
                },
                max: if room {
                    pane.x + pane.width - MIN_STACK_WIDTH
                } else {
                    stack_x
                },
                span: (master.y, master.y + master.height),
            });
        }
        // The stack's own horizontal boundaries come after every vertical
        // one, so a vertical handle wins where they meet.
        for pair in tiles[1..].windows(2) {
            let (above_key, above) = (&pair[0].0, pair[0].1);
            let (below_key, below) = (&pair[1].0, pair[1].1);
            if above.width < 3 || below.y == 0 {
                continue;
            }
            let (min, max) = (
                above.y + MIN_STACK_TILE_HEIGHT,
                (below.y + below.height).saturating_sub(MIN_STACK_TILE_HEIGHT),
            );
            // The gap the eye sees is two rows: the upper tile's bottom border
            // and the lower tile's top border. Both are the handle, from the
            // cell after the left corner (the master/stack handle's) through
            // the right border column — the screen edge, where a pointer
            // lands when the boundary is grabbed from the right. Except the
            // lower row's label cells: those are the tile's own, the handle
            // it is picked up by to be moved ([`Geometry::label_at`]).
            let (_, label_end) = title_columns(below, &label_of(below_key));
            let right = below.x + below.width;
            let lower_start = label_end.max(below.x + 1);
            let mut hits = vec![Rect::new(above.x + 1, below.y - 1, above.width - 1, 1)];
            if lower_start < right {
                hits.push(Rect::new(lower_start, below.y, right - lower_start, 1));
            }
            handles.push(Handle {
                boundary: Boundary::Stack {
                    brigade: *id,
                    above: above_key.clone(),
                    below: below_key.clone(),
                },
                axis: Axis::Horizontal,
                hits,
                position: below.y,
                min,
                max,
                span: (above.x, above.x + above.width),
            });
        }
    }

    let mut labels = Vec::new();
    if matches!(stage, Stage::Brigade { .. }) && tiles.len() >= 3 {
        for (key, tile) in &tiles[1..] {
            let (start, end) = title_columns(*tile, &label_of(key));
            if end > start {
                labels.push((key.clone(), Rect::new(start, tile.y, end - start, 1)));
            }
        }
    }

    Geometry {
        areas,
        tiles,
        handles,
        labels,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn key(id: &str) -> SessionKey {
        SessionKey::from_id(id)
    }

    fn brigade_stage(panes: &[&str]) -> Stage {
        Stage::Brigade {
            id: 1,
            director: Some(key(panes[0])),
            panes: panes.iter().map(|p| key(p)).collect(),
            focused: 0,
        }
    }

    /// What the layout solver gave before heights could be chosen: the pane
    /// area split 50/50 and the right half split into equal rows.
    fn solver_tiles(pane_area: Rect, panes: &[&str]) -> Vec<(SessionKey, Rect)> {
        let (director, workers) = panes.split_first().unwrap();
        if workers.is_empty() {
            return vec![(key(director), pane_area)];
        }
        let [master, stack] =
            Layout::horizontal([Constraint::Percentage(50), Constraint::Percentage(50)])
                .areas(pane_area);
        let rows = Layout::vertical(vec![
            Constraint::Ratio(1, workers.len() as u32);
            workers.len()
        ])
        .split(stack);
        let mut tiles = vec![(key(director), master)];
        for (worker, row) in workers.iter().zip(rows.iter()) {
            tiles.push((key(worker), *row));
        }
        tiles
    }

    #[test]
    fn default_tiles_are_exactly_what_the_solver_gave_for_every_size_and_pane_count() {
        let names = ["d", "w1", "w2", "w3", "w4", "w5", "w6"];
        for count in 2..=names.len() {
            let panes = &names[..count];
            let stage = brigade_stage(panes);
            for width in 0u16..=130 {
                for height in [0u16, 1, 2, 3, 7, 8, 9, 10, 11, 14, 17, 22, 39, 40, 61] {
                    let area = Rect::new(3, 2, width, height);
                    let weights = vec![1; count - 1];
                    assert_eq!(
                        stage_tiles(area, &stage, DEFAULT_MASTER_PERMILLE, &weights),
                        solver_tiles(area, panes),
                        "{count} panes in {width}x{height}"
                    );
                }
            }
        }
    }

    #[test]
    fn default_layout_areas_match_the_old_fixed_sidebar_wherever_the_pane_area_keeps_its_minimum() {
        // Beyond SIDEBAR_WIDTH + MIN_PANE_AREA_WIDTH columns the default
        // sidebar is the old 36 columns, exactly; narrower terminals give the
        // pane area its minimum first.
        for width in SIDEBAR_WIDTH + MIN_PANE_AREA_WIDTH..=200 {
            let areas = layout(Rect::new(0, 0, width, 40), SIDEBAR_WIDTH);
            let old = {
                let [body, _] = Layout::vertical([Constraint::Min(1), Constraint::Length(1)])
                    .areas(Rect::new(0, 0, width, 40));
                let [left, pane] =
                    Layout::horizontal([Constraint::Length(36), Constraint::Min(1)]).areas(body);
                (left.width, pane)
            };
            assert_eq!(areas.sidebar.width, old.0, "{width}");
            assert_eq!(areas.pane, old.1, "{width}");
        }
    }

    #[test]
    fn a_tiny_terminal_gets_the_solvers_squeeze_not_a_panic() {
        for width in 0u16..MIN_SIDEBAR_WIDTH + MIN_PANE_AREA_WIDTH {
            for height in 0u16..30 {
                let areas = layout(Rect::new(0, 0, width, height), SIDEBAR_WIDTH);
                let old = Layout::horizontal([Constraint::Length(36), Constraint::Min(1)])
                    .areas::<2>(
                        Layout::vertical([Constraint::Min(1), Constraint::Length(1)])
                            .areas::<2>(Rect::new(0, 0, width, height))[0],
                    );
                assert_eq!(areas.pane, old[1], "{width}x{height}");
            }
        }
    }

    #[test]
    fn between_the_two_minimums_and_the_old_width_the_pane_area_keeps_its_minimum() {
        // The one place the default layout differs from the old fixed 36
        // columns above the tiny sizes: the pane area is never squeezed under
        // MIN_PANE_AREA_WIDTH to keep the sidebar's full default.
        for width in MIN_SIDEBAR_WIDTH + MIN_PANE_AREA_WIDTH..SIDEBAR_WIDTH + MIN_PANE_AREA_WIDTH {
            let areas = layout(Rect::new(0, 0, width, 40), SIDEBAR_WIDTH);
            assert_eq!(areas.pane.width, MIN_PANE_AREA_WIDTH, "{width}");
        }
    }

    #[test]
    fn the_sidebar_preference_is_clamped_at_use_and_never_rewritten() {
        let wide = layout(Rect::new(0, 0, 200, 40), 60);
        assert_eq!(wide.sidebar.width, 60);
        let narrow = layout(Rect::new(0, 0, 70, 40), 60);
        assert_eq!(
            narrow.pane.width, MIN_PANE_AREA_WIDTH,
            "pane area keeps its minimum"
        );
        assert_eq!(narrow.sidebar.width, 50);
        let thin = layout(Rect::new(0, 0, 200, 40), 3);
        assert_eq!(thin.sidebar.width, MIN_SIDEBAR_WIDTH);
    }

    #[test]
    fn stack_heights_always_sum_to_the_total_and_equal_weights_match_the_solver() {
        for total in 0u16..=120 {
            for n in 1usize..=8 {
                let equal = stack_heights(total, &vec![1; n], MIN_STACK_TILE_HEIGHT);
                assert_eq!(
                    equal.iter().map(|h| u32::from(*h)).sum::<u32>(),
                    u32::from(total)
                );
                let rows = Layout::vertical(vec![Constraint::Ratio(1, n as u32); n])
                    .split(Rect::new(0, 0, 10, total));
                let solver: Vec<u16> = rows.iter().map(|r| r.height).collect();
                assert_eq!(equal, solver, "{n} tiles in {total} rows");
            }
        }
    }

    #[test]
    fn stack_heights_follow_unequal_weights_and_still_sum() {
        for total in 0u16..=90 {
            for weights in [
                vec![1u16, 2],
                vec![3, 1, 1],
                vec![10, 1, 7, 2],
                vec![1, 1, 1, 50],
                vec![200, 1],
            ] {
                let heights = stack_heights(total, &weights, MIN_STACK_TILE_HEIGHT);
                assert_eq!(
                    heights.iter().map(|h| u32::from(*h)).sum::<u32>(),
                    u32::from(total),
                    "{weights:?} in {total}"
                );
            }
        }
        assert_eq!(stack_heights(30, &[20, 10], 4), vec![20, 10]);
    }

    #[test]
    fn stack_heights_never_drop_a_tile_below_the_minimum_when_the_total_allows_it() {
        for total in 8u16..=90 {
            for weights in [vec![1u16, 100], vec![100, 1, 1], vec![1, 1, 1, 400]] {
                if usize::from(total) < weights.len() * 4 {
                    continue;
                }
                let heights = stack_heights(total, &weights, 4);
                assert!(
                    heights.iter().all(|h| *h >= 4),
                    "{weights:?} in {total}: {heights:?}"
                );
            }
        }
    }

    #[test]
    fn stack_heights_committed_from_heights_reproduce_them_at_the_same_size() {
        // The weights a drag leaves behind are the heights it produced; laid
        // out again at the same total they must come back unchanged.
        for heights in [vec![12u16, 8], vec![5, 9, 6], vec![4, 4, 20, 4]] {
            let total: u16 = heights.iter().sum();
            assert_eq!(stack_heights(total, &heights, 4), heights);
        }
    }

    #[test]
    fn master_width_defaults_to_the_solvers_half_and_clamps() {
        for width in 0u16..=300 {
            let [solver, _] =
                Layout::horizontal([Constraint::Percentage(50), Constraint::Percentage(50)])
                    .areas(Rect::new(0, 0, width, 5));
            assert_eq!(
                master_width(width, DEFAULT_MASTER_PERMILLE),
                solver.width,
                "{width}"
            );
        }
        assert_eq!(master_width(100, 50), MIN_MASTER_WIDTH);
        assert_eq!(master_width(100, 990), 100 - MIN_STACK_WIDTH);
        assert_eq!(
            master_width(30, 900),
            27,
            "too narrow for both minimums: no clamp to them"
        );
        assert_eq!(
            master_width(30, 1000),
            29,
            "but one column stays for the stack"
        );
    }

    #[test]
    fn a_permille_round_trips_through_the_width_it_was_made_from() {
        for width in 40u16..=300 {
            for master in MIN_MASTER_WIDTH..=width - MIN_STACK_WIDTH {
                let permille = master_permille_for(master, width);
                assert_eq!(master_width(width, permille), master, "{master} of {width}");
            }
        }
    }

    fn geo(width: u16, height: u16, panes: &[&str]) -> Geometry {
        geo_labelled(width, height, panes, &|key| key.as_str().to_string())
    }

    fn geo_labelled(
        width: u16,
        height: u16,
        panes: &[&str],
        label_of: &dyn Fn(&SessionKey) -> String,
    ) -> Geometry {
        let stage = if panes.len() == 1 {
            Stage::Solo(key(panes[0]))
        } else {
            brigade_stage(panes)
        };
        geometry(
            Rect::new(0, 0, width, height),
            &stage,
            SIDEBAR_WIDTH,
            DEFAULT_MASTER_PERMILLE,
            &vec![1; panes.len().saturating_sub(1)],
            label_of,
        )
    }

    #[test]
    fn the_handles_are_the_three_boundaries_and_only_where_they_exist() {
        let kinds = |g: &Geometry| {
            g.handles()
                .iter()
                .map(|h| h.boundary.clone())
                .collect::<Vec<_>>()
        };
        assert_eq!(kinds(&geo(120, 40, &["d"])), vec![Boundary::Sidebar]);
        assert_eq!(
            kinds(&geo(120, 40, &["d", "w1"])),
            vec![Boundary::Sidebar, Boundary::Master { brigade: 1 }]
        );
        assert_eq!(
            kinds(&geo(120, 40, &["d", "w1", "w2", "w3"])),
            vec![
                Boundary::Sidebar,
                Boundary::Master { brigade: 1 },
                Boundary::Stack {
                    brigade: 1,
                    above: key("w1"),
                    below: key("w2")
                },
                Boundary::Stack {
                    brigade: 1,
                    above: key("w2"),
                    below: key("w3")
                },
            ]
        );
    }

    #[test]
    fn a_vertical_handle_wins_where_it_meets_a_horizontal_one() {
        let g = geo(120, 40, &["d", "w1", "w2"]);
        let stack = g.tiles[1].1;
        let upper = g.tiles[1].1;
        let corner_row = upper.y + upper.height - 1;
        // The stack's left border column on the boundary row: the Master
        // handle's, not the Stack handle's.
        assert_eq!(
            g.handle_at(stack.x, corner_row).map(|h| h.boundary.clone()),
            Some(Boundary::Master { brigade: 1 })
        );
        // The stack's right border column on that row: the Stack handle's,
        // nobody else claims that corner.
        assert!(matches!(
            g.handle_at(stack.x + stack.width - 1, corner_row)
                .map(|h| h.boundary.clone()),
            Some(Boundary::Stack { .. })
        ));
        // One past it is off the tile, so not a handle.
        assert_eq!(g.handle_at(stack.x + stack.width, corner_row), None);
        // Between them: the Stack handle.
        assert!(matches!(
            g.handle_at(stack.x + 3, corner_row)
                .map(|h| h.boundary.clone()),
            Some(Boundary::Stack { .. })
        ));
    }

    /// What a press at (`x`, `y`) would pick up, as a short word.
    fn what_is_at(g: &Geometry, x: u16, y: u16) -> &'static str {
        match g.handle_at(x, y).map(|h| &h.boundary) {
            None => "none",
            Some(Boundary::Stack { .. }) => "stack",
            Some(Boundary::Master { .. }) => "master",
            Some(Boundary::Sidebar) => "sidebar",
        }
    }

    /// Both rows of the gap between the first two stack tiles, every column
    /// of the stack, classified: the left corner is the vertical handle's on
    /// both rows, the lower row's label cells are nobody's, everything else
    /// on either row is the stack boundary's — through the right border.
    fn assert_the_stack_gap_is_classified(width: u16, labels: &[&str], label_width: u16) {
        let panes = ["d", "w1", "w2"];
        let g = geo_labelled(width, 40, &panes, &|key| {
            labels[panes.iter().position(|p| *p == key.as_str()).unwrap()].to_string()
        });
        let (upper, lower) = (g.tiles[1].1, g.tiles[2].1);
        assert_eq!(upper.y + upper.height, lower.y, "tiles touch");
        let (upper_row, lower_row) = (lower.y - 1, lower.y);
        let right = lower.x + lower.width - 1;
        // `label_width` cells after the left corner are the label's.
        let label_cells = lower.x + 1..lower.x + 1 + label_width;
        for x in lower.x..=right {
            let expected_upper = if x == lower.x { "master" } else { "stack" };
            assert_eq!(
                what_is_at(&g, x, upper_row),
                expected_upper,
                "upper row, column {x} (width {width})"
            );
            let expected_lower = if x == lower.x {
                "master"
            } else if label_cells.contains(&x) {
                "none"
            } else {
                "stack"
            };
            assert_eq!(
                what_is_at(&g, x, lower_row),
                expected_lower,
                "lower row, column {x} (width {width})"
            );
        }
        assert_eq!(
            what_is_at(&g, right + 1, upper_row),
            "none",
            "past the tile"
        );
        assert_eq!(
            what_is_at(&g, right + 1, lower_row),
            "none",
            "past the tile"
        );
    }

    #[test]
    fn the_stack_gap_is_two_rows_minus_the_lower_tiles_label_for_a_short_label() {
        // " w2 " is four cells.
        assert_the_stack_gap_is_classified(120, &["d", "w1", "w2"], 4);
    }

    #[test]
    fn the_stack_gap_is_two_rows_minus_the_lower_tiles_label_for_a_long_label() {
        // " an-extremely-long-member-label " is 32 cells; the tile has 42
        // inner columns at this width.
        let long = "an-extremely-long-member-label";
        assert_the_stack_gap_is_classified(120, &["d", "w1", long], 32);
    }

    #[test]
    fn a_label_wider_than_its_tile_is_clipped_to_the_tile_and_leaves_no_handle_on_that_row() {
        // 76 columns: pane area 40, stack tile 20, inner width 18. The label
        // is clipped to 18 cells, the whole lower row between the corners.
        let long = "an-extremely-long-member-label";
        assert_the_stack_gap_is_classified(76, &["d", "w1", long], 18);
        // The upper row's handle is all that is left of the gap.
        let panes = ["d", "w1", "w2"];
        let g = geo_labelled(76, 40, &panes, &|_| long.to_string());
        let lower = g.tiles[2].1;
        assert_eq!(lower.width, 20);
        let right = lower.x + lower.width - 1;
        // The right border column is the one cell past the clipped label.
        assert_eq!(what_is_at(&g, right, lower.y), "stack");
        assert_eq!(what_is_at(&g, right - 1, lower.y), "none");
    }

    // --- stack order ----------------------------------------------------------

    fn names(panes: &[SessionKey]) -> Vec<&str> {
        panes.iter().map(SessionKey::as_str).collect()
    }

    fn tokens_of(key: &SessionKey) -> Option<&'static str> {
        // The member token of `w3` is `worker-3`, the Director's is `director`.
        match key.as_str() {
            "d" => Some("director"),
            "w1" => Some("worker-1"),
            "w2" => Some("worker-2"),
            "w3" => Some("worker-3"),
            "w4" => Some("worker-4"),
            _ => None,
        }
    }

    fn ranked(tokens: &[&str]) -> Vec<MemberToken> {
        tokens.iter().map(|t| t.to_string()).collect()
    }

    #[test]
    fn sort_stack_orders_the_stack_by_rank_and_leaves_the_master_alone() {
        let mut panes = vec![key("d"), key("w1"), key("w2"), key("w3")];
        let mut focused = 0;
        let moved = sort_stack(
            &mut panes,
            &mut focused,
            &ranked(&["worker-3", "worker-1", "worker-2"]),
            tokens_of,
        );
        assert!(moved);
        assert_eq!(names(&panes), ["d", "w3", "w1", "w2"]);
    }

    #[test]
    fn sort_stack_puts_unranked_members_after_the_ranked_ones_in_their_current_order() {
        let mut panes = vec![key("d"), key("w4"), key("w1"), key("w3"), key("w2")];
        let mut focused = 0;
        sort_stack(
            &mut panes,
            &mut focused,
            &ranked(&["worker-2", "worker-1"]),
            tokens_of,
        );
        // w2, w1 ranked; w4 and w3 are not, and keep the order they had.
        assert_eq!(names(&panes), ["d", "w2", "w1", "w4", "w3"]);
        // A key with no token at all is unranked too.
        let mut panes = vec![key("d"), key("ghost"), key("w1")];
        sort_stack(&mut panes, &mut focused, &ranked(&["worker-1"]), tokens_of);
        assert_eq!(names(&panes), ["d", "w1", "ghost"]);
    }

    #[test]
    fn sort_stack_keeps_focus_on_the_same_key_and_reports_no_change_when_sorted() {
        let mut panes = vec![key("d"), key("w1"), key("w2"), key("w3")];
        let mut focused = 1; // w1
        sort_stack(
            &mut panes,
            &mut focused,
            &ranked(&["worker-3", "worker-2", "worker-1"]),
            tokens_of,
        );
        assert_eq!(names(&panes), ["d", "w3", "w2", "w1"]);
        assert_eq!(focused, 3, "still w1");
        assert!(!sort_stack(
            &mut panes,
            &mut focused,
            &ranked(&["worker-3", "worker-2", "worker-1"]),
            tokens_of
        ));
        assert_eq!(focused, 3);
        // The master focused stays at 0.
        let mut panes = vec![key("d"), key("w1"), key("w2")];
        let mut focused = 0;
        sort_stack(&mut panes, &mut focused, &ranked(&["worker-2"]), tokens_of);
        assert_eq!((names(&panes), focused), (vec!["d", "w2", "w1"], 0));
    }

    #[test]
    fn sort_stack_never_moves_the_first_pane_even_if_it_is_ranked() {
        // Whoever sits at `panes[0]` is the master, whatever the preference
        // says about its token.
        let mut panes = vec![key("w1"), key("w2"), key("w3")];
        let mut focused = 0;
        sort_stack(
            &mut panes,
            &mut focused,
            &ranked(&["worker-3", "worker-2", "worker-1"]),
            tokens_of,
        );
        assert_eq!(names(&panes), ["w1", "w3", "w2"]);
        let mut none: Vec<SessionKey> = Vec::new();
        assert!(!sort_stack(
            &mut none,
            &mut focused,
            &ranked(&["worker-1"]),
            tokens_of
        ));
    }

    #[test]
    fn the_drop_slot_is_the_tile_under_the_row_or_the_nearest_end_of_the_stack() {
        // A pane area that starts below row 0, so there is a row above it.
        let stage = brigade_stage(&["d", "w1", "w2", "w3"]);
        let g = geometry(
            Rect::new(0, 2, 120, 38),
            &stage,
            SIDEBAR_WIDTH,
            DEFAULT_MASTER_PERMILLE,
            &[1, 1, 1],
            &|k| k.as_str().to_string(),
        );
        let stack_top = g.tiles[1].1.y;
        assert_eq!(
            g.drop_slot(stack_top - 1),
            Some(1),
            "above the stack: first"
        );
        assert_eq!(g.drop_slot(0), Some(1));
        for (slot, (_, tile)) in g.tiles.iter().enumerate().skip(1) {
            for row in [tile.y, tile.y + tile.height / 2, tile.y + tile.height - 1] {
                assert_eq!(g.drop_slot(row), Some(slot), "row {row} is in slot {slot}");
            }
        }
        let bottom = g.tiles[3].1.y + g.tiles[3].1.height;
        assert_eq!(g.drop_slot(bottom), Some(3), "below the stack: last");
        assert_eq!(g.drop_slot(500), Some(3));
        // No stack, or a stack of one: nowhere to drop.
        assert_eq!(geo(120, 40, &["d"]).drop_slot(5), None);
        assert_eq!(geo(120, 40, &["d", "w1"]).drop_slot(5), None);
    }

    #[test]
    fn the_move_line_is_the_targets_top_edge_going_up_and_its_bottom_edge_going_down() {
        let g = geo(120, 40, &["d", "w1", "w2", "w3"]);
        let (w1, w2, w3) = (g.tiles[1].1, g.tiles[2].1, g.tiles[3].1);
        assert_eq!(g.move_line(3, 1), Some(Rect::new(w1.x, w1.y, w1.width, 1)));
        assert_eq!(g.move_line(2, 1), Some(Rect::new(w1.x, w1.y, w1.width, 1)));
        assert_eq!(
            g.move_line(1, 3),
            Some(Rect::new(w3.x, w3.y + w3.height - 1, w3.width, 1))
        );
        assert_eq!(
            g.move_line(1, 2),
            Some(Rect::new(w2.x, w2.y + w2.height - 1, w2.width, 1))
        );
        assert_eq!(g.move_line(2, 2), None, "its own slot: nothing");
    }

    #[test]
    fn the_label_cells_exist_only_for_stack_tiles_of_a_stack_of_two_or_more() {
        let g = geo(120, 40, &["d", "w1", "w2"]);
        let (master, w1) = (g.tiles[0].1, g.tiles[1].1);
        assert_eq!(g.label_at(w1.x + 2, w1.y), Some(&key("w1")));
        assert_eq!(g.label_at(w1.x + 1, w1.y), Some(&key("w1")));
        assert_eq!(g.label_at(w1.x, w1.y), None, "the corner is not the label");
        // " w1 " is four cells.
        assert_eq!(g.label_at(w1.x + 5, w1.y), None, "past the label");
        assert_eq!(g.label_at(w1.x + 2, w1.y + 1), None, "not the title row");
        assert_eq!(
            g.label_at(master.x + 2, master.y),
            None,
            "the master's label"
        );
        // A stack of one has nothing to reorder.
        let one = geo(120, 40, &["d", "w1"]);
        assert_eq!(one.label_at(one.tiles[1].1.x + 2, one.tiles[1].1.y), None);
    }

    #[test]
    fn the_title_columns_are_the_drawn_text_clipped_to_the_inner_width() {
        let tile = Rect::new(10, 3, 30, 6);
        assert_eq!(title_text("worker-2"), " worker-2 ");
        assert_eq!(title_columns(tile, "worker-2"), (11, 21));
        // 28 inner columns: a longer label stops there.
        assert_eq!(title_columns(tile, &"x".repeat(50)), (11, 39));
        // A tile with no inner width has no label cells.
        assert_eq!(title_columns(Rect::new(4, 0, 2, 3), "w"), (5, 5));
        assert_eq!(title_columns(Rect::new(4, 0, 0, 3), "w"), (5, 5));
        // A wide character counts for its display width, as the drawn
        // title's clipping does.
        assert_eq!(title_columns(tile, "\u{30c6}\u{30b9}\u{30c8}"), (11, 19));
    }

    #[test]
    fn the_first_stack_tile_and_the_master_and_a_solo_tile_have_no_row_handle_on_their_top() {
        let g = geo(120, 40, &["d", "w1", "w2"]);
        for tile in [g.tiles[0].1, g.tiles[1].1] {
            for x in tile.x + 1..tile.x + tile.width - 1 {
                assert_ne!(what_is_at(&g, x, tile.y), "stack", "top row of {tile:?}");
            }
        }
        let solo = geo(120, 40, &["d"]);
        assert!(
            solo.handles()
                .iter()
                .all(|h| h.boundary == Boundary::Sidebar)
        );
    }

    #[test]
    fn a_title_row_the_tile_interior_and_the_sidebar_interior_are_not_handles() {
        let g = geo(120, 40, &["d", "w1", "w2"]);
        let lower = g.tiles[2].1;
        assert_eq!(
            g.handle_at(lower.x + 2, lower.y),
            None,
            "a stack tile's title label"
        );
        assert_eq!(
            g.handle_at(g.tiles[1].1.x + 5, g.tiles[1].1.y),
            None,
            "the first stack tile's title row: nothing above it to resize"
        );
        assert_eq!(
            g.handle_at(lower.x + 5, lower.y + 2),
            None,
            "a tile's interior"
        );
        assert_eq!(g.handle_at(10, 10), None, "the sidebar's interior");
        assert_eq!(
            g.handle_at(g.tiles[0].1.x + 5, 0),
            None,
            "the master's title row"
        );
    }

    #[test]
    fn the_sidebar_handle_spans_both_border_columns_over_the_full_body() {
        let g = geo(120, 40, &["d"]);
        let pane_x = g.areas.pane.x;
        for row in [0u16, 20, 38] {
            assert!(
                g.handle_at(pane_x - 1, row).is_some(),
                "sidebar border, row {row}"
            );
            assert!(g.handle_at(pane_x, row).is_some(), "pane border, row {row}");
        }
        assert_eq!(g.handle_at(pane_x - 2, 20), None);
        assert_eq!(g.handle_at(pane_x + 1, 20), None);
        assert_eq!(g.handle_at(pane_x, 39), None, "the status bar row");
    }

    #[test]
    fn handle_ranges_hold_the_minimums() {
        let g = geo(120, 40, &["d", "w1", "w2"]);
        let sidebar = g
            .handles()
            .iter()
            .find(|h| h.boundary == Boundary::Sidebar)
            .unwrap();
        assert_eq!(
            (sidebar.min, sidebar.max),
            (MIN_SIDEBAR_WIDTH, 120 - MIN_PANE_AREA_WIDTH)
        );
        let master = g
            .handles()
            .iter()
            .find(|h| matches!(h.boundary, Boundary::Master { .. }))
            .unwrap();
        assert_eq!(master.min, g.areas.pane.x + MIN_MASTER_WIDTH);
        assert_eq!(
            master.max,
            g.areas.pane.x + g.areas.pane.width - MIN_STACK_WIDTH
        );
        let stack = g
            .handles()
            .iter()
            .find(|h| matches!(h.boundary, Boundary::Stack { .. }))
            .unwrap();
        assert_eq!(stack.min, g.tiles[1].1.y + MIN_STACK_TILE_HEIGHT);
        assert_eq!(
            stack.max,
            g.tiles[2].1.y + g.tiles[2].1.height - MIN_STACK_TILE_HEIGHT
        );
    }

    #[test]
    fn clamping_a_position_stays_put_when_the_range_is_empty() {
        let g = geo(30, 10, &["d", "w1"]);
        let master = g
            .handles()
            .iter()
            .find(|h| matches!(h.boundary, Boundary::Master { .. }))
            .unwrap();
        assert!(master.min >= master.max);
        assert_eq!(master.clamp(0), master.position);
        assert_eq!(master.clamp(500), master.position);
    }

    #[test]
    fn no_size_panics_for_any_stage_and_the_tiles_stay_inside_the_pane_area() {
        for width in 0u16..=90 {
            for height in 0u16..=30 {
                for panes in [vec!["d"], vec!["d", "w1"], vec!["d", "w1", "w2", "w3"]] {
                    let g = geo(width, height, &panes);
                    for (_, tile) in &g.tiles {
                        assert!(tile.x >= g.areas.pane.x);
                        assert!(tile.right() <= g.areas.pane.right());
                        assert!(tile.bottom() <= g.areas.pane.bottom());
                    }
                    for handle in g.handles() {
                        let _ = handle.line_at(handle.position);
                    }
                }
            }
        }
    }

    #[test]
    fn the_line_for_a_position_sits_on_the_border_before_the_boundary() {
        let g = geo(120, 40, &["d", "w1", "w2"]);
        let sidebar = g
            .handles()
            .iter()
            .find(|h| h.boundary == Boundary::Sidebar)
            .unwrap();
        assert_eq!(sidebar.line_at(50), Rect::new(49, 0, 1, 39));
        let stack = g
            .handles()
            .iter()
            .find(|h| matches!(h.boundary, Boundary::Stack { .. }))
            .unwrap();
        let tile = g.tiles[1].1;
        assert_eq!(stack.line_at(10), Rect::new(tile.x, 9, tile.width, 1));
    }
}
