use anyhow::Result;
use crate::git_graph::{GraphData, accent_colors_count};
use git::{jj::JjLogEntry, repository::InitialGraphCommitData, Oid};
use gpui::{
    App, Context, Entity, EventEmitter, FocusHandle, Focusable, Render, SharedString, Subscription,
    Task, WeakEntity, Window, actions, uniform_list,
};
use project::git_store::{GitStore, GitStoreEvent, RepositoryEvent};
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

/// The jj history panel: a list of the first 200 changes of the first
/// jj repository in the project, if any.
pub struct JjLog {
    focus_handle: FocusHandle,
    git_store: Entity<GitStore>,
    entries: Vec<JjLogEntry>,
    #[allow(dead_code)] // Consumed by upcoming graph lane rendering.
    graph_data: Option<GraphData>,
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
        cx.spawn(async move |this, cx| {
            let (entries, error) = match &repository {
                Some(repository) => match repository.log(LOG_LIMIT).await {
                    Ok(entries) => (entries, None),
                    Err(error) => (Vec::new(), Some(format!("{error:#}"))),
                },
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
}

impl Render for JjLog {
    fn render(&mut self, _window: &mut Window, _cx: &mut Context<Self>) -> impl IntoElement {
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
        v_flex()
            .flex_1()
            .size_full()
            .overflow_hidden()
            .child(uniform_list(
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
                            let timestamp = entry.commit_timestamp.to_string();
                            v_flex()
                                .id(("jj-log-item", index))
                                .w_full()
                                .py_1()
                                .px_2()
                                .gap_0p5()
                                .child(
                                    h_flex()
                                        .gap_1()
                                        .child(Label::new(short_change_id.as_str()))
                                        .when(!bookmarks.is_empty(), |this| {
                                            this.child(
                                                Label::new(bookmarks.as_str()).color(Color::Muted),
                                            )
                                        }),
                                )
                                .child(Label::new(description).color(Color::Muted))
                                .child(
                                    h_flex()
                                        .gap_1()
                                        .child(Label::new(author.as_str()).color(Color::Muted))
                                        .child(Label::new(timestamp.as_str()).color(Color::Muted)),
                                )
                        })
                        .collect()
                },
            ))
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
        _: workspace::WorkspaceId,
        _: workspace::ItemId,
        _window: &mut Window,
        cx: &mut App,
    ) -> Task<Result<Entity<Self>>> {
        let git_store = project.read(cx).git_store().clone();
        Task::ready(Ok(cx.new(|cx| JjLog::new(git_store, cx))))
    }

    fn serialize(
        &mut self,
        _: &mut Workspace,
        _: workspace::ItemId,
        _: bool,
        _: &mut Context<Self>,
    ) -> Option<Task<Result<()>>> {
        Some(Task::ready(Ok(())))
    }

    fn should_serialize(&self, _: &Self::Event) -> bool {
        false
    }
}
