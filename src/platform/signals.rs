use std::io;

use tokio::sync::mpsc;

/// A reusable shutdown-signal subscription for the current platform.
pub struct ShutdownSignals {
    source: SignalSource,
}

enum SignalSource {
    Injected(mpsc::Receiver<()>),
    #[cfg(unix)]
    Unix {
        interrupt: tokio::signal::unix::Signal,
        terminate: tokio::signal::unix::Signal,
        hangup: tokio::signal::unix::Signal,
    },
    #[cfg(not(unix))]
    Platform,
}

impl ShutdownSignals {
    /// Subscribes to the platform's process-termination signals.
    ///
    /// # Errors
    ///
    /// Returns an operating-system error when a signal handler cannot be
    /// registered.
    pub fn new() -> io::Result<Self> {
        #[cfg(unix)]
        {
            use tokio::signal::unix::{SignalKind, signal};

            Ok(Self {
                source: SignalSource::Unix {
                    interrupt: signal(SignalKind::interrupt())?,
                    terminate: signal(SignalKind::terminate())?,
                    hangup: signal(SignalKind::hangup())?,
                },
            })
        }
        #[cfg(not(unix))]
        {
            Ok(Self {
                source: SignalSource::Platform,
            })
        }
    }

    /// Creates a deterministic signal source for an embedding runtime.
    #[must_use]
    pub fn injected(receiver: mpsc::Receiver<()>) -> Self {
        Self {
            source: SignalSource::Injected(receiver),
        }
    }

    /// Waits for one shutdown request.
    ///
    /// # Errors
    ///
    /// Returns an error if the signal subscription closes or the platform
    /// listener fails.
    pub async fn recv(&mut self) -> io::Result<()> {
        match &mut self.source {
            SignalSource::Injected(receiver) => receiver.recv().await.ok_or_else(signal_closed),
            #[cfg(unix)]
            SignalSource::Unix {
                interrupt,
                terminate,
                hangup,
            } => {
                tokio::select! {
                    result = interrupt.recv() => result.ok_or_else(signal_closed),
                    result = terminate.recv() => result.ok_or_else(signal_closed),
                    result = hangup.recv() => result.ok_or_else(signal_closed),
                }
            }
            #[cfg(not(unix))]
            SignalSource::Platform => tokio::signal::ctrl_c().await,
        }
    }
}

fn signal_closed() -> io::Error {
    io::Error::new(io::ErrorKind::BrokenPipe, "shutdown signal source closed")
}
