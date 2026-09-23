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
use futures::future::BoxFuture;
use futures::FutureExt as _;
use gpui::{BackgroundExecutor, SharedString, Task};
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
}

/// A single `jj log` revision, for the history panel.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct JjLogEntry {
    pub change_id: SharedString,
    pub commit_id: SharedString,
    pub parents: Vec<SharedString>,
    pub bookmarks: Vec<SharedString>,
    pub description: SharedString,
    pub author_name: SharedString,
    pub author_email: SharedString,
    pub commit_timestamp: i64,
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
                if sha.is_empty() {
                    None
                } else {
                    Some(sha)
                }
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
        async move {
            map_base_text(this.run_read_only(args).await)
        }
        .boxed()
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
                    if branches.iter().any(|branch| branch.ref_name.as_str() == name) {
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

/// `jj log` template: one row per revision, `|JJSEP|`-separated fields,
/// each row terminated by `"\n"` (without it, rows concatenate).
const LOG_TEMPLATE: &str = "change_id.short() ++ \"|JJSEP|\" ++ commit_id ++ \"|JJSEP|\" ++ parents.map(|p| p.commit_id()).join(\" \") ++ \"|JJSEP|\" ++ bookmarks.join(\",\") ++ \"|JJSEP|\" ++ description.first_line() ++ \"|JJSEP|\" ++ author.name() ++ \"|JJSEP|\" ++ author.email() ++ \"|JJSEP|\" ++ committer.timestamp().format(\"%s\") ++ \"\\n\"";

/// Parses `show` template output (sha, description, committer epoch, author name,
/// author email) into CommitDetails.
fn parse_show_output(output: &str) -> Result<CommitDetails> {
    const SEP: &str = "|JJSEP|";
    let fields: Vec<&str> = output.split(SEP).collect();
    if fields.len() < 5 {
        return Err(anyhow::anyhow!("jj backend: malformed show output: {output:?}"));
    }
    Ok(CommitDetails {
        sha: fields[0].trim().into(),
        message: fields[1].trim_end().into(),
        commit_timestamp: fields[2].trim().parse::<i64>().unwrap_or(0),
        author_name: fields[3].trim().into(),
        author_email: fields[4].trim().into(),
    })
}

/// Parses `jj log` template output into log entries, skipping empty or
/// malformed rows.
fn parse_log_output(output: &str) -> Vec<JjLogEntry> {
    const SEP: &str = "|JJSEP|";
    output
        .lines()
        .filter_map(|line| {
            let fields: Vec<&str> = line.split(SEP).collect();
            if fields.len() < 8 {
                return None;
            }
            Some(JjLogEntry {
                change_id: fields[0].trim().into(),
                commit_id: fields[1].trim().into(),
                parents: fields[2]
                    .split(' ')
                    .map(str::trim)
                    .filter(|parent| !parent.is_empty())
                    .map(SharedString::from)
                    .collect(),
                bookmarks: fields[3]
                    .split(',')
                    .map(str::trim)
                    .filter(|bookmark| !bookmark.is_empty())
                    .map(SharedString::from)
                    .collect(),
                description: fields[4].trim_end().into(),
                author_name: fields[5].trim().into(),
                author_email: fields[6].trim().into(),
                commit_timestamp: fields[7].trim().parse::<i64>().unwrap_or(0),
            })
        })
        .collect()
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
    use std::sync::Arc;
    use super::*;

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
            UpstreamTrackingStatus { ahead: 1, behind: 2 }.into()
        );
        assert_eq!(
            branches.iter().filter(|branch| branch.upstream.is_some()).count(),
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
                (RepoPath::new("f.txt").unwrap(), jj_tracked(StatusCode::Added)),
                (RepoPath::new("g.txt").unwrap(), jj_tracked(StatusCode::Added)),
                (RepoPath::new("h.txt").unwrap(), jj_tracked(StatusCode::Added)),
            ])
        );
        assert_eq!(status.conflicts, vec![RepoPath::new("conflicted.txt").unwrap()]);
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
                (RepoPath::new("a.txt").unwrap(), jj_tracked(StatusCode::Modified)),
                (RepoPath::new("b.txt").unwrap(), jj_tracked(StatusCode::Deleted)),
                (RepoPath::new("c.txt").unwrap(), jj_tracked(StatusCode::Renamed)),
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
        assert_eq!(details.sha.as_str(), "0123456789abcdef0123456789abcdef012345");
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
    #[test]
    fn test_parse_log_output_multi_row() {
        const OUTPUT: &str = concat!(
            "abc123def456|JJSEP|0123456789abcdef0123456789abcdef|JJSEP|1111111111111111111111111111111111111111 2222222222222222222222222222222222222222|JJSEP|main*,feature|JJSEP|fix the thing|JJSEP|Emil Martens|JJSEP|emil@example.com|JJSEP|1790059323\n",
            "789fedcba654|JJSEP|9876543210fedc9876543210fedc9876|JJSEP||JJSEP||JJSEP||JJSEP|Ana Torres|JJSEP|ana@example.com|JJSEP|1790059000\n"
        );
        let entries = parse_log_output(OUTPUT);
        assert_eq!(entries.len(), 2);
        assert_eq!(entries[0].change_id, SharedString::from("abc123def456"));
        assert_eq!(entries[0].commit_id, SharedString::from("0123456789abcdef0123456789abcdef"));
        assert_eq!(
            entries[0].parents,
            vec![
                SharedString::from("1111111111111111111111111111111111111111"),
                SharedString::from("2222222222222222222222222222222222222222"),
            ]
        );
        assert!(entries[1].parents.is_empty());
        assert_eq!(
            entries[0].bookmarks,
            vec![SharedString::from("main*"), SharedString::from("feature")]
        );
        assert_eq!(entries[0].description, SharedString::from("fix the thing"));
        assert_eq!(entries[0].author_name, SharedString::from("Emil Martens"));
        assert_eq!(entries[0].author_email, SharedString::from("emil@example.com"));
        assert_eq!(entries[0].commit_timestamp, 1790059323);
        assert!(entries[1].bookmarks.is_empty());
        assert_eq!(entries[1].description, SharedString::from(""));
        assert_eq!(entries[1].commit_timestamp, 1790059000);
    }

    #[test]
    fn test_parse_log_output_single_row() {
        let entries = parse_log_output(
            "abc123def456|JJSEP|0123|JJSEP|3333333333333333333333333333333333333333|JJSEP|main*|JJSEP|hello|JJSEP|Bob|JJSEP|bob@example.com|JJSEP|123",
        );
        assert_eq!(entries.len(), 1);
        assert_eq!(
            entries[0].parents,
            vec![SharedString::from("3333333333333333333333333333333333333333")]
        );
        assert_eq!(entries[0].bookmarks, vec![SharedString::from("main*")]);
        assert_eq!(entries[0].commit_timestamp, 123);
    }

    #[test]
    fn test_parse_log_output_empty() {
        assert!(parse_log_output("").is_empty());
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
