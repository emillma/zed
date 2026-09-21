# VcsBackend: abstracting Zed's VCS layer for git + jj

## Context

Zed's VCS layer is git-specific today. A `jj` (jujutsu) backend for Zed would need
that layer generalized before it can attach to a second backend.

The upstream situation:

- **zed-industries/zed#21538** — the tracking issue for jj support, still **open** and
  assigned to Veykril. No merged PR, no green light from maintainers.
- **zed-industries/zed#53453** — merged **2026-06-02**. It **removed `libgit2`** in
  favour of shelling out to the `git` CLI (`GitBinary`). The stated motivation was to
  drop a C dependency and to make Zed benefit from **reftable** (which libgit2 did not
  support). This is the single most important recent directional decision: the Zed team
  deliberately moved **away from a native library binding and toward CLI invocations**.
- The overall **approach is still undecided** upstream. There is no accepted shape for a
  VCS abstraction.
- **PR #63651** — a prior jj-related PR, **rejected**. The review feedback was a lesson
  about *how* to contribute to Zed's VCS code: **no cross-crate hacks**, and the codebase
  demands a clean **abstraction layer**, not bolt-on special cases scattered across
  `git`, `project`, and `git_ui`.

## Prior art

Three prior attempts, none upstreamed. Each is a useful data point about what does and
does not survive contact with Zed's codebase.

- **sygi/zed** (https://github.com/sygi/zed) — jj support built on **`jj-lib`** (the Rust
  library behind jj). Adds `crates/jj` and `crates/jj_ui`, with a `JjTracker` that mirrors
  the git tracker's API **1:1**. It does **not** introduce a shared backend trait — git and
  jj are two parallel, independently-plumbed backends. **Never PR'd**; it stalled on
  "no maintainer green light." The 1:1 tracker mirror is the cautionary tale: duplicating
  the tracker plumbing for a second backend is exactly the pattern Zed's review culture
  rejects (see #63651).
- **jeofo/zed** (https://github.com/jeofo/zed) — a **jj CLI wrapper** in `crates/jujutsu`
  (shells out to the `jj` binary rather than linking `jj-lib`), plus a `jj-lsp` adapter and
  a `jjdescription` language definition. A **one-day demo**, never PR'd. This is the
  closer prior art for our CLI-first decision.
- **nilskch/jj-lsp** (https://github.com/nilskch/jj-lsp) — an **LSP** that surfaces
  jj's **conflict markers as diagnostics and code actions**. ~80 stars. Not a Zed backend,
  but the natural prior art for our Slice D (conflict UX) — conflict markers can be handled
  out-of-band via an LSP rather than inside the VCS trait.

## Decisions

1. **CLI-first, not `jj-lib`.** Zed just removed `libgit2` in favour of CLI invocations
   (#53453). Pulling in `jj-lib` (as sygi did) would **contradict that fresh direction** —
   it re-introduces a heavy Rust dependency (and its own build-graph churn) that the team
   has explicitly been moving away from for git. osiewicz was also skeptical of taking
   `jj-lib` wholesale. Independently, the `jj` CLI is the source of truth in our own mono
   migration plan. Staying on the CLI keeps the jj backend's dependency surface small and
   its behavior identical to what a user gets from `jj` on the command line. jeofo's CLI
   wrapper is the closer prior art; sygi's `jj-lib` approach is the cautionary tale for
   upstreaming.
2. **Generalize the existing seam.** The `GitRepository` trait at the **project↔git
   boundary** is the natural seam (verified — see "Verified code facts"). The plan is to
   design a **backend-agnostic `VcsRepository` trait at exactly that boundary**, then
   **refactor git behind it FIRST with zero behavior change**, and add the jj backend
   **SECOND**. This is deliberately **not** the sygi-style parallel-crate approach: that
   duplicates tracker plumbing and would not upstream (see #63651, Decision 5).
3. **Slice A scope = project VCS state + status bar.** Colocated jj repos (both `.git` and
   `.jj`) show the **current bookmark + change id** in the status bar. jj workspaces with
   no `.git` degrade gracefully (jj backend, no git behavior at all). **Git repos are
   byte-for-byte unchanged** in behavior.
4. **jj CLI invocation hygiene.** Every jj invocation must:
   - **never paginate** — pass `-c ui.paginate=never` (or the equivalent `--no-pager`
     flags) so output is never truncated by a pager;
   - **`--no-progress`** (and any analogous flag) so stderr is quiet and parseable;
   - use **machine-readable output** via jj **templates** — e.g. `jj log --no-graph
     --config ui.paginate=never -T '<expr>'` — never scrape human-friendly text;
   - **understand that read-only jj commands snapshot the working copy** — jj commits a
     new working-copy change as a side effect of many commands, so the commands we run
     from a status poll must be ones whose side effects are **benign** (an empty or
     no-op snapshot, never a destructive one);
   - **never let jj open an editor or prompt** — always run non-interactively; a jj
     invocation must never block waiting for a terminal.
5. **Upstream strategy: discussion before PR.** Following the #63651 feedback path and
   sygi's "no maintainer green light" stall, **open a zed-industries/zed discussion
   referencing #21538, the sygi and jeofo forks, and this design, BEFORE opening any
   PR.** Emil opens the PRs/discussions (or explicitly approves each).

## Verified code facts

Verified against `main` @ `97b1e64a17` (clone at `/home/emil/mono/submodules/zed`).

- **Trait.** `pub trait GitRepository: Send + Sync` at
  `crates/git/src/repository.rs:782` — a **single, large trait (68 methods)**, spanning
  lines 782–1135. Representative methods: `load_index_text`, `load_committed_text`,
  `load_blob_content`, `set_index_text`, `remote_urls`, `revparse_batch`, `load_revisions`,
  `head_sha`, `merge_message`, `status`, `diff_tree`, `branches`, `change_branch`,
  `create_branch`, `reset`, `blame`, `path`, `stage_paths`, `unstage_paths`, `commit`,
  `push`, `pull`, `fetch`, `diff`, `checkpoint`, `initial_graph_data`, `commit_data_reader`,
  `update_ref`, `delete_ref`, `set_trusted`/`is_trusted`, among others.
  - **This contradicts a "small seam" assumption.** The trait is far more git-shaped than
    a two-method interface. Git-specific types flow through it: **`Oid`** (SHA-1/SHA-256,
    `git.rs:180`), **`RepoPath`** (`repository.rs:4088`), staging/index (`stage_paths`,
    `unstage_paths`, `set_index_text`, `load_index_text`), branches (`branches`,
    `change_branch`, …), and refs (`update_ref`, `delete_ref`). See "Risks."
- **Implementations** of `GitRepository`:
  - `RealGitRepository` — `impl GitRepository for RealGitRepository` at
    `crates/git/src/repository.rs:1662`.
  - `FakeGitRepository` (test double) — `crates/fs/src/fake_git_repo.rs:168`.
- **Consumer (the seam).** `crates/project/src/git_store.rs:750`:
  `LocalRepositoryState { pub fs: Arc<dyn Fs>, pub backend: Arc<dyn GitRepository>, … }`.
  This `Arc<dyn GitRepository>` is exactly the project↔git boundary the decision refers to.
  Call sites, e.g. `git_store.rs:2355` (`backend.head_sha()`), `git_store.rs:6394`
  (`backend.merge_message()`), `git_store.rs:7157` (`backend.show(commit)`).
- **Status scan / "tracker".** There is **no `struct GitTracker`** (grep for it fails).
  The scan logic lives directly on the `Repository` entity in `git_store.rs`:
  `schedule_scan` (`git_store.rs:10081`) schedules a keyed job (`GitJobKey::ReloadGitState`)
  whose `compute_snapshot` runs `backend.status`/`branches`. `refresh_branch_list`
  (`git_store.rs:8604`) calls `backend.branches()`. State is tracked by `scan_id`
  (`git_store.rs:619`) and a pending-paths list updated on worktree events
  (`git_store.rs:686`). There is **no `UpdatedGitRepositories` struct** (grep fails); the
  related types are `UpdatedGitRepositoriesSet` / `UpdatedGitRepository`
  (`git_store.rs:97`, `2682`).
- **Branch display.** The bottom status bar (`crates/zed/src/zed.rs:636-651`) has **no
  dedicated branch item** — only `git_blame_status` and `merge_conflict_indicator` (plus
  the file name, diagnostics, etc.). The **branch name is shown by the GitPanel and the
  branch picker**, both of which read `repo.branch.name()` (e.g. `git_ui.rs:541`,
  `git_ui.rs:565`). That `Branch` value originates in the **git crate**:
  `refresh_branch_list` → `backend.branches()` → `Branch` (`repository.rs:233`, fields
  `is_head`/`ref_name`/`upstream`/`most_recent_commit`; `name()` strips `refs/heads/`). It
  is surfaced to the UI via the project's `RepositorySnapshot.branch`
  (`git_store.rs:615-617`).
  - **Note:** this **contradicts the task's expectation** of a status-bar branch display —
    current `main` has no such item. Slice A therefore **adds** a status-bar VCS item for
    jj (bookmark + change id) rather than modifying an existing branch display. Git repos
    keep their existing (GitPanel/branch-picker) display.
- **CLI plumbing (the #53453 target).** `GitBinary` (`repository.rs:3863`) builds a
  `util::command::Command` with `--no-optional-locks` and `--no-pager`
  (`repository.rs:3918`, `3924`); `GitBinary::run` returns `stdout` as `String`
  (`repository.rs:3943`). `status` is parsed from porcelain output (`status.rs:431`,
  `GitStatusFromStr`). This is the shape our jj CLI wrapper will mirror.

**Inferred (not verified line-by-line):** that `compute_snapshot` is the function
invoked by `schedule_scan`'s job (inferred from the job body at `git_store.rs:10093`);
the exact jj template expressions to use for each query (to be finalized in Slice A).

## Trait design sketch

Name it **`VcsRepository`** rather than keeping `GitRepository` + a wrapper. Rationale:
the seam *is* the abstraction; keeping the name `GitRepository` bakes "git" into the type
that jj will implement, and forces every consumer to know the git-specific name even when
the backend is jj. It is a mechanical rename (trait + the two impls + the
`LocalRepositoryState` field + downstream casts) done **before** adding jj, with zero
behavior change.

The resolved shape (post-draft review — decision 1): a **core supertrait
`VcsRepository`** carrying only the methods the project's status scan + status bar
paths actually consume (from the verified `git_store.rs` call sites — target ≤15:
head/state sha, branch/bookmark info, status/diff hunks, show/merge message, repo
detection), with `GitRepository` remaining the existing 68-method trait. Its
`VcsRepository` supertrait bound is satisfied by a **blanket delegation impl** —
`impl<T: GitRepository + ?Sized> VcsRepository for T` — that fully-qualifies each
core method to the corresponding `GitRepository` method. Git therefore has **zero
behavior change and no duplicated bodies** (no 60 no-op bodies), and jj implements
`VcsRepository` only.

```rust
/// Backend-agnostic core: the methods the project's status scan + status bar
/// paths actually consume (target ≤15, finalized by auditing `git_store.rs`).
pub trait VcsRepository: Send + Sync {
    /// "git" | "jj" — drives the UI (icon/label, status-item content).
    fn backend_id(&self) -> &'static str;

    // Head / state sha (git_store.rs:2355)
    fn head_sha(&self) -> BoxFuture<'_, Option<String>>;

    // Branch/bookmark info (git_store.rs:8604); jj fills these with bookmarks.
    fn branches(&self) -> BoxFuture<'_, Result<BranchesScanResult>>;

    // Status / diff hunks (status scan; gutter indicators arrive in slice B)
    fn status(&self, path_prefixes: &[RepoPath]) -> Task<Result<GitStatus>>;
    fn diff(&self, diff: DiffType) -> BoxFuture<'_, Result<String>>;
    fn diff_tree(&self, request: DiffTreeType) -> BoxFuture<'_, Result<TreeDiff>>;

    // Commit view / message (git_store.rs:6394, 7157)
    fn show(&self, commit: String) -> BoxFuture<'_, Result<CommitDetails>>;
    fn merge_message(&self) -> BoxFuture<'_, Option<String>>;

    // Repo detection / paths
    fn path(&self) -> PathBuf;
    fn main_repository_path(&self) -> PathBuf;
    fn commit_data_reader(&self) -> Result<CommitDataReader>;
    fn default_branch(&self, include_remote_name: bool)
    -> BoxFuture<'_, Result<Option<SharedString>>>;
    fn check_access(&self) -> BoxFuture<'_, Result<()>>;

    // blame / richer hunk APIs land in later slices as needed; target total ≤15.
}

/// Existing git surface: all 68 methods, unchanged — no new bodies. The
/// `VcsRepository` supertrait bound is satisfied by the blanket impl below.
pub trait GitRepository: VcsRepository {
    // … the existing 68 methods, unchanged (Oid/RepoPath/index/branches/stash/push/blame/…) …
}

/// Any git backend is also a `VcsRepository`: each core method fully-qualifies to
/// the corresponding git method. Zero behavior change, no duplicate bodies, and
/// no "unsupported" no-ops anywhere.
impl<T: GitRepository + ?Sized> VcsRepository for T {
    fn backend_id(&self) -> &'static str { "git" }
    fn head_sha(&self) -> BoxFuture<'_, Option<String>> {
        GitRepository::head_sha(self).boxed()
    }
    fn status(&self, path_prefixes: &[RepoPath]) -> Task<Result<GitStatus>> {
        GitRepository::status(self, path_prefixes)
    }
    // … each remaining core method forwards via `GitRepository::method(self)` …
}
```

For slice A the core trait reuses the existing git-crate data types (`GitStatus`,
`BranchesScanResult`, `TreeDiff`, `CommitDetails`) unchanged — they are plain data
(paths + status codes; refs with an `is_head` flag) that jj fills naturally
(bookmark → `ref_name`, no `upstream`). Generalizing them into backend-neutral
names is follow-up work, not slice A's job.


The seam becomes `LocalRepositoryState { backend: Arc<dyn VcsRepository>, … }`
(`git_store.rs:750`). Git-specific operations (branch picker, push/pull, stash) downcast
via `backend.as_any().downcast_ref::<dyn GitRepository>()` (or a small enum
`RepoBackend::Git(Arc<RealGitRepository>) | Jj(Arc<JjRepository>)`) — mirroring how
`git_store` already pattern-matches `RepositoryState::Local`.

**How jj maps onto the core (with Decision 4's hygiene on every line):**

- **head**: `jj log --no-graph --config ui.paginate=never -r @ -T 'change_id.short() + " " + commit_id.short()'`
  plus the bookmark from `jj bookmark list` (or a template listing bookmarks pointing at
  `@`). change id → `change_id`, commit id → `commit_id`, the pointing bookmark →
  `bookmark`.
- **status**: `jj status --config ui.paginate=never` (or `jj diff --name-status`) →
  working-copy-vs-`@` entries. **No index/staging area exists in jj**, so there is no
  "staged" file state; `VcsFileStatus` uses the working-copy subset only.
- **diff**: `jj diff --config ui.paginate=never` (text) / a unified template for hunks.

**Detection & colocation.** At repository discovery (where `git_store` currently keys off
`dot_git_abs_path`, `git_store.rs:127/6094`), look in the worktree root for `.jj` and
`.git`:
- `.jj` only → `JjRepository` backend.
- `.git` only → `RealGitRepository` backend (unchanged).
- **Colocated** (both `.git` and `.jj`) → **both backends run** (resolved decision 3):
  the GitPanel keeps showing git state, and the new status-bar item shows jj state —
  no precedence conflict, no user setting for now (flagged for the upstream discussion).
  Git-only operations remain reachable via the downcast (a user can still `git log`, etc.).

## Slice plan

- **A — trait + status bar (this design's target).** Introduce `VcsRepository`/
  `GitRepository` with the blanket delegation (resolved decision 1) and move git behind
  it with zero behavior change; instantiate the right backend from `.jj`/`.git`
  detection; add the new status-bar item driven by the active backend — git repos show
  the branch name, jj repos (colocated or `.jj`-only) show `<bookmark> @ <change-id>` —
  while the GitPanel keeps its current git-only behavior (resolved decision 2). What the
  backend must expose: the core `VcsRepository` methods (head/branch-bookmark, status,
  diff) plus the status-scan hook so `schedule_scan` drives jj polls.
- **B — diff indicators + blame.** The rich hunk-level diff for gutter indicators and the
  blame view. What the backend must expose: a structured diff (hunks with line ranges) and
  per-line blame. jj maps via `jj diff` (hunks) and `jj log -p`-style per-line attribution
  (jj has no `git blame` equivalent; blame must be derived from the working-copy's commit
  history via `jj log`/`jj show -p`).
- **C — history view (jj log).** The commit-graph/history panel. What the backend must
  expose: `initial_graph_data`-equivalent — a stream of (id, parent ids, author, message)
  so the graph renders. jj maps via `jj log --no-graph -T '<template>'`; the "graph" is
  just the parent/child link set from `jj log`.
- **D — conflict UX.** Surface jj's conflict markers as inline diagnostics + code actions
  (resolve/rebase). What the backend must expose: a list of conflicted files and a
  resolve action. This slice is likely delegated to an **LSP** (nilskch/jj-lsp) rather than
  the VCS trait — the conflict data flows through the language server, not the repository
  seam.

## Resolved decisions (post-draft review)

The draft's open questions are resolved as follows (design review, 2026-09-21):

1. **Trait mechanism = split + blanket delegation.** Introduce a core supertrait
   `VcsRepository` containing only the methods the project/status paths actually consume
   (from the verified call sites in `git_store.rs` — target ≤15 methods: head/state sha,
   branch/bookmark info, status/diff hunks, show/merge message, repo detection).
   `GitRepository: VcsRepository` becomes a supertrait, satisfied by a blanket impl
   `impl<T: GitRepository + ?Sized> VcsRepository for T` that fully-qualifies delegation
   to the `GitRepository` methods — zero behavior change, no 60 no-op bodies. jj then
   implements `VcsRepository` only. (The trait sketch above is updated accordingly.)
2. **Status bar = new item, git panel untouched.** Main has no status-bar branch item
   (verified: `zed.rs:636-651`). Slice A adds a new status-bar item driven by the active
   backend: git repos → branch name (consistent with the panel); jj repos (colocated or
   `.jj`-only) → `<bookmark> @ <change-id>`. The GitPanel keeps its current git-only
   behavior.
3. **Colocation precedence.** When both `.git` and `.jj` exist, BOTH backends run —
   the git panel keeps showing git state, the new status item shows jj state. No user
   setting for now; flagged as an upstream-discussion topic.
4. **jj availability.** Locate `jj` on `PATH`, mirroring the `GitBinary` pattern
   (`repository.rs:3863`). Missing binary → jj backend disabled silently; git behavior
   unaffected.
5. **Snapshot side effects.** Read-only polls run with `jj --ignore-working-copy` so
   status polling never mutates the working copy; an explicit refresh/user action may
   snapshot. Tradeoff: status then reflects the last-snapshotted state rather than the
   live buffer.
6. **Performance.** Debounce/coalesce status polls; one templated jj invocation per
   refresh (e.g. a single `jj log`/`jj status` with a template carrying bookmark +
   change id); minimum-interval throttle. Concrete numbers deferred to T5.

## Risks and open questions

The draft's first, third, fifth, and sixth questions (trait shape, colocation, jj
availability, status-bar UX) are resolved in "Resolved decisions (post-draft review)"
above and are struck here. Remaining:

- **Safe commands under `--ignore-working-copy`.** Polling runs jj with
  `--ignore-working-copy` (resolved decision 5), so polling no longer mutates the working
  copy. **Open question:** which jj commands remain safe/correct under
  `--ignore-working-copy`, and which need a real snapshot (and thus are limited to
  explicit refresh/user actions)?
- **Concrete throttle numbers.** Polls are debounced/coalesced with a minimum interval,
  and each refresh is one templated jj invocation (resolved decision 6). **Open
  question:** the concrete throttle numbers (minimum interval, max in-flight polls) —
  deferred to T5.
- **Core trait coverage.** Does the core ≤15-method set (resolved decision 1) cover
  everything the `Repository` entity in `git_store.rs` needs for its scan/branch-refresh
  plumbing, or does some plumbing stay git-specific in slice A?
