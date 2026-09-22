use gpui::{
    App, Context, Entity, Empty, Render, SharedString, Subscription, Window,
};
use project::{Project, git_store::{GitStore, GitStoreEvent, RepositoryEvent}};
use std::time::{Duration, Instant};
use ui::prelude::*;
use workspace::{HideStatusItem, StatusItemView, Workspace, item::ItemHandle};

/// Minimum time between polls of the `jj` status for the status bar item.
const POLL_INTERVAL: Duration = Duration::from_secs(1);

/// A status bar item that shows the bookmark(s) and change id of the working
/// copy of the first jj repository in the project, if any.
///
/// The item is jj-only: it appears only when the project has a jj repository
/// (colocated or pure); pure git repositories show nothing new.
pub struct JjStatusIndicator {
    project: Entity<Project>,
    text: Option<SharedString>,
    poll_scheduled: bool,
    last_poll: Option<Instant>,
    warned: bool,
    _subscription: Subscription,
}

impl JjStatusIndicator {
    pub fn new(workspace: &Workspace, cx: &mut Context<Self>) -> Self {
        let project = workspace.project().clone();
        let git_store = project.read(cx).git_store().clone();
        let mut this = Self {
            project,
            text: None,
            poll_scheduled: false,
            last_poll: None,
            warned: false,
            _subscription: cx.subscribe(&git_store, Self::on_git_store_event),
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

    /// Polls the jj status at most once in flight and at least `POLL_INTERVAL`
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
        self.last_poll = Some(Instant::now());
        let repository = self
            .project
            .read(cx)
            .git_store()
            .read(cx)
            .jj_repositories()
            .values()
            .find_map(|state| state.backend().cloned());
        cx.spawn(async move |this, cx| {
            let (text, error) = match &repository {
                Some(repository) => match repository
                    .run_read_only(vec![
                        "log",
                        "-r",
                        "@",
                        "--no-graph",
                        "-T",
                        "bookmarks.join(\",\") ++ \" @ \" ++ change_id.short()",
                    ])
                    .await
                {
                    Ok(output) => (parse_jj_status_output(&output), None),
                    Err(error) => (None, Some(format!("{error:#}"))),
                },
                None => (None, None),
            };
            this.update(cx, move |this, cx| {
                this.poll_scheduled = false;
                this.text = text;
                if let Some(error) = error {
                    if !this.warned {
                        this.warned = true;
                        log::warn!("failed to read jj status: {error}");
                    }
                }
                cx.notify();
            })
            .ok();
        })
        .detach();
    }
}

/// Parses the status template output: `"<bookmarks> @ <change-id>"`, where
/// `<bookmarks>` is empty when the working copy has no bookmarks.
fn parse_jj_status_output(output: &str) -> Option<SharedString> {
    let line = output.lines().next()?;
    let (bookmarks, change_id) = line.rsplit_once(" @ ")?;
    let bookmarks = bookmarks.trim();
    let change_id = change_id.trim();
    let text = if bookmarks.is_empty() {
        change_id.to_string()
    } else {
        format!("{bookmarks} @ {change_id}")
    };
    Some(SharedString::from(text))
}

impl Render for JjStatusIndicator {
    fn render(&mut self, _window: &mut Window, _cx: &mut Context<Self>) -> impl IntoElement {
        let Some(text) = &self.text else {
            return Empty.into_any_element();
        };
        Label::new(text.clone()).size(LabelSize::Small).into_any_element()
    }
}

impl StatusItemView for JjStatusIndicator {
    fn set_active_pane_item(
        &mut self,
        _: Option<&dyn ItemHandle>,
        _window: &mut Window,
        _: &mut Context<Self>,
    ) {
    }

    fn hide_setting(&self, _: &App) -> Option<HideStatusItem> {
        None
    }
}
