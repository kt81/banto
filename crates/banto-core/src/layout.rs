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

use std::collections::{HashMap, HashSet};

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

/// Rows a collapsed stack tile takes: its border, three content rows, its
/// border.
pub const COLLAPSED_TILE_HEIGHT: u16 = 5;

/// The stack column's default width while every stack tile is collapsed, in
/// columns. A width, not a share: a strip needs a fixed amount of room to be
/// readable however wide the terminal is.
pub const DEFAULT_COLLAPSED_STACK_WIDTH: u16 = 36;

/// Width of one title-row button (`[S]`, `[_]`, `[+]`).
const BUTTON_WIDTH: u16 = 3;

/// Layout preferences for one brigade, kept per [`BrigadeId`] and keyed by
/// member rather than by pane or position, so they survive the brigade going
/// off-stage, a pane being re-keyed, and a member being relaunched.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct BrigadeLayout {
    /// The master tile's share of the pane area's width, in permille.
    pub master_permille: u16,
    /// The stack column's width in columns while every stack tile is
    /// collapsed (the master takes the rest); the ordinary share applies the
    /// moment any tile is open. Its own preference, set by dragging the
    /// master/stack boundary in that state.
    pub collapsed_stack_width: u16,
    /// Relative heights of the stack's tiles, by member token.
    pub row_weights: HashMap<MemberToken, u16>,
    /// The operator's preferred order of the stack's tiles, top to bottom,
    /// by member token. `Stage::Brigade::panes` is what is on screen; this is
    /// what puts it back after something rebuilds `panes` (staging the
    /// brigade again, a Worker arriving). Empty until a tile has been moved.
    pub stack_order: Vec<MemberToken>,
    /// The members the operator collapsed one by one.
    pub minimized: HashSet<MemberToken>,
    /// The member the operator soloed, if any: while that member is a staged
    /// tile — the master or a stack tile — every stack tile but it is shown
    /// collapsed, whatever `minimized` says. An overlay, never a rewrite, so
    /// turning it off brings back exactly what was collapsed before. Named for the mixer's solo, not
    /// `Stage::Solo` (a single session that is no brigade's).
    pub solo_member: Option<MemberToken>,
}

impl Default for BrigadeLayout {
    fn default() -> Self {
        Self {
            master_permille: DEFAULT_MASTER_PERMILLE,
            collapsed_stack_width: DEFAULT_COLLAPSED_STACK_WIDTH,
            row_weights: HashMap::new(),
            stack_order: Vec::new(),
            minimized: HashSet::new(),
            solo_member: None,
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

/// The master tile's width when the stack beside it is all strips and wants
/// `stack_width` columns.
///
/// **Invariant: the result is never narrower than [`master_width`] at the same
/// area width and `master_permille`** (the master of the ordinary split). The
/// all-collapsed state exists to give the master room, so entering it can
/// only widen the master or leave it alone: whatever fit on the master before
/// (a title with its `[S]` button, say) still fits. To keep it, the stack's
/// share is first capped at what the ordinary split leaves it
/// (`area_width - master_width(..)`), then clamped like [`master_width`]: the
/// stack keeps [`MIN_STACK_WIDTH`] and the master [`MIN_MASTER_WIDTH`] when
/// both fit, one column each in a pane area too narrow for that. (Both clamps
/// preserve the invariant: the ordinary split already respects them.) So the
/// preference can make the strips narrower than the ordinary stack but never
/// wider.
pub fn master_width_beside_strips(area_width: u16, stack_width: u16, master_permille: u16) -> u16 {
    let ordinary = master_width(area_width, master_permille);
    let stack_width = stack_width.min(area_width - ordinary);
    if area_width >= MIN_MASTER_WIDTH + MIN_STACK_WIDTH {
        area_width - stack_width.clamp(MIN_STACK_WIDTH, area_width - MIN_MASTER_WIDTH)
    } else if area_width >= 2 {
        area_width
            .saturating_sub(stack_width)
            .clamp(1, area_width - 1)
    } else {
        area_width
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
    place(
        pane_area,
        stage,
        master_permille,
        DEFAULT_COLLAPSED_STACK_WIDTH,
        stack_weights,
        &[],
    )
    .tiles
}

/// Where [`place`] put every tile, and how.
struct Placement {
    tiles: Vec<(SessionKey, Rect)>,
    /// Per tile (`tiles` order, the master's entry always false): drawn as a
    /// strip.
    collapsed: Vec<bool>,
    /// Every stack tile is collapsed (and there is at least one).
    all_collapsed: bool,
}

/// [`stage_tiles`] with collapsed tiles (`collapsed`: one flag per stack tile,
/// in `panes` order; a missing one counts as expanded).
///
/// The stack is always the right-hand column. A collapsed tile keeps its
/// place in the order at [`COLLAPSED_TILE_HEIGHT`] rows; the expanded tiles
/// share the rest by their weights (four rows at least). While *every* stack
/// tile is collapsed the column narrows to `collapsed_stack_width` columns
/// (the master taking the rest), the strips stack from the top, and the space
/// below them is empty: there is nothing there to draw or to hit.
///
/// A column too short for its strips and the expanded tiles' minimums
/// degrades to a plain proportional split of the rows it has — every tile
/// shrinks, none is dropped — rather than panicking.
fn place(
    pane_area: Rect,
    stage: &Stage,
    master_permille: u16,
    collapsed_stack_width: u16,
    stack_weights: &[u16],
    collapsed_in: &[bool],
) -> Placement {
    let single = |key: &SessionKey| Placement {
        tiles: vec![(key.clone(), pane_area)],
        collapsed: vec![false],
        all_collapsed: false,
    };
    let nothing = || Placement {
        tiles: Vec::new(),
        collapsed: Vec::new(),
        all_collapsed: false,
    };
    match stage {
        Stage::Empty => nothing(),
        Stage::Solo(key) => single(key),
        Stage::Brigade { panes, .. } => match panes.split_first() {
            None => nothing(),
            Some((director, [])) => single(director),
            Some((director, workers)) => {
                let n = workers.len();
                let collapsed: Vec<bool> = (0..n)
                    .map(|i| collapsed_in.get(i).copied().unwrap_or(false))
                    .collect();
                let weights: Vec<u16> = (0..n)
                    .map(|i| stack_weights.get(i).copied().unwrap_or(1))
                    .collect();
                let all_collapsed = collapsed.iter().all(|c| *c);
                let mut collapsed_flags = vec![false];
                collapsed_flags.extend(&collapsed);
                let master_w = if all_collapsed {
                    master_width_beside_strips(
                        pane_area.width,
                        collapsed_stack_width,
                        master_permille,
                    )
                } else {
                    master_width(pane_area.width, master_permille)
                };
                let master = Rect {
                    width: master_w,
                    ..pane_area
                };
                let stack = Rect {
                    x: pane_area.x + master_w,
                    width: pane_area.width - master_w,
                    ..pane_area
                };
                let collapsed_count = collapsed.iter().filter(|c| **c).count() as u16;
                let expanded_count = n as u16 - collapsed_count;
                let fixed = COLLAPSED_TILE_HEIGHT.saturating_mul(collapsed_count);
                let fits = expanded_count > 0
                    && stack.height >= fixed.saturating_add(MIN_STACK_TILE_HEIGHT * expanded_count);
                let heights: Vec<u16> = if fits {
                    // Collapsed tiles keep their fixed height; the expanded
                    // ones share the rest by their weights.
                    let shared: Vec<u16> = weights
                        .iter()
                        .zip(&collapsed)
                        .filter(|(_, c)| !**c)
                        .map(|(w, _)| *w)
                        .collect();
                    let mut shared =
                        stack_heights(stack.height - fixed, &shared, MIN_STACK_TILE_HEIGHT)
                            .into_iter();
                    collapsed
                        .iter()
                        .map(|c| {
                            if *c {
                                COLLAPSED_TILE_HEIGHT
                            } else {
                                shared.next().unwrap_or(0)
                            }
                        })
                        .collect()
                } else if all_collapsed && stack.height >= fixed {
                    // All strips and room for them: five rows each from the
                    // top, the rest of the column left empty.
                    vec![COLLAPSED_TILE_HEIGHT; n]
                } else if collapsed_count == 0 {
                    stack_heights(stack.height, &weights, MIN_STACK_TILE_HEIGHT)
                } else {
                    // Not enough room for the strips and the expanded tiles'
                    // minimums (or for the strips alone): split what there
                    // is in proportion to what each would like.
                    let wanted: Vec<u16> = collapsed
                        .iter()
                        .map(|c| {
                            if *c {
                                COLLAPSED_TILE_HEIGHT
                            } else {
                                MIN_STACK_TILE_HEIGHT
                            }
                        })
                        .collect();
                    stack_heights(stack.height, &wanted, 1)
                };
                let mut tiles = vec![(director.clone(), master)];
                let mut y = stack.y;
                for (worker, height) in workers.iter().zip(heights) {
                    tiles.push((worker.clone(), Rect { y, height, ..stack }));
                    y += height;
                }
                Placement {
                    tiles,
                    collapsed: collapsed_flags,
                    all_collapsed,
                }
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
    /// Per tile, in `tiles` order: drawn as a strip (the master never is).
    pub collapsed: Vec<bool>,
    /// Every stack tile is collapsed: the stack column is narrowed to the
    /// collapsed stack width and the master takes the rest.
    pub all_collapsed: bool,
    /// The title-row buttons of every stack tile that has room for them.
    buttons: Vec<TileButton>,
}

/// What a title-row button does.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ButtonKind {
    /// `[S]`: solo this tile.
    Solo,
    /// `[_]` on an expanded tile, `[+]` on a collapsed one: collapse or open
    /// this tile.
    Minimize,
}

/// One button in a stack tile's title row. The geometry owns its cells, like
/// the handles and the label, so drawing and hit-testing cannot disagree.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TileButton {
    pub key: SessionKey,
    pub kind: ButtonKind,
    pub rect: Rect,
    /// What is drawn there, ASCII: `[S]`, `[_]` or `[+]`.
    pub text: &'static str,
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

    /// Whether the tile of `key` is drawn as a strip.
    pub fn is_collapsed(&self, key: &SessionKey) -> bool {
        self.tiles
            .iter()
            .position(|(tile, _)| tile == key)
            .is_some_and(|index| self.collapsed[index])
    }

    /// Every title-row button, in stack order.
    pub fn buttons(&self) -> &[TileButton] {
        &self.buttons
    }

    /// The button a press at (`column`, `row`) lands on.
    pub fn button_at(&self, column: u16, row: u16) -> Option<&TileButton> {
        let at = ratatui_core::layout::Position::new(column, row);
        self.buttons.iter().find(|button| button.rect.contains(at))
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
    /// falls in, the first stack slot above the stack, the last below it —
    /// including the empty space under a column of strips, which is "below".
    /// `None` when there is no stack to drop into.
    pub fn drop_slot(&self, row: u16) -> Option<usize> {
        let stack = self.tiles.get(1..).filter(|stack| stack.len() >= 2)?;
        let last = stack.len();
        if row < stack[0].1.y {
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

/// Everything the operator has chosen (and the collapsing in force) that the
/// geometry depends on, resolved by the caller.
#[derive(Debug, Clone, Copy)]
pub struct LayoutPrefs<'a> {
    pub sidebar_width: u16,
    pub master_permille: u16,
    /// The stack column's width while every stack tile is collapsed.
    pub collapsed_stack_width: u16,
    /// One entry per stack tile of the staged brigade, in `panes` order (see
    /// [`place`]).
    pub stack_weights: &'a [u16],
    /// One entry per stack tile, in `panes` order: shown collapsed.
    pub stack_collapsed: &'a [bool],
    /// The tile in the master position has a member to solo, so it gets a
    /// `[S]` button. (The geometry knows keys, not members.)
    pub master_soloable: bool,
}

/// Compute the whole geometry for `area`.
///
/// `label_of` is a tile's
/// title label (the shell draws it with [`title_text`]); the stack row handle
/// keeps off the lower tile's label and buttons, which are the tile's own.
pub fn geometry(
    area: Rect,
    stage: &Stage,
    prefs: &LayoutPrefs,
    label_of: &dyn Fn(&SessionKey) -> String,
) -> Geometry {
    let LayoutPrefs {
        sidebar_width,
        master_permille,
        collapsed_stack_width,
        stack_weights,
        stack_collapsed,
        master_soloable,
    } = *prefs;
    let areas = layout(area, sidebar_width);
    let Placement {
        tiles,
        collapsed,
        all_collapsed,
    } = place(
        areas.pane,
        stage,
        master_permille,
        collapsed_stack_width,
        stack_weights,
        stack_collapsed,
    );
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

    // Buttons first: the stack row handles keep off them. Every stack tile
    // gets solo and collapse; the master gets solo alone (it cannot be
    // collapsed), and only where there is a stack to collapse.
    let mut buttons = Vec::new();
    if matches!(stage, Stage::Brigade { .. }) && tiles.len() >= 2 {
        for (index, (key, tile)) in tiles.iter().enumerate() {
            let (_, label_end) = title_columns(*tile, &label_of(key));
            if index == 0 {
                if master_soloable {
                    buttons.extend(buttons_for(key, *tile, label_end, false, false));
                }
            } else {
                buttons.extend(buttons_for(key, *tile, label_end, collapsed[index], true));
            }
        }
    }

    // The master/stack boundary exists whatever is collapsed (what dragging
    // it sets depends on whether the stack is all strips). The row boundaries
    // below lie between expanded tiles, so an all-strips stack has none.
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
                // While every tile is collapsed the master is never narrower
                // than the ordinary split's (see `master_width_beside_strips`),
                // so that is as far left as the boundary may be dragged.
                min: if room {
                    pane.x
                        + if all_collapsed {
                            master_width(pane.width, master_permille).max(MIN_MASTER_WIDTH)
                        } else {
                            MIN_MASTER_WIDTH
                        }
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
        // one, so a vertical handle wins where they meet. A boundary lies
        // between two *expanded* tiles that are neighbours once the collapsed
        // ones are ignored; dragging it changes those two heights only.
        let expanded: Vec<usize> = (1..tiles.len()).filter(|i| !collapsed[*i]).collect();
        for pair in expanded.windows(2) {
            let (above_key, above) = (&tiles[pair[0]].0, tiles[pair[0]].1);
            let (below_key, below) = (&tiles[pair[1]].0, tiles[pair[1]].1);
            if above.width < 3 || above.height == 0 || below.height == 0 {
                continue;
            }
            let position = above.y + above.height;
            let (min, max) = (
                above.y + MIN_STACK_TILE_HEIGHT,
                (position + below.height).saturating_sub(MIN_STACK_TILE_HEIGHT),
            );
            // The gap the eye sees between directly adjacent tiles is two
            // rows: the upper tile's bottom border and the lower tile's top
            // border. Both are the handle, from the cell after the left
            // corner (the master/stack handle's) through the right border
            // column — the screen edge, where a pointer lands when the
            // boundary is grabbed from the right. Except the lower row's
            // label cells (the tile's own: the handle it is picked up by to
            // be moved, [`Geometry::label_at`]) and its buttons. With a
            // collapsed tile in between there is only the upper row.
            let mut hits = vec![Rect::new(above.x + 1, position - 1, above.width - 1, 1)];
            if below.y == position {
                let (_, label_end) = title_columns(below, &label_of(below_key));
                let right = below.x + below.width;
                let lower_start = label_end.max(below.x + 1);
                let kept_off = buttons
                    .iter()
                    .filter(|button| button.key == *below_key)
                    .map(|button| button.rect.x)
                    .min();
                // Up to the first button, and the right corner past the last.
                let first_run_end = kept_off.unwrap_or(right);
                if lower_start < first_run_end {
                    hits.push(Rect::new(
                        lower_start,
                        below.y,
                        first_run_end - lower_start,
                        1,
                    ));
                }
                if kept_off.is_some() {
                    hits.push(Rect::new(right - 1, below.y, 1, 1));
                }
            }
            handles.push(Handle {
                boundary: Boundary::Stack {
                    brigade: *id,
                    above: above_key.clone(),
                    below: below_key.clone(),
                },
                axis: Axis::Horizontal,
                hits,
                position,
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
        collapsed,
        all_collapsed,
        buttons,
    }
}

/// The title-row buttons of a tile — solo and collapse for a stack tile, solo
/// alone (`with_minimize` false) for the master — right-aligned and ending just
/// before the right corner, when the row has room for the label, one gap cell
/// and all of them; none otherwise (the keys still work).
fn buttons_for(
    key: &SessionKey,
    tile: Rect,
    label_end: u16,
    collapsed: bool,
    with_minimize: bool,
) -> Vec<TileButton> {
    let count = if with_minimize { 2 } else { 1 };
    let inner = tile.width.saturating_sub(2);
    let label_width = label_end.saturating_sub(tile.x.saturating_add(1));
    if inner < label_width + 1 + count * BUTTON_WIDTH || tile.width < 2 {
        return Vec::new();
    }
    let first = tile.x + tile.width - 1 - count * BUTTON_WIDTH;
    let mut buttons = vec![TileButton {
        key: key.clone(),
        kind: ButtonKind::Solo,
        rect: Rect::new(first, tile.y, BUTTON_WIDTH, 1),
        text: "[S]",
    }];
    if with_minimize {
        buttons.push(TileButton {
            key: key.clone(),
            kind: ButtonKind::Minimize,
            rect: Rect::new(first + BUTTON_WIDTH, tile.y, BUTTON_WIDTH, 1),
            text: if collapsed { "[+]" } else { "[_]" },
        });
    }
    buttons
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
            &LayoutPrefs {
                sidebar_width: SIDEBAR_WIDTH,
                master_permille: DEFAULT_MASTER_PERMILLE,
                collapsed_stack_width: DEFAULT_COLLAPSED_STACK_WIDTH,
                stack_weights: &vec![1; panes.len().saturating_sub(1)],
                stack_collapsed: &[],
                master_soloable: false,
            },
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
        // The two buttons, six cells ending before the right corner, when
        // the row has room for the label, a gap and both.
        let button_cells = if lower.width - 2 >= label_width + 1 + 6 {
            right - 6..right
        } else {
            0..0
        };
        for x in lower.x..=right {
            let expected_upper = if x == lower.x { "master" } else { "stack" };
            assert_eq!(
                what_is_at(&g, x, upper_row),
                expected_upper,
                "upper row, column {x} (width {width})"
            );
            let expected_lower = if x == lower.x {
                "master"
            } else if label_cells.contains(&x) || button_cells.contains(&x) {
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

    // --- collapsed tiles ------------------------------------------------------

    fn geo_collapsed(width: u16, height: u16, panes: &[&str], collapsed: &[bool]) -> Geometry {
        geo_collapsed_weighted(width, height, panes, collapsed, &vec![1; panes.len() - 1])
    }

    fn geo_collapsed_weighted(
        width: u16,
        height: u16,
        panes: &[&str],
        collapsed: &[bool],
        weights: &[u16],
    ) -> Geometry {
        geometry(
            Rect::new(0, 0, width, height),
            &brigade_stage(panes),
            &LayoutPrefs {
                sidebar_width: SIDEBAR_WIDTH,
                master_permille: DEFAULT_MASTER_PERMILLE,
                collapsed_stack_width: DEFAULT_COLLAPSED_STACK_WIDTH,
                stack_weights: weights,
                stack_collapsed: collapsed,
                master_soloable: false,
            },
            &|key| key.as_str().to_string(),
        )
    }

    fn heights(g: &Geometry) -> Vec<u16> {
        g.tiles
            .iter()
            .skip(1)
            .map(|(_, rect)| rect.height)
            .collect()
    }

    #[test]
    fn with_nothing_collapsed_the_geometry_is_what_it_was() {
        for n in 1..=5 {
            let mut panes = vec!["d"];
            panes.extend(["w1", "w2", "w3", "w4", "w5"].iter().take(n));
            let plain = geo(120, 40, &panes);
            let flags = geo_collapsed(120, 40, &panes, &vec![false; n]);
            assert_eq!(plain.tiles, flags.tiles, "{n} workers");
            assert!(!flags.all_collapsed);
            assert!(flags.collapsed.iter().all(|c| !c));
        }
    }

    #[test]
    fn a_collapsed_tile_keeps_its_place_at_five_rows_and_the_rest_share_what_is_left() {
        let g = geo_collapsed(120, 40, &["d", "w1", "w2", "w3"], &[false, true, false]);
        // 39 stack rows: 5 for the strip, 34 shared by two equal weights.
        assert_eq!(heights(&g), vec![17, COLLAPSED_TILE_HEIGHT, 17]);
        assert_eq!(g.collapsed, vec![false, false, true, false]);
        assert!(g.is_collapsed(&key("w2")) && !g.is_collapsed(&key("w1")));
        // In order, touching, filling the stack.
        let mut y = g.tiles[1].1.y;
        for (_, tile) in &g.tiles[1..] {
            assert_eq!(tile.y, y);
            y += tile.height;
        }
        assert_eq!(y, 39);
        // The master is untouched.
        assert_eq!(
            g.tiles[0].1,
            geo(120, 40, &["d", "w1", "w2", "w3"]).tiles[0].1
        );
    }

    #[test]
    fn expanded_tiles_share_the_remaining_rows_by_their_weights() {
        let g = geo_collapsed_weighted(
            120,
            40,
            &["d", "w1", "w2", "w3"],
            &[false, true, false],
            &[1, 50, 3],
        );
        // The collapsed tile's weight is nothing to anyone: 34 rows by 1:3.
        assert_eq!(heights(&g), vec![9, COLLAPSED_TILE_HEIGHT, 25]);
    }

    #[test]
    fn collapsed_tiles_degrade_without_panicking_when_there_is_no_room() {
        for height in 0..=30u16 {
            for collapsed in [
                [false, true, false],
                [true, true, false],
                [true, true, true],
                [false, false, true],
            ] {
                let g = geo_collapsed(120, height, &["d", "w1", "w2", "w3"], &collapsed);
                // Whatever the room, no tile leaves the pane area.
                let pane = g.areas.pane;
                for (key, tile) in &g.tiles {
                    assert!(
                        tile.x >= pane.x
                            && tile.y >= pane.y
                            && tile.right() <= pane.right()
                            && tile.bottom() <= pane.bottom(),
                        "{key:?} {tile:?} outside {pane:?} ({height} rows, {collapsed:?})"
                    );
                }
            }
        }
        // One row short of the strip plus two minimums: proportional, summing.
        let g = geo_collapsed(120, 14, &["d", "w1", "w2", "w3"], &[false, true, false]);
        assert_eq!(heights(&g).iter().sum::<u16>(), g.areas.pane.height);
    }

    #[test]
    fn when_every_stack_tile_is_collapsed_the_stack_is_a_narrow_column_of_strips_from_the_top() {
        let g = geo_collapsed(120, 40, &["d", "w1", "w2", "w3"], &[true, true, true]);
        assert!(g.all_collapsed);
        let pane = g.areas.pane;
        let master = g.tiles[0].1;
        // The column is the collapsed stack width wide; the master has the rest.
        for (key, tile) in &g.tiles[1..] {
            assert_eq!(tile.width, DEFAULT_COLLAPSED_STACK_WIDTH, "{key:?}");
            assert_eq!(tile.x, pane.x + pane.width - DEFAULT_COLLAPSED_STACK_WIDTH);
        }
        assert_eq!(master.width, pane.width - DEFAULT_COLLAPSED_STACK_WIDTH);
        assert_eq!(master.height, pane.height, "the master is as tall as ever");
        // Strips: five rows each, stacked from the top in stack order.
        let mut y = pane.y;
        for (key, tile) in &g.tiles[1..] {
            assert_eq!(tile.y, y, "{key:?} follows the previous strip");
            assert_eq!(tile.height, COLLAPSED_TILE_HEIGHT);
            y += tile.height;
        }
        assert_eq!(
            g.tiles[1..]
                .iter()
                .map(|(k, _)| k.as_str())
                .collect::<Vec<_>>(),
            ["w1", "w2", "w3"]
        );
        // Below them the column is empty: nothing there to draw or hit.
        assert!(y < pane.y + pane.height);
        let empty_row = y + 2;
        assert!(
            g.tiles
                .iter()
                .all(|(_, t)| !t.contains(ratatui_core::layout::Position::new(
                    pane.x + pane.width - 5,
                    empty_row
                )))
        );
        assert_eq!(what_is_at(&g, pane.x + pane.width - 5, empty_row), "none");
        assert_eq!(g.button_at(pane.x + pane.width - 5, empty_row), None);
        assert_eq!(g.label_at(pane.x + pane.width - 5, empty_row), None);
    }

    #[test]
    fn the_collapsed_stack_width_is_a_preference_clamped_at_use() {
        let width_with = |width: u16, preferred: u16| {
            let g = geometry(
                Rect::new(0, 0, width, 40),
                &brigade_stage(&["d", "w1", "w2"]),
                &LayoutPrefs {
                    sidebar_width: SIDEBAR_WIDTH,
                    master_permille: DEFAULT_MASTER_PERMILLE,
                    collapsed_stack_width: preferred,
                    stack_weights: &[1, 1],
                    stack_collapsed: &[true, true],
                    master_soloable: false,
                },
                &|key| key.as_str().to_string(),
            );
            (g.tiles[0].1.width, g.tiles[1].1.width, g.areas.pane.width)
        };
        // A pane area 84 wide (120 - 36), the ordinary stack 42 of it. A
        // preference narrower than that is used as is.
        assert_eq!(width_with(120, 30), (54, 30, 84));
        // Too small: the stack keeps its minimum...
        assert_eq!(width_with(120, 2).1, MIN_STACK_WIDTH);
        // ...too large: the strips never get more than the ordinary stack
        // would have (the master is never narrower than in the ordinary
        // split).
        assert_eq!(width_with(120, 50), (42, 42, 84));
        assert_eq!(width_with(120, 200), (42, 42, 84));
        // Not rewritten: asking again with a roomier terminal gives the
        // preferred width back.
        assert_eq!(width_with(160, 50).1, 50);
        // A pane area too narrow for both minimums degrades, one column at least each.
        for width in 36..60u16 {
            let (master, stack, pane) = width_with(width, 36);
            assert_eq!(master + stack, pane, "{width}");
            assert!(master >= 1 && stack >= 1, "{width}");
        }
        // The ordinary share is untouched by it while a tile is open.
        let open = geo_collapsed(120, 40, &["d", "w1", "w2"], &[true, false]);
        assert!(!open.all_collapsed);
        assert_eq!(open.tiles[0].1.width, 42);
    }

    #[test]
    fn the_all_collapsed_master_is_never_narrower_than_the_ordinary_master() {
        // Every terminal width (so every pane-area width the layout produces),
        // every preference including ones larger than the pane, every share.
        let preferences = [0u16, 1, 10, 19, 20, 36, 50, 84, 150, 400, u16::MAX];
        let shares = [0u16, 1, 100, 250, 499, 500, 501, 750, 900, 1000];
        for width in 0..=240u16 {
            for &preferred in &preferences {
                for &share in &shares {
                    let build = |collapsed: &[bool]| {
                        geometry(
                            Rect::new(0, 0, width, 40),
                            &brigade_stage(&["d", "w1", "w2"]),
                            &LayoutPrefs {
                                sidebar_width: SIDEBAR_WIDTH,
                                master_permille: share,
                                collapsed_stack_width: preferred,
                                stack_weights: &[1, 1],
                                stack_collapsed: collapsed,
                                master_soloable: true,
                            },
                            &|key| key.as_str().to_string(),
                        )
                    };
                    let ordinary = build(&[false, false]);
                    let collapsed = build(&[true, true]);
                    let context = format!("width {width}, preference {preferred}, share {share}");
                    assert!(
                        collapsed.all_collapsed && !ordinary.all_collapsed,
                        "{context}"
                    );
                    let (before, after) = (ordinary.tiles[0].1, collapsed.tiles[0].1);
                    assert!(
                        after.width >= before.width,
                        "{context}: {before:?} -> {after:?}"
                    );
                    // The tiles still partition the pane area's width.
                    assert_eq!(
                        after.width + collapsed.tiles[1].1.width,
                        collapsed.areas.pane.width,
                        "{context}"
                    );
                    // So a button that fit before still fits.
                    let has = |g: &Geometry| g.buttons().iter().any(|b| b.key.as_str() == "d");
                    assert!(!has(&ordinary) || has(&collapsed), "{context}");
                }
            }
        }
    }

    #[test]
    fn the_master_stack_handle_cannot_be_dragged_left_of_the_ordinary_masters_edge_when_all_collapsed()
     {
        let g = geo_collapsed(120, 40, &["d", "w1", "w2"], &[true, true]);
        let handle = g
            .handles()
            .iter()
            .find(|h| matches!(h.boundary, Boundary::Master { .. }))
            .unwrap();
        let pane = g.areas.pane;
        // 84 wide: the ordinary master is 42, so the boundary stops there.
        assert_eq!(handle.min, pane.x + 42);
        assert_eq!(handle.clamp(0), pane.x + 42);
        // The ordinary state still goes down to the master's own minimum.
        let open = geo_collapsed(120, 40, &["d", "w1", "w2"], &[false, false]);
        let open_handle = open
            .handles()
            .iter()
            .find(|h| matches!(h.boundary, Boundary::Master { .. }))
            .unwrap();
        assert_eq!(open_handle.min, pane.x + MIN_MASTER_WIDTH);
    }

    #[test]
    fn more_strips_than_rows_degrade_proportionally_and_nothing_panics() {
        // Eight strips want 40 rows; the pane has 39. Every strip shrinks a
        // little rather than the last being dropped or clipped.
        let panes: Vec<&str> = ["d", "w1", "w2", "w3", "w4", "w5", "w6", "w7", "w8"].to_vec();
        let g = geo_collapsed(120, 40, &panes, &[true; 8]);
        assert!(g.all_collapsed);
        assert_eq!(heights(&g).iter().sum::<u16>(), g.areas.pane.height);
        assert!(heights(&g).iter().all(|h| *h >= 4), "{:?}", heights(&g));
        // Enough rows for all of them: five each from the top, the rest empty.
        let roomy = geo_collapsed(120, 50, &panes, &[true; 8]);
        assert_eq!(heights(&roomy), vec![COLLAPSED_TILE_HEIGHT; 8]);
        for height in 0..14 {
            for width in 0..60 {
                let _ = geo_collapsed(width, height, &["d", "w1", "w2"], &[true, true]);
            }
        }
    }

    #[test]
    fn the_master_stack_handle_exists_in_both_states_and_row_handles_only_between_open_tiles() {
        let all = geo_collapsed(120, 40, &["d", "w1", "w2", "w3"], &[true, true, true]);
        let kinds: Vec<&Boundary> = all.handles().iter().map(|h| &h.boundary).collect();
        assert!(kinds.contains(&&Boundary::Sidebar));
        assert!(kinds.contains(&&Boundary::Master { brigade: 1 }));
        assert!(
            kinds.iter().all(|b| !matches!(b, Boundary::Stack { .. })),
            "no row handle between strips"
        );
        let handle = all
            .handles()
            .iter()
            .find(|h| matches!(h.boundary, Boundary::Master { .. }))
            .unwrap();
        assert_eq!(
            handle.position, all.tiles[1].1.x,
            "at the narrow column's edge"
        );
        assert_eq!(
            handle.hits()[0].height,
            all.areas.pane.height,
            "the full body height, strips or not"
        );
        let column = geo_collapsed(120, 40, &["d", "w1", "w2", "w3"], &[true, true, false]);
        assert!(
            column
                .handles()
                .iter()
                .any(|h| matches!(h.boundary, Boundary::Master { .. }))
        );
    }

    #[test]
    fn a_row_handle_lies_between_expanded_neighbours_ignoring_collapsed_tiles() {
        // w1 expanded, w2 collapsed, w3 expanded: one boundary, w1 | w3.
        let g = geo_collapsed(120, 40, &["d", "w1", "w2", "w3"], &[false, true, false]);
        let rows: Vec<&Handle> = g
            .handles()
            .iter()
            .filter(|h| matches!(h.boundary, Boundary::Stack { .. }))
            .collect();
        assert_eq!(rows.len(), 1);
        let handle = rows[0];
        assert_eq!(
            handle.boundary,
            Boundary::Stack {
                brigade: 1,
                above: key("w1"),
                below: key("w3")
            }
        );
        let (w1, w2, w3) = (g.tiles[1].1, g.tiles[2].1, g.tiles[3].1);
        // The upper tile's bottom border row only: the tiles are not adjacent.
        assert_eq!(handle.position, w1.y + w1.height);
        assert_eq!(handle.hits().len(), 1);
        assert_eq!(what_is_at(&g, w1.x + 5, w1.y + w1.height - 1), "stack");
        assert_eq!(
            what_is_at(&g, w2.x + 5, w2.y),
            "none",
            "the strip's own title row"
        );
        assert_eq!(
            what_is_at(&g, w3.x + 20, w3.y),
            "none",
            "a title row past a gap is not a handle"
        );
        // It moves the two expanded tiles against each other, keeping both at
        // four rows or more.
        assert_eq!(handle.min, w1.y + MIN_STACK_TILE_HEIGHT);
        assert_eq!(
            handle.max,
            w1.y + w1.height + w3.height - MIN_STACK_TILE_HEIGHT
        );
    }

    #[test]
    fn adjacent_expanded_tiles_keep_the_two_row_gap_beside_a_collapsed_one() {
        let g = geo_collapsed(120, 40, &["d", "w1", "w2", "w3"], &[false, false, true]);
        let rows: Vec<&Handle> = g
            .handles()
            .iter()
            .filter(|h| matches!(h.boundary, Boundary::Stack { .. }))
            .collect();
        assert_eq!(rows.len(), 1, "only w1 | w2: w3 is collapsed");
        assert_eq!(
            rows[0].boundary,
            Boundary::Stack {
                brigade: 1,
                above: key("w1"),
                below: key("w2")
            }
        );
        let w2 = g.tiles[2].1;
        assert_eq!(
            what_is_at(&g, w2.x + 20, w2.y),
            "stack",
            "the lower title row past the label"
        );
    }

    #[test]
    fn buttons_sit_right_aligned_before_the_corner_only_where_the_row_has_room() {
        let g = geo_collapsed(120, 40, &["d", "w1", "w2"], &[false, true]);
        for (name, text_of_minimize) in [("w1", "[_]"), ("w2", "[+]")] {
            let tile = g.tiles.iter().find(|(k, _)| k.as_str() == name).unwrap().1;
            let buttons: Vec<&TileButton> = g
                .buttons()
                .iter()
                .filter(|b| b.key.as_str() == name)
                .collect();
            assert_eq!(buttons.len(), 2, "{name}");
            assert_eq!(buttons[0].kind, ButtonKind::Solo);
            assert_eq!(buttons[0].text, "[S]");
            assert_eq!(buttons[1].kind, ButtonKind::Minimize);
            assert_eq!(buttons[1].text, text_of_minimize);
            let right_corner = tile.x + tile.width - 1;
            assert_eq!(buttons[0].rect, Rect::new(right_corner - 6, tile.y, 3, 1));
            assert_eq!(buttons[1].rect, Rect::new(right_corner - 3, tile.y, 3, 1));
        }
        // The master has none.
        assert!(g.buttons().iter().all(|b| b.key.as_str() != "d"));
        // A cell on a button finds it.
        let w1 = g.tiles[1].1;
        assert_eq!(
            g.button_at(w1.x + w1.width - 5, w1.y).map(|b| b.kind),
            Some(ButtonKind::Solo)
        );
        assert_eq!(
            g.button_at(w1.x + w1.width - 2, w1.y).map(|b| b.kind),
            Some(ButtonKind::Minimize)
        );
        assert_eq!(
            g.button_at(w1.x + w1.width - 1, w1.y),
            None,
            "the corner is not a button"
        );
    }

    #[test]
    fn buttons_need_room_for_the_label_a_gap_and_both_buttons() {
        // The label " w1 " is 4 cells, so a tile needs 4 + 1 + 6 inner columns.
        let buttons_at = |width: u16| {
            // A pane area exactly `width` columns wide: sidebar 36 + width.
            let g = geo_collapsed(36 + width, 40, &["d", "w1", "w2"], &[false, false]);
            let stack = g.tiles[1].1;
            (
                stack.width,
                g.buttons()
                    .iter()
                    .filter(|b| b.key.as_str() == "w1")
                    .count(),
            )
        };
        // Master 50/50: the stack tile is about half of `width`. Find the
        // threshold from the geometry itself.
        let mut seen_none = false;
        let mut seen_both = false;
        for width in 20..=60 {
            let (tile_width, count) = buttons_at(width);
            let room = tile_width.saturating_sub(2) >= 4 + 1 + 6;
            assert_eq!(count == 2, room, "stack tile {tile_width} wide");
            seen_none |= count == 0;
            seen_both |= count == 2;
        }
        assert!(seen_none && seen_both, "the sweep crosses the threshold");
    }

    #[test]
    fn the_row_handle_keeps_off_the_lower_tiles_buttons_but_not_the_right_corner() {
        let g = geo_collapsed(120, 40, &["d", "w1", "w2"], &[false, false]);
        let w2 = g.tiles[2].1;
        let right = w2.x + w2.width - 1;
        // Label (10 cells from x+1), then handle, then six button cells, then
        // the corner.
        assert_eq!(what_is_at(&g, w2.x + 3, w2.y), "none", "label");
        assert_eq!(
            what_is_at(&g, w2.x + 12, w2.y),
            "stack",
            "gap before the buttons"
        );
        for x in right - 6..right {
            assert_eq!(what_is_at(&g, x, w2.y), "none", "button cell {x}");
        }
        assert_eq!(what_is_at(&g, right, w2.y), "stack", "the right corner");
        // The upper row is whole.
        for x in w2.x + 1..=right {
            assert_eq!(what_is_at(&g, x, w2.y - 1), "stack");
        }
    }

    #[test]
    fn among_strips_the_drop_slot_is_by_row_and_the_empty_space_below_is_the_last_slot() {
        let g = geo_collapsed(120, 40, &["d", "w1", "w2", "w3"], &[true, true, true]);
        let (w1, w2, w3) = (g.tiles[1].1, g.tiles[2].1, g.tiles[3].1);
        assert_eq!(g.drop_slot(w1.y + 1), Some(1));
        assert_eq!(g.drop_slot(w2.y + 2), Some(2));
        assert_eq!(g.drop_slot(w3.y + 4), Some(3));
        assert_eq!(
            g.drop_slot(w3.y + w3.height + 6),
            Some(3),
            "under the strips"
        );
        assert_eq!(
            g.move_line(3, 1),
            Some(Rect::new(w1.x, w1.y, w1.width, 1)),
            "a horizontal line across the strip column"
        );
        assert_eq!(
            g.move_line(1, 3),
            Some(Rect::new(w3.x, w3.y + w3.height - 1, w3.width, 1))
        );
    }

    fn geo_with_master_button(width: u16, height: u16, panes: &[&str]) -> Geometry {
        geometry(
            Rect::new(0, 0, width, height),
            &brigade_stage(panes),
            &LayoutPrefs {
                sidebar_width: SIDEBAR_WIDTH,
                master_permille: DEFAULT_MASTER_PERMILLE,
                collapsed_stack_width: DEFAULT_COLLAPSED_STACK_WIDTH,
                stack_weights: &vec![1; panes.len().saturating_sub(1)],
                stack_collapsed: &[],
                master_soloable: true,
            },
            &|key| key.as_str().to_string(),
        )
    }

    #[test]
    fn the_master_gets_a_solo_button_and_no_collapse_button() {
        let g = geo_with_master_button(120, 40, &["d", "w1", "w2"]);
        let master = g.tiles[0].1;
        let mine: Vec<&TileButton> = g
            .buttons()
            .iter()
            .filter(|b| b.key.as_str() == "d")
            .collect();
        assert_eq!(mine.len(), 1);
        assert_eq!(mine[0].kind, ButtonKind::Solo);
        assert_eq!(mine[0].text, "[S]");
        let right_corner = master.x + master.width - 1;
        assert_eq!(mine[0].rect, Rect::new(right_corner - 3, master.y, 3, 1));
        // The stack tiles keep both of theirs.
        assert_eq!(
            g.buttons()
                .iter()
                .filter(|b| b.key.as_str() == "w1")
                .count(),
            2
        );
        // Found by a press, and the corner beside it is not a button.
        assert_eq!(
            g.button_at(right_corner - 2, master.y).map(|b| b.kind),
            Some(ButtonKind::Solo)
        );
        assert_eq!(g.button_at(right_corner, master.y), None);
    }

    #[test]
    fn no_master_button_without_a_stack_a_member_or_room() {
        // Not soloable (no member): none, whatever the width.
        let g = geo_collapsed(120, 40, &["d", "w1"], &[false]);
        assert!(g.buttons().iter().all(|b| b.key.as_str() != "d"));
        // A brigade of one: nothing to collapse.
        let one = geo_with_master_button(120, 40, &["d"]);
        assert!(one.buttons().is_empty());
        // A solo-session stage.
        let solo = geometry(
            Rect::new(0, 0, 120, 40),
            &Stage::Solo(key("d")),
            &LayoutPrefs {
                sidebar_width: SIDEBAR_WIDTH,
                master_permille: DEFAULT_MASTER_PERMILLE,
                collapsed_stack_width: DEFAULT_COLLAPSED_STACK_WIDTH,
                stack_weights: &[],
                stack_collapsed: &[],
                master_soloable: true,
            },
            &|k| k.as_str().to_string(),
        );
        assert!(solo.buttons().is_empty());
        // A master title too narrow for label + gap + button: none.
        let narrow = geometry(
            Rect::new(0, 0, 60, 40),
            &brigade_stage(&["d", "w1"]),
            &LayoutPrefs {
                sidebar_width: SIDEBAR_WIDTH,
                master_permille: DEFAULT_MASTER_PERMILLE,
                collapsed_stack_width: DEFAULT_COLLAPSED_STACK_WIDTH,
                stack_weights: &[1],
                stack_collapsed: &[],
                master_soloable: true,
            },
            &|_| "a-much-longer-master-label".to_string(),
        );
        assert!(narrow.buttons().iter().all(|b| b.key.as_str() != "d"));
    }

    #[test]
    fn the_masters_title_row_has_no_handle_cells_and_no_move_label() {
        let g = geo_with_master_button(120, 40, &["d", "w1", "w2"]);
        let master = g.tiles[0].1;
        let right_corner = master.x + master.width - 1;
        // Between the corners (the left corner is the sidebar boundary's, the
        // right one the master/stack boundary's).
        for x in master.x + 1..right_corner {
            assert_eq!(what_is_at(&g, x, master.y), "none", "column {x}");
        }
        assert_eq!(g.label_at(master.x + 2, master.y), None);
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
            &LayoutPrefs {
                sidebar_width: SIDEBAR_WIDTH,
                master_permille: DEFAULT_MASTER_PERMILLE,
                collapsed_stack_width: DEFAULT_COLLAPSED_STACK_WIDTH,
                stack_weights: &[1, 1, 1],
                stack_collapsed: &[],
                master_soloable: false,
            },
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
        let across = |rect: Rect, row: u16| Some(Rect::new(rect.x, row, rect.width, 1));
        assert_eq!(g.move_line(3, 1), across(w1, w1.y));
        assert_eq!(g.move_line(2, 1), across(w1, w1.y));
        assert_eq!(g.move_line(1, 3), across(w3, w3.y + w3.height - 1));
        assert_eq!(g.move_line(1, 2), across(w2, w2.y + w2.height - 1));
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
