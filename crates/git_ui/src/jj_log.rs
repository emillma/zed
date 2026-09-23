use anyhow::Result;
use crate::git_graph::{
    CommitLineSegment, GraphData, accent_colors_count,
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

/// Fixed row height for the uniform history list; each row carries its own
/// small graph canvas, so no shared canvas geometry is needed.
const ROW_HEIGHT: Pixels = px(32.);

/// Per-row graph state for the jj history panel: the row's node (lane +
/// accent color), the lane columns whose lines pass through this row, and
/// the bend landing on this row, if any (`(from_column, to_column)`).
/// Rows scroll inside the uniform_list, so graph and text move together.
#[derive(Clone, Default)]
struct RowGraph {
    node_lane: usize,
    color_idx: usize,
    passes: Vec<usize>,
    bend: Option<(usize, usize)>,
}

/// The jj history panel: a list of the first 200 changes of the first
/// jj repository in the project, if any.
pub struct JjLog {
    focus_handle: FocusHandle,
    git_store: Entity<GitStore>,
    entries: Vec<JjLogEntry>,
    graph_data: Option<GraphData>,
    /// Per-row graph state, rebuilt on every successful poll (cleared on
    /// fetch error); rows fall back to `RowGraph::default()`.
    row_graphs: Vec<RowGraph>,
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
            row_graphs: Vec::new(),
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
                this.row_graphs = match &graph_data {
                    Some(graph) => build_row_graphs(graph, entries.len()),
                    None => Vec::new(),
                };
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

}
    /// Builds per-row graph state from `graph_data`: walks each lane line
    /// from its child column at the first row, accumulating the column that
    /// passes through each row; a `Curve` landing on a row becomes that
    /// row's bend. Placeholders (`usize::MAX`) in unfinished lines are
    /// clamped to the row count so nothing spills into later rows.
    fn build_row_graphs(graph: &GraphData, row_count: usize) -> Vec<RowGraph> {
        let mut row_graphs = vec![RowGraph::default(); row_count];
        for (ix, commit) in graph.commits.iter().enumerate() {
            row_graphs[ix].node_lane = commit.lane;
            row_graphs[ix].color_idx = commit.color_idx;
        }
        for line in &graph.lines {
            if line.full_interval.start >= row_count {
                continue;
            }
            let mut current_column = line.child_column;
            let mut current_row = line.full_interval.start;
            for segment in &line.segments {
                match segment {
                    CommitLineSegment::Straight { to_row } => {
                        let to_row = (*to_row).min(row_count);
                        if current_row < to_row {
                            for row in current_row..to_row {
                                row_graphs[row].passes.push(current_column);
                            }
                        }
                        current_row = to_row;
                    }
                    CommitLineSegment::Curve { to_column, on_row, .. } => {
                        let on_row = (*on_row).min(row_count);
                        if current_row < on_row {
                            for row in current_row..on_row {
                                row_graphs[row].passes.push(current_column);
                            }
                        }
                        if current_row < row_count {
                            row_graphs[current_row]
                                .bend = Some((current_column, *to_column));
                        }
                        current_column = *to_column;
                        current_row = on_row;
                    }
                }
            }
        }
        row_graphs
    }

    /// Renders a 24px-wide graph cell for one row: the lanes that pass
    /// through as vertical strokes, the bend as a diagonal, and the node as
    /// a filled accent circle at the row center.
    fn render_graph_cell(
        row_graph: RowGraph,
        window: &mut Window,
        cx: &mut App,
    ) -> impl IntoElement {
        const CELL_WIDTH: Pixels = px(24.0);
        const COLUMN_WIDTH: Pixels = px(18.0);
        const NODE_RADIUS: Pixels = px(3.5);
        const LINE_WIDTH: Pixels = px(1.5);

        canvas(
            move |_bounds, _window, _cx| {},
            move |bounds: Bounds<Pixels>, _: (), window: &mut Window, cx: &mut App| {
                let text_color = cx.theme().colors().text;
                let accent_colors = cx.theme().accents();
                let lane_x = |lane: usize| -> Pixels {
                    bounds.origin.x + lane as f32 * COLUMN_WIDTH + COLUMN_WIDTH / 2.0
                };

                // Passing lane lines first (under the node): full-height
                // vertical strokes in the text color.
                for &lane in &row_graph.passes {
                    let mut builder = PathBuilder::stroke(LINE_WIDTH);
                    builder.move_to(point(lane_x(lane), bounds.top()));
                    builder.line_to(point(lane_x(lane), bounds.bottom()));
                    window.paint_path(builder.build().expect("two-point path"), text_color);
                }

                // The bend landing on this row: a diagonal from the source
                // column at the bottom to the target column at the top.
                if let Some((from_lane, to_lane)) = row_graph.bend {
                    let mut builder = PathBuilder::stroke(LINE_WIDTH);
                    builder.move_to(point(lane_x(from_lane), bounds.bottom()));
                    builder.line_to(point(lane_x(to_lane), bounds.top()));
                    window.paint_path(builder.build().expect("two-point path"), text_color);
                }

                // Node on top: filled circle in the lane's accent color.
                let x = lane_x(row_graph.node_lane);
                let y = (bounds.top() + bounds.bottom()) / 2.0;
                let node_bounds = Bounds::new(
                    point(x - NODE_RADIUS, y - NODE_RADIUS),
                    gpui::Size::new(NODE_RADIUS * 2.0, NODE_RADIUS * 2.0),
                );
                let node_color = accent_colors
                    .0
                    .get(row_graph.color_idx)
                    .copied()
                    .unwrap_or_default();
                window.paint_quad(
                    gpui::fill(node_bounds, node_color).corner_radii(NODE_RADIUS),
                );
            },
        )
        .w(CELL_WIDTH)
        .h(ROW_HEIGHT)
    }

    /// Formats a unix timestamp as a short relative time: "5s ago",
    /// "3m ago", "2h ago", "4d ago", "3mo ago", "2y ago".
    fn format_timestamp(timestamp: i64) -> String {
        let now = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap_or_default()
            .as_secs();
        let seconds = now.saturating_sub(timestamp.max(0) as u64) as i64;
        match seconds {
            s if s < 60 => format!("{s}s ago"),
            s if s < 3_600 => format!("{}m ago", s / 60),
            s if s < 86_400 => format!("{}h ago", s / 3600),
            s if s < 2_592_000 => format!("{}d ago", s / 86_400),
            s if s < 31_536_000 => format!("{}mo ago", s / 2_592_000),
            s => format!("{}y ago", s / 31_536_000),
        }
    }

impl Render for JjLog {
    fn render(&mut self, window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let entries = self.entries.clone();
        let row_graphs = self.row_graphs.clone();
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
                uniform_list(
                "jj_log_list",
                item_count,
                move |range, window, cx| {
                    entries[range.clone()]
                        .iter()
                        .enumerate()
                        .map(|(ix, entry)| {
                            let index = range.start + ix;
                            let change_id = entry.change_id.to_string();
                            let change_short = change_id[..change_id.len().min(8)].to_string();
                            let bookmarks = entry
                                .bookmarks
                                .iter()
                                .map(|bookmark| bookmark.to_string())
                                .collect::<Vec<_>>()
                                .join(", ");
                            let description = entry.description.lines().next().unwrap_or("");
                            let has_description = !description.is_empty();
                            let author_name = entry.author_name.to_string();
                            let timestamp = format_timestamp(entry.commit_timestamp);
                            h_flex()
                                .id(("jj-log-item", index))
                                .w_full()
                                .h(ROW_HEIGHT)
                                .items_center()
                                .child(render_graph_cell(
                                    row_graphs.get(index).cloned().unwrap_or_default(),
                                    window,
                                    cx,
                                ))
                                .child(
                                    h_flex()
                                        .w_full()
                                        .px_2()
                                        .items_center()
                                        .gap_2()
                                        .child(
                                            Label::new(change_short.as_str()).color(Color::Default),
                                        )
                                        .when(!bookmarks.is_empty(), |this| {
                                            this.child(
                                                Label::new(bookmarks.as_str()).color(Color::Accent),
                                            )
                                        })
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
                                        .child(Label::new(author_name.as_str()).color(Color::Muted))
                                        .child(Label::new(timestamp.as_str()).color(Color::Muted)),
                                )
                        })
                        .collect()
                },
                )
                // Plain list: no explicit height, no shared scroll container —
                // each row's graph cell scrolls with its text.
                .flex_1(),
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
