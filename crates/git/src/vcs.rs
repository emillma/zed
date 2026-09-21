//! The backend-agnostic core of Zed's VCS layer.
//!
//! [`VcsRepository`] is the core supertrait of [`GitRepository`]: only the methods the
//! project's status scan and branch/status-bar paths actually consume — head/state sha,
//! branch/bookmark info, status/diff hunks, show/merge message, repo detection.
//!
//! [`GitRepository`] keeps its full git-specific surface and is bounded on this trait;
//! the blanket [`impl`] below delegates every core method to the corresponding
//! [`GitRepository`] method, so git has zero behavior change and no duplicated bodies.
//! A second backend (e.g. `jj`) implements only [`VcsRepository`].
//!
//! See `VCS_BACKEND_DESIGN.md`, "Resolved decisions" (decision 1).

use crate::repository::{
    BranchesScanResult, CommitDataReader, CommitDetails, DiffType, GitRepository, RepoPath,
};
use crate::status::{DiffTreeType, GitStatus, TreeDiff};
use anyhow::Result;
use futures::future::BoxFuture;
use gpui::{SharedString, Task};
use std::path::PathBuf;

/// Backend-agnostic core: the methods the project's status scan + branch/status-bar
/// paths actually consume (finalized by auditing `git_store.rs`).
///
/// Signatures are identical to the [`GitRepository`] methods they mirror; the types
/// are the git-crate's, deliberately not yet generalized.
pub trait VcsRepository: Send + Sync {
    /// "git" | "jj" — drives the UI (icon/label, status-item content).
    fn backend_id(&self) -> &'static str;

    /// The HEAD / working-copy state sha (git_store.rs:2355).
    fn head_sha(&self) -> BoxFuture<'_, Option<String>>;

    /// Branch/bookmark info (git_store.rs:8604); a `jj` backend fills these with bookmarks.
    fn branches(&self) -> BoxFuture<'_, Result<BranchesScanResult>>;

    /// Working status for the given path prefixes (the status scan).
    fn status(&self, path_prefixes: &[RepoPath]) -> Task<Result<GitStatus>>;

    /// A unified diff of the working state.
    fn diff(&self, diff: DiffType) -> BoxFuture<'_, Result<String>>;

    /// A structured tree diff.
    fn diff_tree(&self, request: DiffTreeType) -> BoxFuture<'_, Result<TreeDiff>>;

    /// Details for a single commit (git_store.rs:7157).
    fn show(&self, commit: String) -> BoxFuture<'_, Result<CommitDetails>>;

    /// The in-progress merge/commit message, if any (git_store.rs:6394).
    fn merge_message(&self) -> BoxFuture<'_, Option<String>>;

    /// The absolute path to the repository.
    fn path(&self) -> PathBuf;

    /// The absolute path to the main repository (for worktrees, the parent repo).
    fn main_repository_path(&self) -> PathBuf;

    /// A reader for commit data, used to lazy-load history.
    fn commit_data_reader(&self) -> Result<CommitDataReader>;

    /// The repository's default branch, if determinable.
    fn default_branch(
        &self,
        include_remote_name: bool,
    ) -> BoxFuture<'_, Result<Option<SharedString>>>;

    /// Checks that the repository is accessible and safe to operate on.
    fn check_access(&self) -> BoxFuture<'_, Result<()>>;
}

/// Any `GitRepository` backend is also a `VcsRepository`: each core method
/// fully-qualifies to the corresponding `GitRepository` method. Zero behavior
/// change, no duplicated bodies, and no "unsupported" no-ops anywhere.
impl<T: GitRepository + ?Sized> VcsRepository for T {
    fn backend_id(&self) -> &'static str {
        "git"
    }

    fn head_sha(&self) -> BoxFuture<'_, Option<String>> {
        GitRepository::head_sha(self)
    }

    fn branches(&self) -> BoxFuture<'_, Result<BranchesScanResult>> {
        GitRepository::branches(self)
    }

    fn status(&self, path_prefixes: &[RepoPath]) -> Task<Result<GitStatus>> {
        GitRepository::status(self, path_prefixes)
    }

    fn diff(&self, diff: DiffType) -> BoxFuture<'_, Result<String>> {
        GitRepository::diff(self, diff)
    }

    fn diff_tree(&self, request: DiffTreeType) -> BoxFuture<'_, Result<TreeDiff>> {
        GitRepository::diff_tree(self, request)
    }

    fn show(&self, commit: String) -> BoxFuture<'_, Result<CommitDetails>> {
        GitRepository::show(self, commit)
    }

    fn merge_message(&self) -> BoxFuture<'_, Option<String>> {
        GitRepository::merge_message(self)
    }

    fn path(&self) -> PathBuf {
        GitRepository::path(self)
    }

    fn main_repository_path(&self) -> PathBuf {
        GitRepository::main_repository_path(self)
    }

    fn commit_data_reader(&self) -> Result<CommitDataReader> {
        GitRepository::commit_data_reader(self)
    }

    fn default_branch(
        &self,
        include_remote_name: bool,
    ) -> BoxFuture<'_, Result<Option<SharedString>>> {
        GitRepository::default_branch(self, include_remote_name)
    }

    fn check_access(&self) -> BoxFuture<'_, Result<()>> {
        GitRepository::check_access(self)
    }
}
