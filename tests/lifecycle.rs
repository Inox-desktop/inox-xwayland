//! Process-level lifecycle tests for the `inox-xwayland` binary.
//!
//! These tests drive the real binary with a stand-in "satellite": a tiny shell
//! script that ignores the display argument the service passes it. That keeps
//! the tests deterministic and independent of a real XWayland installation
//! while still exercising spawning, readiness probing, signal forwarding and
//! the exit-code policy end to end.

use std::{
    fs,
    os::unix::{fs::PermissionsExt, net::UnixListener},
    path::{Path, PathBuf},
    process::{Child, Command, ExitStatus, Stdio},
    thread,
    time::{Duration, Instant},
};

use rustix::process::{Pid, Signal, kill_process};
use tempfile::TempDir;

/// The binary under test, provided by Cargo.
const BIN: &str = env!("CARGO_BIN_EXE_inox-xwayland");

/// Slack added to every wait so a slow CI machine cannot turn a pass into a hang.
const SLACK: Duration = Duration::from_secs(5);

/// Poll `predicate` until it holds or `timeout` elapses.
fn wait_until<F>(timeout: Duration, mut predicate: F) -> bool
where
    F: FnMut() -> bool,
{
    let deadline = Instant::now() + timeout;

    loop {
        if predicate() {
            return true;
        }

        if Instant::now() >= deadline {
            return false;
        }

        thread::sleep(Duration::from_millis(25));
    }
}

/// Write an executable stand-in for `xwayland-satellite`.
///
/// The script records its own pid so the test can clean up a leaked child, then
/// runs `body`.
fn write_stand_in(dir: &Path, pid_file: &Path, body: &str) -> PathBuf {
    let script = dir.join("satellite.sh");
    let contents = format!("#!/bin/sh\necho $$ > '{}'\n{body}", pid_file.display());

    fs::write(&script, contents).expect("write the stand-in satellite");

    let mut permissions = fs::metadata(&script)
        .expect("stat the stand-in satellite")
        .permissions();
    permissions.set_mode(0o755);
    fs::set_permissions(&script, permissions).expect("make the stand-in satellite executable");

    script
}

/// A running `inox-xwayland` process that is killed if a test fails.
struct ServiceProcess {
    child: Child,
}

impl ServiceProcess {
    /// Start the service with `args`.
    fn start(args: &[String]) -> Self {
        let child = Command::new(BIN)
            .args(args)
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .spawn()
            .expect("failed to start inox-xwayland");

        Self { child }
    }

    /// Pid of the service, for signal delivery.
    fn pid(&self) -> Pid {
        Pid::from_child(&self.child)
    }

    /// Wait up to `timeout` for the service to exit.
    fn wait_for_exit(&mut self, timeout: Duration) -> Option<ExitStatus> {
        let deadline = Instant::now() + timeout;

        loop {
            if let Some(status) = self.child.try_wait().expect("try_wait on the service") {
                return Some(status);
            }

            if Instant::now() >= deadline {
                return None;
            }

            thread::sleep(Duration::from_millis(25));
        }
    }
}

impl Drop for ServiceProcess {
    fn drop(&mut self) {
        if self.child.try_wait().ok().flatten().is_none() {
            let _ = self.child.kill();
            let _ = self.child.wait();
        }
    }
}

/// Kill a stand-in satellite that outlived its service.
///
/// The recorded pid is only signalled when `/proc` still reports a `sleep`
/// process for it, so a recycled pid can never be hit by accident. Where
/// `/proc` is unavailable the cleanup is skipped entirely; a leaked stand-in
/// then just exits on its own.
fn kill_stand_in(pid_file: &Path) {
    let Ok(raw) = fs::read_to_string(pid_file) else {
        return;
    };

    let Ok(raw) = raw.trim().parse::<i32>() else {
        return;
    };

    let Ok(cmdline) = fs::read(format!("/proc/{raw}/cmdline")) else {
        return;
    };

    if !cmdline.starts_with(b"sleep") {
        return;
    }

    if let Some(pid) = Pid::from_raw(raw) {
        let _ = kill_process(pid, Signal::KILL);
    }
}

/// A private temp dir plus a private X11 socket directory.
struct Layout {
    _temp: TempDir,
    socket_dir: PathBuf,
    ready_file: PathBuf,
    pid_file: PathBuf,
}

impl Layout {
    fn new() -> Self {
        let temp = TempDir::new().expect("temp dir");
        let socket_dir = temp.path().join("x11-unix");

        fs::create_dir_all(&socket_dir).expect("create the socket dir");

        Self {
            socket_dir,
            ready_file: temp.path().join("ready"),
            pid_file: temp.path().join("satellite.pid"),
            _temp: temp,
        }
    }

    fn script_dir(&self) -> &Path {
        self._temp.path()
    }

    fn socket_path(&self, display: u16) -> PathBuf {
        self.socket_dir.join(format!("X{display}"))
    }

    fn base_args(&self, display: u16, satellite: &Path) -> Vec<String> {
        vec![
            "--display".to_owned(),
            format!(":{display}"),
            "--satellite".to_owned(),
            satellite.display().to_string(),
            "--x11-socket-dir".to_owned(),
            self.socket_dir.display().to_string(),
        ]
    }
}

#[test]
fn the_service_becomes_ready_and_shuts_down_cleanly_on_sigterm() {
    let layout = Layout::new();
    let display = 97;

    // A listening socket is what the readiness probe connects to.
    let _listener = UnixListener::bind(layout.socket_path(display)).expect("bind the X socket");

    let satellite = write_stand_in(layout.script_dir(), &layout.pid_file, "exec sleep 300\n");

    let mut args = layout.base_args(display, &satellite);
    args.extend([
        "--ready-timeout".to_owned(),
        "10".to_owned(),
        "--shutdown-grace".to_owned(),
        "5".to_owned(),
        "--ready-file".to_owned(),
        layout.ready_file.display().to_string(),
    ]);

    let mut service = ServiceProcess::start(&args);

    assert!(
        wait_until(Duration::from_secs(10), || layout.ready_file.exists()),
        "the service never wrote its readiness file"
    );

    let contents = fs::read_to_string(&layout.ready_file).expect("read the readiness file");
    assert_eq!(contents.trim(), format!(":{display}"));

    kill_process(service.pid(), Signal::TERM).expect("signal the service");

    let status = service
        .wait_for_exit(SLACK + Duration::from_secs(6))
        .expect("the service did not exit after SIGTERM");

    assert_eq!(
        status.code(),
        Some(0),
        "expected a clean shutdown, got {status:?}"
    );

    kill_stand_in(&layout.pid_file);
}

#[test]
fn a_missing_x_socket_times_out_with_a_non_zero_exit() {
    let layout = Layout::new();
    let display = 98;

    // No listener is created: Xwayland never becomes ready.
    let satellite = write_stand_in(layout.script_dir(), &layout.pid_file, "exec sleep 300\n");

    let mut args = layout.base_args(display, &satellite);
    args.extend([
        "--ready-timeout".to_owned(),
        "1".to_owned(),
        "--shutdown-grace".to_owned(),
        "5".to_owned(),
    ]);

    let mut service = ServiceProcess::start(&args);

    let status = service
        .wait_for_exit(SLACK + Duration::from_secs(6))
        .expect("the service did not give up on readiness");

    assert_ne!(
        status.code(),
        Some(0),
        "expected a failure exit code, got {status:?}"
    );

    kill_stand_in(&layout.pid_file);
}

#[test]
fn an_early_satellite_exit_is_reported_through_the_exit_code() {
    let layout = Layout::new();
    let display = 99;

    let satellite = write_stand_in(layout.script_dir(), &layout.pid_file, "exit 7\n");

    let mut args = layout.base_args(display, &satellite);
    args.extend([
        "--ready-timeout".to_owned(),
        "10".to_owned(),
        "--shutdown-grace".to_owned(),
        "5".to_owned(),
    ]);

    let mut service = ServiceProcess::start(&args);

    let status = service
        .wait_for_exit(SLACK + Duration::from_secs(6))
        .expect("the service did not notice the satellite exit");

    assert_eq!(
        status.code(),
        Some(7),
        "expected the satellite's exit code, got {status:?}"
    );
}

#[test]
fn a_signalled_satellite_is_reported_as_a_failure() {
    let layout = Layout::new();
    let display = 96;

    // The stand-in kills itself, so the service sees a child without an exit
    // code (killed by a signal) and must not report success.
    let satellite = write_stand_in(layout.script_dir(), &layout.pid_file, "kill -KILL $$\n");

    let mut args = layout.base_args(display, &satellite);
    args.extend([
        "--ready-timeout".to_owned(),
        "10".to_owned(),
        "--shutdown-grace".to_owned(),
        "5".to_owned(),
    ]);

    let mut service = ServiceProcess::start(&args);

    let status = service
        .wait_for_exit(SLACK + Duration::from_secs(6))
        .expect("the service did not notice the signalled child");

    assert_ne!(
        status.code(),
        Some(0),
        "a signalled satellite must not look like a clean shutdown, got {status:?}"
    );
}
