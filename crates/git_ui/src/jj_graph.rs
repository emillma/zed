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
//! Mutable commits are hollow rings, immutable ones filled diamonds —
//! jj's filled-vs-hollow distinction.
//!
//! `@` and `~` are monochrome SVG marks tinted at paint time (Lucide's
//! at-sign; a hand-drawn wave); `◆`/`×`/`○` are vector shapes.
//!
//! (`json()`/`stringify()` strip jj's own color labels, so all styling is
//! flag-driven — see `jjlog_graph.py`'s coloring note.)
//!
//! A parent outside the emitted window has its edge land on a synthesized
//! elided row (`flags.elided`, jj's `(elided revisions)` row the data layer
//! inserts after the child) instead of dangling; `finish()` is only a
//! fallback for a window that is missing that row.

use std::{
    collections::{HashMap, HashSet},
    ops::Range,
    rc::Rc,
};

use smallvec::SmallVec;

use git::jj::{JjLogEntry, JjLogFlags};
use gpui::{App, Pixels, Point, SharedString, TransformationMatrix, Window, point, px};
use lyon::tessellation::{LineCap, LineJoin};
use theme::StatusColors;

use crate::git_graph::{CommitLineSegment, CurveKind};

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

/// Geometry of one child→parent edge for the 3-part edge renderer:
/// top stub (child's row, `child_col`→`column`), vertical (in `column`),
/// bottom stub (parent's row, `column`→`parent_col`).
///
/// Consumed by the Phase B renderer (and the tests); nothing reads the
/// fields in the non-test build yet, hence the allow.
#[allow(dead_code)]
#[derive(Debug)]
pub(crate) struct EdgeLayout {
    #[cfg(test)]
    pub(crate) child: String,
    #[cfg(test)]
    pub(crate) parent: String,
    /// Row of the child commit's node — the edge starts here.
    pub(crate) child_row: usize,
    /// Row of the parent commit's node — the edge ends here.
    pub(crate) parent_row: usize,
    /// The column the vertical segment runs in.
    pub(crate) column: usize,
    /// The child node's column.
    pub(crate) child_col: usize,
    /// The parent node's column.
    pub(crate) parent_col: usize,
    /// The lane's color index: an edge inherits its lane's color, same
    /// semantics as its sibling `JjCommitLine`.
    pub(crate) color_idx: usize,
}

/// An extra-parent edge that shares a sibling's already-pending lane instead
/// of allocating its own (jj overlaps sibling edges in one column). Resolved
/// into an [`EdgeLayout`] when the shared parent is reached.
#[derive(Debug)]
struct PiggybackEdge {
    #[cfg(test)]
    child: String,
    #[cfg(test)]
    parent: String,
    child_row: usize,
    child_col: usize,
    column: usize,
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
    /// Per-edge geometry for the 3-part edge renderer (see [`EdgeLayout`]).
    /// Edges to parents outside the emitted window land on the elided row
    /// after their child (`parent_col` = the edge's own column, so the
    /// bottom stub is zero-length); a window missing that row leaves a line
    /// in `lines` without an `EdgeLayout` (the `finish()` fallback).
    pub(crate) edges: Vec<EdgeLayout>,
    /// Lanes of edges whose parent is outside the emitted window, waiting
    /// for the elided row the data layer inserts after their child; an
    /// elided row resolves them all at its own row index.
    elided_landings: SmallVec<[usize; 2]>,
    /// Extra-parent edges sharing a sibling's already-pending lane (jj
    /// overlaps sibling edges in one column instead of detouring out to a
    /// fresh lane). Resolved when the shared parent is reached.
    piggyback_edges: HashMap<String, SmallVec<[PiggybackEdge; 1]>>,
    /// Extra-parent edges whose parent is outside the window, collapsed
    /// into the primary's elided landing column (jj draws one line through
    /// an elided node, not one lane per missing parent). Resolved when the
    /// elided row is laid out.
    elided_piggybacks: Vec<PiggybackEdge>,
    /// The widest the lane area ever got while laying out the window.
    pub(crate) max_lanes: usize,
}

impl JjGraphData {
    pub(crate) fn from_entries(entries: &[JjLogEntry], accent_colors_count: usize) -> Self {
        let mut graph = JjGraphData {
            lane_states: SmallVec::default(),
            lane_colors: HashMap::default(),
            parent_to_lanes: HashMap::default(),
            elided_landings: SmallVec::default(),
            piggyback_edges: HashMap::default(),
            elided_piggybacks: Vec::default(),
            next_color: 0,
            accent_colors_count,
            commits: Vec::with_capacity(entries.len()),
            lines: Vec::with_capacity(entries.len() / 2),
            edges: Vec::with_capacity(entries.len()),
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
        // Change ids present in the emitted window: parents outside it get
        // no merge lane (allocating lanes for unseen parents only sprawls
        // the graph right, e.g. for `heads(all())`); the primary edge to
        // such a parent lands on the elided row the data layer inserts
        // after its child.
        let emitted: HashSet<&str> = entries.iter().map(|e| e.change_id.as_ref()).collect();
        // Topo row of each in-window change id: extra-parent lanes are
        // allocated topmost-first (see the parent handling below).
        let row_of: HashMap<&str, usize> = entries
            .iter()
            .enumerate()
            .map(|(idx, entry)| (entry.change_id.as_ref(), idx))
            .collect();
        for entry in entries {
            let commit_row = self.commits.len();
            let commit_id = entry.change_id.to_string();

            // An elided row lands the edges pending from above: lanes whose
            // parent is outside the window terminate here. jj puts the `~`
            // at the primary edge's column (the first landing) and the other
            // branches' edges converge into it, so a multi-branch elision
            // reads as one node instead of a floating glyph on the right.
            let mut landing_lane: Option<usize> = None;
            if entry.flags.elided {
                let landings = std::mem::take(&mut self.elided_landings);
                let glyph_lane = landings.first().copied();
                landing_lane = glyph_lane;
                for (landing_idx, lane) in landings.iter().copied().enumerate() {
                    let parent_col = if landing_idx == 0 {
                        lane
                    } else {
                        glyph_lane.unwrap_or(lane)
                    };
                    let color = self.lane_colors.get(&lane).copied().unwrap_or(0);
                    let state = &mut self.lane_states[lane];
                    let edge_source = match state {
                        JjLaneState::Active {
                            child,
                            parent,
                            starting_row,
                            starting_col,
                            ..
                        } => Some((child.clone(), parent.clone(), *starting_row, *starting_col)),
                        JjLaneState::Empty => None,
                    };
                    if let Some(commit_line) =
                        state.to_commit_lines(commit_row, lane, parent_col, color)
                    {
                        let color_idx = commit_line.color_idx;
                        self.lines.push(Rc::new(commit_line));
                        #[cfg_attr(not(test), allow(unused_variables))]
                        if let Some((child, parent, child_row, child_col)) = edge_source {
                            self.edges.push(EdgeLayout {
                                #[cfg(test)]
                                child,
                                #[cfg(test)]
                                parent,
                                child_row,
                                parent_row: commit_row,
                                column: lane,
                                child_col,
                                parent_col,
                                color_idx,
                            });
                        }
                    }
                }

                // The collapsed missing-parent extras converge into the ~
                // at the primary's column, in the shared lane's color.
                for pig in std::mem::take(&mut self.elided_piggybacks) {
                    let color_idx = self.get_lane_color(pig.column) as usize;
                    self.lines.push(Rc::new(JjCommitLine {
                        #[cfg(test)]
                        child: pig.child.clone(),
                        #[cfg(test)]
                        parent: pig.parent.clone(),
                        child_column: pig.child_col,
                        full_interval: pig.child_row..commit_row,
                        color_idx,
                        segments: smallvec::smallvec![CommitLineSegment::Straight {
                            to_row: usize::MAX
                        }],
                    }));
                    self.edges.push(EdgeLayout {
                        #[cfg(test)]
                        child: pig.child.clone(),
                        #[cfg(test)]
                        parent: pig.parent.clone(),
                        child_row: pig.child_row,
                        parent_row: commit_row,
                        column: pig.column,
                        child_col: pig.child_col,
                        parent_col: glyph_lane.unwrap_or(pig.column),
                        color_idx,
                    });
                }
            }

            let commit_lane = self
                .parent_to_lanes
                .get(&commit_id)
                .and_then(|lanes| lanes.iter().min().copied());

            let commit_lane = commit_lane
                // An elided row with no incoming edge sits in the column
                // where its primary edge lands (the `~` is that edge's
                // terminus; the other branches converge into it).
                .or(landing_lane.filter(|_| entry.flags.elided))
                .unwrap_or_else(|| self.first_empty_lane_idx());

            let commit_color = self.get_lane_color(commit_lane);

            if let Some(lanes) = self.parent_to_lanes.remove(&commit_id) {
                for lane_column in lanes {
                    let state = &mut self.lane_states[lane_column];

                    // Edge geometry, captured while the state is still Active:
                    // `to_commit_lines` consumes it. `child`/`starting_col` are
                    // where the edge starts, `lane_column` where its vertical
                    // runs, `commit_lane` where the parent node sits.
                    let edge_source = match state {
                        JjLaneState::Active {
                            child,
                            parent,
                            starting_row,
                            starting_col,
                            ..
                        } => Some((child.clone(), parent.clone(), *starting_row, *starting_col)),
                        JjLaneState::Empty => None,
                    };

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
                        let color_idx = commit_line.color_idx;
                        self.lines.push(Rc::new(commit_line));

                        #[cfg_attr(not(test), allow(unused_variables))]
                        if let Some((child, parent, child_row, child_col)) = edge_source {
                            self.edges.push(EdgeLayout {
                                #[cfg(test)]
                                child,
                                #[cfg(test)]
                                parent,
                                child_row,
                                parent_row: commit_row,
                                column: lane_column,
                                child_col,
                                parent_col: commit_lane,
                                color_idx,
                            });
                        }
                    }
                }

                // Extra-parent edges that piggybacked on a sibling's pending
                // lane resolve here. They take the shared lane's color so the
                // overlap renders as one continuous line.
                for pig in self.piggyback_edges.remove(&commit_id).unwrap_or_default() {
                    let color_idx = self.get_lane_color(pig.column) as usize;
                    self.lines.push(Rc::new(JjCommitLine {
                        #[cfg(test)]
                        child: pig.child.clone(),
                        #[cfg(test)]
                        parent: pig.parent.clone(),
                        child_column: pig.child_col,
                        full_interval: pig.child_row..commit_row,
                        color_idx,
                        segments: smallvec::smallvec![CommitLineSegment::Straight {
                            to_row: usize::MAX
                        }],
                    }));
                    self.edges.push(EdgeLayout {
                        #[cfg(test)]
                        child: pig.child.clone(),
                        #[cfg(test)]
                        parent: pig.parent.clone(),
                        child_row: pig.child_row,
                        parent_row: commit_row,
                        column: pig.column,
                        child_col: pig.child_col,
                        parent_col: commit_lane,
                        color_idx,
                    });
                }
            }

            // Elided rows have no outgoing edges: they exist to be landed
            // on, never to point at a parent themselves.
            if !entry.flags.elided {
                // Primary parent first: the edge continues in the commit's
                // own lane. Extra parents then allocate new lanes, topmost
                // (smallest topo row) first, so a merge's fan starts from
                // the top rather than the bottom; parents outside the window
                // land on the elided row after this one, after the emitted
                // ones.
                let primary = entry.parent_change_ids.first();
                let mut extra: Vec<&SharedString> =
                    entry.parent_change_ids.iter().skip(1).collect();
                extra.sort_by_key(|parent| {
                    row_of.get(parent.as_str()).copied().unwrap_or(usize::MAX)
                });

                let mut handle_parent = |parent: &SharedString, primary_lane: bool| {
                    let parent = parent.to_string();
                    if primary_lane {
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

                        if emitted.contains(parent.as_str()) {
                            self.parent_to_lanes
                                .entry(parent)
                                .or_default()
                                .push(commit_lane);
                        } else {
                            // The parent never appears in the window:
                            // the data layer inserts an elided row
                            // after this one, and the edge lands there
                            // (or in `finish()` if it does not).
                            self.elided_landings.push(commit_lane);
                        }
                    } else if !emitted.contains(parent.as_str()) && !self.elided_landings.is_empty()
                    {
                        // jj collapses multiple missing-parent edges into the
                        // single elided landing: run this edge in the
                        // primary's landing column instead of sprawling a
                        // fresh lane per missing parent.
                        let column = *self.elided_landings.last().unwrap();
                        self.elided_piggybacks.push(PiggybackEdge {
                            #[cfg(test)]
                            child: commit_id.clone(),
                            #[cfg(test)]
                            parent: parent.clone(),
                            child_row: commit_row,
                            child_col: commit_lane,
                            column,
                        });
                    } else if emitted.contains(parent.as_str())
                        && self
                            .parent_to_lanes
                            .get(parent.as_str())
                            .is_some_and(|lanes| !lanes.is_empty())
                    {
                        // jj-style lane sharing: another child's edge to this
                        // parent is already pending; run this edge in the
                        // sibling's column — the two overlap between the
                        // siblings' rows and converge at the parent — instead
                        // of detouring out to a fresh lane beyond both
                        // endpoints.
                        let shared = self.parent_to_lanes[parent.as_str()]
                            .iter()
                            .copied()
                            .min_by_key(|lane| lane.abs_diff(commit_lane))
                            .unwrap();
                        self.piggyback_edges
                            .entry(parent.clone())
                            .or_default()
                            .push(PiggybackEdge {
                                #[cfg(test)]
                                child: commit_id.clone(),
                                #[cfg(test)]
                                parent: parent.clone(),
                                child_row: commit_row,
                                child_col: commit_lane,
                                column: shared,
                            });
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

                        if emitted.contains(parent.as_str()) {
                            self.parent_to_lanes
                                .entry(parent)
                                .or_default()
                                .push(new_lane);
                        } else {
                            // Outside the window: land on the elided row
                            // too, instead of dropping the edge entirely.
                            self.elided_landings.push(new_lane);
                        }
                    }
                };

                if let Some(primary) = primary {
                    handle_parent(primary, true);
                }
                for parent in extra {
                    handle_parent(parent, false);
                }
            }

            self.max_lanes = self.max_lanes.max(self.lane_states.len());

            self.commits.push(Rc::new(JjCommitRow {
                lane: commit_lane,
                color_idx: commit_color as usize,
            }));
        }
    }

    /// Fallback for a window that is missing an elided row: a dangling edge
    /// whose landing row never came terminates at the last row instead of
    /// vanishing. Unreachable for well-formed input — every dangling
    /// primary edge is registered with the row below its child, which the
    /// data layer guarantees to be the elided row (or an in-window parent
    /// that resolves the lane before `finish` runs), and an elided row —
    /// the only row that may follow a dangling one — resolves its own.
    fn finish(&mut self) {
        let Some(last_row) = self.commits.len().checked_sub(1) else {
            return;
        };
        for lane in self.elided_landings.drain(..) {
            let color = self.lane_colors.get(&lane).copied().unwrap_or(0);
            let state = &mut self.lane_states[lane];
            let (starting_row, edge_source) = match state {
                JjLaneState::Active {
                    child,
                    parent,
                    starting_row,
                    starting_col,
                    ..
                } => (
                    *starting_row,
                    Some((child.clone(), parent.clone(), *starting_row, *starting_col)),
                ),
                JjLaneState::Empty => continue,
            };
            // The child already sits on the last row: a zero-length edge.
            if starting_row >= last_row {
                continue;
            }
            if let Some(commit_line) = state.to_commit_lines(last_row, lane, lane, color) {
                let color_idx = commit_line.color_idx;
                self.lines.push(Rc::new(commit_line));
                #[cfg_attr(not(test), allow(unused_variables))]
                if let Some((child, parent, child_row, child_col)) = edge_source {
                    self.edges.push(EdgeLayout {
                        #[cfg(test)]
                        child,
                        #[cfg(test)]
                        parent,
                        child_row,
                        parent_row: last_row,
                        column: lane,
                        child_col,
                        parent_col: lane,
                        color_idx,
                    });
                }
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

const AT_SIGN_SVG: &str = include_str!("../assets/jj_at_sign.svg");
const WAVE_SVG: &str = include_str!("../assets/jj_wave.svg");
const CONFLICT_X_SVG: &str = include_str!("../assets/jj_conflict_x.svg");

/// Node geometry for the jj panel's graph — decoupled from GitGraph's
/// constants so the jj nodes can scale independently.
pub(crate) const JJ_NODE_RADIUS: Pixels = px(3.5);
pub(crate) const JJ_NODE_STROKE_WIDTH: Pixels = px(2.0);
/// Vertical clearance between a node and the lane lines' endpoints: text
/// glyphs (`@`, `~`) need more room than the circles so the lines don't
/// cross them.
pub(crate) const JJ_GLYPH_CLEARANCE: Pixels = px(6.5);

/// Rendered box sizes for the SVG node marks — tuned so the ink clears
/// the lane lines (the commit ring is 4.5px radius).
const AT_SIGN_SIZE: Pixels = px(13.0);
const WAVE_SIZE: Pixels = px(10.0);
const CONFLICT_X_SIZE: Pixels = px(14.0);

/// Node marks as monochrome SVGs, tinted with the node color at paint
/// time. The `@` is Lucide's at-sign (ISC); the wave is hand-drawn to read
/// as jj's elision mark at node scale.

/// A stroke path builder with round caps and joins — the node marks read
/// softer than lyon's default butt caps.
fn round_stroke_builder(width: Pixels) -> gpui::PathBuilder {
    gpui::PathBuilder::default().with_style(gpui::PathStyle::Stroke(
        gpui::StrokeOptions::default()
            .with_line_width(f32::from(width))
            .with_line_cap(LineCap::Round)
            .with_line_join(LineJoin::Round),
    ))
}

/// Appends a circular arc (centered `center`, radius `r`) from `start_deg` to
/// `end_deg` — degrees in y-down screen coordinates — to the builder, which
/// must already be positioned at the arc's start point. Uses gpui's native
/// elliptical-arc primitive in ≤90° chunks.
pub(crate) fn append_arc(
    builder: &mut gpui::PathBuilder,
    center: Point<Pixels>,
    r: Pixels,
    start_deg: f32,
    end_deg: f32,
) {
    let sweep = end_deg - start_deg;
    let segments = ((sweep.abs() / 90.0).ceil() as usize).max(1);
    let step = sweep / segments as f32;
    let point_at = |deg: f32| -> Point<Pixels> {
        let (s, c) = deg.to_radians().sin_cos();
        point(center.x + r * c, center.y + r * s)
    };
    let mut prev = start_deg;
    for _ in 0..segments {
        let next = prev + step;
        builder.arc_to(point(r, r), px(0.0), false, step > 0.0, point_at(next));
        prev = next;
    }
}

/// Appends a full circle (centered `center`, radius `r`) to a fill builder.
pub(crate) fn append_fill_circle(
    builder: &mut gpui::PathBuilder,
    center: Point<Pixels>,
    r: Pixels,
) {
    builder.move_to(point(center.x + r, center.y));
    append_arc(builder, center, r, 0.0, 360.0);
    builder.close();
}

fn paint_node_svg(
    svg: &'static str,
    name: &'static str,
    size: Pixels,
    center_x: Pixels,
    center_y: Pixels,
    color: gpui::Hsla,
    window: &mut Window,
    cx: &mut App,
) {
    let half = size / 2.0;
    let bounds = gpui::Bounds::new(
        point(center_x - half, center_y - half),
        gpui::Size {
            width: size,
            height: size,
        },
    );
    window
        .paint_svg(
            bounds,
            SharedString::from(name),
            Some(svg.as_bytes()),
            TransformationMatrix::unit(),
            color,
            cx,
        )
        .ok();
}

/// Paints one commit node in jj's conventions: `○` normal (solid dot, the
/// approved v2 base, hollow ring), `◆` immutable (filled diamond), `@`
/// working copy and
/// `~` hidden as real text glyphs, `×` conflict (crossed strokes).
/// `empty` fades normal/immutable glyphs to 40% opacity — never conflict,
/// working copy or hidden, whose states must stay loud.
pub(crate) fn draw_jj_node(
    glyph: JjNodeGlyph,
    flags: &JjLogFlags,
    center_x: Pixels,
    center_y: Pixels,
    color: gpui::Hsla,
    window: &mut Window,
    cx: &mut App,
) {
    let color = if flags.empty && matches!(glyph, JjNodeGlyph::Normal | JjNodeGlyph::Immutable) {
        color.alpha(0.4)
    } else {
        color
    };
    let radius = JJ_NODE_RADIUS;

    match glyph {
        // `○`: hollow ring — mutable commits are hollow, immutable (◆)
        // filled, mirroring jj's filled-vs-hollow distinction. Drawn as a
        // real circle path: the rounded-quad approximation rasterized
        // slightly oval at fractional pixel positions.
        JjNodeGlyph::Normal => {
            let mut builder = gpui::PathBuilder::default().with_style(gpui::PathStyle::Stroke(
                gpui::StrokeOptions::default()
                    .with_line_width(f32::from(JJ_NODE_STROKE_WIDTH))
                    .with_line_cap(LineCap::Round)
                    .with_line_join(LineJoin::Round),
            ));
            builder.move_to(point(center_x + radius, center_y));
            append_arc(&mut builder, point(center_x, center_y), radius, 0.0, 360.0);
            builder.close();
            if let Ok(path) = builder.build() {
                window.paint_path(path, color);
            }
        }
        // `@`: the working copy, as jj's literal character (SVG sprite —
        // path-drawn variants of this glyph kept rendering wrong).
        JjNodeGlyph::WorkingCopy => {
            paint_node_svg(
                AT_SIGN_SVG,
                "jj-at-sign",
                AT_SIGN_SIZE,
                center_x,
                center_y,
                color,
                window,
                cx,
            );
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
        // `×`: conflict — the crossed-lines mark as an SVG sprite (path
        // stroking mangled the thin arms at this size).
        JjNodeGlyph::Conflict => {
            paint_node_svg(
                CONFLICT_X_SVG,
                "jj-conflict-x",
                CONFLICT_X_SIZE,
                center_x,
                center_y,
                color,
                window,
                cx,
            );
        }
        // `~`: hidden — the wave mark.
        JjNodeGlyph::Hidden => {
            paint_node_svg(
                WAVE_SVG, "jj-wave", WAVE_SIZE, center_x, center_y, color, window, cx,
            );
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
            elided: false,
        }
    }

    /// A synthesized `(elided revisions)` row (the data layer builds these
    /// in `jj_log`, Phase C2; the layout just consumes them).
    fn elided_row(change_id: &str) -> JjLogEntry {
        let mut entry = entry(change_id, &[]);
        entry.flags.elided = true;
        entry
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

    fn edge<'g>(graph: &'g JjGraphData, child: &str, parent: &str) -> &'g EdgeLayout {
        graph
            .edges
            .iter()
            .find(|e| e.child == child && e.parent == parent)
            .unwrap_or_else(|| panic!("no edge {child} → {parent}"))
    }

    fn full<'g>(e: &'g EdgeLayout) -> (usize, usize, usize, usize, usize, usize) {
        (
            e.child_row,
            e.parent_row,
            e.column,
            e.child_col,
            e.parent_col,
            e.color_idx,
        )
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
    fn missing_parents_land_on_elided_rows() {
        // heads(all()): every node's parents are outside the window. The data
        // layer synthesizes an elided row after each dangling entry, and every
        // edge — primary or extra — lands there; nothing sprawls to fresh
        // lanes beyond the two the fan needs.
        let entries = vec![
            entry("h1", &["x", "y"]),
            elided_row("el-h1"),
            entry("h2", &["x"]),
            elided_row("el-h2"),
            entry("h3", &["y"]),
            elided_row("el-h3"),
        ];
        let graph = JjGraphData::from_entries(&entries, 8);
        assert_eq!(graph.commits.len(), 6);
        assert_eq!(lanes(&graph), vec![0, 0, 0, 0, 0, 0]);
        // The missing extra collapses into the primary's landing column —
        // one line through the ~, no lane sprawl.
        assert_eq!(graph.max_lanes, 1);
        assert_eq!(graph.edges.len(), 4);
        assert_eq!(graph.lines.len(), graph.edges.len());
        assert_eq!(full(edge(&graph, "h1", "x")), (0, 1, 0, 0, 0, 0));
        // The extra branch overlaps the primary's column and converges into
        // the ~ at the same column.
        assert_eq!(full(edge(&graph, "h1", "y")), (0, 1, 0, 0, 0, 0));
        assert_eq!(full(edge(&graph, "h2", "x")), (2, 3, 0, 0, 0, 0));
        assert_eq!(full(edge(&graph, "h3", "y")), (4, 5, 0, 0, 0, 0));
    }

    #[test]
    fn freed_lanes_reused_at_leftmost_free() {
        // c1→p and c2→p both free at p's row; p reuses the freed lane 0 for
        // its own edge, and x (no incoming) takes the leftmost free lane —
        // the freed lane 1, not a fresh lane 2.
        let entries = vec![
            entry("c1", &["p"]),
            entry("c2", &["p"]),
            entry("p", &["a"]),
            entry("x", &["a"]),
            entry("a", &[]),
        ];
        let graph = JjGraphData::from_entries(&entries, 8);
        assert_eq!(lanes(&graph), vec![0, 1, 0, 1, 0]);
        assert_eq!(graph.edges.len(), graph.lines.len());
        assert_eq!(full(edge(&graph, "c1", "p")), (0, 2, 0, 0, 0, 0));
        assert_eq!(full(edge(&graph, "c2", "p")), (1, 2, 1, 1, 0, 1));
        assert_eq!(full(edge(&graph, "p", "a")), (2, 4, 0, 0, 0, 0));
        assert_eq!(
            full(edge(&graph, "x", "a")),
            (3, 4, 1, 1, 0, 1),
            "x reuses freed lane 1"
        );
    }

    #[test]
    fn free_happens_on_last_use_only() {
        // x is processed while c2→p is still pending: lane 1 must not be
        // taken (one of the two pending uses of p is unresolved), so x goes
        // to a fresh lane 2. Both lanes free together when p is reached.
        let entries = vec![
            entry("c1", &["p"]),
            entry("c2", &["p"]),
            entry("x", &["a"]),
            entry("p", &["a"]),
            entry("a", &[]),
        ];
        let graph = JjGraphData::from_entries(&entries, 8);
        assert_eq!(lanes(&graph), vec![0, 1, 2, 0, 0]);
        assert_eq!(
            edge(&graph, "x", "a").column,
            2,
            "lane 1 stays reserved until p's row"
        );
        assert_eq!(full(edge(&graph, "c1", "p")), (0, 3, 0, 0, 0, 0));
        assert_eq!(full(edge(&graph, "c2", "p")), (1, 3, 1, 1, 0, 1));
        assert_eq!(full(edge(&graph, "x", "a")), (2, 4, 2, 2, 0, 2));
        assert_eq!(full(edge(&graph, "p", "a")), (3, 4, 0, 0, 0, 0));
    }

    #[test]
    fn three_parent_octopus_allocates_leftmost_free_per_extra_parent() {
        let entries = vec![
            entry("m", &["p1", "p2", "p3"]),
            entry("p3", &["a"]),
            entry("p2", &["a"]),
            entry("p1", &["a"]),
            entry("a", &[]),
        ];
        let graph = JjGraphData::from_entries(&entries, 8);
        // The octopus sits in its first parent's column (0); the extra
        // parents' edges take leftmost-free lanes topmost-first: p3 (row 1)
        // gets lane 1, p2 (row 2) lane 2 — the fan starts from the top.
        assert_eq!(lanes(&graph), vec![0, 1, 2, 0, 0]);
        assert_eq!(graph.edges.len(), graph.lines.len());
        assert_eq!(full(edge(&graph, "m", "p1")), (0, 3, 0, 0, 0, 0));
        assert_eq!(full(edge(&graph, "m", "p2")), (0, 2, 2, 0, 2, 2));
        assert_eq!(full(edge(&graph, "m", "p3")), (0, 1, 1, 0, 1, 1));
        assert_eq!(full(edge(&graph, "p3", "a")), (1, 4, 1, 1, 0, 1));
        assert_eq!(full(edge(&graph, "p2", "a")), (2, 4, 2, 2, 0, 2));
        assert_eq!(full(edge(&graph, "p1", "a")), (3, 4, 0, 0, 0, 0));
    }

    #[test]
    fn four_parent_merge_allocates_three_leftmost_free_lanes() {
        let entries = vec![
            entry("m", &["p1", "p2", "p3", "p4"]),
            entry("p4", &["a"]),
            entry("p3", &["a"]),
            entry("p2", &["a"]),
            entry("p1", &["a"]),
            entry("a", &[]),
        ];
        let graph = JjGraphData::from_entries(&entries, 8);
        // Extra parents topmost-first: p4 (row 1) lane 1, p3 lane 2, p2
        // lane 3.
        assert_eq!(lanes(&graph), vec![0, 1, 2, 3, 0, 0]);
        assert_eq!(graph.max_lanes, 4);
        assert_eq!(graph.edges.len(), graph.lines.len());
        assert_eq!(full(edge(&graph, "m", "p1")), (0, 4, 0, 0, 0, 0));
        assert_eq!(full(edge(&graph, "m", "p2")), (0, 3, 3, 0, 3, 3));
        assert_eq!(full(edge(&graph, "m", "p3")), (0, 2, 2, 0, 2, 2));
        assert_eq!(full(edge(&graph, "m", "p4")), (0, 1, 1, 0, 1, 1));
        assert_eq!(full(edge(&graph, "p4", "a")), (1, 5, 1, 1, 0, 1));
        assert_eq!(full(edge(&graph, "p3", "a")), (2, 5, 2, 2, 0, 2));
        assert_eq!(full(edge(&graph, "p2", "a")), (3, 5, 3, 3, 0, 3));
        assert_eq!(full(edge(&graph, "p1", "a")), (4, 5, 0, 0, 0, 0));
    }

    #[test]
    fn working_copy_and_root_stay_in_column_zero() {
        // @ is the newest row and the root the oldest: the mainline keeps
        // lane 0 from @ through to the root, so both sit in column 0.
        let mut wc = entry("w", &["m"]);
        wc.flags.working_copy = true;
        let entries = vec![wc, entry("b", &["m"]), entry("m", &["a"]), entry("a", &[])];
        let graph = JjGraphData::from_entries(&entries, 8);
        assert_eq!(lanes(&graph), vec![0, 1, 0, 0]);
        assert_eq!(graph.commits[0].lane, 0, "working copy in column 0");
        assert_eq!(graph.commits.last().unwrap().lane, 0, "root in column 0");
        assert_eq!(full(edge(&graph, "w", "m")), (0, 2, 0, 0, 0, 0));
        assert_eq!(full(edge(&graph, "b", "m")), (1, 2, 1, 1, 0, 1));
        assert_eq!(full(edge(&graph, "m", "a")), (2, 3, 0, 0, 0, 0));
    }

    #[test]
    fn elided_rows_land_dangling_edges() {
        // The C1 fixture: @ merges two branches, each of whose next row has
        // its parent outside the window. The elided rows are hand-built (the
        // synthesis itself lands in `jj_log` in C2): the data layer inserts
        // one directly after each row whose parent is missing.
        let mut at = entry("kozmttlk", &["rzvnrvrz", "xpzmvpqp"]);
        at.flags.working_copy = true;
        let entries = vec![
            at,
            entry("xpzmvpqp", &["uktnvvqq"]),
            elided_row("el-xp"),
            entry("rzvnrvrz", &["rz-out"]),
            elided_row("el-rz"),
            entry("root", &[]),
        ];
        let graph = JjGraphData::from_entries(&entries, 8);
        assert_eq!(graph.commits.len(), 6, "elided rows consume row indices");
        // Lanes: @ and the mainline in 0, xpzmvpqp in its merge lane 1, each
        // elided row in the column of the edge that lands on it, and the
        // root reusing the freed lane 0 — nothing sprawls a fresh lane.
        assert_eq!(lanes(&graph), vec![0, 1, 1, 0, 0, 0]);
        assert_eq!(graph.max_lanes, 2);
        // Every edge lands on a row: no line is left dangling in `lines`.
        assert_eq!(graph.edges.len(), 4);
        assert_eq!(graph.lines.len(), graph.edges.len());
        // @→rzvnrvrz runs down lane 0, passing the elided row at row 2.
        assert_eq!(
            full(edge(&graph, "kozmttlk", "rzvnrvrz")),
            (0, 3, 0, 0, 0, 0)
        );
        // @→xpzmvpqp is the merge curve into lane 1.
        assert_eq!(
            full(edge(&graph, "kozmttlk", "xpzmvpqp")),
            (0, 1, 1, 0, 1, 1)
        );
        // xpzmvpqp's outside parent lands on the elided row: zero-length
        // bottom stub (parent_col == column), straight into the `~`.
        assert_eq!(
            full(edge(&graph, "xpzmvpqp", "uktnvvqq")),
            (1, 2, 1, 1, 1, 1)
        );
        // rzvnrvrz's outside parent lands on its elided row.
        assert_eq!(full(edge(&graph, "rzvnrvrz", "rz-out")), (3, 4, 0, 0, 0, 0));
    }

    #[test]
    fn elided_row_lands_merge_primary_edge() {
        // An octopus merge whose primary parent is outside the window: the
        // elided row lands it in the mainline lane, and the in-window
        // parents take their merge lanes topmost-first (p3 before p2).
        let entries = vec![
            entry("m", &["oout", "p2", "p3"]),
            elided_row("el-m"),
            entry("p3", &["a"]),
            entry("p2", &["a"]),
            entry("a", &[]),
        ];
        let graph = JjGraphData::from_entries(&entries, 8);
        assert_eq!(graph.commits.len(), 5);
        assert_eq!(lanes(&graph), vec![0, 0, 1, 2, 1]);
        // An edge's color is its own column's lane color (assigned at first
        // use: lane 0 for m, lane 1 at p3, lane 2 at p2); it lands in the
        // parent's lane, so `a` in lane 1 makes both bottom edges bend into
        // column 1.
        assert_eq!(full(edge(&graph, "m", "oout")), (0, 1, 0, 0, 0, 0));
        assert_eq!(full(edge(&graph, "m", "p2")), (0, 3, 2, 0, 2, 2));
        assert_eq!(full(edge(&graph, "m", "p3")), (0, 2, 1, 0, 1, 1));
        assert_eq!(full(edge(&graph, "p3", "a")), (2, 4, 1, 1, 1, 1));
        assert_eq!(full(edge(&graph, "p2", "a")), (3, 4, 2, 2, 1, 2));
        assert_eq!(graph.lines.len(), graph.edges.len());
    }

    #[test]
    fn extra_parent_shares_sibling_lane() {
        // c2's extra parent b already has a pending edge (from c1): the new
        // edge piggybacks in that lane instead of detouring out to a fresh
        // one beyond both endpoints — the two overlap between the siblings'
        // rows and converge at b.
        let entries = vec![
            entry("m", &["a", "b"]),
            entry("c1", &["b"]),
            entry("c2", &["a", "b"]),
            entry("b", &["x"]),
            entry("a", &[]),
            entry("x", &[]),
        ];
        let graph = JjGraphData::from_entries(&entries, 8);
        // x continues b's lane (min-incoming), hence the trailing 1.
        assert_eq!(lanes(&graph), vec![0, 2, 3, 1, 0, 1]);
        assert_eq!(graph.edges.len(), graph.lines.len());
        // The piggyback edge runs between its endpoints (col 3 -> col 1 via
        // the sibling's lane 2) and takes the shared lane's color, so the
        // overlap with c1's edge reads as one continuous line.
        assert_eq!(full(edge(&graph, "c2", "b")), (2, 3, 2, 3, 1, 1));
        assert_eq!(full(edge(&graph, "c1", "b")), (1, 3, 2, 2, 1, 1));
        assert_eq!(full(edge(&graph, "m", "b")), (0, 3, 1, 0, 1, 3));
    }

    #[test]
    fn missing_extra_parents_share_the_elided_landing() {
        // An octopus whose primary AND extra parents are all outside the
        // window: one elided landing, and the extras collapse into the
        // primary's column (jj draws one line through the ~) — no fresh
        // lane per missing parent.
        let entries = vec![
            entry("m", &["o1", "o2", "o3"]),
            elided_row("el-m"),
            entry("root", &[]),
        ];
        let graph = JjGraphData::from_entries(&entries, 8);
        assert_eq!(lanes(&graph), vec![0, 0, 0]);
        assert_eq!(graph.max_lanes, 1, "no lane sprawl for missing parents");
        assert_eq!(graph.edges.len(), 3);
        assert_eq!(graph.lines.len(), graph.edges.len());
        assert_eq!(full(edge(&graph, "m", "o1")), (0, 1, 0, 0, 0, 0));
        // The collapsed extras overlap the primary's column and converge
        // into the ~ at the same column.
        assert_eq!(full(edge(&graph, "m", "o2")), (0, 1, 0, 0, 0, 0));
        assert_eq!(full(edge(&graph, "m", "o3")), (0, 1, 0, 0, 0, 0));
    }

    #[test]
    fn empty_window_produces_nothing() {
        let graph = JjGraphData::from_entries(&[], 8);
        assert!(graph.commits.is_empty());
        assert!(graph.lines.is_empty());
        assert!(graph.edges.is_empty());
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
