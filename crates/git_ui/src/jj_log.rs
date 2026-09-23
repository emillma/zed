use anyhow::Result;
use crate::git_graph::{
    CurveKind, CommitLineSegment, GraphData, accent_colors_count, draw_commit_circle,
    lane_center_x, to_row_center, COMMIT_CIRCLE_RADIUS, COMMIT_CIRCLE_STROKE_WIDTH, LANE_WIDTH,
    LEFT_PADDING, LINE_WIDTH,
};
use editor::Editor;
use git::{jj::JjLogEntry, repository::InitialGraphCommitData, Oid};
use gpui::{
    App, Bounds, Context, Entity, EventEmitter, FocusHandle, Focusable, PathBuilder, Render,
    SharedString, Subscription, Task, WeakEntity, Window, actions, canvas, px, point, uniform_list,
};
use project::git_store::{GitStore, GitStoreEvent, RepositoryEvent};
use std::collections::{BTreeMap, HashMap};
use std::str::FromStr;
use std::sync::Arc;
use std::time::{Duration, Instant};
use ui::prelude::*;
use workspace::{
    SerializableItem, Workspace,
    item::{Item, ItemEvent},
};

/// Minimum time between polls of the jj log for the history panel.
const POLL_INTERVAL: Duration = Duration::from_secs(1);

/// Maximum number of log entries fetched for the history panel.
const LOG_LIMIT: usize = 200;

/// Fixed row height for the history list and the graph canvas; rows are
/// uniform so the canvas can position circles at `index * ROW_HEIGHT`.
const ROW_HEIGHT: Pixels = px(32.);

/// The jj history panel: a list of the first 200 changes of the first
/// jj repository in the project, if any.
pub struct JjLog {
    focus_handle: FocusHandle,
    git_store: Entity<GitStore>,
    entries: Vec<JjLogEntry>,
    #[allow(dead_code)] // Consumed by upcoming graph lane rendering.
    graph_data: Option<GraphData>,
    // Revset filter input, created lazily on first render: the JjLog
    // constructors have no Window, which Editor::single_line requires.
    revset_editor: Option<Entity<Editor>>,
    current_revset: Option<String>,
    saved_revsets: Vec<String>,
    loading: bool,
    error: Option<String>,
    poll_scheduled: bool,
    last_poll: Option<Instant>,
    _subscription: Subscription,
}

impl JjLog {
    pub fn new(git_store: Entity<GitStore>, cx: &mut Context<Self>) -> Self {
        let subscription = cx.subscribe(&git_store, Self::on_git_store_event);
        let mut this = Self {
            focus_handle: cx.focus_handle(),
            git_store,
            entries: Vec::new(),
            graph_data: None,
            revset_editor: None,
            current_revset: None,
            saved_revsets: revset_config::load(),
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
            let (entries, error) = match &repository {
                Some(repository) => {
                    let result = match current_revset {
                        Some(revset) => repository.log_revset(revset, LOG_LIMIT).await,
                        None => repository.log(LOG_LIMIT).await,
                    };
                    match result {
                        Ok(entries) => (entries, None),
                        Err(error) => (Vec::new(), Some(format!("{error:#}"))),
                    }
                }
                None => (Vec::new(), None),
            };
            this.update(cx, move |this, cx| {
                this.poll_scheduled = false;
                // Only a poll that actually invoked jj consumes the throttle
                // interval; a no-op poll (no jj repository yet) must not delay
                // the next trigger, or the event carrying the newly discovered
                // repository gets dropped and the panel stays blank.
                if repository.is_some() {
                    this.last_poll = Some(Instant::now());
                }
                // Lane data is only built on a successful fetch; jj log
                // entries arrive children-first, the order add_commits expects.
                let graph_data = error
                    .as_ref()
                    .is_none()
                    .then(|| {
                        let commits: Vec<Arc<InitialGraphCommitData>> = entries
                            .iter()
                            .filter_map(|entry| {
                                let sha = Oid::from_str(&entry.commit_id).ok()?;
                                Some(Arc::new(InitialGraphCommitData {
                                    sha,
                                    parents: entry
                                        .parents
                                        .iter()
                                        .filter_map(|parent| Oid::from_str(parent).ok())
                                        .collect(),
                                    ref_names: entry.bookmarks.clone(),
                                }))
                            })
                            .collect();
                        let mut graph_data =
                            GraphData::new(accent_colors_count(&cx.theme().accents()));
                        graph_data.add_commits(&commits);
                        graph_data
                    });
                this.entries = entries;
                this.graph_data = graph_data;
                this.loading = false;
                this.error = error;
                cx.notify();
            })
            .ok();
        })
        .detach();
    }

    /// Renders the lane graph as a full-content-height canvas; the shared
    /// scroll container moves it in lockstep with the list, so no scroll
    /// offset math is needed. Line geometry is ported from GitGraph's
    /// `render_graph_canvas` (curves included).
    fn render_graph_canvas(&self, item_count: usize) -> impl IntoElement {
        let content_height = ROW_HEIGHT * item_count as f32;
        let commits = self
            .graph_data
            .as_ref()
            .map(|graph| graph.commits.clone())
            .unwrap_or_default();
        let lines = self
            .graph_data
            .as_ref()
            .map(|graph| graph.lines.clone())
            .unwrap_or_default();
        let lane_count = commits.iter().map(|commit| commit.lane).max().unwrap_or(0) + 1;
        let graph_width = LEFT_PADDING * 2.0 + LANE_WIDTH * lane_count as f32;

        canvas(
            move |_bounds, _window, _cx| {},
            move |bounds: Bounds<Pixels>, _: (), window: &mut Window, cx: &mut App| {
                window.paint_layer(bounds, |window| {
                    let accent_colors = cx.theme().accents();

                    // Lane lines, ported from GitGraph's render_graph_canvas
                    // with first_visible_row = 0 and no scroll offset.
                    let mut lines_by_color: BTreeMap<usize, Vec<PathBuilder>> = BTreeMap::new();
                    for line in lines.iter() {
                        let Some((start_segment_idx, start_column)) =
                            line.get_first_visible_segment_idx(0)
                        else {
                            continue;
                        };
                        let line_x = lane_center_x(bounds, start_column as f32);
                        let from_y = bounds.origin.y
                            + line.full_interval.start as f32 * ROW_HEIGHT
                            + ROW_HEIGHT / 2.0
                            + COMMIT_CIRCLE_RADIUS;
                        let mut current_row = from_y;
                        let mut current_column = line_x;
                        let mut builder = PathBuilder::stroke(LINE_WIDTH);
                        builder.move_to(point(line_x, from_y));
                        let segments = &line.segments[start_segment_idx..];
                        let desired_curve_height = ROW_HEIGHT / 3.0;
                        let desired_curve_width = LANE_WIDTH / 3.0;
                        for (segment_idx, segment) in segments.iter().enumerate() {
                            let is_last = segment_idx + 1 == segments.len();
                            match segment {
                                CommitLineSegment::Straight { to_row } => {
                                    let mut dest_row =
                                        to_row_center(*to_row, ROW_HEIGHT, px(0.), bounds);
                                    if is_last {
                                        dest_row -= COMMIT_CIRCLE_RADIUS;
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
                                    let to_column = lane_center_x(bounds, *to_column as f32);
                                    let to_row =
                                        to_row_center(*on_row, ROW_HEIGHT, px(0.), bounds);
                                    let going_right = to_column > current_column;
                                    let column_shift = if going_right {
                                        COMMIT_CIRCLE_RADIUS + COMMIT_CIRCLE_STROKE_WIDTH
                                    } else {
                                        -COMMIT_CIRCLE_RADIUS - COMMIT_CIRCLE_STROKE_WIDTH
                                    };
                                    match curve_kind {
                                        CurveKind::Checkout => {
                                            let mut to_column = to_column;
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
                                            let curve_end = point(
                                                current_column + signed_curve_width,
                                                to_row,
                                            );
                                            let curve_control =
                                                point(current_column, to_row);
                                            builder.move_to(point(current_column, current_row));
                                            builder.line_to(curve_start);
                                            builder.move_to(curve_start);
                                            builder.curve_to(curve_end, curve_control);
                                            builder.move_to(curve_end);
                                            builder.line_to(point(to_column, to_row));
                                        }
                                        CurveKind::Merge => {
                                            let mut to_row = to_row;
                                            if is_last {
                                                to_row -= COMMIT_CIRCLE_RADIUS;
                                            }
                                            let merge_start = point(
                                                current_column + column_shift,
                                                current_row - COMMIT_CIRCLE_RADIUS,
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
                        lines_by_color
                            .entry(line.color_idx)
                            .or_default()
                            .push(builder);
                    }

                    // Paint each color in its own layer so overlapping lines
                    // of different colors don't blend (same as GitGraph).
                    for (color_idx, builders) in lines_by_color {
                        let line_color = accent_colors.color_for_index(color_idx as u32);
                        for builder in builders {
                            if let Ok(path) = builder.build() {
                                window.paint_layer(bounds, |window| {
                                    window.paint_path(path, line_color);
                                });
                            }
                        }
                    }

                    // Commit circles on top.
                    for (ix, commit) in commits.iter().enumerate() {
                        let x = lane_center_x(bounds, commit.lane as f32);
                        let y = bounds.origin.y + ix as f32 * ROW_HEIGHT + ROW_HEIGHT / 2.0;
                        let color = accent_colors.color_for_index(commit.color_idx as u32);
                        draw_commit_circle(x, y, color, window);
                    }
                })
            },
        )
        .w(graph_width)
        .h(content_height)
    }

    /// Formats a unix timestamp as a short relative age (v1: no locale
    /// handling; falls back to years for anything older).
    fn format_timestamp(epoch_seconds: i64) -> String {
        let now = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|d| d.as_secs() as i64)
            .unwrap_or(0);
        let seconds = (now - epoch_seconds).max(0);
        match seconds {
            0..=59 => format!("{}s ago", seconds),
            60..=3599 => format!("{}m ago", seconds / 60),
            3600..=86399 => format!("{}h ago", seconds / 3600),
            86400..=2591999 => format!("{}d ago", seconds / 86400),
            2592000..=31535999 => format!("{}mo ago", seconds / 2592000),
            _ => format!("{}y ago", seconds / 31536000),
        }
    }
}
impl Render for JjLog {
    fn render(&mut self, window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let entries = self.entries.clone();
        if let Some(error) = self.error.as_deref() {
            return v_flex()
                .size_full()
                .p_2()
                .child(Label::new(error).color(Color::Muted));
        }
        if self.loading {
            return v_flex()
                .size_full()
                .p_2()
                .child(Label::new("Loading…").color(Color::Muted));
        }
        if entries.is_empty() {
            return v_flex()
                .size_full()
                .p_2()
                .child(Label::new("no jj repository").color(Color::Muted));
        }
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
        let graph_canvas = self.render_graph_canvas(item_count);
        let revset_bar = h_flex()
            .w_full()
            .px_2()
            .py(px(4.))
            .items_center()
            .gap_2()
            .child(revset_editor)
            .child({
                let mut apply_button = div()
                    .px_2()
                    .text_sm()
                    .child(Label::new("Apply"));
                apply_button
                    .interactivity()
                    .on_click(cx.listener(|this, _, _window, cx| {
                        let text = this
                            .revset_editor
                            .as_ref()
                            .map(|editor| editor.read(cx).text(cx).trim().to_string());
                        this.current_revset = match text {
                            Some(text) if !text.is_empty() => Some(text),
                            _ => None,
                        };
                        cx.emit(ItemEvent::UpdateTab);
                        this.schedule_poll(cx);
                    }));
                apply_button
            });
        let save_button = {
            let mut save_button = div()
                .px_2()
                .text_sm()
                .child(Label::new("Save"));
            save_button
                .interactivity()
                .on_click(cx.listener(|this, _, _window, cx| {
                    let text = this
                        .revset_editor
                        .as_ref()
                        .map(|editor| editor.read(cx).text(cx).trim().to_string());
                    if let Some(text) = text.filter(|text| !text.is_empty()) {
                        if !this.saved_revsets.contains(&text) {
                            this.saved_revsets.push(text);
                            if let Err(err) = revset_config::save(&this.saved_revsets) {
                                log::warn!("failed to save jj revsets: {err}");
                            }
                            cx.emit(ItemEvent::UpdateTab);
                        }
                        cx.notify();
                    }
                }));
            save_button
        };
        let saved_chips = h_flex()
            .w_full()
            .px_2()
            .py(px(2.))
            .gap_1()
            .flex_wrap()
            .children(self.saved_revsets.clone().into_iter().enumerate().map(
                |(ix, revset)| {
                    let click_revset = revset.clone();
                    let remove_revset = revset.clone();
                    let mut chip = div()
                        .px_2()
                        .text_sm()
                        .child(Label::new(revset));
                    chip.interactivity().on_click(cx.listener(
                        move |this, _, window, cx| {
                            if let Some(editor) = this.revset_editor.as_ref() {
                                editor.update(cx, |editor, cx| {
                                    editor.set_text(click_revset.clone(), window, cx)
                                });
                            }
                            this.current_revset = Some(click_revset.clone());
                            cx.emit(ItemEvent::UpdateTab);
                            this.schedule_poll(cx);
                        },
                    ));
                    let remove = {
                        let mut remove = div().px_1().child(Label::new("x").color(Color::Muted));
                        remove.interactivity().on_click(cx.listener(
                            move |this, _, _window, cx| {
                                this.saved_revsets.retain(|saved| saved != &remove_revset);
                                if let Err(err) = revset_config::save(&this.saved_revsets) {
                                    log::warn!("failed to save jj revsets: {err}");
                                }
                                cx.emit(ItemEvent::UpdateTab);
                                cx.notify();
                            },
                        ));
                        remove
                    };
                    h_flex().gap_0p5().child(chip).child(remove)
                },
            ))
            .when(self.saved_revsets.is_empty(), |this| this.hidden());
        v_flex()
            .flex_1()
            .size_full()
            .overflow_hidden()
            .child(revset_bar)
            .child(save_button)
            .child(saved_chips)
            .child(
                h_flex()
                    .id("jj_log_scroll")
                    .w_full()
                    .flex_1()
                    .overflow_y_scroll()
                    .child(graph_canvas)
                    .child(
                        uniform_list(
                "jj_log_list",
                item_count,
                move |range, _window, _cx| {
                    entries[range.clone()]
                        .iter()
                        .enumerate()
                        .map(|(ix, entry)| {
                            let index = range.start + ix;
                            let change_id = entry.change_id.to_string();
                            let short_change_id = change_id[..change_id.len().min(8)].to_string();
                            let bookmarks = entry
                                .bookmarks
                                .iter()
                                .map(|bookmark| bookmark.to_string())
                                .collect::<Vec<_>>()
                                .join(", ");
                            let description = entry.description.lines().next().unwrap_or("");
                            let author = entry.author_name.to_string();
                            let timestamp = JjLog::format_timestamp(entry.commit_timestamp);
                            h_flex()
                                .id(("jj-log-item", index))
                                .w_full()
                                .h(ROW_HEIGHT)
                                .px_2()
                                .items_center()
                                .gap_2()
                                .child(Label::new(short_change_id.as_str()))
                                .when(!bookmarks.is_empty(), |this| {
                                    this.child(Label::new(bookmarks.as_str()).color(Color::Accent))
                                })
                                .child(
                                    div()
                                        .flex_1()
                                        .min_w_0()
                                        .overflow_hidden()
                                        .child(Label::new(description)),
                                )
                                .child(Label::new(author.as_str()).color(Color::Muted))
                                .child(Label::new(timestamp.as_str()).color(Color::Muted))
                        })
                        .collect()
                },
                            )
                            // Explicit content height: the list's internal
                            // scroll range becomes zero (it never scrolls),
                            // so the shared container scrolls both children.
                            // Do NOT use .overflow_hidden() here — it skips
                            // uniform_list's scroll-offset initialization
                            // and panics in prepaint.
                            .h(ROW_HEIGHT * item_count as f32),
                    ),
            )
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

mod revset_config {
    /// Saved revsets live in a user-visible config file
    /// (`~/.config/zed/jj-revsets.json`) rather than the workspace db, so
    /// they are global, inspectable, and hand-editable.
    use anyhow::Result;
    use paths::config_dir;
    use serde::{Deserialize, Serialize};
    use std::{fs, path::PathBuf};

    #[derive(Serialize, Deserialize, Default)]
    struct SavedRevsets {
        #[serde(default, skip_serializing_if = "Vec::is_empty")]
        saved_revsets: Vec<String>,
    }

    fn config_path() -> PathBuf {
        config_dir().join("jj-revsets.json")
    }

    pub fn load() -> Vec<String> {
        fs::read_to_string(config_path())
            .ok()
            .and_then(|content| serde_json::from_str::<SavedRevsets>(&content).ok())
            .map(|saved| saved.saved_revsets)
            .unwrap_or_default()
    }

    pub fn save(saved_revsets: &[String]) -> Result<()> {
        let path = config_path();
        if let Some(parent) = path.parent() {
            fs::create_dir_all(parent).ok();
        }
        let json = serde_json::to_string_pretty(&SavedRevsets {
            saved_revsets: saved_revsets.to_vec(),
        })?;
        fs::write(path, json + "\n")?;
        Ok(())
    }
}
