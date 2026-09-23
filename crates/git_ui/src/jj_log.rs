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
use crate::jj_settings::JjSettings;
use settings::Settings as _;
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
const ROW_HEIGHT: Pixels = px(30.);

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
    loading: bool,
    error: Option<String>,
    poll_scheduled: bool,
    last_poll: Option<Instant>,
    _subscription: Subscription,
}

impl JjLog {
    pub fn new(git_store: Entity<GitStore>, cx: &mut Context<Self>) -> Self {
        let subscription = cx.subscribe(&git_store, Self::on_git_store_event);
        cx.observe_global::<JjSettings>(|_, cx| cx.notify()).detach();
        let mut this = Self {
            focus_handle: cx.focus_handle(),
            git_store,
            entries: Vec::new(),
            graph_data: None,
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

    /// Renders the lane graph as a full-content-height canvas in the style
    /// of gg (submodules/gg): hollow ring nodes with a filled dot for the
    /// working copy, text-colored lines with rounded bends, 18px columns.
    /// The shared scroll container moves it in lockstep with the list.
    fn render_graph_canvas(&self, item_count: usize) -> impl IntoElement {
        const COLUMN_WIDTH: Pixels = px(18.0);
        const NODE_RADIUS: Pixels = px(6.0);
        const BEND_RADIUS: Pixels = px(6.0);
        const LINE_WIDTH: Pixels = px(1.5);

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
        let graph_width = LEFT_PADDING * 2.0 + COLUMN_WIDTH * lane_count as f32;

        canvas(
            move |_bounds, _window, _cx| {},
            move |bounds: Bounds<Pixels>, _: (), window: &mut Window, cx: &mut App| {
                window.paint_layer(bounds, |window| {
                    let text_color = cx.theme().colors().text;
                    let working_copy_color = ui::Color::Created.color(cx);

                    let lane_x = |lane: usize| -> Pixels {
                        bounds.origin.x
                            + LEFT_PADDING
                            + lane as f32 * COLUMN_WIDTH
                            + COLUMN_WIDTH / 2.0
                    };
                    let row_center = |row: usize| -> Pixels {
                        bounds.origin.y + row as f32 * ROW_HEIGHT + ROW_HEIGHT / 2.0
                    };

                    // Lane lines first (under the nodes): text-colored strokes
                    // with gg's right-angle bends, rounded by quadratic corners.
                    for line in lines.iter() {
                        let Some((start_segment_idx, start_column)) =
                            line.get_first_visible_segment_idx(0)
                        else {
                            continue;
                        };
                        let child_y = row_center(line.full_interval.start);
                        let mut builder = PathBuilder::stroke(LINE_WIDTH);
                        builder.move_to(point(lane_x(start_column), child_y + NODE_RADIUS));
                        let mut current_column = start_column;
                        let mut current_y = child_y + NODE_RADIUS;
                        for segment in &line.segments[start_segment_idx..] {
                            match segment {
                                CommitLineSegment::Straight { to_row } => {
                                    let to_y = row_center(*to_row) - NODE_RADIUS;
                                    builder.line_to(point(lane_x(current_column), to_y));
                                    current_y = to_y;
                                }
                                CommitLineSegment::Curve {
                                    to_column,
                                    on_row,
                                    ..
                                } => {
                                    let from_x = lane_x(current_column);
                                    let to_x = lane_x(*to_column);
                                    let to_y = row_center(*on_row) - NODE_RADIUS;
                                    let mid_y = (current_y + to_y) / 2.0;
                                    let going_right = to_x > from_x;
                                    let sign = if going_right { 1.0 } else { -1.0 };
                                    // vertical run, rounded corner, horizontal
                                    // run, rounded corner, vertical run.
                                    builder.line_to(point(from_x, mid_y - BEND_RADIUS));
                                    builder.curve_to(
                                        point(from_x + sign * BEND_RADIUS, mid_y),
                                        point(from_x, mid_y),
                                    );
                                    builder.move_to(point(from_x + sign * BEND_RADIUS, mid_y));
                                    builder.line_to(point(to_x - sign * BEND_RADIUS, mid_y));
                                    builder.curve_to(point(to_x, mid_y), point(to_x, mid_y));
                                    builder.move_to(point(to_x, mid_y + BEND_RADIUS));
                                    builder.line_to(point(to_x, to_y));
                                    current_column = *to_column;
                                    current_y = to_y;
                                }
                            }
                        }
                        if let Ok(path) = builder.build() {
                            window.paint_path(path, text_color);
                        }
                    }

                    // Nodes on top: hollow rings (mutable commits); the first
                    // row is the working copy and gets a filled inner dot.
                    for (ix, commit) in commits.iter().enumerate() {
                        let x = lane_x(commit.lane);
                        let y = row_center(ix);
                        let diameter = NODE_RADIUS * 2.0;
                        let node_bounds = Bounds::new(
                            point(x - NODE_RADIUS, y - NODE_RADIUS),
                            gpui::Size {
                                width: diameter,
                                height: diameter,
                            },
                        );
                        window.paint_quad(
                            gpui::fill(node_bounds, gpui::transparent_black())
                                .corner_radii(NODE_RADIUS)
                                .border_widths(px(1.5))
                                .border_color(text_color),
                        );
                        if ix == 0 {
                            let inner_radius = NODE_RADIUS / 2.0;
                            let inner_bounds = Bounds::new(
                                point(x - inner_radius, y - inner_radius),
                                gpui::Size {
                                    width: inner_radius * 2.0,
                                    height: inner_radius * 2.0,
                                },
                            );
                            window.paint_quad(
                                gpui::fill(inner_bounds, working_copy_color)
                                    .corner_radii(inner_radius),
                            );
                        }
                    }
                })
            },
        )
        .w(graph_width)
        .h(content_height)
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
        let saved_revsets = JjSettings::get_global(cx).saved_revsets.clone();
        let saved_chips = h_flex()
            .w_full()
            .px_2()
            .py(px(2.))
            .gap_1()
            .flex_wrap()
            .children(saved_revsets.iter().cloned().enumerate().map(|(ix, revset)| {
                let click_revset = revset.clone();
                let mut chip = div().px_2().text_sm().child(Label::new(revset));
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
                chip
            }))
            .when(saved_revsets.is_empty(), |this| this.hidden());
        v_flex()
            .flex_1()
            .size_full()
            .overflow_hidden()
            .child(revset_bar)
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
                            let change_prefix = change_id[..change_id.len().min(3)].to_string();
                            let change_rest = change_id[change_prefix.len()..].to_string();
                            let bookmarks = entry
                                .bookmarks
                                .iter()
                                .map(|bookmark| bookmark.to_string())
                                .collect::<Vec<_>>()
                                .join(", ");
                            let description = entry.description.lines().next().unwrap_or("");
                            let has_description = !description.is_empty();
                            let author_email = entry.author_email.to_string();
                            h_flex()
                                .id(("jj-log-item", index))
                                .w_full()
                                .h(ROW_HEIGHT)
                                .px_2()
                                .items_center()
                                .gap_2()
                                .child(
                                    h_flex()
                                        .child(
                                            Label::new(change_prefix.as_str()).color(Color::Accent),
                                        )
                                        .child(Label::new(change_rest.as_str()).color(Color::Muted)),
                                )
                                .child(
                                    div()
                                        .flex_1()
                                        .min_w_0()
                                        .overflow_hidden()
                                        .child(Label::new(if has_description {
                                            description
                                        } else {
                                            "(no description set)"
                                        })
                                        .color(if has_description {
                                            Color::Default
                                        } else {
                                            Color::Muted
                                        })),
                                )
                                .child(Label::new(author_email.as_str()).color(Color::Muted))
                                .when(!bookmarks.is_empty(), |this| {
                                    this.child(Label::new(bookmarks.as_str()).color(Color::Accent))
                                })
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
