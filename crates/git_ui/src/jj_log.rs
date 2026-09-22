use gpui::{uniform_list, Context, Entity, Render, Subscription, Window};
use git::jj::JjLogEntry;
use project::git_store::{GitStore, GitStoreEvent, RepositoryEvent};
use std::time::{Duration, Instant};
use ui::prelude::*;

/// Minimum time between polls of the jj log for the history panel.
const POLL_INTERVAL: Duration = Duration::from_secs(1);

/// Maximum number of log entries fetched for the history panel.
const LOG_LIMIT: usize = 200;

/// The jj history panel: a list of the first 200 changes of the first
/// jj repository in the project, if any.
///
/// Registration and actions come later; this entity only holds and
/// renders the entries (and the loading, error, and empty states).
pub struct JjLog {
    git_store: Entity<GitStore>,
    entries: Vec<JjLogEntry>,
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
            git_store,
            entries: Vec::new(),
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
                this.entries = entries;
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
    fn render(
        &mut self,
        _window: &mut Window,
        _cx: &mut Context<Self>,
    ) -> impl IntoElement {
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
                                    let short_change_id =
                                        change_id[..change_id.len().min(8)].to_string();
                                    let bookmarks = entry
                                        .bookmarks
                                        .iter()
                                        .map(|bookmark| bookmark.to_string())
                                        .collect::<Vec<_>>()
                                        .join(", ");
                                    let description = entry
                                        .description
                                        .lines()
                                        .next()
                                        .unwrap_or("");
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
                                                .child(
                                                    Label::new(
                                                        short_change_id.as_str(),
                                                    )
                                                )
                                                .when(
                                                    !bookmarks.is_empty(),
                                                    |this| {
                                                        this.child(
                                                            Label::new(
                                                                bookmarks.as_str(),
                                                            )
                                                            .color(Color::Muted),
                                                        )
                                                    },
                                                ),
                                        )
                                        .child(
                                            Label::new(description).color(Color::Muted),
                                        )
                                        .child(
                                            h_flex()
                                                .gap_1()
                                                .child(
                                                    Label::new(author.as_str())
                                                        .color(Color::Muted),
                                                )
                                                .child(
                                                    Label::new(timestamp.as_str())
                                                        .color(Color::Muted),
                                                ),
                                        )
                                })
                                .collect()
                    },
                ),
            )
    }
}
