//! jj-native lane graph engine, keyed by change ids.
//!
//! Port of GitGraph's lane algorithm (`git_graph::GraphData`) over jj's data
//! model: nodes are changes ([`JjLogEntry`]), edges are `parent_change_ids` —
//! stable across rebase, which is what lets the graph survive amend/rebase
//! the way `jj log` draws it. No git types and no git code paths: the engine
//! consumes jj CLI output only.
//!
//! The output mirrors what the GitGraph canvas consumes (per-row lane +
//! color index, per-edge segment lists in the shared `CommitLineSegment` /
//! `CurveKind` vocabulary), so `jj_log.rs` keeps GitGraph's drawing code and
//! only swaps the data source.
//!
//! ## jj conventions rendered on top of the lanes
//!
//! Node glyphs, precedence mirroring jj's renderer (conflict wins over
//! working copy, working copy over immutable):
//!
//! | flag(s)                     | glyph | color                       |
//! |-----------------------------|-------|-----------------------------|
//! | `conflict`                  | `×`   | `status().conflict`         |
//! | `working_copy`              | `@`   | `status().created`          |
//! | `immutable`                 | `◆`   | lane accent (unchanged)     |
//! | `hidden`                    | `~`   | `status().hidden` (dimmed)  |
//! | `empty`                     | `○`   | lane accent at 40% opacity  |
//! | default                     | `○`   | lane accent                 |
//!
//! (`json()`/`stringify()` strip jj's own color labels, so all styling is
//! flag-driven — see `jjlog_graph.py`'s coloring note.)
//!
//! Truncated edges (parent outside the emitted window) run to the last row
//! instead of vanishing, matching `jj log`'s rendering of a cut-off graph.

use std::{collections::HashMap, ops::Range, rc::Rc};

use smallvec::SmallVec;

use git::jj::{JjLogEntry, JjLogFlags};
use gpui::{Pixels, Window, point, px};
use theme::StatusColors;

use crate::git_graph::{
    COMMIT_CIRCLE_RADIUS, CommitLineSegment, CurveKind, LINE_WIDTH, draw_commit_circle,
};

/// One row of the lane graph, aligned by index with the panel's `entries`.
#[derive(Debug)]
pub(crate) struct JjCommitRow {
    /// The lane (column) this change's node is drawn in.
    pub(crate) lane: usize,
    /// Index into the theme's accent colors for this row's lane.
    pub(crate) color_idx: usize,
}

/// One drawn edge: a child change to one of its parents.
#[derive(Debug)]
pub(crate) struct JjCommitLine {
    #[cfg(test)]
    pub(crate) child: String,
    #[cfg(test)]
    pub(crate) parent: String,
    /// The lane the edge starts in (the child's lane).
    pub(crate) child_column: usize,
    /// Rows the edge spans, inclusive of both endpoint rows.
    pub(crate) full_interval: Range<usize>,
    pub(crate) color_idx: usize,
    pub(crate) segments: SmallVec<[CommitLineSegment; 1]>,
}

impl JjCommitLine {
    /// Mirrors GitGraph's `CommitLine::get_first_visible_segment_idx`.
    pub(crate) fn get_first_visible_segment_idx(
        &self,
        first_visible_row: usize,
    ) -> Option<(usize, usize)> {
        if first_visible_row > self.full_interval.end {
            return None;
        } else if first_visible_row <= self.full_interval.start {
            return Some((0, self.child_column));
        }

        let mut current_column = self.child_column;

        for (idx, segment) in self.segments.iter().enumerate() {
            match segment {
                CommitLineSegment::Straight { to_row } => {
                    if *to_row >= first_visible_row {
                        return Some((idx, current_column));
                    }
                }
                CommitLineSegment::Curve {
                    to_column, on_row, ..
                } => {
                    if *on_row >= first_visible_row {
                        return Some((idx, current_column));
                    }
                    current_column = *to_column;
                }
            }
        }

        None
    }
}

#[derive(Debug)]
enum JjLaneState {
    Empty,
    Active {
        child: String,
        parent: String,
        color: Option<u8>,
        starting_row: usize,
        starting_col: usize,
        destination_column: Option<usize>,
        segments: SmallVec<[CommitLineSegment; 1]>,
    },
}

impl JjLaneState {
    fn to_commit_lines(
        &mut self,
        ending_row: usize,
        lane_column: usize,
        parent_column: usize,
        parent_color: u8,
    ) -> Option<JjCommitLine> {
        let state = std::mem::replace(self, JjLaneState::Empty);

        match state {
            JjLaneState::Active {
                #[cfg_attr(not(test), allow(unused_variables))]
                child,
                #[cfg_attr(not(test), allow(unused_variables))]
                parent,
                color,
                starting_row,
                starting_col,
                destination_column,
                mut segments,
            } => {
                let final_destination = destination_column.unwrap_or(parent_column);
                let final_color = color.unwrap_or(parent_color);

                Some(JjCommitLine {
                    #[cfg(test)]
                    child,
                    #[cfg(test)]
                    parent,
                    child_column: starting_col,
                    full_interval: starting_row..ending_row,
                    color_idx: final_color as usize,
                    segments: {
                        match segments.last_mut() {
                            Some(CommitLineSegment::Straight { to_row })
                                if *to_row == usize::MAX =>
                            {
                                if final_destination != lane_column {
                                    *to_row = ending_row - 1;

                                    let curved_line = CommitLineSegment::Curve {
                                        to_column: final_destination,
                                        on_row: ending_row,
                                        curve_kind: CurveKind::Checkout,
                                    };

                                    if *to_row == starting_row {
                                        let last_index = segments.len() - 1;
                                        segments[last_index] = curved_line;
                                    } else {
                                        segments.push(curved_line);
                                    }
                                } else {
                                    *to_row = ending_row;
                                }
                            }
                            Some(CommitLineSegment::Curve {
                                on_row,
                                to_column,
                                curve_kind,
                            }) if *on_row == usize::MAX => {
                                if *to_column == usize::MAX {
                                    *to_column = final_destination;
                                }
                                if matches!(curve_kind, CurveKind::Merge) {
                                    *on_row = starting_row + 1;
                                    if *on_row < ending_row {
                                        if *to_column != final_destination {
                                            segments.push(CommitLineSegment::Straight {
                                                to_row: ending_row - 1,
                                            });
                                            segments.push(CommitLineSegment::Curve {
                                                to_column: final_destination,
                                                on_row: ending_row,
                                                curve_kind: CurveKind::Checkout,
                                            });
                                        } else {
                                            segments.push(CommitLineSegment::Straight {
                                                to_row: ending_row,
                                            });
                                        }
                                    } else if *to_column != final_destination {
                                        segments.push(CommitLineSegment::Curve {
                                            to_column: final_destination,
                                            on_row: ending_row,
                                            curve_kind: CurveKind::Checkout,
                                        });
                                    }
                                } else {
                                    *on_row = ending_row;
                                    if *to_column != final_destination {
                                        segments.push(CommitLineSegment::Straight {
                                            to_row: ending_row,
                                        });
                                        segments.push(CommitLineSegment::Curve {
                                            to_column: final_destination,
                                            on_row: ending_row,
                                            curve_kind: CurveKind::Checkout,
                                        });
                                    }
                                }
                            }
                            Some(CommitLineSegment::Curve {
                                on_row, to_column, ..
                            }) => {
                                if *on_row < ending_row {
                                    if *to_column != final_destination {
                                        segments.push(CommitLineSegment::Straight {
                                            to_row: ending_row - 1,
                                        });
                                        segments.push(CommitLineSegment::Curve {
                                            to_column: final_destination,
                                            on_row: ending_row,
                                            curve_kind: CurveKind::Checkout,
                                        });
                                    } else {
                                        segments.push(CommitLineSegment::Straight {
                                            to_row: ending_row,
                                        });
                                    }
                                } else if *to_column != final_destination {
                                    segments.push(CommitLineSegment::Curve {
                                        to_column: final_destination,
                                        on_row: ending_row,
                                        curve_kind: CurveKind::Checkout,
                                    });
                                }
                            }
                            _ => {}
                        }

                        segments
                    },
                })
            }
            JjLaneState::Empty => None,
        }
    }

    fn is_empty(&self) -> bool {
        match self {
            JjLaneState::Empty => true,
            JjLaneState::Active { .. } => false,
        }
    }
}

/// The lane graph for a jj log window: per-row node positions and per-edge
/// segment lists, keyed by change ids throughout.
#[derive(Debug, Default)]
pub(crate) struct JjGraphData {
    lane_states: SmallVec<[JjLaneState; 8]>,
    lane_colors: HashMap<usize, u8>,
    parent_to_lanes: HashMap<String, SmallVec<[usize; 1]>>,
    next_color: u8,
    accent_colors_count: usize,
    pub(crate) commits: Vec<Rc<JjCommitRow>>,
    pub(crate) lines: Vec<Rc<JjCommitLine>>,
    /// The widest the lane area ever got while laying out the window.
    pub(crate) max_lanes: usize,
}

impl JjGraphData {
    pub(crate) fn from_entries(entries: &[JjLogEntry], accent_colors_count: usize) -> Self {
        let mut graph = JjGraphData {
            lane_states: SmallVec::default(),
            lane_colors: HashMap::default(),
            parent_to_lanes: HashMap::default(),
            next_color: 0,
            accent_colors_count,
            commits: Vec::with_capacity(entries.len()),
            lines: Vec::with_capacity(entries.len() / 2),
            max_lanes: 0,
        };
        graph.add_entries(entries);
        graph.finish();
        graph
    }

    fn first_empty_lane_idx(&mut self) -> usize {
        self.lane_states
            .iter()
            .position(JjLaneState::is_empty)
            .unwrap_or_else(|| {
                self.lane_states.push(JjLaneState::Empty);
                self.lane_states.len() - 1
            })
    }

    fn get_lane_color(&mut self, lane_idx: usize) -> u8 {
        let accent_colors_count = self.accent_colors_count;
        *self.lane_colors.entry(lane_idx).or_insert_with(|| {
            let color_idx = self.next_color;
            self.next_color = (self.next_color + 1) % accent_colors_count.max(1) as u8;
            color_idx
        })
    }

    fn add_entries(&mut self, entries: &[JjLogEntry]) {
        for entry in entries {
            let commit_row = self.commits.len();
            let commit_id = entry.change_id.to_string();

            let commit_lane = self
                .parent_to_lanes
                .get(&commit_id)
                .and_then(|lanes| lanes.iter().min().copied());

            let commit_lane = commit_lane.unwrap_or_else(|| self.first_empty_lane_idx());

            let commit_color = self.get_lane_color(commit_lane);

            if let Some(lanes) = self.parent_to_lanes.remove(&commit_id) {
                for lane_column in lanes {
                    let state = &mut self.lane_states[lane_column];

                    if let JjLaneState::Active {
                        starting_row,
                        segments,
                        ..
                    } = state
                    {
                        if let Some(CommitLineSegment::Curve {
                            to_column,
                            curve_kind: CurveKind::Merge,
                            ..
                        }) = segments.first_mut()
                        {
                            let curve_row = *starting_row + 1;
                            let would_overlap =
                                if lane_column != commit_lane && curve_row < commit_row {
                                    self.commits[curve_row..commit_row]
                                        .iter()
                                        .any(|c| c.lane == commit_lane)
                                } else {
                                    false
                                };

                            if would_overlap {
                                *to_column = lane_column;
                            }
                        }
                    }

                    if let Some(commit_line) =
                        state.to_commit_lines(commit_row, lane_column, commit_lane, commit_color)
                    {
                        self.lines.push(Rc::new(commit_line));
                    }
                }
            }

            entry
                .parent_change_ids
                .iter()
                .enumerate()
                .for_each(|(parent_idx, parent)| {
                    let parent = parent.to_string();
                    if parent_idx == 0 {
                        self.lane_states[commit_lane] = JjLaneState::Active {
                            child: commit_id.clone(),
                            parent: parent.clone(),
                            color: Some(commit_color),
                            starting_col: commit_lane,
                            starting_row: commit_row,
                            destination_column: None,
                            segments: smallvec::smallvec![CommitLineSegment::Straight {
                                to_row: usize::MAX
                            }],
                        };

                        self.parent_to_lanes
                            .entry(parent)
                            .or_default()
                            .push(commit_lane);
                    } else {
                        let new_lane = self.first_empty_lane_idx();

                        self.lane_states[new_lane] = JjLaneState::Active {
                            child: commit_id.clone(),
                            parent: parent.clone(),
                            color: None,
                            starting_col: commit_lane,
                            starting_row: commit_row,
                            destination_column: None,
                            segments: smallvec::smallvec![CommitLineSegment::Curve {
                                to_column: usize::MAX,
                                on_row: usize::MAX,
                                curve_kind: CurveKind::Merge,
                            }],
                        };

                        self.parent_to_lanes
                            .entry(parent)
                            .or_default()
                            .push(new_lane);
                    }
                });

            self.max_lanes = self.max_lanes.max(self.lane_states.len());

            self.commits.push(Rc::new(JjCommitRow {
                lane: commit_lane,
                color_idx: commit_color as usize,
            }));
        }
    }

    /// Terminates lanes whose parent never appeared in the emitted window:
    /// `jj log` draws such cut-off edges running to the bottom of the graph
    /// instead of dropping them.
    fn finish(&mut self) {
        let Some(last_row) = self.commits.len().checked_sub(1) else {
            return;
        };
        let lane_colors = self.lane_colors.clone();
        for (lane_column, state) in self.lane_states.iter_mut().enumerate() {
            let starting_row = match state {
                JjLaneState::Active { starting_row, .. } => *starting_row,
                JjLaneState::Empty => continue,
            };
            if starting_row >= last_row {
                continue;
            }
            let color = lane_colors.get(&lane_column).copied().unwrap_or(0);
            if let Some(line) = state.to_commit_lines(last_row, lane_column, lane_column, color) {
                self.lines.push(Rc::new(line));
            }
        }
    }
}

/// jj log's node glyphs, in the precedence jj's renderer uses: conflict wins
/// over working copy, working copy over immutable.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum JjNodeGlyph {
    Normal,
    WorkingCopy,
    Immutable,
    Conflict,
    Hidden,
}

/// Maps a commit's flags to its glyph (see the module doc for the table).
pub(crate) fn node_glyph(flags: &JjLogFlags) -> JjNodeGlyph {
    if flags.conflict {
        JjNodeGlyph::Conflict
    } else if flags.working_copy {
        JjNodeGlyph::WorkingCopy
    } else if flags.immutable {
        JjNodeGlyph::Immutable
    } else if flags.hidden {
        JjNodeGlyph::Hidden
    } else {
        JjNodeGlyph::Normal
    }
}

/// The status colors the node mapping uses, extracted from the theme so the
/// mapping is testable without a full theme.
#[derive(Clone, Copy)]
pub(crate) struct NodeStatusColors {
    pub(crate) conflict: gpui::Hsla,
    pub(crate) created: gpui::Hsla,
    pub(crate) hidden: gpui::Hsla,
}

impl NodeStatusColors {
    pub(crate) fn from_theme(status: &StatusColors) -> Self {
        NodeStatusColors {
            conflict: status.conflict,
            created: status.created,
            hidden: status.hidden,
        }
    }
}

/// Maps a glyph to its color: flag-driven overrides on top of the lane's
/// accent color (jj colors these states semantically; Zed's status palette
/// carries the same semantics).
pub(crate) fn node_color(
    glyph: JjNodeGlyph,
    lane_color: gpui::Hsla,
    status: &NodeStatusColors,
) -> gpui::Hsla {
    match glyph {
        JjNodeGlyph::Conflict => status.conflict,
        JjNodeGlyph::WorkingCopy => status.created,
        JjNodeGlyph::Hidden => status.hidden,
        JjNodeGlyph::Normal | JjNodeGlyph::Immutable => lane_color,
    }
}

/// Paints one commit node in jj's conventions: `○` normal (solid dot, the
/// approved v2 base), `◆` immutable (filled diamond), `@` working copy
/// (larger solid dot), `×` conflict (crossed strokes), `~` hidden (small
/// dimmed dot). `empty` fades whatever glyph applies to 40% opacity.
pub(crate) fn draw_jj_node(
    glyph: JjNodeGlyph,
    flags: &JjLogFlags,
    center_x: Pixels,
    center_y: Pixels,
    color: gpui::Hsla,
    window: &mut Window,
) {
    let color = if flags.empty { color.alpha(0.4) } else { color };
    let radius = COMMIT_CIRCLE_RADIUS;

    match glyph {
        // Solid dot — the approved v2 base drawing.
        JjNodeGlyph::Normal => draw_commit_circle(center_x, center_y, color, window),
        // `@`: the anchor node — same dot, slightly larger.
        JjNodeGlyph::WorkingCopy => {
            let r = radius + px(1.5);
            let diameter = r * 2.0;
            let bounds = gpui::Bounds::new(
                point(center_x - r, center_y - r),
                gpui::Size {
                    width: diameter,
                    height: diameter,
                },
            );
            window.paint_quad(gpui::fill(bounds, color).corner_radii(r));
        }
        // `◆`: filled diamond.
        JjNodeGlyph::Immutable => {
            let r = radius * 1.2;
            let mut builder = gpui::PathBuilder::fill();
            builder.move_to(point(center_x, center_y - r));
            builder.line_to(point(center_x + r, center_y));
            builder.line_to(point(center_x, center_y + r));
            builder.line_to(point(center_x - r, center_y));
            builder.close();
            if let Ok(path) = builder.build() {
                window.paint_path(path, color);
            }
        }
        // `×`: two crossed strokes.
        JjNodeGlyph::Conflict => {
            let r = radius * 1.1;
            let mut builder = gpui::PathBuilder::stroke(LINE_WIDTH);
            builder.move_to(point(center_x - r, center_y - r));
            builder.line_to(point(center_x + r, center_y + r));
            builder.move_to(point(center_x - r, center_y + r));
            builder.line_to(point(center_x + r, center_y - r));
            builder.close();
            if let Ok(path) = builder.build() {
                window.paint_path(path, color);
            }
        }
        // `~`: small dimmed dot.
        JjNodeGlyph::Hidden => {
            let r = radius * 0.6;
            let diameter = r * 2.0;
            let bounds = gpui::Bounds::new(
                point(center_x - r, center_y - r),
                gpui::Size {
                    width: diameter,
                    height: diameter,
                },
            );
            window.paint_quad(gpui::fill(bounds, color).corner_radii(r));
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use git::jj::JjLogFlags;

    fn flags() -> JjLogFlags {
        JjLogFlags {
            working_copy: false,
            root: false,
            divergent: false,
            hidden: false,
            conflict: false,
            empty: false,
            immutable: false,
            mine: false,
            ancestor_of_wc: false,
            descendant_of_wc: false,
        }
    }

    fn entry(change_id: &str, parents: &[&str]) -> JjLogEntry {
        JjLogEntry {
            change_id: change_id.into(),
            commit_id: format!("commit-{change_id}").into(),
            parents: parents
                .iter()
                .map(|p| format!("commit-{p}").into())
                .collect(),
            parent_change_ids: parents.iter().map(|p| (*p).to_string().into()).collect(),
            bookmarks: Vec::new(),
            tags: Vec::new(),
            description: String::new().into(),
            author_name: String::new().into(),
            author_email: String::new().into(),
            commit_timestamp: 0,
            flags: flags(),
            is_merge: parents.len() > 1,
            is_head: false,
            dist_to_head: None,
        }
    }

    fn lanes(graph: &JjGraphData) -> Vec<usize> {
        graph.commits.iter().map(|row| row.lane).collect()
    }

    #[test]
    fn linear_chain_stays_in_one_lane() {
        let entries = vec![
            entry("c", &["b"]), // children-first: newest change first
            entry("b", &["a"]),
            entry("a", &[]),
        ];
        let graph = JjGraphData::from_entries(&entries, 8);
        assert_eq!(lanes(&graph), vec![0, 0, 0]);
        // Two edges: c→b, b→a. The root (a) has no outgoing edge.
        assert_eq!(graph.lines.len(), 2);
        assert_eq!(graph.max_lanes, 1);
    }

    #[test]
    fn fork_gets_two_lanes() {
        let entries = vec![entry("b1", &["a"]), entry("b2", &["a"]), entry("a", &[])];
        let graph = JjGraphData::from_entries(&entries, 8);
        // b1 takes lane 0; b2 lands in a fresh lane; both edges curve into a.
        assert_eq!(lanes(&graph), vec![0, 1, 0]);
        assert_eq!(graph.lines.len(), 2);
        assert_eq!(graph.max_lanes, 2);
        let colors: Vec<usize> = graph.commits.iter().map(|row| row.color_idx).collect();
        assert_ne!(colors[0], colors[1]);
    }

    #[test]
    fn merge_child_bridges_two_parents() {
        let entries = vec![
            entry("m", &["b1", "b2"]),
            entry("b2", &["a"]),
            entry("b1", &["a"]),
            entry("a", &[]),
        ];
        let graph = JjGraphData::from_entries(&entries, 8);
        assert_eq!(lanes(&graph), vec![0, 1, 0, 0]);
        // m→b1 (straight in lane 0), m→b2 (merge curve), b2→a, b1→a.
        assert_eq!(graph.lines.len(), 4);
        let merge_line = graph
            .lines
            .iter()
            .find(|line| {
                line.segments.iter().any(|s| {
                    matches!(
                        s,
                        CommitLineSegment::Curve {
                            curve_kind: CurveKind::Merge,
                            ..
                        }
                    )
                })
            })
            .expect("a merge curve exists");
        assert_eq!(merge_line.child_column, 0);
    }

    #[test]
    fn truncated_parent_runs_to_last_row() {
        // The oldest change's parent is outside the window: the edge must
        // still be drawn down to the last row (jj's cut-off convention),
        // not dropped. The last row's own edge would have zero length, so
        // it is skipped.
        let entries = vec![entry("b", &["outside"]), entry("a", &["outside"])];
        let graph = JjGraphData::from_entries(&entries, 8);
        assert_eq!(graph.lines.len(), 1);
        assert_eq!(
            graph.lines[0].full_interval.end, 1,
            "edge terminates at the last row"
        );
    }

    #[test]
    fn divergent_change_ids_get_separate_rows() {
        // Two commits sharing one change id (divergent): the first claims the
        // reserved lane, the second falls into a fresh one.
        let entries = vec![entry("d", &["a"]), entry("d", &["a"]), entry("a", &[])];
        let graph = JjGraphData::from_entries(&entries, 8);
        assert_eq!(lanes(&graph), vec![0, 1, 0]);
        assert_eq!(graph.lines.len(), 2);
    }

    #[test]
    fn empty_window_produces_nothing() {
        let graph = JjGraphData::from_entries(&[], 8);
        assert!(graph.commits.is_empty());
        assert!(graph.lines.is_empty());
        assert_eq!(graph.max_lanes, 0);
    }

    #[test]
    fn glyph_precedence_mirrors_jj() {
        let mut f = flags();
        assert_eq!(node_glyph(&f), JjNodeGlyph::Normal);

        f.immutable = true;
        assert_eq!(node_glyph(&f), JjNodeGlyph::Immutable);

        f.working_copy = true;
        assert_eq!(
            node_glyph(&f),
            JjNodeGlyph::WorkingCopy,
            "wc wins over immutable"
        );

        f.conflict = true;
        assert_eq!(
            node_glyph(&f),
            JjNodeGlyph::Conflict,
            "conflict wins over wc"
        );

        let mut h = flags();
        h.hidden = true;
        assert_eq!(node_glyph(&h), JjNodeGlyph::Hidden);
    }

    #[test]
    fn flag_colors_come_from_status_palette() {
        let status = NodeStatusColors {
            conflict: gpui::Hsla::default(),
            created: gpui::hsla(0.3, 0.5, 0.5, 1.0),
            hidden: gpui::hsla(0.6, 0.1, 0.4, 1.0),
        };
        let lane = gpui::hsla(0.1, 0.5, 0.5, 1.0);
        assert_eq!(
            node_color(JjNodeGlyph::Conflict, lane, &status),
            status.conflict
        );
        assert_eq!(
            node_color(JjNodeGlyph::WorkingCopy, lane, &status),
            status.created
        );
        assert_eq!(
            node_color(JjNodeGlyph::Hidden, lane, &status),
            status.hidden
        );
        assert_eq!(node_color(JjNodeGlyph::Normal, lane, &status), lane);
        assert_eq!(node_color(JjNodeGlyph::Immutable, lane, &status), lane);
    }
}
