use std::collections::HashMap;
use std::convert::TryInto;
use std::ffi::OsString;
use std::io::{stderr, stdout, Write};
use std::path::PathBuf;
use std::process::{Command, ExitStatus, Stdio};

use anyhow::Context;
use fn_error_context::context;

use crate::core::eventlog::{EventTransactionId, BRANCHLESS_TRANSACTION_ID_ENV_VAR};
use crate::util::get_sh;

/// Path to the `git` executable on disk to be executed.
#[derive(Clone, Debug)]
pub struct GitRunInfo {
    /// The path to the Git executable on disk.
    pub path_to_git: PathBuf,

    /// The working directory that the Git executable should be run in.
    pub working_directory: PathBuf,

    /// The environment variables that should be passed to the Git process.
    pub env: HashMap<OsString, OsString>,
}

impl GitRunInfo {
    /// Run Git in a subprocess, and inform the user.
    ///
    /// This is suitable for commands which affect the working copy or should run
    /// hooks. We don't want our process to be responsible for that.
    ///
    /// `args` contains the list of arguments to pass to Git, not including the Git
    /// executable itself.
    ///
    /// Returns the exit code of Git (non-zero signifies error).
    #[context("Running Git ({:?}) with args: {:?}", &self, args)]
    #[must_use = "The return code for `run_git` must be checked"]
    pub fn run<S: AsRef<str> + std::fmt::Debug>(
        &self,
        event_tx_id: Option<EventTransactionId>,
        args: &[S],
    ) -> anyhow::Result<isize> {
        let GitRunInfo {
            path_to_git,
            working_directory,
            env,
        } = self;
        println!(
            "branchless: {} {}",
            path_to_git.to_string_lossy(),
            args.iter()
                .map(|arg| arg.as_ref())
                .collect::<Vec<_>>()
                .join(" ")
        );
        stdout().flush()?;
        stderr().flush()?;

        let mut command = Command::new(path_to_git);
        command.current_dir(working_directory);
        command.args(args.iter().map(|arg| arg.as_ref()));
        command.env_clear();
        command.envs(env.iter());
        if let Some(event_tx_id) = event_tx_id {
            command.env(BRANCHLESS_TRANSACTION_ID_ENV_VAR, event_tx_id.to_string());
        }
        let mut child = command
            .spawn()
            .with_context(|| format!("Spawning Git subrocess: {:?} {:?}", path_to_git, args))?;
        let exit_status = child.wait().with_context(|| {
            format!(
                "Waiting for Git subprocess to complete: {:?} {:?}",
                path_to_git, args
            )
        })?;

        // On Unix, if the child process was terminated by a signal, we need to call
        // some Unix-specific functions to access the signal that terminated it. For
        // simplicity, just return `1` in those cases.
        let exit_code = exit_status.code().unwrap_or(1);
        let exit_code = exit_code
            .try_into()
            .with_context(|| format!("Converting exit code {} from i32 to isize", exit_code))?;
        Ok(exit_code)
    }

    /// Run Git silently (don't display output to the user).
    ///
    /// Whenever possible, use `git2`'s bindings to Git instead, as they're
    /// considerably more lightweight and reliable.
    ///
    /// Returns the stdout of the Git invocation.
    pub fn run_silent<S: AsRef<str> + std::fmt::Debug>(
        &self,
        repo: &git2::Repository,
        event_tx_id: Option<EventTransactionId>,
        args: &[S],
    ) -> anyhow::Result<String> {
        let GitRunInfo {
            path_to_git,
            working_directory,
            env,
        } = self;

        // Technically speaking, we should be able to work with non-UTF-8 repository
        // paths. Need to make the typechecker accept it.
        let repo_path = repo.path();
        let repo_path = repo_path.to_str().ok_or_else(|| {
            anyhow::anyhow!(
                "Path to Git repo could not be converted to UTF-8 string: {:?}",
                repo_path
            )
        })?;

        let args = {
            let mut result = vec!["-C", repo_path];
            result.extend(args.iter().map(|arg| arg.as_ref()));
            result
        };
        let mut command = Command::new(path_to_git);
        command.args(&args);
        command.current_dir(working_directory);
        command.env_clear();
        command.envs(env.iter());
        if let Some(event_tx_id) = event_tx_id {
            command.env(BRANCHLESS_TRANSACTION_ID_ENV_VAR, event_tx_id.to_string());
        }
        let result = command
            .output()
            .with_context(|| format!("Spawning Git subprocess: {:?} {:?}", path_to_git, args))?;
        let result = String::from_utf8(result.stdout).with_context(|| {
            format!(
                "Decoding stdout from Git subprocess: {:?} {:?}",
                path_to_git, args
            )
        })?;
        Ok(result)
    }

    /// Run a provided Git hook if it exists for the repository.
    ///
    /// See the man page for `githooks(5)` for more detail on Git hooks.
    #[context("Running Git hook: {}", hook_name)]
    pub fn run_hook(
        &self,
        repo: &git2::Repository,
        hook_name: &str,
        event_tx_id: EventTransactionId,
        args: &[impl AsRef<str>],
        stdin: Option<String>,
    ) -> anyhow::Result<()> {
        let hook_dir = repo
            .config()?
            .get_path("core.hooksPath")
            .unwrap_or_else(|_| repo.path().join("hooks"));

        let GitRunInfo {
            // We're calling a Git hook, but not Git itself.
            path_to_git: _,
            // We always want to call the hook in the Git working copy,
            // regardless of where the Git executable was invoked.
            working_directory: _,
            env,
        } = self;
        let path = {
            let mut path_components: Vec<PathBuf> = vec![std::fs::canonicalize(&hook_dir)?];
            if let Some(path) = env.get(&OsString::from("PATH")) {
                path_components.extend(std::env::split_paths(path));
            }
            std::env::join_paths(path_components)?
        };

        if hook_dir.join(hook_name).exists() {
            let mut child = Command::new(get_sh().context("shell needed to run hook")?)
                // From `githooks(5)`: Before Git invokes a hook, it changes its
                // working directory to either $GIT_DIR in a bare repository or the
                // root of the working tree in a non-bare repository.
                .current_dir(repo.workdir().unwrap_or_else(|| repo.path()))
                .arg("-c")
                .arg(format!("{} \"$@\"", hook_name))
                .arg(hook_name) // "$@" expands "$1" "$2" "$3" ... but we also must specify $0.
                .args(args.iter().map(|arg| arg.as_ref()))
                .env_clear()
                .envs(env.iter())
                .env(BRANCHLESS_TRANSACTION_ID_ENV_VAR, event_tx_id.to_string())
                .env("PATH", &path)
                .stdin(Stdio::piped())
                .spawn()
                .with_context(|| format!("Invoking {} hook with PATH: {:?}", &hook_name, &path))?;

            if let Some(stdin) = stdin {
                write!(child.stdin.as_mut().unwrap(), "{}", stdin)
                    .with_context(|| "Writing hook process stdin")?;
            }

            let _ignored: ExitStatus = child.wait()?;
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use crate::testing::make_git;

    #[test]
    fn test_hook_working_dir() -> anyhow::Result<()> {
        let git = make_git()?;

        if !git.supports_reference_transactions()? {
            return Ok(());
        }

        git.init_repo()?;
        git.commit_file("test1", 1)?;

        std::fs::write(
            git.repo_path
                .join(".git")
                .join("hooks")
                .join("post-rewrite"),
            r#"#!/bin/sh
                   # This won't work unless we're running the hook in the Git working copy.
                   echo "Contents of test1.txt:"
                   cat test1.txt
                   "#,
        )?;

        {
            // Trigger the `post-rewrite` hook that we wrote above.
            let (stdout, stderr) = git.run(&["commit", "--amend", "-m", "foo"])?;
            insta::assert_snapshot!(stderr, @r###"
                branchless: processing 2 updates to branches/refs
                branchless: processing commit
                Contents of test1.txt:
                test1 contents
                "###);
            insta::assert_snapshot!(stdout, @r###"
                [master f23bf8f] foo
                 Date: Thu Oct 29 12:34:56 2020 -0100
                 1 file changed, 1 insertion(+)
                 create mode 100644 test1.txt
                "###);
        }

        Ok(())
    }
}