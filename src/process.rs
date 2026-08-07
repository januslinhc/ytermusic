use std::{
    ffi::OsString,
    fmt, io,
    path::{Path, PathBuf},
    process::Stdio,
    time::Duration,
};

use async_trait::async_trait;
use thiserror::Error;
use tokio::io::{AsyncRead, AsyncReadExt as _};

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct ProcessLimits {
    pub timeout: Duration,
    pub max_stdout_bytes: usize,
    pub max_stderr_bytes: usize,
}

impl Default for ProcessLimits {
    fn default() -> Self {
        Self {
            timeout: Duration::from_secs(10),
            max_stdout_bytes: 1_048_576,
            max_stderr_bytes: 1_048_576,
        }
    }
}

#[derive(Clone, Eq, PartialEq)]
pub struct CommandSpec {
    pub program: PathBuf,
    pub args: Vec<OsString>,
    pub limits: ProcessLimits,
}

pub struct ProcessOutput {
    pub status: std::process::ExitStatus,
    pub stdout: Vec<u8>,
    pub stderr: Vec<u8>,
}

impl fmt::Debug for CommandSpec {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("CommandSpec")
            .field("program", &self.program)
            .field("arg_count", &self.args.len())
            .field("limits", &self.limits)
            .finish()
    }
}

impl fmt::Debug for ProcessOutput {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("ProcessOutput")
            .field("status", &self.status)
            .field("stdout_bytes", &self.stdout.len())
            .field("stderr_bytes", &self.stderr.len())
            .finish()
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum OutputStream {
    Stdout,
    Stderr,
}

impl fmt::Display for OutputStream {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str(match self {
            Self::Stdout => "stdout",
            Self::Stderr => "stderr",
        })
    }
}

#[derive(Debug, Error)]
pub enum ProcessError {
    #[error("could not start {program}: {source}")]
    Spawn {
        program: PathBuf,
        #[source]
        source: io::Error,
    },
    #[error("could not wait for {program}: {source}")]
    Wait {
        program: PathBuf,
        #[source]
        source: io::Error,
    },
    #[error("could not read {stream} from {program}: {source}")]
    Read {
        program: PathBuf,
        stream: OutputStream,
        #[source]
        source: io::Error,
    },
    #[error("{program} did not expose a piped {stream}")]
    CaptureUnavailable {
        program: PathBuf,
        stream: OutputStream,
    },
    #[error("{program} exceeded its {timeout:?} timeout")]
    Timeout { program: PathBuf, timeout: Duration },
    #[error("{program} {stream} exceeded its {limit}-byte capture limit")]
    OutputLimitExceeded {
        program: PathBuf,
        stream: OutputStream,
        limit: usize,
    },
    #[error("could not terminate {program}: {source}")]
    Terminate {
        program: PathBuf,
        #[source]
        source: io::Error,
    },
}

#[async_trait]
pub trait ProcessRunner: Send + Sync {
    /// Executes one program without involving a shell.
    ///
    /// # Errors
    ///
    /// Returns a typed spawn, capture, timeout, output-limit, termination, or
    /// wait error.
    async fn output(&self, spec: CommandSpec) -> Result<ProcessOutput, ProcessError>;
}

#[derive(Clone, Copy, Debug, Default)]
pub struct TokioProcessRunner;

#[async_trait]
impl ProcessRunner for TokioProcessRunner {
    async fn output(&self, spec: CommandSpec) -> Result<ProcessOutput, ProcessError> {
        let mut command = tokio::process::Command::new(&spec.program);
        command
            .args(&spec.args)
            .stdin(Stdio::null())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .kill_on_drop(true);

        let mut child = command.spawn().map_err(|source| ProcessError::Spawn {
            program: spec.program.clone(),
            source,
        })?;

        let Some(stdout) = child.stdout.take() else {
            terminate_and_reap(&mut child, &spec.program).await?;
            return Err(ProcessError::CaptureUnavailable {
                program: spec.program,
                stream: OutputStream::Stdout,
            });
        };
        let Some(stderr) = child.stderr.take() else {
            terminate_and_reap(&mut child, &spec.program).await?;
            return Err(ProcessError::CaptureUnavailable {
                program: spec.program,
                stream: OutputStream::Stderr,
            });
        };

        let completion = tokio::time::timeout(spec.limits.timeout, async {
            tokio::try_join!(
                async { child.wait().await.map_err(CaptureFailure::Wait) },
                read_bounded(stdout, OutputStream::Stdout, spec.limits.max_stdout_bytes),
                read_bounded(stderr, OutputStream::Stderr, spec.limits.max_stderr_bytes),
            )
        })
        .await;

        match completion {
            Ok(Ok((status, stdout, stderr))) => Ok(ProcessOutput {
                status,
                stdout,
                stderr,
            }),
            Ok(Err(failure)) => {
                terminate_and_reap(&mut child, &spec.program).await?;
                Err(failure.into_process_error(spec.program))
            }
            Err(_) => {
                terminate_and_reap(&mut child, &spec.program).await?;
                Err(ProcessError::Timeout {
                    program: spec.program,
                    timeout: spec.limits.timeout,
                })
            }
        }
    }
}

#[derive(Debug)]
enum CaptureFailure {
    Wait(io::Error),
    Read {
        stream: OutputStream,
        source: io::Error,
    },
    Limit {
        stream: OutputStream,
        limit: usize,
    },
}

impl CaptureFailure {
    fn into_process_error(self, program: PathBuf) -> ProcessError {
        match self {
            Self::Wait(source) => ProcessError::Wait { program, source },
            Self::Read { stream, source } => ProcessError::Read {
                program,
                stream,
                source,
            },
            Self::Limit { stream, limit } => ProcessError::OutputLimitExceeded {
                program,
                stream,
                limit,
            },
        }
    }
}

async fn read_bounded(
    mut reader: impl AsyncRead + Unpin,
    stream: OutputStream,
    limit: usize,
) -> Result<Vec<u8>, CaptureFailure> {
    const CHUNK_BYTES: usize = 8_192;

    let mut captured = Vec::with_capacity(limit.min(CHUNK_BYTES));
    let mut chunk = Box::new([0_u8; CHUNK_BYTES]);
    loop {
        let remaining = limit.saturating_sub(captured.len());
        let read_capacity = remaining.saturating_add(1).min(CHUNK_BYTES);
        let read = reader
            .read(&mut chunk[..read_capacity])
            .await
            .map_err(|source| CaptureFailure::Read { stream, source })?;
        if read == 0 {
            return Ok(captured);
        }
        if read > remaining {
            return Err(CaptureFailure::Limit { stream, limit });
        }
        captured.extend_from_slice(&chunk[..read]);
    }
}

async fn terminate_and_reap(
    child: &mut tokio::process::Child,
    program: &Path,
) -> Result<(), ProcessError> {
    match child.try_wait() {
        Ok(Some(_)) => return Ok(()),
        Ok(None) => {}
        Err(source) => {
            return Err(ProcessError::Wait {
                program: program.to_path_buf(),
                source,
            });
        }
    }

    if let Err(source) = child.kill().await
        && source.kind() != io::ErrorKind::InvalidInput
    {
        return Err(ProcessError::Terminate {
            program: program.to_path_buf(),
            source,
        });
    }
    child.wait().await.map_err(|source| ProcessError::Wait {
        program: program.to_path_buf(),
        source,
    })?;
    Ok(())
}

#[derive(Debug, Error)]
pub enum LocatorError {
    #[error("could not locate {executable}: {message}")]
    Lookup { executable: String, message: String },
}

pub trait ExecutableLocator: Send + Sync {
    /// Finds an executable using the host's executable search path.
    ///
    /// # Errors
    ///
    /// Returns an error when the search path cannot be inspected reliably.
    fn find(&self, executable: &str) -> Result<Option<PathBuf>, LocatorError>;
}

#[derive(Clone, Copy, Debug, Default)]
pub struct SystemExecutableLocator;

impl ExecutableLocator for SystemExecutableLocator {
    fn find(&self, executable: &str) -> Result<Option<PathBuf>, LocatorError> {
        match which::which(executable) {
            Ok(path) => Ok(Some(path)),
            Err(which::Error::CannotFindBinaryPath) => Ok(None),
            Err(error) => Err(LocatorError::Lookup {
                executable: executable.to_owned(),
                message: error.to_string(),
            }),
        }
    }
}

impl CommandSpec {
    #[must_use]
    pub fn new(program: impl AsRef<Path>, args: impl IntoIterator<Item = OsString>) -> Self {
        Self {
            program: program.as_ref().to_path_buf(),
            args: args.into_iter().collect(),
            limits: ProcessLimits::default(),
        }
    }

    #[must_use]
    pub const fn with_limits(mut self, limits: ProcessLimits) -> Self {
        self.limits = limits;
        self
    }
}
