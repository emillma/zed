use crate::repository::{
    Branch, BranchesScanResult, CommitDataReader, CommitDetails, DiffType, RepoPath, Upstream,
    UpstreamTrackingStatus,
};
use crate::status::{
    DiffTreeType, FileStatus, GitStatus, StatusCode, TrackedStatus, TreeDiff, UnmergedStatus,
    UnmergedStatusCode,
};
use crate::vcs::VcsRepository;
use anyhow::Result;
use collections::HashMap;
use futures::FutureExt as _;
use futures::future::BoxFuture;
use gpui::{BackgroundExecutor, SharedString, Task};
use serde::Deserialize;
use thiserror::Error;
use util::command::{Command, new_command};

use std::ffi::OsStr;
use std::path::PathBuf;
use std::process::ExitStatus;

#[derive(Clone)]
pub(crate) struct JjBinary {
    jj_binary_path: PathBuf,
    working_directory: PathBuf,
    jj_directory: PathBuf,
    executor: BackgroundExecutor,
    envs: HashMap<String, String>,
    is_trusted: bool,
}

impl JjBinary {
    pub(crate) fn new(
        jj_binary_path: PathBuf,
        working_directory: PathBuf,
        jj_directory: PathBuf,
        executor: BackgroundExecutor,
        is_trusted: bool,
    ) -> Self {
        Self {
            jj_binary_path,
            working_directory,
            jj_directory,
            executor,
            envs: HashMap::default(),
            is_trusted,
        }
    }

    fn envs(mut self, envs: HashMap<String, String>) -> Self {
        self.envs = envs;
        self
    }

    pub async fn run<S>(&self, args: &[S]) -> Result<String>
    where
        S: AsRef<OsStr>,
    {
        let mut stdout = self.run_raw(args).await?;
        if stdout.chars().last() == Some('\n') {
            stdout.pop();
        }
        Ok(stdout)
    }

    /// Runs a read-only command: `--ignore-working-copy` is inserted with the other
    /// global flags so the command never snapshots the working copy.
    pub async fn run_read_only<S>(&self, args: &[S]) -> Result<String>
    where
        S: AsRef<OsStr>,
    {
        let mut stdout = self.run_read_only_raw(args).await?;
        if stdout.chars().last() == Some('\n') {
            stdout.pop();
        }
        Ok(stdout)
    }

    /// Returns the result of the command without trimming the trailing newline.
    pub async fn run_raw<S>(&self, args: &[S]) -> Result<String>
    where
        S: AsRef<OsStr>,
    {
        let mut command = self.build_command(args);
        let output = command.output().await?;
        anyhow::ensure!(
            output.status.success(),
            JjBinaryCommandError {
                stdout: String::from_utf8_lossy(&output.stdout).to_string(),
                stderr: String::from_utf8_lossy(&output.stderr).to_string(),
                status: output.status,
            }
        );
        Ok(String::from_utf8(output.stdout)?)
    }

    /// Returns the result of a read-only command without trimming the trailing newline.
    pub async fn run_read_only_raw<S>(&self, args: &[S]) -> Result<String>
    where
        S: AsRef<OsStr>,
    {
        let mut command = self.build_command_read_only(args);
        let output = command.output().await?;
        anyhow::ensure!(
            output.status.success(),
            JjBinaryCommandError {
                stdout: String::from_utf8_lossy(&output.stdout).to_string(),
                stderr: String::from_utf8_lossy(&output.stderr).to_string(),
                status: output.status,
            }
        );
        Ok(String::from_utf8(output.stdout)?)
    }

    /// Builds the `jj` command with the global flags inserted right after the binary
    /// path: jj's global flags must come before the subcommand.
    pub(crate) fn build_command<S>(&self, args: &[S]) -> Command
    where
        S: AsRef<OsStr>,
    {
        self.build_command_internal(args, false)
    }

    fn build_command_read_only<S>(&self, args: &[S]) -> Command
    where
        S: AsRef<OsStr>,
    {
        self.build_command_internal(args, true)
    }

    #[allow(clippy::disallowed_methods)]
    fn build_command_internal<S>(&self, args: &[S], read_only: bool) -> Command
    where
        S: AsRef<OsStr>,
    {
        let mut command = new_command(&self.jj_binary_path);
        // jj (0.45) has no --no-progress flag; progress is only rendered on a TTY,
        // and Zed captures stdout through a pipe, so --no-pager alone suffices.
        command.args(["--no-pager"]);
        if read_only {
            command.arg("--ignore-working-copy");
        }
        command.current_dir(&self.working_directory);
        command.args(args);
        command.envs(&self.envs);
        command
    }
}

#[derive(Error, Debug)]
#[error("jj command failed:\n{stdout}{stderr}\n")]
struct JjBinaryCommandError {
    stdout: String,
    stderr: String,
    status: ExitStatus,
}

pub struct JjRepository {
    jj_binary: JjBinary,
    work_dir: PathBuf,
    jj_dir: PathBuf,
    executor: BackgroundExecutor,
}

impl JjRepository {
    pub fn new(
        jj_binary_path: PathBuf,
        working_directory: PathBuf,
        jj_directory: PathBuf,
        executor: BackgroundExecutor,
        is_trusted: bool,
    ) -> Self {
        Self {
            jj_binary: JjBinary::new(
                jj_binary_path,
                working_directory.clone(),
                jj_directory.clone(),
                executor.clone(),
                is_trusted,
            ),
            work_dir: working_directory,
            jj_dir: jj_directory,
            executor,
        }
    }

    /// Runs a read-only `jj` command in the repository's working directory.
    pub fn run_read_only<S>(&self, args: Vec<S>) -> BoxFuture<'static, Result<String>>
    where
        S: AsRef<OsStr> + Send + Sync + 'static,
    {
        let jj = self.jj_binary.clone();
        self.executor
            .spawn(async move { jj.run_read_only(&args).await })
            .boxed()
    }

    /// Returns the most recent `limit` revisions, newest first, as log
    /// entries for the history panel.
    pub fn log(&self, limit: usize) -> BoxFuture<'_, Result<Vec<JjLogEntry>>> {
        self.log_revset_inner(None, limit)
    }

    /// Returns up to `limit` revisions matching `revset`, newest first, as
    /// log entries for the graph panel's revset search. On failure the error
    /// carries jj's full stderr so the UI can show its explanation.
    pub fn log_revset(
        &self,
        revset: String,
        limit: usize,
    ) -> BoxFuture<'_, Result<Vec<JjLogEntry>>> {
        self.log_revset_inner(Some(revset), limit)
    }

    fn log_revset_inner(
        &self,
        revset: Option<String>,
        limit: usize,
    ) -> BoxFuture<'_, Result<Vec<JjLogEntry>>> {
        let jj = self.jj_binary.clone();
        let mut args: Vec<String> = vec!["log".into()];
        if let Some(revset) = revset {
            args.push("-r".into());
            args.push(revset);
        }
        args.extend([
            "-n".into(),
            limit.to_string(),
            "--no-graph".into(),
            "-T".into(),
            LOG_TEMPLATE.into(),
        ]);
        self.executor
            .spawn(async move {
                let output = jj.run_read_only(&args).await?;
                Ok(parse_log_output(&output))
            })
            .boxed()
    }

    /// Returns the revset aliases defined in jj's config layers (user,
    /// repo, workspace — merged, as jj itself sees them) as
    /// `(name, expansion)` pairs. jj has no dedicated saved-revsets store;
    /// `[revset-aliases]` config entries are its native named-revset
    /// mechanism, so the history panel's dropdown offers these.
    pub fn revset_aliases(&self) -> BoxFuture<'_, Result<Vec<(SharedString, SharedString)>>> {
        let jj = self.jj_binary.clone();
        self.executor
            .spawn(async move {
                let args: Vec<String> =
                    vec!["config".into(), "list".into(), "revset-aliases".into()];
                let output = jj.run_read_only(&args).await?;
                Ok(parse_revset_aliases(&output))
            })
            .boxed()
    }
}

/// State flags for a `jj log` revision, computed in jj with jj's own
/// semantics. `json`/`stringify` strip jj's color labels, so all styling is
/// carried by these booleans and mapped to theme colors in the `jj_log`
/// renderer.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct JjLogFlags {
    /// The working copy (`@`).
    pub working_copy: bool,
    /// The repository's root commit.
    pub root: bool,
    /// Its change id holds more than one commit (after a rebase).
    pub divergent: bool,
    /// Hidden from `jj log` by default (e.g. abandoned).
    pub hidden: bool,
    /// The commit has a file conflict.
    pub conflict: bool,
    /// The commit has no diff against its first parent.
    pub empty: bool,
    /// The commit is at or under an immutable boundary.
    pub immutable: bool,
    /// Authored by the current user.
    pub mine: bool,
    /// An ancestor of the working copy.
    pub ancestor_of_wc: bool,
    /// A descendant of the working copy.
    pub descendant_of_wc: bool,
}

/// A single `jj log` revision, for the history panel.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct JjLogEntry {
    pub change_id: SharedString,
    pub commit_id: SharedString,
    /// Commit ids of the direct parents — these key the lane graph's lanes.
    pub parents: Vec<SharedString>,
    /// Change ids of the direct parents — stable across rebase; the edges
    /// `jj log --graph` draws between revisions.
    pub parent_change_ids: Vec<SharedString>,
    /// Bookmarks in display form (`name` or `name@remote`).
    pub bookmarks: Vec<SharedString>,
    pub tags: Vec<SharedString>,
    pub description: SharedString,
    pub author_name: SharedString,
    pub author_email: SharedString,
    /// The committer timestamp, as Unix seconds.
    pub commit_timestamp: i64,
    pub flags: JjLogFlags,
    /// A merge: more than one parent commit.
    pub is_merge: bool,
    /// No other emitted revision lists this one as a parent.
    pub is_head: bool,
    /// Distance to the nearest emitted head (`0` for a head); `None` when the
    /// branch continues outside the emitted window.
    pub dist_to_head: Option<u32>,
}

/// Maps the outcome of `jj file show -r @- <path>` for `load_base_text`:
/// success -> `Some(content)`, a `No such path` error -> `None`,
/// any other error -> `Err`.
fn map_base_text(output: Result<String>) -> Result<Option<String>> {
    match output {
        Ok(content) => Ok(Some(content)),
        Err(error) if error.to_string().contains("No such path") => Ok(None),
        Err(error) => Err(error),
    }
}

impl VcsRepository for JjRepository {
    fn backend_id(&self) -> &'static str {
        "jj"
    }

    fn head_sha(&self) -> BoxFuture<'_, Option<String>> {
        let jj = self.jj_binary.clone();
        self.executor
            .spawn(async move {
                let output = jj
                    .run_read_only(&["log", "-r", "@", "--no-graph", "-T", "commit_id"])
                    .await
                    .ok()?;
                let sha = output.trim().to_string();
                if sha.is_empty() { None } else { Some(sha) }
            })
            .boxed()
    }

    fn merge_message(&self) -> BoxFuture<'_, Option<String>> {
        let jj = self.jj_binary.clone();
        self.executor
            .spawn(async move {
                let output = jj
                    .run_read_only(&[
                        "log",
                        "-r",
                        "@",
                        "--no-graph",
                        "-T",
                        "description.first_line()",
                    ])
                    .await
                    .ok()?;
                let message = output.lines().next().unwrap_or_default().trim().to_string();
                if message.is_empty() {
                    None
                } else {
                    Some(message)
                }
            })
            .boxed()
    }

    fn path(&self) -> PathBuf {
        self.work_dir.clone()
    }

    fn main_repository_path(&self) -> PathBuf {
        self.jj_dir.clone()
    }

    fn check_access(&self) -> BoxFuture<'_, Result<()>> {
        let jj = self.jj_binary.clone();
        self.executor
            .spawn(async move {
                jj.run_read_only(&["workspace", "root"]).await?;
                Ok(())
            })
            .boxed()
    }

    fn branches(&self) -> BoxFuture<'_, Result<BranchesScanResult>> {
        let jj = self.jj_binary.clone();
        self.executor
            .spawn(async move {
                let output = jj.run_read_only(&["bookmark", "list"]).await?;
                // The change id of the working-copy commit; flags the bookmark(s) holding @.
                let head_change_id = jj
                    .run_read_only(&["log", "-r", "@", "--no-graph", "-T", "change_id.short()"])
                    .await
                    .ok()
                    .map(|output| output.trim().to_string())
                    .unwrap_or_default();
                Ok(BranchesScanResult::from(parse_bookmarks(
                    &output,
                    &head_change_id,
                )))
            })
            .boxed()
    }

    fn status(&self, path_prefixes: &[RepoPath]) -> Task<Result<GitStatus>> {
        let jj = self.jj_binary.clone();
        let path_prefixes = path_prefixes.to_vec();
        self.executor.spawn(async move {
            let output = jj.run_read_only(&["status"]).await?;
            parse_jj_status(&output, &path_prefixes)
        })
    }

    fn diff(&self, diff: DiffType) -> BoxFuture<'_, Result<String>> {
        let jj = self.jj_binary.clone();
        self.executor
            .spawn(async move {
                // jj has no index, so HeadToIndex and HeadToWorktree produce the same diff.
                let args = match &diff {
                    DiffType::HeadToIndex | DiffType::HeadToWorktree => vec!["diff", "--git"],
                    DiffType::MergeBase { base_ref } => {
                        vec!["diff", "--git", "--from", base_ref.as_str(), "--to", "@"]
                    }
                };
                jj.run_read_only(&args).await
            })
            .boxed()
    }
    fn load_base_text(&self, path: &RepoPath) -> BoxFuture<'_, Result<Option<String>>> {
        let this = self;
        let args = vec![
            "file".to_string(),
            "show".to_string(),
            "-r".to_string(),
            "@-".to_string(),
            path.as_unix_str().to_string(),
        ];
        async move { map_base_text(this.run_read_only(args).await) }.boxed()
    }

    fn diff_tree(&self, _request: DiffTreeType) -> BoxFuture<'_, Result<TreeDiff>> {
        // Slice A doesn't consume diff_tree; an honest error beats fake OIDs.
        async move {
            Err(anyhow::anyhow!(
                "jj backend: tree diff not yet supported (jj's abbreviated blob ids cannot fill TreeDiffStatus::Modified/Deleted old Oids)"
            ))
        }
        .boxed()
    }

    fn show(&self, commit: String) -> BoxFuture<'_, Result<CommitDetails>> {
        let jj = self.jj_binary.clone();
        self.executor
            .spawn(async move {
                let output = jj
                    .run_read_only(&[
                        "log",
                        "-r",
                        commit.as_str(),
                        "--no-graph",
                        "-T",
                        SHOW_TEMPLATE,
                    ])
                    .await?;
                parse_show_output(&output)
            })
            .boxed()
    }

    fn commit_data_reader(&self) -> Result<CommitDataReader> {
        // A git-object-shaped reader would need a separate backend path; slice A never consumes it.
        Err(anyhow::anyhow!(
            "jj backend: commit data reader not yet supported (git-object-shaped; slice A does not consume it)"
        ))
    }

    fn default_branch(
        &self,
        _include_remote_name: bool,
    ) -> BoxFuture<'_, Result<Option<SharedString>>> {
        let jj = self.jj_binary.clone();
        self.executor
            .spawn(async move {
                // jj has no default-branch concept; fall back to conventional bookmark names.
                let output = jj.run_read_only(&["bookmark", "list"]).await?;
                let branches = parse_bookmarks(&output, "");
                for name in ["trunk", "main", "master"] {
                    if branches
                        .iter()
                        .any(|branch| branch.ref_name.as_str() == name)
                    {
                        return Ok(Some(name.into()));
                    }
                }
                Ok(None)
            })
            .boxed()
    }
}

/// `show` template: sha, description, committer epoch, author name, and author
/// email, joined by the literal separator `|JJSEP|` - a description containing the
/// separator would misparse.
const SHOW_TEMPLATE: &str = "commit_id ++ \"|JJSEP|\" ++ description ++ \"|JJSEP|\" ++ committer.timestamp().format(\"%s\") ++ \"|JJSEP|\" ++ author.name() ++ \"|JJSEP|\" ++ author.email()";

/// `jj log` template: one JSON object per revision (JSONL). `json(self)`
/// carries the full commit (commit_id, parent commit ids, change_id,
/// description, author/committer); the trailing fields add change-id edges
/// (stable across rebase), display-form bookmarks, tags, and the styling
/// flags. `json`/`stringify` strip jj's color labels, so all styling is
/// carried by the boolean flags. Mirrors Emil's `jjlog_graph.py` `TPL`.
const LOG_TEMPLATE: &str = r#"'{"commit":' ++ json(self) ++ ',"parent_change_ids":[' ++ parents.map(|p| json(p.change_id())).join(",") ++ '],"bookmarks":[' ++ bookmarks.map(|b| stringify(b.name() ++ if(b.remote(), "@" ++ b.remote(), "")).escape_json()).join(",") ++ '],"tags":[' ++ tags.map(|t| json(t.name())).join(",") ++ '],"flags":{"working_copy":' ++ current_working_copy ++ ',"root":' ++ root ++ ',"divergent":' ++ divergent ++ ',"hidden":' ++ hidden ++ ',"conflict":' ++ conflict ++ ',"empty":' ++ empty ++ ',"immutable":' ++ immutable ++ ',"mine":' ++ mine ++ ',"ancestor_of_wc":' ++ self.contained_in("ancestors(@)") ++ ',"descendant_of_wc":' ++ self.contained_in("descendants(@)") ++ '}}' ++ "\n""#;

/// Parses `show` template output (sha, description, committer epoch, author name,
/// author email) into CommitDetails.
fn parse_show_output(output: &str) -> Result<CommitDetails> {
    const SEP: &str = "|JJSEP|";
    let fields: Vec<&str> = output.split(SEP).collect();
    if fields.len() < 5 {
        return Err(anyhow::anyhow!(
            "jj backend: malformed show output: {output:?}"
        ));
    }
    Ok(CommitDetails {
        sha: fields[0].trim().into(),
        message: fields[1].trim_end().into(),
        commit_timestamp: fields[2].trim().parse::<i64>().unwrap_or(0),
        author_name: fields[3].trim().into(),
        author_email: fields[4].trim().into(),
    })
}

/// Parses `jj log` JSONL output (one JSON object per revision, see
/// `LOG_TEMPLATE`) into log entries. Empty and malformed lines are skipped
/// so a single bad row never fails the batch; the head metrics are then
/// computed over the surviving entries.
fn parse_log_output(output: &str) -> Vec<JjLogEntry> {
    let mut entries = output
        .lines()
        .filter(|line| !line.trim().is_empty())
        .filter_map(|line| serde_json::from_str::<LogLine>(line).ok())
        .map(log_line_to_entry)
        .collect::<Vec<_>>();
    compute_head_metrics(&mut entries);
    entries
}

/// The `json(self)` commit object embedded in each `LOG_TEMPLATE` line.
#[derive(Deserialize)]
struct LogLineCommit {
    commit_id: String,
    #[serde(default)]
    parents: Vec<String>,
    change_id: String,
    #[serde(default)]
    description: String,
    #[serde(default)]
    author: LogLineActor,
    #[serde(default)]
    committer: LogLineActor,
}

#[derive(Deserialize, Default)]
struct LogLineActor {
    #[serde(default)]
    name: String,
    #[serde(default)]
    email: String,
    #[serde(default)]
    timestamp: String,
}

/// The `flags` object in each `LOG_TEMPLATE` line (see `JjLogFlags`).
#[derive(Deserialize, Default)]
struct LogLineFlags {
    #[serde(default)]
    working_copy: bool,
    #[serde(default)]
    root: bool,
    #[serde(default)]
    divergent: bool,
    #[serde(default)]
    hidden: bool,
    #[serde(default)]
    conflict: bool,
    #[serde(default)]
    empty: bool,
    #[serde(default)]
    immutable: bool,
    #[serde(default)]
    mine: bool,
    #[serde(default)]
    ancestor_of_wc: bool,
    #[serde(default)]
    descendant_of_wc: bool,
}

/// One `jj log` JSONL line (see `LOG_TEMPLATE`).
#[derive(Deserialize)]
struct LogLine {
    commit: LogLineCommit,
    #[serde(default)]
    parent_change_ids: Vec<String>,
    #[serde(default)]
    bookmarks: Vec<String>,
    #[serde(default)]
    tags: Vec<String>,
    #[serde(default)]
    flags: LogLineFlags,
}

/// Converts a parsed `LOG_TEMPLATE` line into a `JjLogEntry`. The head
/// metrics (`is_head`, `dist_to_head`) are left as placeholders and filled in
/// by `compute_head_metrics`.
fn log_line_to_entry(line: LogLine) -> JjLogEntry {
    let commit = line.commit;
    let flags = line.flags;
    JjLogEntry {
        change_id: commit.change_id.into(),
        commit_id: commit.commit_id.into(),
        parents: commit.parents.iter().map(SharedString::from).collect(),
        parent_change_ids: line
            .parent_change_ids
            .iter()
            .map(SharedString::from)
            .collect(),
        bookmarks: line.bookmarks.iter().map(SharedString::from).collect(),
        tags: line.tags.iter().map(SharedString::from).collect(),
        description: commit.description.into(),
        author_name: commit.author.name.into(),
        author_email: commit.author.email.into(),
        commit_timestamp: parse_rfc3339_to_epoch(&commit.committer.timestamp),
        flags: JjLogFlags {
            working_copy: flags.working_copy,
            root: flags.root,
            divergent: flags.divergent,
            hidden: flags.hidden,
            conflict: flags.conflict,
            empty: flags.empty,
            immutable: flags.immutable,
            mine: flags.mine,
            ancestor_of_wc: flags.ancestor_of_wc,
            descendant_of_wc: flags.descendant_of_wc,
        },
        is_merge: commit.parents.len() > 1,
        is_head: false,
        dist_to_head: None,
    }
}

/// Parses an RFC-3339 timestamp (jj's `json(self)` actor timestamp, e.g.
/// `2026-09-23T22:55:31+02:00`) into Unix seconds; `0` on failure, matching
/// the old template's epoch handling.
fn parse_rfc3339_to_epoch(timestamp: &str) -> i64 {
    time::OffsetDateTime::parse(timestamp, &time::format_description::well_known::Rfc3339)
        .map(|dt| dt.unix_timestamp())
        .unwrap_or(0)
}

/// Computes `is_head` and `dist_to_head` over the emitted revisions. A
/// revision is a head when no other emitted revision lists it in its
/// `parent_change_ids`; `dist_to_head` is `0` for a head, otherwise `1 +` the
/// minimum over its emitted children (or `None` when no child reaches a head,
/// i.e. the branch continues outside the emitted window). Exact when the
/// emitted set is ancestor-closed (a plain `log` near the top, or a closed
/// revset); best-effort under `-n` truncation.
fn compute_head_metrics(entries: &mut [JjLogEntry]) {
    if entries.is_empty() {
        return;
    }
    let index: HashMap<SharedString, usize> = entries
        .iter()
        .enumerate()
        .map(|(i, entry)| (entry.change_id.clone(), i))
        .collect();
    // Children of `i`: the revisions that list `i` as a parent (by change id).
    let mut children: Vec<Vec<usize>> = vec![Vec::new(); entries.len()];
    for (child, entry) in entries.iter().enumerate() {
        for parent_change_id in &entry.parent_change_ids {
            if let Some(&parent) = index.get(parent_change_id) {
                children[parent].push(child);
            }
        }
    }
    let is_head: Vec<bool> = (0..entries.len()).map(|i| children[i].is_empty()).collect();

    let mut computed = vec![false; entries.len()];
    let mut distance: Vec<Option<u32>> = vec![None; entries.len()];
    for (i, entry) in entries.iter_mut().enumerate() {
        entry.is_head = is_head[i];
        entry.dist_to_head = head_distance(i, &children, &is_head, &mut computed, &mut distance);
    }
}

/// Distance from revision `i` to its nearest head (see `compute_head_metrics`),
/// memoized in `computed`/`distance`.
fn head_distance(
    i: usize,
    children: &[Vec<usize>],
    is_head: &[bool],
    computed: &mut [bool],
    distance: &mut [Option<u32>],
) -> Option<u32> {
    if computed[i] {
        return distance[i];
    }
    computed[i] = true;
    distance[i] = if is_head[i] {
        Some(0)
    } else {
        children[i]
            .iter()
            .filter_map(|&child| head_distance(child, children, is_head, computed, distance))
            .min()
            .map(|d| d.saturating_add(1))
    };
    distance[i]
}

/// Parses `jj config list revset-aliases` output into `(name, expansion)`
/// pairs. Lines look like `revset-aliases."trunk()" = "master@origin"` or
/// `revset-aliases.all-local = "all()"`; malformed lines are skipped.
fn parse_revset_aliases(output: &str) -> Vec<(SharedString, SharedString)> {
    const PREFIX: &str = "revset-aliases.";
    output
        .lines()
        .filter_map(|line| {
            let (key, value) = line.split_once(" = ")?;
            let name = unquote_toml_string(key.strip_prefix(PREFIX)?)?;
            let expansion = unquote_toml_string(value.trim())?;
            Some((SharedString::from(name), SharedString::from(expansion)))
        })
        .collect()
}

/// Unquotes a TOML basic-string rendering (`"..."`) or passes a bare key
/// through, unescaping the common escapes. Returns `None` for anything that
/// is neither.
fn unquote_toml_string(value: &str) -> Option<String> {
    let value = value.trim();
    let Some(inner) = value.strip_prefix('"').and_then(|v| v.strip_suffix('"')) else {
        // Bare TOML keys (letters, digits, dashes) pass through unquoted.
        return (!value.is_empty()
            && value
                .chars()
                .all(|c| c.is_ascii_alphanumeric() || c == '-' || c == '_'))
        .then(|| value.to_string());
    };
    let mut result = String::with_capacity(inner.len());
    let mut chars = inner.chars();
    while let Some(c) = chars.next() {
        if c != '\\' {
            result.push(c);
            continue;
        }
        match chars.next()? {
            'n' => result.push('\n'),
            't' => result.push('\t'),
            'r' => result.push('\r'),
            '"' => result.push('"'),
            '\\' => result.push('\\'),
            other => {
                result.push('\\');
                result.push(other);
            }
        }
    }
    Some(result)
}

/// Parses `jj bookmark list` output into branches. A bookmark line
/// `<name>: <change> <commit> <desc...>` becomes a branch; an indented
/// `  @<remote> (ahead by N commits, behind by M commits): ...` line attaches an
/// upstream to the previous bookmark. Unparseable lines are skipped.
fn parse_bookmarks(output: &str, head_change_id: &str) -> Vec<Branch> {
    let mut branches: Vec<Branch> = Vec::new();
    for line in output.lines() {
        if line.starts_with(' ') || line.starts_with('\t') {
            if let Some((remote, ahead, behind)) = parse_upstream_line(line) {
                if let Some(previous) = branches.last_mut() {
                    previous.upstream = Some(Upstream {
                        ref_name: format!(
                            "refs/remotes/{remote}/{name}",
                            name = previous.ref_name.as_str()
                        )
                        .into(),
                        tracking: UpstreamTrackingStatus { ahead, behind }.into(),
                    });
                }
            }
            continue;
        }
        let Some((name, change_id)) = parse_bookmark_line(line) else {
            continue;
        };
        branches.push(Branch {
            is_head: is_head_change(&change_id, head_change_id),
            ref_name: name.into(),
            upstream: None,
            // Bookmark list carries no author or timestamp; enrichment comes later.
            most_recent_commit: None,
        });
    }
    branches
}

/// Splits a bookmark line `<name>: <change> <commit> <desc...>` into
/// (name, change id); `None` for anything else.
fn parse_bookmark_line(line: &str) -> Option<(String, String)> {
    let (name, rest) = line.split_once(": ")?;
    if name.is_empty() || name.contains(char::is_whitespace) {
        return None;
    }
    let mut tokens = rest.split_whitespace();
    let change_id = tokens.next()?;
    let _commit_id = tokens.next()?;
    Some((name.to_string(), change_id.to_string()))
}

/// Parses an indented `  @<remote> (ahead by N commits, behind by M commits): ...`
/// line into (remote, ahead, behind); `None` for anything else.
fn parse_upstream_line(line: &str) -> Option<(String, u32, u32)> {
    let rest = line.trim_start().strip_prefix('@')?;
    let (remote, tail) = rest.split_once(' ')?;
    let tail = tail.strip_prefix("(ahead by ")?;
    let (ahead, tail) = tail.split_once(" commits, behind by ")?;
    let (behind, _) = tail.split_once(" commits):")?;
    Some((
        remote.to_string(),
        ahead.parse().ok()?,
        behind.parse().ok()?,
    ))
}

/// A bookmark holds the working copy when its change id matches the @ change id.
/// jj may abbrevify change ids to different lengths across commands, so prefix
/// matches count.
fn is_head_change(bookmark_change_id: &str, head_change_id: &str) -> bool {
    !head_change_id.is_empty()
        && (bookmark_change_id == head_change_id
            || head_change_id.starts_with(bookmark_change_id)
            || bookmark_change_id.starts_with(head_change_id))
}

/// Parses `jj status` output into a GitStatus.
///
/// Only the "Working copy changes" section is consumed, stopping at the
/// following section header. jj's working-copy commit plays the index role,
/// so every change is a tracked entry with an unmodified worktree status.
fn parse_jj_status(output: &str, path_prefixes: &[RepoPath]) -> Result<GitStatus> {
    let mut entries = Vec::new();
    let mut conflicts = Vec::new();
    let mut in_section = false;
    let mut in_conflicts = false;
    for line in output.lines() {
        if !in_section {
            if line.trim() == "Working copy changes:" {
                in_section = true;
            }
            continue;
        }
        match line.split_once(' ') {
            Some((letter, path)) if matches!(letter, "A" | "M" | "D" | "R") => {
                let path = RepoPath::new(path)?;
                if !path_prefixes.is_empty()
                    && !path_prefixes.iter().any(|prefix| path.starts_with(prefix))
                {
                    continue;
                }
                let index_status = match letter {
                    "A" => StatusCode::Added,
                    "M" => StatusCode::Modified,
                    "D" => StatusCode::Deleted,
                    _ => StatusCode::Renamed,
                };
                entries.push((
                    path,
                    FileStatus::Tracked(TrackedStatus {
                        index_status,
                        worktree_status: StatusCode::Unmodified,
                    }),
                ));
            }
            _ => {
                // The working-copy section ends at the first non-change line;
                // keep scanning for the unresolved-conflicts warning block.
                if line.trim() == "Warning: There are unresolved conflicts at these paths:" {
                    in_conflicts = true;
                } else if in_conflicts {
                    // Conflict lines: `<path><whitespace run><N>-sided conflict`.
                    let Some((path_str, tail)) = line.split_once("  ") else {
                        in_conflicts = false;
                        continue;
                    };
                    let path_str = path_str.trim_end();
                    if path_str.is_empty() || !tail.contains("-sided conflict") {
                        in_conflicts = false;
                        continue;
                    }
                    let path = RepoPath::new(path_str)?;
                    if !path_prefixes.is_empty()
                        && !path_prefixes.iter().any(|prefix| path.starts_with(prefix))
                    {
                        continue;
                    }
                    conflicts.push(path.clone());
                    if !entries.iter().any(|(existing, _)| existing == &path) {
                        entries.push((
                            path,
                            FileStatus::Unmerged(UnmergedStatus {
                                first_head: UnmergedStatusCode::Updated,
                                second_head: UnmergedStatusCode::Updated,
                            }),
                        ));
                    }
                }
            }
        }
    }
    entries.sort_unstable_by(|(a, _), (b, _)| a.cmp(b));
    Ok(GitStatus {
        entries: entries.into(),
        conflicts,
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::Arc;

    const BOOKMARK_LIST_OUTPUT: &str = concat!(
        "b2: lpuuqymm 9193e9e5 line1|pipe\n",
        "dev: uxqzktsy 1966a08c (empty) dev commit\n",
        "feature: vvqslmxu 8a1abaea local desc\n",
        "main: vvqslmxu 8a1abaea local desc\n",
        "  @origin (ahead by 1 commits, behind by 2 commits): stnrqzly 9fcf9f47 (empty) origin main\n",
        "noded: vvqslmxu 8a1abaea local desc\n",
        "trunk: mpptkwut 1c3efb75 (empty) second commit",
    );

    #[test]
    fn test_parse_bookmarks() {
        let branches = parse_bookmarks(BOOKMARK_LIST_OUTPUT, "mpptkwutmkoz");
        assert_eq!(
            branches
                .iter()
                .map(|branch| branch.ref_name.as_str())
                .collect::<Vec<_>>(),
            vec!["b2", "dev", "feature", "main", "noded", "trunk"]
        );

        let main = &branches[3];
        let upstream = main.upstream.as_ref().unwrap();
        assert_eq!(upstream.ref_name.as_str(), "refs/remotes/origin/main");
        assert_eq!(
            upstream.tracking,
            UpstreamTrackingStatus {
                ahead: 1,
                behind: 2
            }
            .into()
        );
        assert_eq!(
            branches
                .iter()
                .filter(|branch| branch.upstream.is_some())
                .count(),
            1
        );
        assert!(branches[5].is_head); // trunk holds @
        assert!(!main.is_head);

        // feature and main point at the same change, so a @ on that change marks
        // both as head.
        let branches = parse_bookmarks(BOOKMARK_LIST_OUTPUT, "vvqslmxu");
        assert!(branches[2].is_head && branches[3].is_head);
        assert!(branches.iter().any(|branch| !branch.is_head));
    }

    #[test]
    fn test_parse_bookmarks_empty() {
        assert!(parse_bookmarks("", "mpptkwutmkoz").is_empty());
    }

    #[test]
    fn test_parse_bookmarks_no_head_match() {
        let branches = parse_bookmarks(BOOKMARK_LIST_OUTPUT, "qqqqqqqqqqqq");
        assert!(!branches.is_empty());
        assert!(branches.iter().all(|branch| !branch.is_head));
    }

    const JJ_STATUS_OUTPUT: &str = concat!(
        "Working copy changes:\n",
        "A f.txt\n",
        "A g.txt\n",
        "A h.txt\n",
        "Working copy  (@) : mpptkwut d1cad5a8 trunk | third\n",
        "Parent commit (@-): vvqslmxu 8a1abaea feature main* noded | local desc\n",
        "Warning: There are unresolved conflicts at these paths:\n",
        "conflicted.txt    2-sided conflict",
    );

    fn jj_tracked(index_status: StatusCode) -> FileStatus {
        FileStatus::Tracked(TrackedStatus {
            index_status,
            worktree_status: StatusCode::Unmodified,
        })
    }

    #[test]
    fn test_parse_jj_status() {
        let status = parse_jj_status(JJ_STATUS_OUTPUT, &[]).unwrap();
        assert_eq!(
            status.entries,
            Arc::from([
                (
                    RepoPath::new("conflicted.txt").unwrap(),
                    FileStatus::Unmerged(UnmergedStatus {
                        first_head: UnmergedStatusCode::Updated,
                        second_head: UnmergedStatusCode::Updated,
                    })
                ),
                (
                    RepoPath::new("f.txt").unwrap(),
                    jj_tracked(StatusCode::Added)
                ),
                (
                    RepoPath::new("g.txt").unwrap(),
                    jj_tracked(StatusCode::Added)
                ),
                (
                    RepoPath::new("h.txt").unwrap(),
                    jj_tracked(StatusCode::Added)
                ),
            ])
        );
        assert_eq!(
            status.conflicts,
            vec![RepoPath::new("conflicted.txt").unwrap()]
        );
    }

    #[test]
    fn test_parse_jj_status_modified_deleted_renamed() {
        const OUTPUT: &str = concat!(
            "Working copy changes:\n",
            "M a.txt\n",
            "D b.txt\n",
            "R c.txt\n",
            "Working copy  (@) : mpptkwut d1cad5a8 trunk | third",
        );
        let status = parse_jj_status(OUTPUT, &[]).unwrap();
        assert_eq!(
            status.entries,
            Arc::from([
                (
                    RepoPath::new("a.txt").unwrap(),
                    jj_tracked(StatusCode::Modified)
                ),
                (
                    RepoPath::new("b.txt").unwrap(),
                    jj_tracked(StatusCode::Deleted)
                ),
                (
                    RepoPath::new("c.txt").unwrap(),
                    jj_tracked(StatusCode::Renamed)
                ),
            ])
        );
    }

    #[test]
    fn test_parse_jj_status_filters_prefixes() {
        const OUTPUT: &str = concat!(
            "Working copy changes:\n",
            "A src/a.txt\n",
            "A top.txt\n",
            "Working copy  (@) : mpptkwut d1cad5a8 trunk | third",
        );
        let src = RepoPath::new("src").unwrap();
        let status = parse_jj_status(OUTPUT, &[src]).unwrap();
        assert_eq!(
            status.entries,
            Arc::from([(
                RepoPath::new("src/a.txt").unwrap(),
                jj_tracked(StatusCode::Added)
            )])
        );
    }

    #[test]
    fn test_parse_show_output() {
        let output = "0123456789abcdef0123456789abcdef012345|JJSEP|fix the thing|JJSEP|1790059323|JJSEP|Emil Martens|JJSEP|emil.martens@gmail.com";
        let details = parse_show_output(output).unwrap();
        assert_eq!(
            details.sha.as_str(),
            "0123456789abcdef0123456789abcdef012345"
        );
        assert_eq!(details.message.as_str(), "fix the thing");
        assert_eq!(details.commit_timestamp, 1790059323);
        assert_eq!(details.author_name.as_str(), "Emil Martens");
        assert_eq!(details.author_email.as_str(), "emil.martens@gmail.com");
    }

    #[test]
    fn test_parse_show_output_multi_line_description() {
        let output = "0123456789abcdef0123456789abcdef012345|JJSEP|line one\nline two\n|JJSEP|1790059323|JJSEP|Emil Martens|JJSEP|emil.martens@gmail.com";
        let details = parse_show_output(output).unwrap();
        assert_eq!(details.message.as_str(), "line one\nline two");
        assert_eq!(details.commit_timestamp, 1790059323);
    }
    /// One `LOG_TEMPLATE` line: a working-copy merge whose second parent's
    /// change id falls outside the emitted window.
    const LOG_LINE_MERGE_HEAD: &str = r#"{"commit":{"commit_id":"0123456789abcdef0123456789abcdef01234567","parents":["1111111111111111111111111111111111111111","2222222222222222222222222222222222222222"],"change_id":"aaaabbbbccccdddd","description":"merge side branch\nwith body","author":{"name":"Emil Martens","email":"emil@example.com","timestamp":"2026-01-05T10:00:00Z"},"committer":{"name":"Emil Martens","email":"emil@example.com","timestamp":"2026-01-05T10:00:00Z"}},"parent_change_ids":["bbbbaaaaccccddee","fff0fff0fff0fff0"],"bookmarks":["main@origin"],"tags":[],"flags":{"working_copy":true,"root":false,"divergent":false,"hidden":false,"conflict":true,"empty":false,"immutable":true,"mine":true,"ancestor_of_wc":false,"descendant_of_wc":false}}"#;

    const LOG_LINE_LINEAR: &str = r#"{"commit":{"commit_id":"9876543210fedc9876543210fedc9876543210","parents":["4141414141414141414141414141414141414141"],"change_id":"bbbbaaaaccccddee","description":"linear child","author":{"name":"Ana Torres","email":"ana@example.com","timestamp":"2025-06-01T02:00:00+02:00"},"committer":{"name":"Ana Torres","email":"ana@example.com","timestamp":"2025-06-01T02:00:00+02:00"}},"parent_change_ids":["ccccdddd00001111"],"bookmarks":["dev"],"tags":[],"flags":{"working_copy":false,"root":false,"divergent":false,"hidden":false,"conflict":false,"empty":false,"immutable":false,"mine":false,"ancestor_of_wc":true,"descendant_of_wc":false}}"#;

    const LOG_LINE_OLDEST: &str = r#"{"commit":{"commit_id":"4141414141414141414141414141414141414141","parents":[],"change_id":"ccccdddd00001111","description":"","author":{"name":"Bob Lee","email":"bob@example.com","timestamp":"2025-02-01T00:00:00Z"},"committer":{"name":"Bob Lee","email":"bob@example.com","timestamp":"2025-02-01T00:00:00Z"}},"parent_change_ids":[],"bookmarks":[],"tags":["release-1.0"],"flags":{"working_copy":false,"root":true,"divergent":false,"hidden":false,"conflict":false,"empty":false,"immutable":false,"mine":false,"ancestor_of_wc":true,"descendant_of_wc":false}}"#;

    #[test]
    fn test_parse_log_output_multi_row() {
        // Newest first, like `jj log`: a merge head, a linear child, and the
        // oldest emitted revision (its parent is truncated away, so its
        // `parent_change_ids` is empty even though it is not a head).
        let output =
            LOG_LINE_MERGE_HEAD.to_owned() + "\n" + LOG_LINE_LINEAR + "\n" + LOG_LINE_OLDEST;
        let entries = parse_log_output(&output);
        assert_eq!(entries.len(), 3);

        assert_eq!(entries[0].change_id, SharedString::from("aaaabbbbccccdddd"));
        assert_eq!(
            entries[0].commit_id,
            SharedString::from("0123456789abcdef0123456789abcdef01234567")
        );
        assert_eq!(
            entries[0].parents,
            vec![
                SharedString::from("1111111111111111111111111111111111111111"),
                SharedString::from("2222222222222222222222222222222222222222"),
            ]
        );
        assert_eq!(
            entries[0].parent_change_ids,
            vec![
                SharedString::from("bbbbaaaaccccddee"),
                SharedString::from("fff0fff0fff0fff0"),
            ]
        );
        assert!(entries[0].is_merge);
        assert!(entries[0].is_head);
        assert_eq!(entries[0].dist_to_head, Some(0));
        assert_eq!(
            entries[0].bookmarks,
            vec![SharedString::from("main@origin")]
        );
        assert_eq!(
            entries[0].description,
            SharedString::from("merge side branch\nwith body")
        );
        assert_eq!(entries[0].author_name, SharedString::from("Emil Martens"));
        assert_eq!(entries[0].commit_timestamp, 1767607200);
        let flags = &entries[0].flags;
        assert!(
            flags.working_copy && flags.conflict && flags.immutable && flags.mine,
            "expected working_copy, conflict, immutable, and mine flags"
        );
        assert!(!flags.root && !flags.divergent && !flags.hidden && !flags.empty);

        assert_eq!(entries[1].change_id, SharedString::from("bbbbaaaaccccddee"));
        assert_eq!(
            entries[1].parent_change_ids,
            vec![SharedString::from("ccccdddd00001111")]
        );
        assert!(!entries[1].is_merge);
        assert!(!entries[1].is_head);
        assert_eq!(entries[1].dist_to_head, Some(1));
        assert_eq!(entries[1].bookmarks, vec![SharedString::from("dev")]);
        assert_eq!(entries[1].description, SharedString::from("linear child"));
        // `+02:00` offsets normalize to the same epoch as `2025-06-01T00:00:00Z`.
        assert_eq!(entries[1].commit_timestamp, 1748736000);
        assert!(entries[1].flags.ancestor_of_wc && !entries[1].flags.working_copy);

        assert_eq!(entries[2].change_id, SharedString::from("ccccdddd00001111"));
        assert!(entries[2].parents.is_empty());
        assert!(entries[2].parent_change_ids.is_empty());
        assert!(!entries[2].is_head);
        assert_eq!(entries[2].dist_to_head, Some(2));
        assert_eq!(entries[2].tags, vec![SharedString::from("release-1.0")]);
        assert_eq!(entries[2].commit_timestamp, 1738368000);
        assert!(entries[2].flags.root && entries[2].flags.ancestor_of_wc);
    }

    #[test]
    fn test_parse_log_output_single_row() {
        const OUTPUT: &str = r#"{"commit":{"commit_id":"0123","parents":[],"change_id":"abc123def456","description":"hello","author":{"name":"Bob","email":"bob@example.com","timestamp":"2024-01-01T00:00:00Z"},"committer":{"name":"Bob","email":"bob@example.com","timestamp":"2024-01-01T00:00:00Z"}},"parent_change_ids":[],"bookmarks":["main*"],"tags":[],"flags":{"working_copy":false,"root":false,"divergent":false,"hidden":false,"conflict":false,"empty":false,"immutable":false,"mine":false,"ancestor_of_wc":false,"descendant_of_wc":false}}"#;
        let entries = parse_log_output(OUTPUT);
        assert_eq!(entries.len(), 1);
        assert!(entries[0].parents.is_empty());
        assert!(entries[0].is_head);
        assert_eq!(entries[0].dist_to_head, Some(0));
        assert_eq!(entries[0].bookmarks, vec![SharedString::from("main*")]);
        assert_eq!(entries[0].commit_timestamp, 1704067200);
    }

    #[test]
    fn test_parse_log_output_skips_malformed_lines() {
        let output = "this is not json\n".to_owned() + LOG_LINE_LINEAR;
        let entries = parse_log_output(&output);
        assert_eq!(entries.len(), 1);
        assert_eq!(entries[0].change_id, SharedString::from("bbbbaaaaccccddee"));
    }

    #[test]
    fn test_parse_log_output_empty() {
        assert!(parse_log_output("").is_empty());
    }

    #[test]
    fn test_parse_revset_aliases() {
        const OUTPUT: &str = concat!(
            "revset-aliases.\"trunk()\" = \"master@origin\"\n",
            "revset-aliases.all-local = \"all()\"\n",
            "revset-aliases.mine = \"author(exact:me@example.com)\"\n",
            "not-an-alias-line\n",
            "revset-aliases.broken\n",
        );
        let aliases = parse_revset_aliases(OUTPUT);
        assert_eq!(
            aliases,
            vec![
                (
                    SharedString::from("trunk()"),
                    SharedString::from("master@origin")
                ),
                (SharedString::from("all-local"), SharedString::from("all()")),
                (
                    SharedString::from("mine"),
                    SharedString::from("author(exact:me@example.com)")
                ),
            ]
        );
    }

    #[test]
    fn test_parse_revset_aliases_empty() {
        assert!(parse_revset_aliases("").is_empty());
    }

    #[test]
    fn test_map_base_text_present() {
        let result = map_base_text(Ok("base contents".to_string()));
        match result {
            Ok(Some(content)) => assert_eq!(content, "base contents"),
            other => panic!("expected Ok(Some), got {other:?}"),
        }
    }

    #[test]
    fn test_map_base_text_missing_path() {
        let output: Result<String> = Err(anyhow::anyhow!("Error: No such path: docs/notes.md"));
        assert!(matches!(map_base_text(output), Ok(None)));
    }
}
