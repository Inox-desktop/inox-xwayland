//! The satellite supervision state machine.
//!
//! [`XwaylandService`] owns a satellite child and answers three questions on
//! every poll: did the child exit, is Xwayland ready, and has the readiness
//! deadline passed. Both collaborators are traits, so the state machine is unit
//! tested with a fake child and a fake probe.

use std::{
    io,
    path::{Path, PathBuf},
    process::ExitStatus,
    time::{Duration, Instant},
};

use tracing::warn;

use crate::{
    config::ServiceConfig,
    process::{ChildSupervisor, ProcessError, SatelliteProcess},
    x11::Display,
};

/// Readiness probe abstraction (unit-test seam).
pub trait ReadinessProbe {
    /// Whether Xwayland on `display` accepts X11 connections right now.
    fn is_ready(&self, display: Display) -> bool;
}

/// Probes readiness by connecting to the X11 UNIX socket.
#[derive(Debug, Clone)]
struct SocketProbe {
    dir: PathBuf,
}

impl SocketProbe {
    /// Probe the sockets in `dir`.
    fn new(dir: PathBuf) -> Self {
        Self { dir }
    }
}

impl ReadinessProbe for SocketProbe {
    fn is_ready(&self, display: Display) -> bool {
        display.is_ready_in(&self.dir)
    }
}

/// Lifecycle phase.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Phase {
    /// The satellite is running and Xwayland is not known to be ready.
    Starting,
    /// Xwayland accepts connections on the given display.
    Ready(Display),
    /// A shutdown is in progress.
    Stopping,
    /// The satellite has exited and is no longer supervised.
    Stopped,
}

/// Result of one readiness poll.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Progress {
    /// The satellite is alive but Xwayland is not ready yet.
    Pending,
    /// Xwayland accepts connections on the given display.
    Ready(Display),
    /// The satellite exited on its own with the given status.
    Exited(ExitStatus),
}

/// Errors produced while supervising the satellite.
#[derive(Debug, thiserror::Error)]
pub enum ServiceError {
    /// The satellite process failed.
    #[error(transparent)]
    Process(#[from] ProcessError),

    /// Xwayland did not become ready in time.
    #[error("timed out after {timeout:?} waiting for Xwayland on {display} to become ready")]
    ReadinessTimeout {
        /// The display that never became ready.
        display: Display,
        /// The deadline that was exceeded.
        timeout: Duration,
    },

    /// The readiness file could not be written.
    #[error("failed to write readiness file `{path}`")]
    ReadyFile {
        /// The file that could not be written.
        path: PathBuf,
        /// The underlying I/O error.
        #[source]
        source: std::io::Error,
    },

    /// The calloop event loop could not be set up or driven.
    ///
    /// This is not part of the frozen §3 error list: [`crate::runtime::run`]
    /// has to report event-loop failures through the same type, and an event
    /// loop failure is neither a process, readiness nor readiness-file error.
    #[error("the event loop failed: {reason}")]
    EventLoop {
        /// The rendered calloop error.
        reason: String,
    },
}

/// Supervises one `xwayland-satellite` child.
pub struct XwaylandService {
    config: ServiceConfig,
    supervisor: Box<dyn ChildSupervisor>,
    probe: Box<dyn ReadinessProbe>,
    phase: Phase,
    started: Instant,
    last_status: Option<ExitStatus>,
}

impl XwaylandService {
    /// Spawn the satellite and start supervising. Uses the real process + socket probe.
    ///
    /// # Errors
    ///
    /// Returns [`ServiceError::Process`] when the satellite cannot be spawned.
    pub fn start(config: ServiceConfig) -> Result<Self, ServiceError> {
        let probe = SocketProbe::new(config.socket_dir.clone());
        let supervisor = SatelliteProcess::spawn(&config)?;

        Ok(Self::with_parts(
            config,
            Box::new(supervisor),
            Box::new(probe),
        ))
    }

    /// Build a service from injected parts, started "now".
    ///
    /// This is the unit-test seam: the starting instant is available to
    /// in-module tests through the `started` field, which lets them poll with
    /// synthetic `now` values.
    fn with_parts(
        config: ServiceConfig,
        supervisor: Box<dyn ChildSupervisor>,
        probe: Box<dyn ReadinessProbe>,
    ) -> Self {
        Self {
            config,
            supervisor,
            probe,
            phase: Phase::Starting,
            started: Instant::now(),
            last_status: None,
        }
    }

    /// Poll once: child liveness, then readiness, then the ready-timeout deadline.
    ///
    /// # Errors
    ///
    /// Returns [`ServiceError::ReadinessTimeout`] once the deadline has passed
    /// without Xwayland becoming ready, [`ServiceError::ReadyFile`] when the
    /// readiness file cannot be written, and [`ServiceError::Process`] when the
    /// child cannot be queried.
    pub fn poll(&mut self, now: Instant) -> Result<Progress, ServiceError> {
        if let Phase::Stopped = self.phase {
            // Polling after a stop is a programming error on the caller's side,
            // but it must never panic: report what is already known.
            return Ok(match self.last_status {
                Some(status) => Progress::Exited(status),
                None => Progress::Pending,
            });
        }

        if let Some(status) = self.supervisor.try_wait().map_err(ProcessError::Wait)? {
            self.phase = Phase::Stopped;
            self.last_status = Some(status);

            return Ok(Progress::Exited(status));
        }

        let display = self.config.display;

        if self.probe.is_ready(display) {
            let becoming_ready = self.phase != Phase::Ready(display);
            self.phase = Phase::Ready(display);

            if becoming_ready {
                if let Some(path) = self.config.ready_file.clone() {
                    write_ready_file(&path, display)
                        .map_err(|source| ServiceError::ReadyFile { path, source })?;
                }
            }

            return Ok(Progress::Ready(display));
        }

        if now.saturating_duration_since(self.started) >= self.config.ready_timeout {
            return Err(ServiceError::ReadinessTimeout {
                display,
                timeout: self.config.ready_timeout,
            });
        }

        Ok(Progress::Pending)
    }

    /// Terminate the child, set `Stopped`, return its exit status.
    ///
    /// Calling this more than once is harmless: once the child is stopped, the
    /// already known status is returned without terminating it again.
    ///
    /// # Errors
    ///
    /// Returns [`ServiceError::Process`] when the child cannot be terminated.
    pub fn shutdown(&mut self) -> Result<ExitStatus, ServiceError> {
        if let Phase::Stopped = self.phase {
            if let Some(status) = self.last_status {
                return Ok(status);
            }
        }

        self.phase = Phase::Stopping;

        let status = self
            .supervisor
            .terminate(self.config.shutdown_grace)
            .map_err(ProcessError::Signal)?;

        self.phase = Phase::Stopped;
        self.last_status = Some(status);

        Ok(status)
    }

    /// Current phase.
    pub fn phase(&self) -> Phase {
        self.phase.clone()
    }
}

impl Drop for XwaylandService {
    fn drop(&mut self) {
        if let Phase::Stopped = self.phase {
            return;
        }

        // The service is going away without a clean shutdown (for example
        // because the event loop could not be set up); do not leave the
        // satellite running.
        if let Err(error) = self.supervisor.terminate(self.config.shutdown_grace) {
            warn!(%error, "failed to terminate the satellite while dropping the service");
        }
    }
}

/// Write `"{display}\n"` to `path`.
fn write_ready_file(path: &Path, display: Display) -> io::Result<()> {
    std::fs::write(path, format!("{display}\n"))
}

#[cfg(test)]
mod tests {
    use super::*;

    use std::{cell::Cell, fs, os::unix::process::ExitStatusExt, rc::Rc};

    /// Build an [`ExitStatus`] that looks like `exit(code)`.
    fn exit_status(code: i32) -> ExitStatus {
        ExitStatus::from_raw(code << 8)
    }

    struct FakeSupervisor {
        /// What `try_wait` reports; `None` means "still running".
        exit: Rc<Cell<Option<ExitStatus>>>,
        /// How often `terminate` was called.
        terminations: Rc<Cell<u32>>,
        /// The status `terminate` settles on.
        terminate_status: ExitStatus,
    }

    impl ChildSupervisor for FakeSupervisor {
        fn try_wait(&mut self) -> io::Result<Option<ExitStatus>> {
            Ok(self.exit.get())
        }

        fn terminate(&mut self, _grace: Duration) -> io::Result<ExitStatus> {
            self.terminations.set(self.terminations.get() + 1);

            Ok(self.terminate_status)
        }
    }

    struct FakeProbe {
        ready: Rc<Cell<bool>>,
    }

    impl ReadinessProbe for FakeProbe {
        fn is_ready(&self, _display: Display) -> bool {
            self.ready.get()
        }
    }

    struct Fixture {
        service: XwaylandService,
        ready: Rc<Cell<bool>>,
        exit: Rc<Cell<Option<ExitStatus>>>,
        terminations: Rc<Cell<u32>>,
        timeout: Duration,
        started: Instant,
    }

    impl Fixture {
        fn poll(&mut self, now: Instant) -> Result<Progress, ServiceError> {
            self.service.poll(now)
        }

        fn now(&self) -> Instant {
            self.started
        }
    }

    fn fixture_with(config: ServiceConfig) -> Fixture {
        let ready = Rc::new(Cell::new(false));
        let exit = Rc::new(Cell::new(None));
        let terminations = Rc::new(Cell::new(0));

        let supervisor = FakeSupervisor {
            exit: Rc::clone(&exit),
            terminations: Rc::clone(&terminations),
            terminate_status: exit_status(0),
        };

        let probe = FakeProbe {
            ready: Rc::clone(&ready),
        };

        let timeout = config.ready_timeout;
        let service = XwaylandService::with_parts(config, Box::new(supervisor), Box::new(probe));
        let started = service.started;

        Fixture {
            service,
            ready,
            exit,
            terminations,
            timeout,
            started,
        }
    }

    fn fixture() -> Fixture {
        fixture_with(ServiceConfig {
            ready_timeout: Duration::from_secs(10),
            ..ServiceConfig::default()
        })
    }

    #[test]
    fn polling_goes_from_pending_to_ready() {
        let mut fixture = fixture();
        let display = fixture.service.config.display;

        assert_eq!(
            fixture.poll(fixture.now()).expect("poll"),
            Progress::Pending
        );
        assert_eq!(fixture.service.phase(), Phase::Starting);

        fixture.ready.set(true);

        assert_eq!(
            fixture.poll(fixture.now()).expect("poll"),
            Progress::Ready(display)
        );
        assert_eq!(fixture.service.phase(), Phase::Ready(display));
    }

    #[test]
    fn a_child_exit_wins_over_readiness() {
        let mut fixture = fixture();
        let status = exit_status(7);

        fixture.ready.set(true);
        fixture.exit.set(Some(status));

        assert_eq!(
            fixture.poll(fixture.now()).expect("poll"),
            Progress::Exited(status)
        );
        assert_eq!(fixture.service.phase(), Phase::Stopped);

        // A later poll must not panic and keeps reporting the known status.
        assert_eq!(
            fixture.poll(fixture.now()).expect("poll"),
            Progress::Exited(status)
        );
    }

    #[test]
    fn the_readiness_timeout_fires_exactly_at_the_deadline() {
        let mut fixture = fixture();
        let timeout = fixture.timeout;
        let started = fixture.now();

        assert_eq!(
            fixture
                .poll(started + (timeout - Duration::from_nanos(1)))
                .expect("poll"),
            Progress::Pending
        );

        assert!(matches!(
            fixture.poll(started + timeout),
            Err(ServiceError::ReadinessTimeout { timeout: reported, .. }) if reported == timeout
        ));
    }

    #[test]
    fn the_ready_file_is_written_once_when_ready() {
        let dir = tempfile::tempdir().expect("temp dir");
        let path = dir.path().join("ready");

        let mut fixture = fixture_with(ServiceConfig {
            ready_timeout: Duration::from_secs(10),
            ready_file: Some(path.clone()),
            ..ServiceConfig::default()
        });
        let display = fixture.service.config.display;

        fixture.ready.set(true);
        assert_eq!(
            fixture.poll(fixture.now()).expect("poll"),
            Progress::Ready(display)
        );

        let contents = fs::read_to_string(&path).expect("readiness file must exist");
        assert_eq!(contents.trim(), display.name());
        assert!(contents.ends_with('\n'));

        // Polling again stays ready and keeps the file intact.
        assert_eq!(
            fixture.poll(fixture.now()).expect("poll"),
            Progress::Ready(display)
        );
        assert_eq!(
            fs::read_to_string(&path).expect("readiness file must exist"),
            contents
        );
    }

    #[test]
    fn a_failed_ready_file_write_is_reported() {
        let dir = tempfile::tempdir().expect("temp dir");

        let mut fixture = fixture_with(ServiceConfig {
            ready_timeout: Duration::from_secs(10),
            // The directory itself can never be written as a file.
            ready_file: Some(dir.path().to_path_buf()),
            ..ServiceConfig::default()
        });

        fixture.ready.set(true);

        assert!(matches!(
            fixture.poll(fixture.now()),
            Err(ServiceError::ReadyFile { .. })
        ));
    }

    #[test]
    fn shutdown_terminates_the_child_only_once() {
        let mut fixture = fixture();

        let status = fixture.service.shutdown().expect("shutdown");

        assert_eq!(status.code(), Some(0));
        assert_eq!(fixture.terminations.get(), 1);
        assert_eq!(fixture.service.phase(), Phase::Stopped);

        assert_eq!(
            fixture.service.shutdown().expect("shutdown").code(),
            Some(0)
        );
        assert_eq!(fixture.terminations.get(), 1);
    }

    #[test]
    fn shutdown_after_a_child_exit_does_not_signal_it_again() {
        let mut fixture = fixture();
        let status = exit_status(3);

        fixture.exit.set(Some(status));
        assert_eq!(
            fixture.poll(fixture.now()).expect("poll"),
            Progress::Exited(status)
        );

        assert_eq!(fixture.service.shutdown().expect("shutdown"), status);
        assert_eq!(fixture.terminations.get(), 0);
    }

    #[test]
    fn process_failures_are_mapped_into_service_errors() {
        let error: ServiceError = ProcessError::Wait(io::Error::other("boom")).into();

        assert!(matches!(
            error,
            ServiceError::Process(ProcessError::Wait(_))
        ));
    }
}
