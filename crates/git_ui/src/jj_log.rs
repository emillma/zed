use crate::git_graph::{
    LANE_WIDTH, LEFT_PADDING, LINE_WIDTH, accent_colors_count, lane_center_x, timestamp_format,
};
use crate::jj_graph::{
    JJ_GLYPH_CLEARANCE, JJ_NODE_RADIUS, JjGraphData, JjNodeGlyph, NodeStatusColors,
    append_fill_circle, draw_jj_node, node_color, node_glyph,
};
use crate::jj_settings::JjSettings;
use anyhow::Result;
use editor::Editor;
use git::jj::{JjLogEntry, JjLogFlags};
use gpui::{
    Anchor, AnyElement, App, Bounds, Context, DefiniteLength, DismissEvent, Entity, EventEmitter,
    FocusHandle, Focusable, Hsla, MouseButton, MouseDownEvent, PathBuilder, Pixels, Point, Render,
    ScrollWheelEvent, SharedString, Subscription, Task, WeakEntity, Window, actions, anchored,
    deferred, point, px,
};
use menu::Confirm;
use project::git_store::{GitStore, GitStoreEvent, RepositoryEvent};
use std::collections::{HashMap, HashSet, VecDeque};
use std::time::{Duration, Instant};
use time::{OffsetDateTime, UtcOffset};
use ui::{
    Chip, ColumnWidthConfig, ContextMenu, ContextMenuEntry, DocumentationSide, HeaderResizeInfo,
    IconButtonShape, RedistributableColumnsState, ScrollableHandle, Table, TableInteractionState,
    TableRenderContext, TableResizeBehavior, Tooltip, bind_redistributable_columns, prelude::*,
    redistribute_hidden_fractions, redistribute_hidden_widths,
    render_redistributable_columns_resize_handles, render_table_header, table_row::TableRow,
};
use workspace::{
    SerializableItem, Workspace,
    item::{Item, ItemEvent},
};

/// Minimum time between polls of the jj log for the history panel.
const POLL_INTERVAL: Duration = Duration::from_secs(1);

/// Maximum number of log entries fetched for the history panel.
const LOG_LIMIT: usize = 200;

/// Vertical padding added to the line height for each row. Mirrors
/// GitGraph's private `ROW_VERTICAL_PADDING` so the canvas and the table
/// agree on row geometry.
const ROW_VERTICAL_PADDING: Pixels = px(4.0);

/// Column fractions of the full panel width, copied from GitGraph's
/// 5-column defaults (graph, description, date, author, commit).
const GRAPH_COLUMN_FRACTION: f32 = 0.14;
const DESCRIPTION_COLUMN_FRACTION: f32 = 0.6192;
const DATE_COLUMN_FRACTION: f32 = 0.1032;
const AUTHOR_COLUMN_FRACTION: f32 = 0.086;
const COMMIT_COLUMN_FRACTION: f32 = 0.0516;

/// Extra width (logical px) added PER SIDE to each lane stroke: every
/// commit line is stroked twice with identical geometry — a
/// background-colored border `LINE_WIDTH + 2 * EDGE_BORDER_WIDTH` wide
/// under the lane-colored fill — so a later line's border cuts through
/// an earlier line's fill at crossings while a line's own bends (same
/// path) never self-cut.
const EDGE_BORDER_WIDTH: Pixels = px(1.0);

/// Clearance from a node center to where an edge stub attaches or ends: just
/// outside the glyph (radius) plus the lane's background border, so a stub
/// (and its border) never cuts a node glyph.
///
/// `JJ_NODE_RADIUS + EDGE_BORDER_WIDTH` (5.5px), written out because
/// `Pixels`' derived `Add` is not `const`.
const JJ_EDGE_CLEARANCE: Pixels = px(5.5);
/// y-offset of the stubs from their node centers. The bottom stub attaches
/// BELOW the child node (the edge exits through the node's bottom) and the
/// top stub ABOVE the parent node (it enters through the node's top) — the
/// natural flow direction. The small offset keeps a node's incoming and
/// outgoing stubs close together; the node's background halo (painted over
/// the edges) masks the junction.
const EDGE_STUB_OFFSET: Pixels = px(2.0);
/// Edges whose vertical column reaches this far right are elided (the graph
/// is too wide to draw them); their nodes still render.
const MAX_EDGE_COLUMN: usize = 64;
/// Z-layers, in paint order: verticals first, then stubs.
const LAYER_V: u8 = 0;
const LAYER_BT: u8 = 1;

/// One commit line built twice with identical geometry: the wider
/// background-colored `border` stroke under the narrower lane-colored
/// `fill`. Every geometry op is forwarded to both builders so the two
/// paths stay in lockstep.
struct LaneEdge {
    border: PathBuilder,
    fill: PathBuilder,
}

impl LaneEdge {
    fn new() -> Self {
        Self {
            border: PathBuilder::stroke(LINE_WIDTH + 2.0 * EDGE_BORDER_WIDTH),
            fill: PathBuilder::stroke(LINE_WIDTH),
        }
    }

    fn move_to(&mut self, to: Point<Pixels>) {
        self.border.move_to(to);
        self.fill.move_to(to);
    }

    fn line_to(&mut self, to: Point<Pixels>) {
        self.border.line_to(to);
        self.fill.line_to(to);
    }

    fn curve_to(&mut self, to: Point<Pixels>, ctrl: Point<Pixels>) {
        self.border.curve_to(to, ctrl);
        self.fill.curve_to(to, ctrl);
    }
}

/// The color painted behind the lanes — must match what actually renders
/// there. JjLog is a workspace tab item that paints no background of its
/// own (GitGraph, in contrast, paints `editor_background` on its root
/// div), so the workspace's `background` shows through behind the graph.
fn lane_border_color(cx: &App) -> Hsla {
    cx.theme().colors().background
}

/// One paintable edge fragment: a z-layer (verticals paint before stubs), a
/// sort key (column for verticals, reach for stubs - longer first, so the
/// shorter end up in front), the dual border/fill path, and its accent index.
struct EdgePart {
    layer: u8,
    sort_key: usize,
    lane: LaneEdge,
    color_idx: usize,
}

/// Numeric geometry of one edge, extracted so the `'static` canvas closure can
/// own it instead of borrowing the graph (whose `EdgeLayout`s carry test-only
/// `String`s and aren't `Clone`).
struct EdgeGeom {
    child_row: usize,
    parent_row: usize,
    column: usize,
    child_col: usize,
    parent_col: usize,
    color_idx: usize,
}

impl EdgeGeom {
    /// The widest column this edge touches — the stubs' z-order key
    /// (longer reach sorts first, so shorter stubs end up in front).
    fn reach(&self) -> usize {
        self.column.max(self.child_col).max(self.parent_col)
    }
}

/// The jj history panel: a list of the first 200 changes of the first
/// jj repository in the project, if any. Rendered like GitGraph: a
/// `ui::Table` of text columns with the lane graph painted on a canvas
/// beside it, synced to the table's scroll state.
pub struct JjLog {
    focus_handle: FocusHandle,
    git_store: Entity<GitStore>,
    entries: Vec<JjLogEntry>,
    graph_data: Option<JjGraphData>,
    table_interaction_state: Entity<TableInteractionState>,
    // GitGraph's column machinery: user-draggable widths and per-column
    // visibility toggled from the header's right-click context menu.
    column_widths: Entity<RedistributableColumnsState>,
    column_visibility: TableRow<bool>,
    context_menu: Option<JjLogContextMenu>,
    // Revset aliases from jj's config layers (user, repo, workspace —
    // merged), refreshed on each poll; the dropdown offers these.
    revset_aliases: Vec<(SharedString, SharedString)>,
    // Revset filter input, created lazily on first render: the JjLog
    // constructors have no Window, which Editor::single_line requires.
    revset_editor: Option<Entity<Editor>>,
    current_revset: Option<String>,
    loading: bool,
    error: Option<String>,
    poll_scheduled: bool,
    last_poll: Option<Instant>,
    _subscription: Subscription,
}

struct JjLogContextMenu {
    menu: Entity<ContextMenu>,
    position: Point<Pixels>,
    _subscription: Subscription,
}

impl JjLog {
    pub fn new(git_store: Entity<GitStore>, cx: &mut Context<Self>) -> Self {
        let subscription = cx.subscribe(&git_store, Self::on_git_store_event);
        cx.observe_global::<JjSettings>(|this, cx| {
            // The `uniform_list` powering the table caches the item size from
            // its last layout; invalidate it so a changed row height (font
            // size, scale) re-measures on the next frame. Mirrors GitGraph.
            this.table_interaction_state.update(cx, |state, _cx| {
                state.scroll_handle.0.borrow_mut().last_item_size = None;
            });
            cx.notify();
        })
        .detach();
        let table_interaction_state = cx.new(|cx| {
            let mut state = TableInteractionState::new(cx);
            state.focus_handle = state.focus_handle.tab_index(1).tab_stop(true);
            state
        });
        let column_widths = cx.new(|_cx| {
            RedistributableColumnsState::new(
                5,
                vec![
                    DefiniteLength::Fraction(GRAPH_COLUMN_FRACTION),
                    DefiniteLength::Fraction(DESCRIPTION_COLUMN_FRACTION),
                    DefiniteLength::Fraction(DATE_COLUMN_FRACTION),
                    DefiniteLength::Fraction(AUTHOR_COLUMN_FRACTION),
                    DefiniteLength::Fraction(COMMIT_COLUMN_FRACTION),
                ],
                vec![
                    TableResizeBehavior::Resizable,
                    TableResizeBehavior::Resizable,
                    TableResizeBehavior::Resizable,
                    TableResizeBehavior::Resizable,
                    TableResizeBehavior::Resizable,
                ],
            )
        });
        let mut this = Self {
            focus_handle: cx.focus_handle(),
            git_store,
            entries: Vec::new(),
            graph_data: None,
            table_interaction_state,
            column_widths,
            column_visibility: TableRow::from_element(false, 5),
            context_menu: None,
            revset_aliases: Vec::new(),
            revset_editor: None,
            current_revset: None,
            loading: false,
            error: None,
            poll_scheduled: false,
            last_poll: None,
            _subscription: subscription,
        };
        this.schedule_poll(cx);
        this
    }

    fn on_git_store_event(
        &mut self,
        _: Entity<GitStore>,
        event: &GitStoreEvent,
        cx: &mut Context<Self>,
    ) {
        let refresh = matches!(
            event,
            GitStoreEvent::JjRepositoriesUpdated
                | GitStoreEvent::RepositoryUpdated(_, RepositoryEvent::StatusesChanged, _)
        );
        if refresh {
            self.schedule_poll(cx);
        }
    }

    /// Polls the jj log at most once in flight and at least POLL_INTERVAL
    /// apart: triggers coalesce while a poll is in flight, and triggers within
    /// the interval are dropped.
    fn schedule_poll(&mut self, cx: &mut Context<Self>) {
        if self.poll_scheduled {
            return;
        }
        if let Some(last) = self.last_poll {
            if last.elapsed() < POLL_INTERVAL {
                return;
            }
        }
        self.poll_scheduled = true;
        let repository = self
            .git_store
            .read(cx)
            .jj_repositories()
            .values()
            .find_map(|state| state.backend().cloned());
        if repository.is_some() {
            self.loading = true;
        }
        let current_revset = self.current_revset.clone();
        cx.spawn(async move |this, cx| {
            let (entries, error, aliases) = match &repository {
                Some(repository) => {
                    let result = match current_revset {
                        Some(revset) => repository.log_revset(revset, LOG_LIMIT).await,
                        None => repository.log(LOG_LIMIT).await,
                    };
                    let aliases = repository.revset_aliases().await.unwrap_or_default();
                    match result {
                        Ok(entries) => (entries, None, aliases),
                        Err(error) => (Vec::new(), Some(format!("{error:#}")), aliases),
                    }
                }
                None => (Vec::new(), None, Vec::new()),
            };
            this.update(cx, move |this, cx| {
                this.poll_scheduled = false;
                this.revset_aliases = aliases;
                // Only a poll that actually invoked jj consumes the throttle
                // interval; a no-op poll (no jj repository yet) must not delay
                // the next trigger, or the event carrying the newly discovered
                // repository gets dropped and the panel stays blank.
                if repository.is_some() {
                    this.last_poll = Some(Instant::now());
                }
                // A failed poll (e.g. an invalid revset) keeps the last good
                // rows visible; the error surfaces as a banner under the
                // search bar. Lane data is only built on a successful fetch;
                // jj log entries arrive children-first, the order add_commits
                // expects.
                match error {
                    Some(error) => this.error = Some(error),
                    None => {
                        // The lane engine is jj-native: change-id keyed, fed
                        // straight from the parsed entries — no git types in
                        // the render path.
                        // A parent outside the emitted window has no row of
                        // its own: synthesize the `(elided revisions)` row
                        // after each entry that dangles, so the graph's
                        // dangling edge lands on a real row (and the table
                        // shows the cut-off) instead of running to the
                        // window's end.
                        // First reorder into jj's topo-grouped display order:
                        // `jj log` runs TopoGroupedGraph over the revset
                        // before rendering, and the lane layout needs that
                        // same order or lanes stay occupied across huge row
                        // gaps.
                        let entries = topo_group_entries(entries);
                        let entries = synthesize_elided_entries(entries);
                        let graph_data = JjGraphData::from_entries(
                            &entries,
                            accent_colors_count(&cx.theme().accents()),
                        );
                        this.entries = entries;
                        this.graph_data = Some(graph_data);
                        this.error = None;
                    }
                }
                this.loading = false;
                cx.notify();
            })
            .ok();
        })
        .detach();
    }

    /// Row height mirroring GitGraph's: text line height plus vertical
    /// padding, scale-rounded so the canvas and the table agree on row
    /// geometry.
    fn row_height(window: &Window, _cx: &App) -> Pixels {
        let rem_size = window.rem_size();
        let line_height = window.text_style().line_height_in_pixels(rem_size);
        let raw = line_height + ROW_VERTICAL_PADDING;
        let scale = window.scale_factor();

        (raw * scale).round() / scale
    }

    /// Column fractions of the full panel width, from the redistributable
    /// column state (user-draggable), with hidden columns zeroed. Mirrors
    /// GitGraph's `preview_column_fractions`.
    fn column_fractions(&self, window: &Window, cx: &App) -> [f32; 5] {
        let raw = self
            .column_widths
            .read(cx)
            .preview_fractions(window.rem_size());
        let fractions = redistribute_hidden_fractions(&raw, Some(&self.column_visibility));
        let value = |idx: usize| fractions.as_slice().get(idx).copied().unwrap_or(0.0);
        [value(0), value(1), value(2), value(3), value(4)]
    }

    /// The revset search bar, styled after GitGraph's search bar: a bordered
    /// editor box (Enter applies) with save and saved-revsets icon buttons.
    fn render_search_bar(&self, editor: Entity<Editor>, cx: &mut Context<Self>) -> AnyElement {
        let color = cx.theme().colors();
        let query_focus_handle = editor.focus_handle(cx).tab_index(1).tab_stop(true);

        h_flex()
            .key_context("JjLogSearchBar")
            .tab_index(1)
            .tab_group()
            .tab_stop(false)
            .w_full()
            .p_1p5()
            .gap_1p5()
            .border_b_1()
            .border_color(color.border_variant)
            .child(
                h_flex()
                    .h_8()
                    .flex_1()
                    .min_w_0()
                    .px_1p5()
                    .gap_1()
                    .track_focus(&query_focus_handle)
                    .border_1()
                    .border_color(color.border_variant)
                    .rounded_md()
                    .bg(color.toolbar_background)
                    .on_action(cx.listener(Self::confirm_revset))
                    .child(editor),
            )
            .child(
                IconButton::new("jj-log-revset-aliases", IconName::ChevronDown)
                    .shape(IconButtonShape::Square)
                    .icon_size(IconSize::Small)
                    .tooltip(Tooltip::text("Revset Aliases"))
                    .on_click(cx.listener(|this, _, window, cx| {
                        this.deploy_saved_revsets_menu(window, cx);
                    })),
            )
            .into_any_element()
    }

    /// Applies the editor's contents (Enter); an empty revset clears the
    /// filter and returns to the default log.
    fn confirm_revset(&mut self, _: &Confirm, _window: &mut Window, cx: &mut Context<Self>) {
        let text = self
            .revset_editor
            .as_ref()
            .map(|editor| editor.read(cx).text(cx).trim().to_string());
        self.current_revset = match text {
            Some(text) if !text.is_empty() => Some(text),
            _ => None,
        };
        cx.emit(ItemEvent::UpdateTab);
        self.schedule_poll(cx);
    }

    /// Fills the editor with `revset` and applies it.
    fn apply_revset(&mut self, revset: String, window: &mut Window, cx: &mut Context<Self>) {
        if let Some(editor) = self.revset_editor.as_ref() {
            editor.update(cx, |editor, cx| editor.set_text(revset.clone(), window, cx));
        }
        self.current_revset = Some(revset);
        cx.emit(ItemEvent::UpdateTab);
        self.schedule_poll(cx);
    }

    /// Dropdown of saved revsets, by name; hovering an entry reveals the full
    /// revset (documentation aside), selecting it fills the editor and applies.
    fn deploy_saved_revsets_menu(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        // Refresh on open: adding an alias (`jj config set`) touches no file
        // in the worktree, so no repository event fires and the cache would
        // otherwise stay stale until an unrelated poll.
        let Some(repository) = self
            .git_store
            .read(cx)
            .jj_repositories()
            .values()
            .find_map(|state| state.backend().cloned())
        else {
            return;
        };
        cx.spawn_in(window, async move |this, cx| {
            let aliases = repository.revset_aliases().await.unwrap_or_default();
            this.update_in(cx, |this, window, cx| {
                this.revset_aliases = aliases.clone();
                this.deploy_aliases_menu(aliases, window, cx);
            })
            .ok();
        })
        .detach();
    }

    /// Deploys the revset-alias dropdown from the given (name, expansion)
    /// pairs; hovering an entry reveals the expansion, selecting it applies
    /// the alias name as the revset.
    fn deploy_aliases_menu(
        &mut self,
        aliases: Vec<(SharedString, SharedString)>,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let position = window.mouse_position();
        let focus_handle = self.focus_handle.clone();
        let jj_log = cx.entity();
        let context_menu = ContextMenu::build(window, cx, |mut context_menu, _window, _cx| {
            context_menu = context_menu.context(focus_handle).header("Revset Aliases");
            if aliases.is_empty() {
                context_menu = context_menu
                    .item(ContextMenuEntry::new("No revset-aliases configured").disabled(true));
            }
            for (name, expansion) in aliases {
                let jj_log = jj_log.clone();
                let aside_expansion = expansion.clone();
                context_menu = context_menu.item(
                    ContextMenuEntry::new(name.clone())
                        .handler(move |window, cx| {
                            jj_log.update(cx, |this, cx| {
                                let revset = name.to_string();
                                this.apply_revset(revset, window, cx);
                            });
                        })
                        .documentation_aside(DocumentationSide::Left, move |_| {
                            Label::new(aside_expansion.clone()).into_any_element()
                        }),
                );
            }
            context_menu
        });
        self.set_context_menu(context_menu, position, window, cx);
    }

    fn set_context_menu(
        &mut self,
        context_menu: Entity<ContextMenu>,
        position: Point<Pixels>,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        window.focus(&context_menu.focus_handle(cx), cx);

        let subscription = cx.subscribe_in(
            &context_menu,
            window,
            |this, _, _: &DismissEvent, window, cx| {
                if this.context_menu.as_ref().is_some_and(|context_menu| {
                    context_menu
                        .menu
                        .focus_handle(cx)
                        .contains_focused(window, cx)
                }) {
                    cx.focus_self(window);
                }
                this.context_menu.take();
                cx.notify();
            },
        );
        self.context_menu = Some(JjLogContextMenu {
            menu: context_menu,
            position,
            _subscription: subscription,
        });
        cx.notify();
    }

    fn toggle_column_visibility(&mut self, col_idx: usize, cx: &mut Context<Self>) {
        if let Some(slot) = self.column_visibility.as_mut_slice().get_mut(col_idx) {
            *slot = !*slot;
            cx.emit(ItemEvent::Edit);
        }
    }

    fn deploy_header_context_menu(
        &mut self,
        position: Point<Pixels>,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        const COLUMNS: &[&str] = &["Graph", "Description", "Date", "Author", "Commit"];

        let filter = self.column_visibility.clone();
        let visible_count = filter.as_slice().iter().filter(|hidden| !**hidden).count();
        let focus_handle = self.focus_handle.clone();
        let jj_log = cx.entity();
        let context_menu = ContextMenu::build(window, cx, |mut context_menu, _window, _cx| {
            context_menu = context_menu.context(focus_handle).header("Columns");
            for (col_idx, label) in COLUMNS.iter().enumerate() {
                let is_visible = !filter.get(col_idx).copied().unwrap_or(false);
                // Disable hiding the last remaining visible column.
                let can_toggle = !is_visible || visible_count > 1;
                let jj_log = jj_log.clone();
                context_menu = context_menu.toggleable_entry_disabled_when(
                    label.to_string(),
                    is_visible,
                    !can_toggle,
                    IconPosition::End,
                    None,
                    move |_window, cx| {
                        jj_log.update(cx, |this, cx| {
                            this.toggle_column_visibility(col_idx, cx);
                            cx.notify();
                        });
                    },
                );
            }
            context_menu
        });

        self.set_context_menu(context_menu, position, window, cx);
    }

    /// A wheel event over the graph canvas: the canvas is a sibling of the
    /// table in the h_flex, so the event never reaches the table's own scroll
    /// handler. Forward it to the table's scroll state manually — mirroring
    /// GitGraph's `handle_graph_scroll`.
    fn handle_graph_scroll(
        &mut self,
        event: &ScrollWheelEvent,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let line_height = window.line_height();
        let delta = event.delta.pixel_delta(line_height);

        let table_state = self.table_interaction_state.read(cx);
        let current_offset = table_state.scroll_offset();
        let viewport_height = table_state.scroll_handle.viewport().size.height;
        let content_height = Self::row_height(window, cx) * self.entries.len();
        let max_vertical_scroll = (viewport_height - content_height).min(px(0.));

        let new_y = (current_offset.y + delta.y).clamp(max_vertical_scroll, px(0.));
        let new_offset = Point::new(current_offset.x, new_y);

        if new_offset != current_offset {
            table_state.set_scroll_offset(new_offset);
            cx.notify();
        }
    }

    /// Paints the lane graph over the table's visible rows, synced to the
    /// table's scroll state exactly like GitGraph: rows and lanes are shifted
    /// table's scroll offset and only the visible range is painted.
    /// Every edge is drawn from `jj_graph`'s `EdgeLayout` as up to three
    /// fragments (bottom stub, vertical, top stub), z-ordered by layer.
    fn render_graph_canvas(
        &self,
        graph_data: &JjGraphData,
        row_height: Pixels,
        graph_width: Pixels,
        window: &Window,
        cx: &mut Context<Self>,
    ) -> impl IntoElement {
        let table_state = self.table_interaction_state.read(cx);
        let viewport_height = table_state
            .scroll_handle
            .0
            .borrow()
            .last_item_size
            .map(|size| size.item.height)
            .unwrap_or(window.viewport_size().height);
        let commit_count = graph_data.commits.len();

        let content_height = row_height * commit_count;
        let max_scroll = (content_height - viewport_height).max(px(0.));
        let scroll_offset_y = (-table_state.scroll_offset().y).clamp(px(0.), max_scroll);

        let first_visible_row = (scroll_offset_y / row_height).floor() as usize;
        let vertical_scroll_offset = scroll_offset_y - (first_visible_row as f32 * row_height);

        let visible_row_count = ((viewport_height / row_height).ceil() as usize).min(commit_count);
        let last_visible_row = first_visible_row + visible_row_count + 1;
        let viewport_range = first_visible_row.min(commit_count.saturating_sub(1))
            ..last_visible_row.min(commit_count);
        let rows = graph_data.commits[viewport_range.clone()].to_vec();
        // Flags for the visible rows, aligned with `rows` — the glyph and
        // color mapping reads them at paint time.
        let visible_flags: Vec<git::jj::JjLogFlags> = self.entries[viewport_range.clone()]
            .iter()
            .map(|entry| entry.flags.clone())
            .collect();
        // Per-row vertical clearance where lane lines start/end (absolute-row
        // indexed): text glyphs (`@`, `~`) need more room than the circles so
        // the lines don't cross them, plus `EDGE_BORDER_WIDTH` so the lanes'
        // background borders keep the same gap to the glyphs the fills had.
        let row_clearance: Vec<Pixels> = self
            .entries
            .iter()
            .map(|entry| match elided_glyph(&entry.flags) {
                JjNodeGlyph::WorkingCopy | JjNodeGlyph::Hidden => {
                    JJ_GLYPH_CLEARANCE + EDGE_BORDER_WIDTH
                }
                _ => JJ_NODE_RADIUS + EDGE_BORDER_WIDTH,
            })
            .collect();
        // Complete edges (both endpoints in the window): pre-extract their
        // numeric geometry so the 'static canvas closure can own it (the
        // EdgeLayouts carry test-only Strings and aren't Clone). Edges whose
        // vertical reaches the elision limit are dropped here (nodes still
        // render); the rest are clipped to the visible window.
        let edge_geoms: Vec<EdgeGeom> = graph_data
            .edges
            .iter()
            .filter(|edge| edge.column < MAX_EDGE_COLUMN)
            .filter(|edge| {
                edge.child_row <= viewport_range.end && edge.parent_row >= viewport_range.start
            })
            .map(|edge| EdgeGeom {
                child_row: edge.child_row,
                parent_row: edge.parent_row,
                column: edge.column,
                child_col: edge.child_col,
                parent_col: edge.parent_col,
                color_idx: edge.color_idx,
            })
            .collect();
        gpui::canvas(
            move |_bounds, _window, _cx| {},
            move |bounds: Bounds<Pixels>, _: (), window: &mut Window, cx: &mut App| {
                window.paint_layer(bounds, |window| {
                    let accent_colors = cx.theme().accents().clone();
                    let border_color = lane_border_color(cx);
                    // Row-center y for a graph row — the node loop's math,
                    // keyed by absolute row instead of window index.
                    let row_y = |row: usize| {
                        bounds.origin.y
                            + (row as f32 - first_visible_row as f32) * row_height
                            + row_height / 2.0
                            - vertical_scroll_offset
                    };
                    // Every edge fragment is collected, sorted by
                    // (layer, sort key), and painted in this single layer:
                    // the per-line layers that kept crossings clean are
                    // gone; a border cutting through a crossing is the
                    // intended look now.
                    let mut parts: Vec<EdgePart> = Vec::new();

                    let status_colors = NodeStatusColors::from_theme(cx.theme().status());
                    // Each edge: up to three parts — the bottom stub (with
                    // its bend curve) at the child row, the vertical in
                    // `column`, the top stub (with its bend curve) at the
                    // parent row. Stubs attach below the child node and
                    // above the parent node (the natural flow direction),
                    // hug the nodes, and run to the node centers: the nodes paint on top
                    // with background halos, so nothing needs to be split or inset around
                    // a glyph anymore.
                    for edge in &edge_geoms {
                        let col_x = lane_center_x(bounds, edge.column as f32);
                        let child_x = lane_center_x(bounds, edge.child_col as f32);
                        let parent_x = lane_center_x(bounds, edge.parent_col as f32);
                        // Stubs are tangential to the node's halo from the
                        // inside: the line's outer edge touches the halo
                        // circle (offset = halo radius − half line width).
                        let line_half = LINE_WIDTH / 2.0;
                        let b_y = row_y(edge.child_row)
                            + row_clearance
                                .get(edge.child_row)
                                .copied()
                                .unwrap_or(EDGE_STUB_OFFSET)
                            - line_half;
                        let t_y = row_y(edge.parent_row)
                            - row_clearance
                                .get(edge.parent_row)
                                .copied()
                                .unwrap_or(EDGE_STUB_OFFSET)
                            + line_half;

                        let reach = edge.reach();
                        // Rounded bend radius, as the old per-lane renderer
                        // used: a third of a row, capped at half the vertical
                        // span so both bends always fit.
                        let curve_r = (row_height / 3.0).min((t_y - b_y) / 2.0).max(px(0.0));

                        if child_x != col_x {
                            let dir = (col_x - child_x).signum();
                            let bend_x = col_x - dir * curve_r;
                            let mut lane = LaneEdge::new();
                            lane.move_to(point(child_x, b_y));
                            lane.line_to(point(bend_x, b_y));
                            lane.curve_to(point(col_x, b_y + curve_r), point(col_x, b_y));
                            parts.push(EdgePart {
                                layer: LAYER_BT,
                                sort_key: reach,
                                lane,
                                color_idx: edge.color_idx,
                            });
                        }

                        // The vertical overlaps its bends by half a pixel:
                        // abutting butt caps leave an anti-aliasing seam.
                        let v_start = if child_x != col_x {
                            b_y + curve_r - px(0.5)
                        } else {
                            b_y
                        };
                        let v_end = if parent_x != col_x {
                            t_y - curve_r + px(0.5)
                        } else {
                            t_y
                        };
                        if v_start != v_end {
                            let mut lane = LaneEdge::new();
                            lane.move_to(point(col_x, v_start));
                            lane.line_to(point(col_x, v_end));
                            parts.push(EdgePart {
                                layer: LAYER_V,
                                sort_key: edge.column,
                                lane,
                                color_idx: edge.color_idx,
                            });
                        }

                        if parent_x != col_x {
                            let dir = (parent_x - col_x).signum();
                            let bend_x = col_x + dir * curve_r;
                            let mut lane = LaneEdge::new();
                            lane.move_to(point(col_x, t_y - curve_r));
                            lane.curve_to(point(bend_x, t_y), point(col_x, t_y));
                            lane.line_to(point(parent_x, t_y));
                            parts.push(EdgePart {
                                layer: LAYER_BT,
                                sort_key: reach,
                                lane,
                                color_idx: edge.color_idx,
                            });
                        }
                    }

                    // Verticals paint before stubs; within a layer the sort
                    // key is descending — wider column, longer reach first —
                    // so the shorter/inner fragments end up in front.
                    parts.sort_by(|a, b| {
                        a.layer
                            .cmp(&b.layer)
                            .then_with(|| b.sort_key.cmp(&a.sort_key))
                    });

                    for EdgePart {
                        lane, color_idx, ..
                    } in parts
                    {
                        let line_color = accent_colors.color_for_index(color_idx as u32);
                        if let (Ok(border), Ok(fill)) = (lane.border.build(), lane.fill.build()) {
                            window.paint_path(border, border_color);
                            window.paint_path(fill, line_color);
                        }
                    }

                    // Nodes paint on top of the edges: a background-colored
                    // halo behind each glyph gives it a bit of space and
                    // masks the stub ends that run into it.
                    let halo_radius = |glyph: JjNodeGlyph| match glyph {
                        JjNodeGlyph::WorkingCopy | JjNodeGlyph::Hidden => {
                            JJ_GLYPH_CLEARANCE + EDGE_BORDER_WIDTH
                        }
                        _ => JJ_NODE_RADIUS + EDGE_BORDER_WIDTH,
                    };
                    for (row_idx, (row, flags)) in
                        rows.into_iter().zip(visible_flags.iter()).enumerate()
                    {
                        let lane_color = accent_colors.color_for_index(row.color_idx as u32);
                        let row_y_center =
                            bounds.origin.y + row_idx as f32 * row_height + row_height / 2.0
                                - vertical_scroll_offset;

                        let commit_x = lane_center_x(bounds, row.lane as f32);

                        let glyph = elided_glyph(flags);
                        let color = node_color(glyph, lane_color, &status_colors);
                        let halo = halo_radius(glyph);
                        // A private layer per node: gpui batches primitives
                        // by type within a layer (quads and paths draw in
                        // separate passes), so paint order alone doesn't put
                        // the glyphs above the edge paths — a layer per node
                        // does. The halo is a path circle (not a quad) so it
                        // centers exactly on the glyph — quads snap their
                        // bounds to the device grid and sat off-center.
                        let pad = px(2.0);
                        let node_bounds = gpui::Bounds::new(
                            point(commit_x - halo - pad, row_y_center - halo - pad),
                            gpui::Size {
                                width: (halo + pad) * 2.0,
                                height: (halo + pad) * 2.0,
                            },
                        );
                        window.paint_layer(node_bounds, |window| {
                            let mut halo_builder = gpui::PathBuilder::fill();
                            append_fill_circle(
                                &mut halo_builder,
                                point(commit_x, row_y_center),
                                halo,
                            );
                            if let Ok(path) = halo_builder.build() {
                                window.paint_path(path, border_color);
                            }
                            draw_jj_node(glyph, flags, commit_x, row_y_center, color, window, cx);
                        });
                    }
                })
            },
        )
        .w(graph_width)
        .h_full()
    }
}

/// Formats a unix timestamp for display, mirroring GitGraph's local
/// `format_timestamp`.
fn format_timestamp(timestamp: i64) -> String {
    let Ok(datetime) = OffsetDateTime::from_unix_timestamp(timestamp) else {
        return "Unknown".to_string();
    };

    let local_offset = UtcOffset::current_local_offset().unwrap_or(UtcOffset::UTC);
    let local_datetime = datetime.to_offset(local_offset);

    local_datetime
        .format(timestamp_format())
        .unwrap_or_default()
}

/// The `(elided revisions)` rows the graph needs: one directly after each
/// entry that has a parent change id outside the emitted window (its parent
/// never appears in the log), even when several parents are missing — they
/// all land on that one row. Real entries keep their order and content; the
/// synthetic rows carry a fresh `elided-{n}` change id (real change ids are
/// base-62 and never match), the `elided` flag, and no text of their own.
///
/// Must run before `JjGraphData::from_entries`: the layout lands dangling
/// edges on these rows, and the stored entry list must be the synthesized
/// one, or the table and the canvas disagree on row indices.
/// Reorders the entries into jj's `TopoGroupedGraph` display order — a
/// synchronous port of jj v0.45.1's `TopoGroupedGraph` (core/src/graph.rs),
/// which `jj log` runs over the revset before rendering. Zed's raw revset
/// order is topologically valid but interleaves branches arbitrarily; feeding
/// it straight to the lane layout makes lanes stay occupied across huge row
/// gaps and the graph sprawls. The port keeps jj's exact tie-breaking so the
/// panel's row order (and thus the lane assignment) matches `jj log`:
///
/// - DFS from heads: when a node is emitted, its in-window parents whose last
///   child this was are pushed onto a LIFO stack — at a merge, the *last*
///   parent's branch is walked first.
/// - Heads discovered while populating are queued, never emitted eagerly; a
///   head starts only once the stack runs dry, and `flush_new_head` picks the
///   queued head that actually unblocks the waiting ancestors.
/// - Nodes populate lazily from the input so head-discovery order matches
///   jj's, even though the whole window is already in memory.
fn topo_group_entries(entries: Vec<JjLogEntry>) -> Vec<JjLogEntry> {
    if entries.len() < 2 {
        return entries;
    }
    let in_window: HashSet<SharedString> = entries
        .iter()
        .map(|entry| entry.change_id.clone())
        .collect();

    struct Node {
        /// Graph nodes which must be emitted before this one.
        child_ids: HashSet<SharedString>,
        /// `None` until this node is populated from the input.
        item: Option<JjLogEntry>,
    }

    impl Default for Node {
        fn default() -> Self {
            Self {
                child_ids: HashSet::new(),
                item: None,
            }
        }
    }

    let mut nodes: HashMap<SharedString, Node> = HashMap::new();
    let mut emittable_ids: Vec<SharedString> = Vec::new();
    let mut new_head_ids: VecDeque<SharedString> = VecDeque::new();
    let mut blocked_ids: HashSet<SharedString> = HashSet::new();
    let mut input = entries.into_iter();
    let mut out = Vec::new();

    // Reference `populate_one`: pull one entry, register it in each
    // in-window parent's child set (creating the parent placeholder), then
    // either fill a placeholder or queue a new head.
    // Returns false when the input is exhausted.
    fn populate_one(
        input: &mut std::vec::IntoIter<JjLogEntry>,
        nodes: &mut HashMap<SharedString, Node>,
        new_head_ids: &mut VecDeque<SharedString>,
        in_window: &HashSet<SharedString>,
    ) -> bool {
        let Some(entry) = input.next() else {
            return false;
        };
        let id = entry.change_id.clone();
        for parent in &entry.parent_change_ids {
            if in_window.contains(parent) {
                nodes
                    .entry(parent.clone())
                    .or_default()
                    .child_ids
                    .insert(id.clone());
            }
        }
        match nodes.get_mut(&id) {
            Some(node) => {
                debug_assert!(node.item.is_none());
                node.item = Some(entry);
            }
            None => {
                nodes.insert(
                    id.clone(),
                    Node {
                        child_ids: HashSet::new(),
                        item: Some(entry),
                    },
                );
                new_head_ids.push_back(id);
            }
        }
        true
    }

    // Reference `flush_new_head`: enqueue the first queued head that will
    // unblock the waiting ancestors.
    let flush_new_head = |nodes: &mut HashMap<SharedString, Node>,
                          new_head_ids: &mut VecDeque<SharedString>,
                          blocked_ids: &mut HashSet<SharedString>,
                          emittable_ids: &mut Vec<SharedString>| {
        if blocked_ids.is_empty() || new_head_ids.len() <= 1 {
            // Fast path: orphaned or no choice.
            let new_head_id = new_head_ids.pop_front().unwrap();
            emittable_ids.push(new_head_id);
            blocked_ids.clear();
            return;
        }

        // Mark descendant nodes reachable from the blocking nodes.
        let mut to_visit: Vec<SharedString> = blocked_ids
            .iter()
            .filter(|id| nodes.contains_key(*id))
            .cloned()
            .collect();
        let mut visited: HashSet<SharedString> = to_visit.iter().cloned().collect();
        while let Some(id) = to_visit.pop() {
            if let Some(node) = nodes.get(&id) {
                to_visit.extend(
                    node.child_ids
                        .iter()
                        .filter(|id| visited.insert((*id).clone()))
                        .cloned(),
                );
            }
        }

        // Pick the first reachable head.
        let index = new_head_ids
            .iter()
            .position(|id| visited.contains(id))
            .unwrap_or_else(|| {
                // The blocking head should exist; fall back to the oldest
                // queued head rather than panicking on unexpected input.
                0
            });
        let new_head_id = new_head_ids.remove(index).unwrap();

        // Unmark ancestors of the selected head so they don't contribute to
        // future new-head resolution within the newly-unblocked subgraph.
        let mut to_visit = vec![new_head_id.clone()];
        visited.remove(&new_head_id);
        while let Some(id) = to_visit.pop() {
            if let Some(node) = nodes.get(&id) {
                if let Some(item) = &node.item {
                    to_visit.extend(
                        item.parent_change_ids
                            .iter()
                            .filter(|id| visited.remove(*id))
                            .cloned(),
                    );
                }
            }
        }
        blocked_ids.retain(|id| visited.contains(id));
        emittable_ids.push(new_head_id);
    };

    loop {
        if let Some(current_id) = emittable_ids.last().cloned() {
            let Some(current_node) = nodes.get_mut(&current_id) else {
                // Queued twice because new children populated and emitted.
                emittable_ids.pop();
                continue;
            };
            if !current_node.child_ids.is_empty() {
                // New children populated after emitting the other branch.
                let current_id = emittable_ids.pop().unwrap();
                blocked_ids.insert(current_id);
                continue;
            }
            let Some(item) = current_node.item.take() else {
                // Not yet populated.
                if !populate_one(&mut input, &mut nodes, &mut new_head_ids, &in_window) {
                    // Input exhausted with an unpopulated placeholder: the
                    // parent never appears in the window. Drop it and move
                    // on instead of panicking like the reference does.
                    emittable_ids.pop();
                }
                continue;
            };
            // The second (or the last) parent will be visited first.
            emittable_ids.pop();
            nodes.remove(&current_id);
            for parent in &item.parent_change_ids {
                if !in_window.contains(parent) {
                    continue;
                }
                let parent_node = nodes.get_mut(parent).unwrap();
                parent_node.child_ids.remove(&current_id);
                if parent_node.child_ids.is_empty() {
                    let reusable_id = blocked_ids.take(parent);
                    emittable_ids.push(reusable_id.unwrap_or_else(|| parent.clone()));
                } else {
                    blocked_ids.insert(parent.clone());
                }
            }
            out.push(item);
        } else if !new_head_ids.is_empty() {
            flush_new_head(
                &mut nodes,
                &mut new_head_ids,
                &mut blocked_ids,
                &mut emittable_ids,
            );
        } else if !populate_one(&mut input, &mut nodes, &mut new_head_ids, &in_window) {
            return out;
        }
    }
}

fn synthesize_elided_entries(entries: Vec<JjLogEntry>) -> Vec<JjLogEntry> {
    // Owned: the loop below moves `entries`, so the window's id set cannot
    // borrow from it.
    let emitted: HashSet<String> = entries
        .iter()
        .map(|entry| entry.change_id.to_string())
        .collect();
    let mut rows = Vec::with_capacity(entries.len());
    let mut elided_count = 0;
    for entry in entries {
        let dangling = !entry.flags.elided
            && entry
                .parent_change_ids
                .iter()
                .any(|parent| !emitted.contains(parent.as_str()));
        rows.push(entry);
        if dangling {
            rows.push(elided_entry(elided_count));
            elided_count += 1;
        }
    }
    rows
}

/// The `n`-th synthetic `(elided revisions)` row.
fn elided_entry(index: usize) -> JjLogEntry {
    JjLogEntry {
        change_id: format!("elided-{index}").into(),
        commit_id: SharedString::default(),
        parents: Vec::new(),
        parent_change_ids: Vec::new(),
        bookmarks: Vec::new(),
        tags: Vec::new(),
        description: SharedString::default(),
        author_name: SharedString::default(),
        author_email: SharedString::default(),
        commit_timestamp: 0,
        flags: JjLogFlags {
            elided: true,
            ..Default::default()
        },
        is_merge: false,
        is_head: false,
        dist_to_head: None,
    }
}

/// A row's node glyph: the synthetic elided rows render as the hidden wave
/// mark — `node_glyph` (in `jj_graph`, layout-only) doesn't know the
/// `elided` flag, so the panel maps it here.
fn elided_glyph(flags: &JjLogFlags) -> JjNodeGlyph {
    if flags.elided {
        JjNodeGlyph::Hidden
    } else {
        node_glyph(flags)
    }
}

impl Render for JjLog {
    fn render(&mut self, window: &mut Window, cx: &mut Context<Self>) -> AnyElement {
        let entries = self.entries.clone();
        let item_count = entries.len();
        let revset_editor = if let Some(editor) = self.revset_editor.clone() {
            editor
        } else {
            let editor = cx.new(|cx| {
                let mut editor = Editor::single_line(window, cx);
                editor.set_placeholder_text("revset (e.g. ::trunk)", window, cx);
                editor
            });
            self.revset_editor = Some(editor.clone());
            editor
        };
        let search_bar = self.render_search_bar(revset_editor, cx);
        // A failed poll (e.g. an invalid revset) keeps the last good rows
        // visible and surfaces the error as a banner; the search bar stays
        // usable so the revset can be corrected.
        let error_banner = self.error.as_ref().map(|error| {
            div()
                .w_full()
                .px_2()
                .py_1()
                .child(Label::new(error.clone()).color(Color::Warning).truncate())
                .into_any_element()
        });

        let root = if item_count == 0 {
            let message = if self.loading {
                "Loading…".to_string()
            } else if let Some(error) = self.error.as_deref() {
                format!("Error loading revset: {error}")
            } else {
                "no jj repository".to_string()
            };
            v_flex()
                .flex_1()
                .min_w_0()
                .size_full()
                .child(search_bar)
                .children(error_banner)
                .child(
                    v_flex()
                        .flex_1()
                        .items_center()
                        .justify_center()
                        .child(Label::new(message).color(Color::Muted)),
                )
                .children(self.context_menu.as_ref().map(|context_menu| {
                    deferred(
                        anchored()
                            .position(context_menu.position)
                            .anchor(Anchor::TopLeft)
                            .child(context_menu.menu.clone()),
                    )
                    .with_priority(1)
                }))
                .into_any_element()
        } else {
            let Some(graph_data) = self.graph_data.as_ref() else {
                return v_flex()
                    .size_full()
                    .p_2()
                    .child(Label::new("no jj repository").color(Color::Muted))
                    .into_any_element();
            };

            let row_height = Self::row_height(window, cx);
            // The engine reports its widest lane layout; GitGraph floors the
            // graph at 6 lanes wide.
            let lane_count = graph_data.max_lanes.max(6);
            let graph_width = LANE_WIDTH * lane_count as f32 + LEFT_PADDING * 2.0;
            // Per-commit lane colors for the bookmark chips; cloned so the row
            // closure can own them (it cannot borrow `self`).
            let color_idxs: Vec<usize> = graph_data
                .commits
                .iter()
                .map(|commit| commit.color_idx)
                .collect();

            // Column layout: GitGraph's redistributable columns — widths are
            // draggable via the resize handles and columns toggle from the
            // header's right-click context menu.
            let [
                graph_fraction,
                description_fraction,
                date_fraction,
                author_fraction,
                commit_fraction,
            ] = self.column_fractions(window, cx);
            let table_fraction =
                description_fraction + date_fraction + author_fraction + commit_fraction;
            let table_collapsed = table_fraction <= f32::EPSILON;
            let table_width_config = ColumnWidthConfig::explicit(vec![
                DefiniteLength::Fraction(description_fraction / table_fraction.max(f32::EPSILON)),
                DefiniteLength::Fraction(date_fraction / table_fraction.max(f32::EPSILON)),
                DefiniteLength::Fraction(author_fraction / table_fraction.max(f32::EPSILON)),
                DefiniteLength::Fraction(commit_fraction / table_fraction.max(f32::EPSILON)),
            ]);
            let table_filter = TableRow::from_vec(
                self.column_visibility
                    .as_slice()
                    .get(1..5)
                    .unwrap_or(&[])
                    .to_vec(),
                4,
            );
            let header_resize_info =
                HeaderResizeInfo::from_redistributable(&self.column_widths, cx);
            let header_widths = redistribute_hidden_widths(
                &self.column_widths.read(cx).widths_to_render(),
                Some(&self.column_visibility),
            );
            let header_context = TableRenderContext::for_column_widths(Some(header_widths), true)
                .with_column_filter(Some(self.column_visibility.clone()));
            let graph_visible = !self
                .column_visibility
                .as_slice()
                .first()
                .copied()
                .unwrap_or(false);

            v_flex()
            .flex_1()
            .min_w_0()
            .size_full()
            .child(search_bar)
            .children(error_banner)
            .child(
                div()
                    .on_mouse_down(
                        MouseButton::Right,
                        cx.listener(|this, event: &MouseDownEvent, window, cx| {
                            this.deploy_header_context_menu(event.position, window, cx);
                            cx.stop_propagation();
                        }),
                    )
                    .child(render_table_header(
                        TableRow::from_vec(
                            vec![
                                Label::new("Graph")
                                    .color(Color::Muted)
                                    .truncate()
                                    .into_any_element(),
                                Label::new("Description")
                                    .color(Color::Muted)
                                    .truncate()
                                    .into_any_element(),
                                Label::new("Date")
                                    .color(Color::Muted)
                                    .truncate()
                                    .into_any_element(),
                                Label::new("Author")
                                    .color(Color::Muted)
                                    .truncate()
                                    .into_any_element(),
                                Label::new("Commit")
                                    .color(Color::Muted)
                                    .truncate()
                                    .into_any_element(),
                            ],
                            5,
                        ),
                        header_context,
                        Some(header_resize_info),
                        Some(self.column_widths.entity_id()),
                        cx,
                    )),
            )
            .child(bind_redistributable_columns(
                div()
                    .relative()
                    .flex_1()
                    .w_full()
                    .overflow_hidden()
                    .child(
                        h_flex()
                            .size_full()
                            .when(graph_visible, |this| {
                                this.child(
                                    div()
                                        .id("jj-log-graph")
                                        .map(|this| {
                                            if table_collapsed {
                                                this.w(graph_width)
                                            } else {
                                                this.w(DefiniteLength::Fraction(graph_fraction))
                                            }
                                        })
                                        .h_full()
                                        .min_w_0()
                                        .overflow_hidden()
                                        .on_scroll_wheel(cx.listener(Self::handle_graph_scroll))
                                        .child(div().size_full().child(self.render_graph_canvas(
                                            graph_data,
                                            row_height,
                                            graph_width,
                                            window,
                                            cx,
                                        ))),
                                )
                            })
                            .child(
                                div()
                                    .map(|this| {
                                        if table_collapsed {
                                            this.flex_1()
                                        } else {
                                            this.w(DefiniteLength::Fraction(table_fraction))
                                        }
                                    })
                                    .h_full()
                                    .min_w_0()
                                    .child(
                                        Table::new(4)
                                            .interactable(&self.table_interaction_state)
                                            .hide_row_borders()
                                            .hide_row_hover()
                                            .width_config(table_width_config)
                                            .column_filter(table_filter)
                                            // Pin the row height to the value
                                            // captured at render time so the
                                            // canvas dots and the text rows
                                            // share one geometry, exactly like
                                            // GitGraph's map_row.
                                            .map_row(move |(_, row), _window, _cx| {
                                                row.h(row_height).into_any_element()
                                            })
                                            .uniform_list(
                                                "jj-log-rows",
                                                item_count,
                                                move |range, _window, cx| {
                                                    let accent_colors = cx.theme().accents();
                                                    range
                                                        .map(|idx| {
                                                            let entry = &entries[idx];
                                                            let elided = entry.flags.elided;
                                                            let accent_color = accent_colors
                                                                .0
                                                                .get(color_idxs[idx])
                                                                .copied()
                                                                .unwrap_or_default();
                                                            let change_id =
                                                                entry.change_id.to_string();
                                                            let change_short = change_id
                                                                [..change_id.len().min(8)]
                                                                .to_string();
                                                            let description = entry
                                                                .description
                                                                .lines()
                                                                .next()
                                                                .unwrap_or("");
                                                            let has_description =
                                                                !description.is_empty();
                                                            let timestamp = format_timestamp(
                                                                entry.commit_timestamp,
                                                            );
                                                            let column_label =
                                                                |label: SharedString| {
                                                                    Label::new(label)
                                                                        .color(Color::Muted)
                                                                        .truncate()
                                                                        .into_any_element()
                                                                };
                                                            // Description cell with bookmark
                                                            // chips inline, mirroring
                                                            // GitGraph's render_table_rows.
                                                            // Elided rows are the
                                                            // cut-off marker itself:
                                                            // "(elided revisions)", no
                                                            // chips, placeholder, or
                                                            // other text.
                                                            let description_cell = div()
                                                                .overflow_hidden()
                                                                .child(
                                                                    h_flex()
                                                                        .gap_2()
                                                                        .overflow_hidden()
                                                                        .children(
                                                                            (!entry.bookmarks.is_empty())
                                                                                .then(|| {
                                                                                    h_flex()
                                                                                        .gap_1()
                                                                                        .children(
                                                                                            entry.bookmarks.iter().map(|name| {
                                                                                                Chip::new(name.clone())
                                                                                                    .label_size(LabelSize::Small)
                                                                                                    .truncate()
                                                                                                    .tooltip({
                                                                                                        let name = name.clone();
                                                                                                        move |_, cx| {
                                                                                                            Tooltip::simple(name.clone(), cx)
                                                                                                        }
                                                                                                    })
                                                                                                    .bg_color(accent_color.opacity(0.08))
                                                                                                    .border_color(accent_color.opacity(0.25))
                                                                                            }),
                                                                                        )
                                                                                })
                                                                        )
                                                                        .child(
                                                                            Label::new(if elided {
                                                                                "(elided revisions)"
                                                                            } else if has_description {
                                                                                description
                                                                            } else {
                                                                                "(no description set)"
                                                                            })
                                                                            .color(Color::Muted)
                                                                            .truncate(),
                                                                        ),
                                                                )
                                                                .into_any_element();
                                                            // Elided rows carry no
                                                            // date, author, or commit
                                                            // text — the marker is the
                                                            // row.
                                                            let cells = vec![
                                                                description_cell,
                                                                if elided {
                                                                    column_label("".into())
                                                                } else {
                                                                    column_label(timestamp.into())
                                                                },
                                                                if elided {
                                                                    column_label("".into())
                                                                } else {
                                                                    column_label(
                                                                        entry
                                                                            .author_name
                                                                            .to_string()
                                                                            .into(),
                                                                    )
                                                                },
                                                                if elided {
                                                                    column_label("".into())
                                                                } else {
                                                                    column_label(change_short.into())
                                                                },
                                                            ];
                                                            // Hidden commits and the
                                                            // synthesized elided rows
                                                            // read as dimmed: the row
                                                            // is real (jj renders full
                                                            // rows for hidden commits
                                                            // named in the revset)
                                                            // but faded like jj's
                                                            // dimmed text.
                                                            if entry.flags.hidden || elided {
                                                                cells
                                                                    .into_iter()
                                                                    .map(|cell| {
                                                                        div().opacity(0.4)
                                                                            .child(cell)
                                                                            .into_any_element()
                                                                    })
                                                                    .collect()
                                                            } else {
                                                                cells
                                                            }
                                                        })
                                                        .collect()
                                                },
                                            ),
                                    ),
                            ),
                    )
                    .child(render_redistributable_columns_resize_handles(
                        &self.column_widths,
                        Some(&self.column_visibility),
                        window,
                        cx,
                    )),
                self.column_widths.clone(),
                Some(self.column_visibility.clone()),
            ))
            .children(self.context_menu.as_ref().map(|context_menu| {
                deferred(
                    anchored()
                        .position(context_menu.position)
                        .anchor(Anchor::TopLeft)
                        .child(context_menu.menu.clone()),
                )
                .with_priority(1)
            }))
            .into_any_element()
        };
        root
    }
}

actions!(
    jj_log,
    [
        /// Opens the JJ log panel.
        OpenJjLog,
    ]
);

/// Registers the JJ log panel and its open action.
pub fn init(cx: &mut App) {
    workspace::register_serializable_item::<JjLog>(cx);

    cx.observe_new(|workspace: &mut workspace::Workspace, _, _| {
        workspace.register_action_renderer(|div, workspace, _, _| {
            let workspace = workspace.weak_handle();

            div.on_action(move |_: &OpenJjLog, window, cx| {
                workspace
                    .update(cx, |workspace, cx| {
                        let git_store = workspace.project().read(cx).git_store().clone();
                        open_jj_log(workspace, git_store, window, cx);
                    })
                    .ok();
            })
        });
    })
    .detach();
}

/// Opens the JJ log panel, reusing the one already open if present.
pub fn open_jj_log(
    workspace: &mut Workspace,
    git_store: Entity<GitStore>,
    window: &mut Window,
    cx: &mut Context<Workspace>,
) {
    let existing = workspace.items_of_type::<JjLog>(cx).next();
    if let Some(existing) = existing {
        workspace.activate_item(&existing, true, true, window, cx);
    } else {
        let jj_log = cx.new(|cx| JjLog::new(git_store, cx));
        workspace.add_item_to_active_pane(Box::new(jj_log.clone()), None, true, window, cx);
    }
}
impl Focusable for JjLog {
    fn focus_handle(&self, _cx: &App) -> FocusHandle {
        self.focus_handle.clone()
    }
}

impl EventEmitter<ItemEvent> for JjLog {}

impl Item for JjLog {
    type Event = ItemEvent;

    fn tab_content_text(&self, _detail: usize, _cx: &App) -> SharedString {
        "JJ Log".into()
    }
}

impl SerializableItem for JjLog {
    fn serialized_item_kind() -> &'static str {
        "JjLog"
    }

    fn cleanup(
        _: workspace::WorkspaceId,
        _: Vec<workspace::ItemId>,
        _: &mut Window,
        _: &mut App,
    ) -> Task<Result<()>> {
        Task::ready(Ok(()))
    }

    fn deserialize(
        project: Entity<project::Project>,
        _workspace: WeakEntity<Workspace>,
        workspace_id: workspace::WorkspaceId,
        item_id: workspace::ItemId,
        _window: &mut Window,
        cx: &mut App,
    ) -> Task<Result<Entity<Self>>> {
        let git_store = project.read(cx).git_store().clone();
        let saved = persistence::JjLogDb::global(cx)
            .get_jj_log(item_id, workspace_id)
            .ok()
            .flatten();
        Task::ready(Ok(cx.new(|cx| {
            let mut this = JjLog::new(git_store, cx);
            if let Some(Some(current_revset)) = saved {
                this.current_revset = Some(current_revset);
            }
            this
        })))
    }

    fn serialize(
        &mut self,
        workspace: &mut Workspace,
        item_id: workspace::ItemId,
        _: bool,
        cx: &mut Context<Self>,
    ) -> Option<Task<Result<()>>> {
        let workspace_id = workspace.database_id()?;
        let current_revset = self.current_revset.clone();
        let db = persistence::JjLogDb::global(cx);
        Some(cx.background_spawn(async move {
            db.save_jj_log(item_id, workspace_id, current_revset).await
        }))
    }

    fn should_serialize(&self, event: &Self::Event) -> bool {
        matches!(event, ItemEvent::UpdateTab | ItemEvent::Edit)
    }
}

mod persistence {
    use db::{
        query,
        sqlez::{domain::Domain, thread_safe_connection::ThreadSafeConnection},
        sqlez_macros::sql,
    };
    use workspace::WorkspaceDb;

    pub struct JjLogDb(ThreadSafeConnection);

    impl Domain for JjLogDb {
        const NAME: &str = stringify!(JjLogDb);

        const MIGRATIONS: &[&str] = &[
            sql!(
                CREATE TABLE jj_logs (
                    workspace_id INTEGER,
                    item_id INTEGER UNIQUE,
                    is_open INTEGER DEFAULT FALSE,

                    PRIMARY KEY(workspace_id, item_id),
                    FOREIGN KEY(workspace_id) REFERENCES workspaces(workspace_id)
                    ON DELETE CASCADE
                ) STRICT;
            ),
            sql!(
                ALTER TABLE jj_logs ADD COLUMN current_revset TEXT;
            ),
        ];
    }

    db::static_connection!(JjLogDb, [WorkspaceDb]);

    impl JjLogDb {
        query! {
            pub async fn save_jj_log(
                item_id: workspace::ItemId,
                workspace_id: workspace::WorkspaceId,
                current_revset: Option<String>
            ) -> Result<()> {
                INSERT OR REPLACE INTO jj_logs(
                    item_id, workspace_id, current_revset
                )
                VALUES (?, ?, ?)
            }
        }

        query! {
            pub fn get_jj_log(
                item_id: workspace::ItemId,
                workspace_id: workspace::WorkspaceId
            ) -> Result<Option<Option<String>>> {
                SELECT current_revset
                FROM jj_logs
                WHERE item_id = ? AND workspace_id = ?
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Acceptance check against real fixture data: the topo-grouped row
    /// order must equal `jj log`'s own rendering order. The reference files
    /// are produced by the diagnostic session (out.jsonl = the exact JSONL
    /// Zed's CLI call emits for the fixture's default revset; order_graph.txt
    /// = the row order of `jj log --graph` for the same revset, one
    /// change-id prefix per line). Skipped when the files are absent so the
    /// suite stays portable.
    #[test]
    fn topo_order_matches_jj_log_on_real_fixture_data() {
        let jsonl_path = "/home/emil/mono/.tmp/out.jsonl";
        let order_path = "/home/emil/mono/.tmp/order_graph.txt";
        let (Ok(jsonl), Ok(order)) = (
            std::fs::read_to_string(jsonl_path),
            std::fs::read_to_string(order_path),
        ) else {
            eprintln!("skipping: reference files not present");
            return;
        };

        #[derive(serde::Deserialize)]
        struct Line {
            commit: Commit,
            #[serde(rename = "parent_change_ids")]
            parent_change_ids: Vec<String>,
        }
        #[derive(serde::Deserialize)]
        struct Commit {
            change_id: String,
        }
        let entries: Vec<JjLogEntry> = jsonl
            .lines()
            .filter(|line| !line.trim().is_empty())
            .map(|line| {
                let line: Line = serde_json::from_str(line).unwrap();
                JjLogEntry {
                    change_id: line.commit.change_id.into(),
                    commit_id: SharedString::default(),
                    parents: Vec::new(),
                    parent_change_ids: line
                        .parent_change_ids
                        .into_iter()
                        .map(SharedString::from)
                        .collect(),
                    bookmarks: Vec::new(),
                    tags: Vec::new(),
                    description: SharedString::default(),
                    author_name: SharedString::default(),
                    author_email: SharedString::default(),
                    commit_timestamp: 0,
                    flags: JjLogFlags::default(),
                    is_merge: false,
                    is_head: false,
                    dist_to_head: None,
                }
            })
            .collect();
        let expected: Vec<String> = order
            .lines()
            .map(|line| line.trim().to_string())
            .filter(|line| !line.is_empty())
            .collect();

        let grouped = topo_group_entries(entries.clone());
        let actual: Vec<String> = grouped
            .iter()
            .map(|entry| entry.change_id.to_string().chars().take(12).collect())
            .collect();
        assert_eq!(
            actual, expected,
            "topo-grouped row order must match jj log's rendering order"
        );

        // Detour diagnostic: an edge whose child and parent sit in the same
        // column but whose vertical runs in a different one makes an
        // unnecessary right-hand detour. Print any offenders with context.
        let rows = synthesize_elided_entries(topo_group_entries(entries));
        let graph = JjGraphData::from_entries(&rows, 8);
        for (idx, edge) in graph.edges.iter().enumerate() {
            let lo = edge.child_col.min(edge.parent_col);
            let hi = edge.child_col.max(edge.parent_col);
            if edge.column > hi || edge.column < lo {
                let child = rows
                    .get(edge.child_row)
                    .map(|e| format!("{} '{}'", e.change_id, e.description))
                    .unwrap_or_default();
                let parent = rows
                    .get(edge.parent_row)
                    .map(|e| format!("{} '{}'", e.change_id, e.description))
                    .unwrap_or_default();
                eprintln!(
                    "detour edge #{idx}: child {child} (row {}, col {}) -> parent {parent} (row {}, col {}), column {}",
                    edge.child_row, edge.child_col, edge.parent_row, edge.parent_col, edge.column
                );
            }
        }
    }

    fn test_entry(change_id: &str, parents: &[&str]) -> JjLogEntry {
        JjLogEntry {
            change_id: change_id.into(),
            commit_id: format!("commit-{change_id}").into(),
            parents: Vec::new(),
            parent_change_ids: parents.iter().map(|parent| (*parent).into()).collect(),
            bookmarks: Vec::new(),
            tags: Vec::new(),
            description: SharedString::default(),
            author_name: SharedString::default(),
            author_email: SharedString::default(),
            commit_timestamp: 0,
            flags: JjLogFlags::default(),
            is_merge: parents.len() > 1,
            is_head: false,
            dist_to_head: None,
        }
    }

    #[test]
    fn synthesis_inserts_an_elided_row_after_each_dangling_entry() {
        // The C1 fixture: @ merges two branches, each of whose next row has
        // its parent outside the window. One elided row per dangling entry,
        // directly after it — 6 rows, elided at indices 2 and 4.
        let mut at = test_entry("kozmttlk", &["rzvnrvrz", "xpzmvpqp"]);
        at.flags.working_copy = true;
        let entries = vec![
            at,
            test_entry("xpzmvpqp", &["uktnvvqq"]),
            test_entry("rzvnrvrz", &["rz-out"]),
            test_entry("root", &[]),
        ];
        let rows = synthesize_elided_entries(entries);
        let ids: Vec<&str> = rows.iter().map(|row| row.change_id.as_str()).collect();
        assert_eq!(
            ids,
            vec![
                "kozmttlk", "xpzmvpqp", "elided-0", "rzvnrvrz", "elided-1", "root"
            ]
        );
        for (index, row) in rows.iter().enumerate() {
            assert_eq!(row.flags.elided, index == 2 || index == 4);
        }
        // Synthetic rows carry no text of their own.
        for row in [&rows[2], &rows[4]] {
            assert!(row.description.is_empty());
            assert!(row.author_name.is_empty());
            assert_eq!(row.commit_timestamp, 0);
            assert!(row.parent_change_ids.is_empty());
            assert!(row.bookmarks.is_empty());
        }
    }

    #[test]
    fn synthesis_leaves_a_complete_window_alone() {
        // Every parent present: no elided rows, order untouched.
        let entries = vec![
            test_entry("c", &["b"]),
            test_entry("b", &["a"]),
            test_entry("a", &[]),
        ];
        let rows = synthesize_elided_entries(entries);
        assert_eq!(rows.len(), 3);
        assert!(rows.iter().all(|row| !row.flags.elided));
        let ids: Vec<&str> = rows.iter().map(|row| row.change_id.as_str()).collect();
        assert_eq!(ids, vec!["c", "b", "a"]);
    }

    #[test]
    fn synthesis_elides_the_last_rows_dangling_parent() {
        // The oldest emitted entry's parent is outside the window: the
        // dangling edge still gets its elided row.
        let entries = vec![
            test_entry("b", &["outside"]),
            test_entry("a", &["outside-too"]),
        ];
        let rows = synthesize_elided_entries(entries);
        assert_eq!(rows.len(), 4);
        assert!(rows[1].flags.elided && rows[3].flags.elided);
        assert!(!rows[0].flags.elided && !rows[2].flags.elided);
    }

    #[test]
    fn synthesis_uses_one_row_for_several_missing_parents() {
        let entries = vec![test_entry("m", &["out-1", "out-2"]), test_entry("a", &[])];
        let rows = synthesize_elided_entries(entries);
        assert_eq!(rows.len(), 3);
        assert!(rows[1].flags.elided);
        assert!(!rows[0].flags.elided && !rows[2].flags.elided);
    }

    #[test]
    fn edge_reach_is_the_widest_column_touched() {
        let geom = EdgeGeom {
            child_row: 0,
            parent_row: 3,
            column: 1,
            child_col: 4,
            parent_col: 0,
            color_idx: 0,
        };
        assert_eq!(geom.reach(), 4);
    }
}
