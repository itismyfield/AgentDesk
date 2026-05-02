use std::ffi::{OsStr, OsString};
use std::fmt;
use std::io::{self, Read, Write};
use std::path::{Path, PathBuf};
use std::process::{ExitStatus, Output, Stdio};
use std::thread;
use std::time::{Duration, Instant};

const DEFAULT_GIT_TIMEOUT: Duration = Duration::from_secs(30);
const POLL_INTERVAL: Duration = Duration::from_millis(20);
const STDERR_PREVIEW_BYTES: usize = 4096;

#[derive(Debug)]
pub struct GitCommand {
    repo: Option<PathBuf>,
    args: Vec<OsString>,
    timeout: Duration,
    clear_env: bool,
    envs: Vec<(OsString, OsString)>,
}

impl Default for GitCommand {
    fn default() -> Self {
        Self::new()
    }
}

impl GitCommand {
    pub fn new() -> Self {
        Self {
            repo: None,
            args: Vec::new(),
            timeout: DEFAULT_GIT_TIMEOUT,
            clear_env: false,
            envs: Vec::new(),
        }
    }

    pub fn repo(mut self, repo: impl AsRef<Path>) -> Self {
        self.repo = Some(repo.as_ref().to_path_buf());
        self
    }

    pub fn arg(mut self, arg: impl AsRef<OsStr>) -> Self {
        self.args.push(arg.as_ref().to_os_string());
        self
    }

    pub fn args<I, S>(mut self, args: I) -> Self
    where
        I: IntoIterator<Item = S>,
        S: AsRef<OsStr>,
    {
        self.args
            .extend(args.into_iter().map(|arg| arg.as_ref().to_os_string()));
        self
    }

    pub fn timeout(mut self, timeout: Duration) -> Self {
        self.timeout = timeout;
        self
    }

    #[allow(dead_code)]
    pub fn env_clear(mut self) -> Self {
        self.clear_env = true;
        self
    }

    #[allow(dead_code)]
    pub fn env(mut self, key: impl AsRef<OsStr>, value: impl AsRef<OsStr>) -> Self {
        self.envs
            .push((key.as_ref().to_os_string(), value.as_ref().to_os_string()));
        self
    }

    pub fn run_text(self) -> Result<String, GitCommandError> {
        let output = self.run_output()?;
        Ok(String::from_utf8_lossy(&output.stdout).to_string())
    }

    pub fn run_output(self) -> Result<Output, GitCommandError> {
        self.run_output_inner(None)
    }

    pub fn run_output_with_stdin(self, stdin: impl AsRef<[u8]>) -> Result<Output, GitCommandError> {
        self.run_output_inner(Some(stdin.as_ref().to_vec()))
    }

    fn run_output_inner(self, stdin: Option<Vec<u8>>) -> Result<Output, GitCommandError> {
        let context = GitCommandContext::new(&self);
        let args_for_span = context.args.join(" ");
        let repo_for_span = context
            .repo
            .as_ref()
            .map(|path| path.display().to_string())
            .unwrap_or_else(|| "<current>".to_string());
        let span = tracing::debug_span!(
            "git.command",
            repo = %repo_for_span,
            args = %args_for_span,
            timeout_ms = self.timeout.as_millis()
        );
        let _guard = span.enter();

        let mut command = super::git_command();
        command.args(&self.args);
        if let Some(repo) = &self.repo {
            command.current_dir(repo);
        }
        if self.clear_env {
            command.env_clear();
        }
        for (key, value) in &self.envs {
            command.env(key, value);
        }
        command.stdin(if stdin.is_some() {
            Stdio::piped()
        } else {
            Stdio::null()
        });
        command.stdout(Stdio::piped());
        command.stderr(Stdio::piped());

        let mut child = command
            .spawn()
            .map_err(|source| GitCommandError::spawn(context.clone(), source))?;

        let stdout_handle = child.stdout.take().map(read_pipe);
        let stderr_handle = child.stderr.take().map(read_pipe);

        if let Some(input) = stdin {
            if let Some(mut child_stdin) = child.stdin.take() {
                child_stdin
                    .write_all(&input)
                    .map_err(|source| GitCommandError::stdin(context.clone(), source))?;
            }
        }

        let started_at = Instant::now();
        let status = loop {
            match child.try_wait() {
                Ok(Some(status)) => break status,
                Ok(None) => {
                    if started_at.elapsed() >= self.timeout {
                        let _ = child.kill();
                        let status = child.wait().ok();
                        let stdout = join_pipe(stdout_handle);
                        let stderr = join_pipe(stderr_handle);
                        return Err(GitCommandError::timed_out(
                            context,
                            self.timeout,
                            status,
                            stdout,
                            stderr,
                        ));
                    }
                    thread::sleep(POLL_INTERVAL);
                }
                Err(source) => {
                    let _ = child.kill();
                    let stdout = join_pipe(stdout_handle);
                    let stderr = join_pipe(stderr_handle);
                    return Err(GitCommandError::wait(context, source, stdout, stderr));
                }
            }
        };

        let stdout = join_pipe(stdout_handle);
        let stderr = join_pipe(stderr_handle);
        let output = Output {
            status,
            stdout,
            stderr,
        };
        tracing::debug!(status = %output.status, elapsed_ms = started_at.elapsed().as_millis(), "git command finished");

        if !output.status.success() {
            return Err(GitCommandError::status(context, output));
        }

        Ok(output)
    }
}

fn read_pipe(mut pipe: impl Read + Send + 'static) -> thread::JoinHandle<Vec<u8>> {
    thread::spawn(move || {
        let mut buf = Vec::new();
        let _ = pipe.read_to_end(&mut buf);
        buf
    })
}

fn join_pipe(handle: Option<thread::JoinHandle<Vec<u8>>>) -> Vec<u8> {
    handle
        .and_then(|handle| handle.join().ok())
        .unwrap_or_default()
}

#[derive(Debug, Clone)]
struct GitCommandContext {
    args: Vec<String>,
    repo: Option<PathBuf>,
}

impl GitCommandContext {
    fn new(command: &GitCommand) -> Self {
        Self {
            args: command
                .args
                .iter()
                .map(|arg| arg.to_string_lossy().to_string())
                .collect(),
            repo: command.repo.clone(),
        }
    }

    fn command_line(&self) -> String {
        if self.args.is_empty() {
            "git".to_string()
        } else {
            format!("git {}", self.args.join(" "))
        }
    }
}

#[derive(Debug)]
pub struct GitCommandError {
    kind: GitCommandErrorKind,
    context: GitCommandContext,
    timeout: Option<Duration>,
    status: Option<ExitStatus>,
    stdout: Vec<u8>,
    stderr: Vec<u8>,
    source: Option<io::Error>,
}

impl GitCommandError {
    fn spawn(context: GitCommandContext, source: io::Error) -> Self {
        Self {
            kind: GitCommandErrorKind::Spawn,
            context,
            timeout: None,
            status: None,
            stdout: Vec::new(),
            stderr: Vec::new(),
            source: Some(source),
        }
    }

    fn stdin(context: GitCommandContext, source: io::Error) -> Self {
        Self {
            kind: GitCommandErrorKind::Stdin,
            context,
            timeout: None,
            status: None,
            stdout: Vec::new(),
            stderr: Vec::new(),
            source: Some(source),
        }
    }

    fn wait(
        context: GitCommandContext,
        source: io::Error,
        stdout: Vec<u8>,
        stderr: Vec<u8>,
    ) -> Self {
        Self {
            kind: GitCommandErrorKind::Wait,
            context,
            timeout: None,
            status: None,
            stdout,
            stderr,
            source: Some(source),
        }
    }

    fn timed_out(
        context: GitCommandContext,
        timeout: Duration,
        status: Option<ExitStatus>,
        stdout: Vec<u8>,
        stderr: Vec<u8>,
    ) -> Self {
        Self {
            kind: GitCommandErrorKind::Timeout,
            context,
            timeout: Some(timeout),
            status,
            stdout,
            stderr,
            source: None,
        }
    }

    fn status(context: GitCommandContext, output: Output) -> Self {
        Self {
            kind: GitCommandErrorKind::Status,
            context,
            timeout: None,
            status: Some(output.status),
            stdout: output.stdout,
            stderr: output.stderr,
            source: None,
        }
    }

    pub fn status_code(&self) -> Option<i32> {
        self.status.and_then(|status| status.code())
    }

    #[allow(dead_code)]
    pub fn stdout(&self) -> &[u8] {
        &self.stdout
    }

    pub fn stderr(&self) -> &[u8] {
        &self.stderr
    }

    #[allow(dead_code)]
    pub fn stderr_text(&self) -> String {
        String::from_utf8_lossy(&self.stderr).trim().to_string()
    }

    #[allow(dead_code)]
    pub fn timed_out_flag(&self) -> bool {
        matches!(self.kind, GitCommandErrorKind::Timeout)
    }
}

impl fmt::Display for GitCommandError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{}", self.context.command_line())?;
        if let Some(repo) = &self.context.repo {
            write!(f, " in {}", repo.display())?;
        }
        match self.kind {
            GitCommandErrorKind::Spawn => write!(f, " failed to start")?,
            GitCommandErrorKind::Stdin => write!(f, " failed to receive stdin")?,
            GitCommandErrorKind::Wait => write!(f, " failed while waiting")?,
            GitCommandErrorKind::Timeout => {
                let timeout = self.timeout.unwrap_or(DEFAULT_GIT_TIMEOUT);
                write!(f, " timed out after {}ms", timeout.as_millis())?;
            }
            GitCommandErrorKind::Status => {
                if let Some(status) = self.status {
                    write!(f, " failed with status {status}")?;
                } else {
                    write!(f, " failed")?;
                }
            }
        }
        if let Some(source) = &self.source {
            write!(f, ": {source}")?;
        }
        if !self.stderr.is_empty() {
            write!(f, ": stderr: {}", preview_text(&self.stderr))?;
        }
        Ok(())
    }
}

impl std::error::Error for GitCommandError {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        self.source
            .as_ref()
            .map(|source| source as &(dyn std::error::Error + 'static))
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum GitCommandErrorKind {
    Spawn,
    Stdin,
    Wait,
    Timeout,
    Status,
}

fn preview_text(bytes: &[u8]) -> String {
    let text = String::from_utf8_lossy(bytes);
    let mut preview = text.trim().to_string();
    if preview.len() > STDERR_PREVIEW_BYTES {
        preview.truncate(STDERR_PREVIEW_BYTES);
        preview.push_str("...");
    }
    preview
}

#[cfg(test)]
mod tests {
    use super::GitCommand;
    use std::time::Duration;

    #[test]
    fn run_text_captures_stdout() {
        let version = GitCommand::new()
            .timeout(Duration::from_secs(5))
            .args(["--version"])
            .run_text()
            .expect("git --version should run");

        assert!(version.starts_with("git version "));
    }

    #[test]
    fn status_error_preserves_stderr() {
        let error = GitCommand::new()
            .timeout(Duration::from_secs(5))
            .args(["rev-parse", "--verify", "definitely-not-a-real-ref"])
            .run_output()
            .expect_err("invalid ref should fail");

        assert!(!error.stderr().is_empty());
        assert!(error.status_code().is_some());
    }
}
