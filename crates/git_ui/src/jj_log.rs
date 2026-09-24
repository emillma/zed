use crate::git_graph::{
    CommitLineSegment, CurveKind, LANE_WIDTH, LEFT_PADDING, LINE_WIDTH, accent_colors_count,
    lane_center_x, timestamp_format, to_row_center,
};
use crate::jj_graph::{
    JJ_GLYPH_CLEARANCE, JJ_NODE_RADIUS, JJ_NODE_STROKE_WIDTH, JjGraphData, JjNodeGlyph,
    NodeStatusColors, draw_jj_node, node_color, node_glyph,
};
use crate::jj_settings::JjSettings;
use anyhow::Result;
use editor::Editor;
use git::jj::JjLogEntry;
use gpui::{
    Anchor, AnyElement, App, Bounds, Context, DefiniteLength, DismissEvent, Entity, EventEmitter,
    FocusHandle, Focusable, MouseButton, MouseDownEvent, PathBuilder, Pixels, Point, Render,
    SharedString, Subscription, Task, WeakEntity, Window, actions, anchored, deferred, point, px,
};
use menu::Confirm;
use project::git_store::{GitStore, GitStoreEvent, RepositoryEvent};
use std::collections::BTreeMap;
use std::time::{Duration, Instant};
use time::{OffsetDateTime, UtcOffset};
use ui::{
    Chip, ColumnWidthConfig, ContextMenu, ContextMenuEntry, DocumentationSide, HeaderResizeInfo,
    IconButtonShape, RedistributableColumnsState, Table, TableInteractionState, TableRenderContext,
    TableResizeBehavior, Tooltip, bind_redistributable_columns, prelude::*,
    redistribute_hidden_fractions, redistribute_hidden_widths,
    render_redistributable_columns_resize_handles, render_table_header, table_row::TableRow,
};
use workspace::{
    ModalView, SerializableItem, Workspace,
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

    /// Paints the lane graph over the table's visible rows, synced to the
    /// table's scroll state exactly like GitGraph: rows and lanes are shifted
    /// by the table's scroll offset and only the visible range is painted.
    /// Uses GitGraph's Straight/Curve segments, per-lane accent colors and
    /// solid commit dots.
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
        // the lines don't cross them.
        let row_clearance: Vec<Pixels> = self
            .entries
            .iter()
            .map(|entry| match node_glyph(&entry.flags) {
                JjNodeGlyph::WorkingCopy | JjNodeGlyph::Hidden => JJ_GLYPH_CLEARANCE,
                _ => JJ_NODE_RADIUS,
            })
            .collect();
        let commit_lines: Vec<_> = graph_data
            .lines
            .iter()
            .filter(|line| {
                line.full_interval.start <= viewport_range.end
                    && line.full_interval.end >= viewport_range.start
            })
            .cloned()
            .collect();

        gpui::canvas(
            move |_bounds, _window, _cx| {},
            move |bounds: Bounds<Pixels>, _: (), window: &mut Window, cx: &mut App| {
                window.paint_layer(bounds, |window| {
                    let accent_colors = cx.theme().accents().clone();
                    let mut lines: BTreeMap<usize, Vec<_>> = BTreeMap::new();

                    let status_colors = NodeStatusColors::from_theme(cx.theme().status());
                    for (row_idx, (row, flags)) in
                        rows.into_iter().zip(visible_flags.iter()).enumerate()
                    {
                        let lane_color = accent_colors.color_for_index(row.color_idx as u32);
                        let row_y_center =
                            bounds.origin.y + row_idx as f32 * row_height + row_height / 2.0
                                - vertical_scroll_offset;

                        let commit_x = lane_center_x(bounds, row.lane as f32);

                        let glyph = node_glyph(flags);
                        let color = node_color(glyph, lane_color, &status_colors);
                        draw_jj_node(glyph, flags, commit_x, row_y_center, color, window, cx);
                    }

                    for line in commit_lines {
                        let Some((start_segment_idx, start_column)) =
                            line.get_first_visible_segment_idx(first_visible_row)
                        else {
                            continue;
                        };

                        let line_x = lane_center_x(bounds, start_column as f32);

                        let start_row = line.full_interval.start as i32 - first_visible_row as i32;

                        let from_y =
                            bounds.origin.y + start_row as f32 * row_height + row_height / 2.0
                                - vertical_scroll_offset
                                + row_clearance
                                    .get(line.full_interval.start)
                                    .copied()
                                    .unwrap_or(JJ_NODE_RADIUS);

                        let mut current_row = from_y;
                        let mut current_column = line_x;

                        let mut builder = PathBuilder::stroke(LINE_WIDTH);
                        builder.move_to(point(line_x, from_y));

                        let segments = &line.segments[start_segment_idx..];
                        let desired_curve_height = row_height / 3.0;
                        let desired_curve_width = LANE_WIDTH / 3.0;

                        for (segment_idx, segment) in segments.iter().enumerate() {
                            let is_last = segment_idx + 1 == segments.len();

                            match segment {
                                CommitLineSegment::Straight { to_row } => {
                                    let mut dest_row = to_row_center(
                                        to_row - first_visible_row,
                                        row_height,
                                        vertical_scroll_offset,
                                        bounds,
                                    );
                                    if is_last {
                                        dest_row -= row_clearance
                                            .get(*to_row)
                                            .copied()
                                            .unwrap_or(JJ_NODE_RADIUS);
                                    }

                                    let dest_point = point(current_column, dest_row);

                                    current_row = dest_point.y;
                                    builder.line_to(dest_point);
                                    builder.move_to(dest_point);
                                }
                                CommitLineSegment::Curve {
                                    to_column,
                                    on_row,
                                    curve_kind,
                                } => {
                                    let mut to_column = lane_center_x(bounds, *to_column as f32);

                                    let mut to_row = to_row_center(
                                        *on_row - first_visible_row,
                                        row_height,
                                        vertical_scroll_offset,
                                        bounds,
                                    );

                                    // This means that this branch was a checkout
                                    let going_right = to_column > current_column;
                                    let column_shift = if going_right {
                                        JJ_NODE_RADIUS + JJ_NODE_STROKE_WIDTH
                                    } else {
                                        -JJ_NODE_RADIUS - JJ_NODE_STROKE_WIDTH
                                    };

                                    match curve_kind {
                                        CurveKind::Checkout => {
                                            if is_last {
                                                to_column -= column_shift;
                                            }

                                            let available_curve_width =
                                                (to_column - current_column).abs();
                                            let available_curve_height =
                                                (to_row - current_row).abs();
                                            let curve_width =
                                                desired_curve_width.min(available_curve_width);
                                            let curve_height =
                                                desired_curve_height.min(available_curve_height);
                                            let signed_curve_width = if going_right {
                                                curve_width
                                            } else {
                                                -curve_width
                                            };
                                            let curve_start =
                                                point(current_column, to_row - curve_height);
                                            let curve_end =
                                                point(current_column + signed_curve_width, to_row);
                                            let curve_control = point(current_column, to_row);

                                            builder.move_to(point(current_column, current_row));
                                            builder.line_to(curve_start);
                                            builder.move_to(curve_start);
                                            builder.curve_to(curve_end, curve_control);
                                            builder.move_to(curve_end);
                                            builder.line_to(point(to_column, to_row));
                                        }
                                        CurveKind::Merge => {
                                            if is_last {
                                                to_row -= row_clearance
                                                    .get(*on_row)
                                                    .copied()
                                                    .unwrap_or(JJ_NODE_RADIUS);
                                            }

                                            let merge_start = point(
                                                current_column + column_shift,
                                                current_row - JJ_NODE_RADIUS,
                                            );
                                            let available_curve_width =
                                                (to_column - merge_start.x).abs();
                                            let available_curve_height =
                                                (to_row - merge_start.y).abs();
                                            let curve_width =
                                                desired_curve_width.min(available_curve_width);
                                            let curve_height =
                                                desired_curve_height.min(available_curve_height);
                                            let signed_curve_width = if going_right {
                                                curve_width
                                            } else {
                                                -curve_width
                                            };
                                            let curve_start = point(
                                                to_column - signed_curve_width,
                                                merge_start.y,
                                            );
                                            let curve_end =
                                                point(to_column, merge_start.y + curve_height);
                                            let curve_control = point(to_column, merge_start.y);

                                            builder.move_to(merge_start);
                                            builder.line_to(curve_start);
                                            builder.move_to(curve_start);
                                            builder.curve_to(curve_end, curve_control);
                                            builder.move_to(curve_end);
                                            builder.line_to(point(to_column, to_row));
                                        }
                                    }
                                    current_row = to_row;
                                    current_column = to_column;
                                    builder.move_to(point(current_column, current_row));
                                }
                            }
                        }

                        builder.close();
                        lines.entry(line.color_idx).or_default().push(builder);
                    }

                    for (color_idx, builders) in lines {
                        let line_color = accent_colors.color_for_index(color_idx as u32);

                        for builder in builders {
                            if let Ok(path) = builder.build() {
                                // we paint each color on it's own layer to stop
                                // overlapping lines of different colors changing
                                // the color of a line
                                window.paint_layer(bounds, |window| {
                                    window.paint_path(path, line_color);
                                });
                            }
                        }
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
                                                                            Label::new(if has_description {
                                                                                description
                                                                            } else {
                                                                                "(no description set)"
                                                                            })
                                                                            .color(Color::Muted)
                                                                            .truncate(),
                                                                        ),
                                                                )
                                                                .into_any_element();
                                                            let cells = vec![
                                                                description_cell,
                                                                column_label(timestamp.into()),
                                                                column_label(
                                                                    entry
                                                                        .author_name
                                                                        .to_string()
                                                                        .into(),
                                                                ),
                                                                column_label(change_short.into()),
                                                            ];
                                                            // Hidden commits read as
                                                            // elided: the row is real
                                                            // (jj renders full rows for
                                                            // hidden commits named in the
                                                            // revset) but faded like jj's
                                                            // dimmed hidden text.
                                                            if entry.flags.hidden {
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
