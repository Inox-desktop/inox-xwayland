//! Headless end-to-end test for `inox-xwayland`.
//!
//! This test proves the real integration path rather than a mocked one: a
//! headless Wayland compositor (`weston`), the `inox-xwayland` service
//! supervising the external `xwayland-satellite` process (which in turn spawns
//! `Xwayland`), and a genuine X11 client (`xeyes`) mapping a window that we
//! observe through `xwininfo`.
//!
//! The test is opt-in because the regular CI job does not have the required
//! system tooling. It only runs when `INOX_XWAYLAND_E2E=1` is set; otherwise it
//! prints a message and returns so the default `cargo test` run stays green.
//! When enabled it requires `weston`, `xwayland-satellite`, `xeyes` and
//! `xwininfo` on `PATH` and fails loudly if any of them is missing.
//!
//! Every step is bounded by a timeout so a broken environment can never hang CI.
//! Each long-running child writes to its own log file inside the private
//! runtime directory; those logs are included in the panic message if a step
//! fails, which keeps the CI log quiet on success but debuggable on failure.

use std::fs::{self, File};
use std::io::Read;
use std::os::unix::fs::PermissionsExt;
use std::os::unix::process::CommandExt;
use std::path::{Path, PathBuf};
use std::process::{Child, Command, ExitStatus, Stdio};
use std::thread;
use std::time::{Duration, Instant};

use rustix::process::{Pid, Signal, kill_process, kill_process_group};

/// Environment variable that opts in to the real end-to-end test.
const E2E_ENV: &str = "INOX_XWAYLAND_E2E";
/// Wayland socket name advertised by the headless compositor.
const WAYLAND_SOCKET: &str = "inox-e2e";
/// X11 display exercised by the test.
const X_DISPLAY: &str = ":88";

/// How long to wait for the compositor to create its Wayland socket.
const WESTON_START_TIMEOUT: Duration = Duration::from_secs(20);
/// How long to wait for the service's readiness file (the service times out a
/// little sooner, so a stuck satellite surfaces as an early service exit).
const READY_TIMEOUT: Duration = Duration::from_secs(30);
/// How long to wait for the X11 window to appear.
const XWININFO_TIMEOUT: Duration = Duration::from_secs(20);
/// How long to wait for a clean service exit after SIGTERM.
const TERMINATE_TIMEOUT: Duration = Duration::from_secs(15);
/// Poll interval shared by every wait loop.
const POLL_INTERVAL: Duration = Duration::from_millis(100);
/// Grace period used when force-killing helpers during teardown.
const KILL_GRACE: Duration = Duration::from_secs(5);

/// Discard a value we intentionally do not act on (best-effort teardown).
fn ignore<T>(_value: T) {}

/// A spawned child process that is terminated, with its process group, on drop.
struct Proc {
    name: &'static str,
    child: Child,
    log: PathBuf,
}

impl Proc {
    /// Spawn `command` as the leader of a fresh process group, logging both
    /// stdout and stderr to `log`.
    ///
    /// A dedicated group lets teardown reap grandchildren as well (for example
    /// the `Xwayland` process spawned by `xwayland-satellite`).
    fn spawn(name: &'static str, mut command: Command, log: &Path) -> Self {
        let stdout = File::create(log)
            .unwrap_or_else(|error| panic!("failed to create {}: {error}", log.display()));
        // `try_clone` dups the fd, so stdout and stderr share one offset and
        // interleave correctly instead of overwriting each other.
        let stderr = stdout
            .try_clone()
            .unwrap_or_else(|error| panic!("failed to duplicate {}: {error}", log.display()));
        command
            .stdin(Stdio::null())
            .stdout(Stdio::from(stdout))
            .stderr(Stdio::from(stderr))
            .process_group(0);

        let child = command
            .spawn()
            .unwrap_or_else(|error| panic!("failed to spawn {name}: {error}"));
        Self {
            name,
            child,
            log: log.to_path_buf(),
        }
    }

    /// Return the child's exit status if it has already exited.
    fn try_wait(&mut self) -> Option<ExitStatus> {
        self.child
            .try_wait()
            .unwrap_or_else(|error| panic!("failed to poll {}: {error}", self.name))
    }

    /// Wait up to `timeout` for the child to exit, polling at a fixed interval.
    fn wait_timeout(&mut self, timeout: Duration) -> Option<ExitStatus> {
        let deadline = Instant::now() + timeout;
        loop {
            match self.child.try_wait() {
                Ok(Some(status)) => return Some(status),
                Ok(None) => {}
                Err(error) => panic!("failed to poll {}: {error}", self.name),
            }
            if Instant::now() >= deadline {
                return None;
            }
            thread::sleep(POLL_INTERVAL);
        }
    }

    /// Send `signal` to the child process only (best effort).
    ///
    /// Used to exercise the service's own SIGTERM handling: signalling the
    /// whole group would kill the satellite out-of-band, and the service could
    /// then report `ChildExited` (a fatal signal, i.e. no exit code) instead of
    /// finishing its clean shutdown with status 0.
    fn signal(&self, signal: Signal) {
        ignore(kill_process(Pid::from_child(&self.child), signal));
    }

    /// Send `signal` to the child and its whole process group (teardown).
    fn signal_group(&self, signal: Signal) {
        let pid = Pid::from_child(&self.child);
        ignore(kill_process(pid, signal));
        ignore(kill_process_group(pid, signal));
    }

    /// Best-effort dump of this child's captured output, for failure messages.
    fn diagnostics(&self) -> String {
        match fs::read_to_string(&self.log) {
            Ok(contents) if !contents.trim().is_empty() => {
                format!(
                    "--- {} log ({}) ---\n{contents}",
                    self.name,
                    self.log.display()
                )
            }
            Ok(_) => format!(
                "--- {} log ({}) is empty ---",
                self.name,
                self.log.display()
            ),
            Err(error) => format!(
                "--- {} log ({}) could not be read: {error} ---",
                self.name,
                self.log.display()
            ),
        }
    }
}

impl Drop for Proc {
    fn drop(&mut self) {
        // Never panic while unwinding: use raw, non-panicking polling here.
        if matches!(self.child.try_wait(), Ok(Some(_))) {
            return;
        }
        self.signal_group(Signal::TERM);

        let deadline = Instant::now() + KILL_GRACE;
        loop {
            if matches!(self.child.try_wait(), Ok(Some(_))) {
                return;
            }
            if Instant::now() >= deadline {
                break;
            }
            thread::sleep(POLL_INTERVAL);
        }

        self.signal_group(Signal::KILL);
        ignore(self.child.kill());
        ignore(self.child.wait());
    }
}

/// Captured output of a short-lived command.
struct Captured {
    status: ExitStatus,
    stdout: String,
    stderr: String,
}

/// Run `program` with `args`, capturing stdout/stderr, bounded by `timeout`.
fn run_captured(
    program: &str,
    args: &[&str],
    env: &[(String, String)],
    timeout: Duration,
) -> Captured {
    let mut command = Command::new(program);
    command
        .args(args)
        .envs(env.iter().cloned())
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped());
    let mut child = command
        .spawn()
        .unwrap_or_else(|error| panic!("failed to spawn {program}: {error}"));

    let mut stdout_pipe = child.stdout.take().expect("stdout is piped");
    let mut stderr_pipe = child.stderr.take().expect("stderr is piped");
    let stdout_reader = thread::spawn(move || read_all(&mut stdout_pipe));
    let stderr_reader = thread::spawn(move || read_all(&mut stderr_pipe));

    let deadline = Instant::now() + timeout;
    let status = loop {
        match child.try_wait() {
            Ok(Some(status)) => break status,
            Ok(None) => {}
            Err(error) => panic!("failed to poll {program}: {error}"),
        }
        if Instant::now() >= deadline {
            ignore(child.kill());
            ignore(child.wait());
            panic!("{program} did not finish within {timeout:?}");
        }
        thread::sleep(POLL_INTERVAL);
    };

    Captured {
        status,
        stdout: stdout_reader.join().expect("stdout reader panicked"),
        stderr: stderr_reader.join().expect("stderr reader panicked"),
    }
}

/// Read a stream to a `String`, replacing invalid UTF-8.
fn read_all(reader: &mut impl Read) -> String {
    let mut buffer = Vec::new();
    ignore(reader.read_to_end(&mut buffer));
    String::from_utf8_lossy(&buffer).into_owned()
}

/// Create a private directory with mode 0700 (as `XDG_RUNTIME_DIR` requires).
fn private_dir(prefix: &str) -> tempfile::TempDir {
    let dir = tempfile::Builder::new()
        .prefix(prefix)
        .tempdir()
        .unwrap_or_else(|error| panic!("failed to create temp dir {prefix}: {error}"));
    fs::set_permissions(dir.path(), fs::Permissions::from_mode(0o700))
        .unwrap_or_else(|error| panic!("failed to chmod {}: {error}", dir.path().display()));
    dir
}

/// Environment shared by the compositor, the service and the X11 client.
fn base_env(runtime_dir: &Path, config_dir: &Path) -> Vec<(String, String)> {
    let config = config_dir.display().to_string();
    vec![
        (
            "XDG_RUNTIME_DIR".to_owned(),
            runtime_dir.display().to_string(),
        ),
        ("XDG_CONFIG_HOME".to_owned(), config.clone()),
        ("HOME".to_owned(), config),
        ("WAYLAND_DISPLAY".to_owned(), WAYLAND_SOCKET.to_owned()),
        // The CI runner has no GPU; force Mesa's llvmpipe software rasteriser.
        ("LIBGL_ALWAYS_SOFTWARE".to_owned(), "1".to_owned()),
    ]
}

/// Locate an executable on `PATH`.
fn which(tool: &str) -> Option<PathBuf> {
    let path = std::env::var_os("PATH")?;
    std::env::split_paths(&path)
        .map(|dir| dir.join(tool))
        .find(|candidate| is_executable(candidate))
}

/// Whether `path` is an executable regular file.
fn is_executable(path: &Path) -> bool {
    fs::metadata(path)
        .map(|meta| meta.is_file() && meta.permissions().mode() & 0o111 != 0)
        .unwrap_or(false)
}

/// Fail with a clear message when a required tool is missing from `PATH`.
fn require_tools(tools: &[&str]) {
    let missing: Vec<&str> = tools
        .iter()
        .copied()
        .filter(|tool| which(tool).is_none())
        .collect();
    assert!(
        missing.is_empty(),
        "{E2E_ENV} is set but these required tools are missing from PATH: {}. \
         Install them (for example `weston xwayland x11-apps x11-utils`) and retry.",
        missing.join(", ")
    );
}

/// The exact weston invocation used by the test.
///
/// `--backend=headless` provides a virtual 800x600 output. Weston only loads
/// its own Xwayland when `--xwayland` is given, so simply *not* passing it (and
/// using `--no-config` so a host `weston.ini` cannot enable it) keeps weston
/// from claiming an X display that the service wants to own.
///
/// The renderer is pinned to `gl`/llvmpipe on purpose: the pixman renderer
/// segfaults in weston 13.0.0 when the first client surface is committed,
/// while the software GL renderer is stable headless.
fn weston_command(env: &[(String, String)]) -> Command {
    let mut command = Command::new("weston");
    command
        .args([
            "--backend=headless",
            "--socket=inox-e2e",
            "--width=800",
            "--height=600",
            "--no-config",
            "--renderer=gl",
        ])
        .envs(env.iter().cloned());
    command
}

/// Wait for the compositor's Wayland socket, failing if weston exits first.
fn wait_for_socket(socket: &Path, weston: &mut Proc, timeout: Duration) {
    let deadline = Instant::now() + timeout;
    loop {
        if let Some(status) = weston.try_wait() {
            panic!(
                "weston exited early with {status} before creating {}:\n{}",
                socket.display(),
                weston.diagnostics()
            );
        }
        if socket.exists() {
            return;
        }
        if Instant::now() >= deadline {
            panic!(
                "timed out after {timeout:?} waiting for the weston socket {}:\n{}",
                socket.display(),
                weston.diagnostics()
            );
        }
        thread::sleep(POLL_INTERVAL);
    }
}

/// Wait for the service's readiness file, failing if the service exits first.
fn wait_for_ready_file(ready_file: &Path, service: &mut Proc, timeout: Duration) {
    let deadline = Instant::now() + timeout;
    loop {
        if let Some(status) = service.try_wait() {
            panic!(
                "inox-xwayland exited early with {status} before writing {}:\n{}",
                ready_file.display(),
                service.diagnostics()
            );
        }
        if ready_file.exists() {
            return;
        }
        if Instant::now() >= deadline {
            panic!(
                "timed out after {timeout:?} waiting for the readiness file {}:\n{}",
                ready_file.display(),
                service.diagnostics()
            );
        }
        thread::sleep(POLL_INTERVAL);
    }
}

/// Poll `xwininfo` until a window whose name contains `expected` is mapped.
fn wait_for_window(expected: &str, env: &[(String, String)], xeyes: &mut Proc, timeout: Duration) {
    let deadline = Instant::now() + timeout;
    loop {
        if let Some(status) = xeyes.try_wait() {
            panic!(
                "{expected} exited early with {status} before mapping a window:\n{}",
                xeyes.diagnostics()
            );
        }
        let output = run_captured(
            "xwininfo",
            &["-display", X_DISPLAY, "-root", "-tree"],
            env,
            XWININFO_TIMEOUT,
        );
        if output.status.success() && output.stdout.contains(expected) {
            return;
        }
        if Instant::now() >= deadline {
            panic!(
                "timed out after {timeout:?} waiting for an `{expected}` window; \
                 last `xwininfo` run: status={}, stdout:\n{}\nstderr:\n{}\n{}",
                output.status,
                output.stdout,
                output.stderr,
                xeyes.diagnostics()
            );
        }
        thread::sleep(POLL_INTERVAL);
    }
}

/// Full end-to-end path: compositor -> service -> satellite -> Xwayland -> xeyes.
#[test]
fn maps_x11_window_end_to_end() {
    if std::env::var_os(E2E_ENV).is_none() {
        println!("{E2E_ENV} is not set; skipping headless X11 mapping end-to-end test");
        return;
    }

    require_tools(&["weston", "xwayland-satellite", "xeyes", "xwininfo"]);

    let service_bin = PathBuf::from(env!("CARGO_BIN_EXE_inox-xwayland"));
    assert!(
        service_bin.is_file(),
        "service binary not found at {}",
        service_bin.display()
    );

    let runtime_dir = private_dir("inox-e2e-runtime");
    let config_dir = private_dir("inox-e2e-config");
    let env = base_env(runtime_dir.path(), config_dir.path());

    let mut weston = Proc::spawn(
        "weston",
        weston_command(&env),
        &runtime_dir.path().join("weston.log"),
    );
    wait_for_socket(
        &runtime_dir.path().join(WAYLAND_SOCKET),
        &mut weston,
        WESTON_START_TIMEOUT,
    );

    let ready_file = runtime_dir.path().join("ready");
    let ready_arg = ready_file.display().to_string();
    let mut service_command = Command::new(&service_bin);
    service_command
        .args([
            "--display",
            X_DISPLAY,
            "--ready-file",
            ready_arg.as_str(),
            "--ready-timeout",
            "25",
            "--shutdown-grace",
            "5",
        ])
        .envs(env.iter().cloned());
    let mut service = Proc::spawn(
        "inox-xwayland",
        service_command,
        &runtime_dir.path().join("service.log"),
    );

    wait_for_ready_file(&ready_file, &mut service, READY_TIMEOUT);
    let content = fs::read_to_string(&ready_file)
        .unwrap_or_else(|error| panic!("failed to read {}: {error}", ready_file.display()));
    assert_eq!(
        content.trim(),
        X_DISPLAY,
        "readiness file must contain the X display"
    );

    let mut xeyes_command = Command::new("xeyes");
    xeyes_command
        .args(["-display", X_DISPLAY])
        .envs(env.iter().cloned());
    let mut xeyes = Proc::spawn(
        "xeyes",
        xeyes_command,
        &runtime_dir.path().join("xeyes.log"),
    );

    wait_for_window("xeyes", &env, &mut xeyes, XWININFO_TIMEOUT);

    // The service must terminate cleanly on SIGTERM.
    service.signal(Signal::TERM);
    let status = service.wait_timeout(TERMINATE_TIMEOUT).unwrap_or_else(|| {
        panic!(
            "inox-xwayland did not exit within {TERMINATE_TIMEOUT:?} after SIGTERM:\n{}",
            service.diagnostics()
        )
    });
    assert_eq!(
        status.code(),
        Some(0),
        "inox-xwayland must exit with status 0 on SIGTERM, got {status}:\n{}",
        service.diagnostics()
    );

    // `xeyes` and `weston` are torn down by their `Drop` implementations.
    drop(xeyes);
    drop(weston);
}
