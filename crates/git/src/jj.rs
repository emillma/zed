use crate::repository::{
    BranchesScanResult, CommitDataReader, CommitDetails, DiffType, RepoPath,
};
use crate::status::{DiffTreeType, GitStatus, TreeDiff};
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
        command.args(["--no-pager", "--no-progress"]);
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
        unimplemented!("jj backend: branches not yet wired")
    }

    fn status(&self, _path_prefixes: &[RepoPath]) -> Task<Result<GitStatus>> {
        unimplemented!("jj backend: status not yet wired")
    }

    fn diff(&self, _diff: DiffType) -> BoxFuture<'_, Result<String>> {
        unimplemented!("jj backend: diff not yet wired")
    }

    fn diff_tree(&self, _request: DiffTreeType) -> BoxFuture<'_, Result<TreeDiff>> {
        unimplemented!("jj backend: diff_tree not yet wired")
    }

    fn show(&self, _commit: String) -> BoxFuture<'_, Result<CommitDetails>> {
        unimplemented!("jj backend: show not yet wired")
    }

    fn commit_data_reader(&self) -> Result<CommitDataReader> {
        unimplemented!("jj backend: commit_data_reader not yet wired")
    }

    fn default_branch(
        &self,
        _include_remote_name: bool,
    ) -> BoxFuture<'_, Result<Option<SharedString>>> {
        unimplemented!("jj backend: default_branch not yet wired")
    }
}
