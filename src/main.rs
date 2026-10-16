//! Entry point of the `inox-xwayland` binary.
//!
//! Command line handling and the exit-code policy live here; the supervision
//! logic itself is in the library so that tests can drive it directly.

use std::{
    io::{self, Write},
    process::{ExitCode, ExitStatus},
};

use inox_xwayland::{
    ServiceOutcome, VERSION,
    config::{HELP, Invocation, ServiceConfig, parse_args},
    runtime,
};
use tracing::{error, info};

fn main() -> ExitCode {
    match parse_args(std::env::args().skip(1)) {
        Ok(Invocation::Help) => write_stdout(HELP),
        Ok(Invocation::Version) => write_stdout(&format!("inox-xwayland {VERSION}\n")),
        Ok(Invocation::Run(config)) => run(config),
        Err(error) => write_stderr(&format!("inox-xwayland: {error}\n")),
    }
}

/// Run the service and translate its outcome into an exit code.
///
/// A clean shutdown is a success; a satellite that exited on its own is
/// reported through its own exit code, and anything else is a failure.
fn run(config: ServiceConfig) -> ExitCode {
    // Logging is best effort: starting the service must not fail just because a
    // subscriber is already installed (for example by an embedding harness).
    let _ = inox_common::logging::init();

    info!(version = VERSION, display = %config.display, "starting inox-xwayland");

    match runtime::run(config) {
        Ok(ServiceOutcome::Signalled) => ExitCode::SUCCESS,
        Ok(ServiceOutcome::ChildExited(status)) => exit_code_of(status),
        Err(error) => {
            error!(%error, "inox-xwayland stopped with an error");
            ExitCode::FAILURE
        }
    }
}

/// Mirror the satellite's own exit status, defaulting to failure.
///
/// A status without a code means the child was killed by a signal, which is not
/// a success for the session.
fn exit_code_of(status: ExitStatus) -> ExitCode {
    status
        .code()
        .and_then(|code| u8::try_from(code).ok())
        .map_or(ExitCode::FAILURE, ExitCode::from)
}

/// Write user-facing CLI text to stdout.
///
/// An explicit write is used instead of `println!`, mirroring
/// `inox-compositor`.
fn write_stdout(text: &str) -> ExitCode {
    let mut stdout = io::stdout().lock();

    if stdout.write_all(text.as_bytes()).is_err() || stdout.flush().is_err() {
        return ExitCode::FAILURE;
    }

    ExitCode::SUCCESS
}

/// Write a usage error to stderr and return the failure code.
fn write_stderr(text: &str) -> ExitCode {
    let mut stderr = io::stderr().lock();

    // The exit status already reports the failure, so a broken stderr is not
    // actionable here.
    let _ = stderr.write_all(text.as_bytes());

    ExitCode::FAILURE
}
