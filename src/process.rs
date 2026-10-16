//! Spawning and supervising the external `xwayland-satellite` process.
//!
//! `xwayland-satellite` is a runtime system dependency: the crate never links
//! it, it only spawns the binary and keeps it alive for the lifetime of the
//! service. Everything process related lives behind [`ChildSupervisor`] so the
//! service state machine can be unit tested with a fake child.

use std::{
    process::{Child, Command, ExitStatus},
    thread,
    time::{Duration, Instant},
};

use rustix::process::{Pid, Signal, kill_process};
use tracing::debug;

use crate::config::ServiceConfig;

/// How often the termination path re-checks whether the child exited.
const TERMINATE_POLL_INTERVAL: Duration = Duration::from_millis(50);

/// Something that can supervise the satellite child.
///
/// The trait exists so that the [`crate::service::XwaylandService`] state
/// machine can be exercised without spawning a real process.
pub trait ChildSupervisor {
    /// Return the exit status if the child has already exited.
    ///
    /// # Errors
    ///
    /// Returns the underlying [`std::io::Error`] when the child cannot be
    /// queried.
    fn try_wait(&mut self) -> std::io::Result<Option<ExitStatus>>;

    /// Ask the child to exit and return its status.
    ///
    /// Implementations send `SIGTERM` first, wait up to `grace`, and escalate
    /// to `SIGKILL` afterwards.
    ///
    /// # Errors
    ///
    /// Returns the underlying [`std::io::Error`] when the child cannot be
    /// signalled or waited for.
    fn terminate(&mut self, grace: Duration) -> std::io::Result<ExitStatus>;
}

/// Errors produced by the real satellite process.
#[derive(Debug, thiserror::Error)]
pub enum ProcessError {
    /// The satellite binary could not be spawned.
    #[error("failed to spawn `{program}`")]
    Spawn {
        /// The binary that was supposed to be executed.
        program: String,
        /// The underlying spawn error.
        #[source]
        source: std::io::Error,
    },

    /// Waiting for the satellite failed.
    #[error("failed to wait for the satellite process")]
    Wait(#[source] std::io::Error),

    /// Signalling the satellite failed.
    #[error("failed to signal the satellite process")]
    Signal(#[source] std::io::Error),
}

/// Arguments passed to the satellite binary: display first, then extras.
pub fn satellite_args(config: &ServiceConfig) -> Vec<String> {
    let mut args = Vec::with_capacity(config.satellite_args.len() + 1);

    args.push(config.display.name());
    args.extend(config.satellite_args.iter().cloned());

    args
}

/// A live satellite child.
#[derive(Debug)]
pub struct SatelliteProcess {
    child: Child,
}

impl SatelliteProcess {
    /// Spawn the satellite configured in `config`.
    ///
    /// The child inherits this process' standard streams and environment, so
    /// `WAYLAND_DISPLAY` reaches the satellite unchanged.
    ///
    /// # Errors
    ///
    /// Returns [`ProcessError::Spawn`] when the binary cannot be executed.
    pub fn spawn(config: &ServiceConfig) -> Result<Self, ProcessError> {
        let program = config.satellite_bin.to_string_lossy().into_owned();
        let args = satellite_args(config);

        let child = Command::new(&config.satellite_bin)
            .args(&args)
            .spawn()
            .map_err(|source| ProcessError::Spawn {
                program: program.clone(),
                source,
            })?;

        debug!(pid = child.id(), ?args, %program, "satellite spawned");

        Ok(Self { child })
    }

    /// Process id of the satellite child.
    pub fn id(&self) -> u32 {
        self.child.id()
    }
}

impl ChildSupervisor for SatelliteProcess {
    fn try_wait(&mut self) -> std::io::Result<Option<ExitStatus>> {
        self.child.try_wait()
    }

    fn terminate(&mut self, grace: Duration) -> std::io::Result<ExitStatus> {
        request_termination(&self.child)?;

        let deadline = Instant::now().checked_add(grace);

        loop {
            if let Some(status) = self.child.try_wait()? {
                return Ok(status);
            }

            match deadline {
                Some(deadline) if Instant::now() < deadline => {
                    thread::sleep(TERMINATE_POLL_INTERVAL);
                }
                Some(_) => break,
                // A grace period too large to represent as an `Instant` is not
                // worth waiting for; escalate straight away.
                None => break,
            }
        }

        debug!(
            pid = self.child.id(),
            "satellite ignored SIGTERM, sending SIGKILL"
        );
        self.child.kill()?;
        self.child.wait()
    }
}

/// Send `SIGTERM` to `child`, ignoring the error that means "already gone".
fn request_termination(child: &Child) -> std::io::Result<()> {
    match kill_process(Pid::from_child(child), Signal::TERM) {
        Ok(()) => Ok(()),
        // `ESRCH` is reported as `NotFound` and simply means the child exited
        // between the liveness check and the signal; the wait below reaps it.
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(()),
        Err(error) => Err(error.into()),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    use std::{fs, os::unix::fs::PermissionsExt, path::PathBuf};

    /// Write an executable stand-in for the satellite.
    fn stand_in(dir: &std::path::Path, body: &str) -> PathBuf {
        let script = dir.join("satellite.sh");
        fs::write(&script, body).expect("write stand-in");

        let mut permissions = fs::metadata(&script).expect("metadata").permissions();
        permissions.set_mode(0o755);
        fs::set_permissions(&script, permissions).expect("chmod");

        script
    }

    #[test]
    fn the_display_comes_first_and_extras_follow_in_order() {
        let config = ServiceConfig {
            satellite_args: vec!["--verbose".to_owned(), "extra".to_owned()],
            ..ServiceConfig::default()
        };

        let args = satellite_args(&config);

        assert_eq!(args.len(), 3);
        assert_eq!(args[0], config.display.name());
        assert!(args[0].starts_with(':'));
        assert_eq!(
            args[1..].to_vec(),
            vec!["--verbose".to_owned(), "extra".to_owned()]
        );
    }

    #[test]
    fn a_display_only_configuration_produces_a_single_argument() {
        let config = ServiceConfig {
            display: crate::x11::Display::new(42),
            ..ServiceConfig::default()
        };

        assert_eq!(satellite_args(&config), vec![":42".to_owned()]);
    }

    #[test]
    fn spawning_a_missing_binary_reports_the_program() {
        let config = ServiceConfig {
            satellite_bin: PathBuf::from("/nonexistent/inox-xwayland-satellite"),
            ..ServiceConfig::default()
        };

        let error = SatelliteProcess::spawn(&config).expect_err("spawn must fail");

        assert!(matches!(
            &error,
            ProcessError::Spawn { program, .. } if program == "/nonexistent/inox-xwayland-satellite"
        ));
        assert!(error.to_string().contains("inox-xwayland-satellite"));
    }

    #[test]
    fn terminating_a_running_child_stops_it() {
        let dir = tempfile::tempdir().expect("temp dir");
        let script = stand_in(dir.path(), "#!/bin/sh\nexec sleep 60\n");

        let config = ServiceConfig {
            satellite_bin: script,
            ..ServiceConfig::default()
        };

        let mut process = SatelliteProcess::spawn(&config).expect("spawn");
        assert!(process.id() > 0);

        let status = process
            .terminate(Duration::from_secs(5))
            .expect("terminate");

        // `sleep` honours SIGTERM, so the child is reported as signalled rather
        // than exiting with a code of its own.
        assert!(
            status.code().is_none(),
            "expected a signalled child, got {status:?}"
        );
    }
}
