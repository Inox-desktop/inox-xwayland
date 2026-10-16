//! Runtime configuration for the XWayland service.
//!
//! This module turns command line arguments into plain data: no process is
//! spawned and no socket is touched here, which keeps parsing unit-testable.

use std::{ffi::OsString, path::PathBuf, time::Duration};

use crate::x11::{DEFAULT_SOCKET_DIR, Display, DisplayError};

/// Default name of the satellite binary.
pub const DEFAULT_SATELLITE_BIN: &str = "xwayland-satellite";

/// Environment variable overriding the satellite binary path.
pub const SATELLITE_ENV: &str = "INOX_XWAYLAND_SATELLITE";

/// Default X11 display number.
pub const DEFAULT_DISPLAY: u16 = 1;

/// Default number of seconds to wait for Xwayland to become ready.
pub const DEFAULT_READY_TIMEOUT_SECS: u64 = 10;

/// Default number of seconds the satellite gets to exit after `SIGTERM`.
pub const DEFAULT_SHUTDOWN_GRACE_SECS: u64 = 5;

/// Usage text printed by `--help`.
pub const HELP: &str = "\
inox-xwayland - XWayland integration service for the Inox compositor

USAGE:
    inox-xwayland [OPTIONS]

OPTIONS:
    --display <:N>            X11 display to expose [default: :1]
    --satellite <PATH>        xwayland-satellite binary to supervise
                              [default: $INOX_XWAYLAND_SATELLITE or \"xwayland-satellite\"]
    --satellite-arg <ARG>     Extra argument for xwayland-satellite (repeatable)
    --x11-socket-dir <DIR>    Directory holding the X11 sockets [default: /tmp/.X11-unix]
    --ready-timeout <SECS>    Seconds to wait for Xwayland to be ready [default: 10]
    --shutdown-grace <SECS>   Seconds the satellite gets after SIGTERM [default: 5]
    --ready-file <PATH>       Write the DISPLAY name to this file once ready
    -h, --help                Print this help and exit
    -V, --version             Print the version and exit
";

/// Validated configuration of the XWayland service.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ServiceConfig {
    /// Display number to expose, for example `:1`.
    pub display: Display,
    /// `xwayland-satellite` binary that is spawned and supervised.
    pub satellite_bin: PathBuf,
    /// Extra arguments passed to the satellite, after the display.
    pub satellite_args: Vec<String>,
    /// Directory probed for the X11 socket, normally `/tmp/.X11-unix`.
    pub socket_dir: PathBuf,
    /// How long to wait for Xwayland to accept connections before failing.
    pub ready_timeout: Duration,
    /// How long the satellite gets to exit after `SIGTERM` before `SIGKILL`.
    pub shutdown_grace: Duration,
    /// Optional file that receives the display name once Xwayland is ready.
    pub ready_file: Option<PathBuf>,
}

impl Default for ServiceConfig {
    fn default() -> Self {
        Self {
            display: Display::new(DEFAULT_DISPLAY),
            satellite_bin: satellite_bin_from(std::env::var_os(SATELLITE_ENV)),
            satellite_args: Vec::new(),
            socket_dir: PathBuf::from(DEFAULT_SOCKET_DIR),
            ready_timeout: Duration::from_secs(DEFAULT_READY_TIMEOUT_SECS),
            shutdown_grace: Duration::from_secs(DEFAULT_SHUTDOWN_GRACE_SECS),
            ready_file: None,
        }
    }
}

/// Resolve the satellite binary from the overriding environment value.
///
/// An unset or empty value falls back to [`DEFAULT_SATELLITE_BIN`].
fn satellite_bin_from(value: Option<OsString>) -> PathBuf {
    value
        .filter(|value| !value.is_empty())
        .map_or_else(|| PathBuf::from(DEFAULT_SATELLITE_BIN), PathBuf::from)
}

/// What the command line asked the binary to do.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Invocation {
    /// Run the service with the given configuration.
    Run(ServiceConfig),
    /// Print [`HELP`] and exit successfully.
    Help,
    /// Print the crate version and exit successfully.
    Version,
}

/// Errors produced while parsing command line arguments.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum ConfigError {
    /// An option name was not recognised.
    #[error("unknown option `{0}`")]
    UnknownOption(String),

    /// A positional argument was given, but the service accepts none.
    #[error("unexpected argument `{0}`")]
    UnexpectedArgument(String),

    /// An option that needs a value was given without one.
    #[error("option `{option}` requires a value")]
    MissingValue {
        /// The option that is missing its value.
        option: String,
    },

    /// An option value could not be accepted.
    #[error("invalid value `{value}` for `{option}`: {reason}")]
    InvalidValue {
        /// The option that was given a bad value.
        option: String,
        /// The rejected value.
        value: String,
        /// Why the value was rejected.
        reason: &'static str,
    },
}

/// Parse command line arguments, excluding the program name.
///
/// Both `--option value` and `--option=value` spellings are accepted.
/// `--satellite-arg` may be repeated and preserves the given order.
///
/// # Errors
///
/// Returns a [`ConfigError`] for unknown options, positional arguments, missing
/// values and values that fail validation (for example `--display 1`).
pub fn parse_args<I>(args: I) -> Result<Invocation, ConfigError>
where
    I: IntoIterator<Item = String>,
{
    let mut config = ServiceConfig::default();
    let mut args = args.into_iter();

    while let Some(arg) = args.next() {
        let (option, inline) = split_inline_value(&arg);

        match option.as_str() {
            "-h" | "--help" => return Ok(Invocation::Help),
            "-V" | "--version" => return Ok(Invocation::Version),
            "--display" => {
                let value = take_value(&option, inline, &mut args)?;
                config.display = parse_display(&value)?;
            }
            "--satellite" => {
                let value = take_value(&option, inline, &mut args)?;
                config.satellite_bin = parse_path(&option, value)?;
            }
            "--satellite-arg" => {
                let value = take_value(&option, inline, &mut args)?;
                config.satellite_args.push(value);
            }
            "--x11-socket-dir" => {
                let value = take_value(&option, inline, &mut args)?;
                config.socket_dir = parse_path(&option, value)?;
            }
            "--ready-timeout" => {
                let value = take_value(&option, inline, &mut args)?;
                config.ready_timeout = Duration::from_secs(parse_seconds(&option, &value)?);
            }
            "--shutdown-grace" => {
                let value = take_value(&option, inline, &mut args)?;
                config.shutdown_grace = Duration::from_secs(parse_seconds(&option, &value)?);
            }
            "--ready-file" => {
                let value = take_value(&option, inline, &mut args)?;
                config.ready_file = Some(parse_path(&option, value)?);
            }
            other if other.starts_with('-') => {
                return Err(ConfigError::UnknownOption(other.to_owned()));
            }
            other => return Err(ConfigError::UnexpectedArgument(other.to_owned())),
        }
    }

    Ok(Invocation::Run(config))
}

fn split_inline_value(arg: &str) -> (String, Option<String>) {
    match arg.split_once('=') {
        Some((option, value)) if option.starts_with("--") => {
            (option.to_owned(), Some(value.to_owned()))
        }
        _ => (arg.to_owned(), None),
    }
}

fn take_value<I>(option: &str, inline: Option<String>, rest: &mut I) -> Result<String, ConfigError>
where
    I: Iterator<Item = String>,
{
    match inline {
        Some(value) => Ok(value),
        None => rest.next().ok_or_else(|| ConfigError::MissingValue {
            option: option.to_owned(),
        }),
    }
}

fn invalid_value(option: &str, value: &str, reason: &'static str) -> ConfigError {
    ConfigError::InvalidValue {
        option: option.to_owned(),
        value: value.to_owned(),
        reason,
    }
}

fn parse_display(value: &str) -> Result<Display, ConfigError> {
    Display::parse(value).map_err(|error| {
        let reason = match error {
            DisplayError::Empty => "must not be empty",
            DisplayError::MissingColon(_) => "must start with `:`",
            DisplayError::NotANumber(_) => "must be a display number such as `:1`",
            DisplayError::OutOfRange(_) => "display number must be between 0 and 9999",
        };

        invalid_value("--display", value, reason)
    })
}

fn parse_path(option: &str, value: String) -> Result<PathBuf, ConfigError> {
    if value.is_empty() {
        return Err(invalid_value(option, &value, "must not be empty"));
    }

    Ok(PathBuf::from(value))
}

fn parse_seconds(option: &str, value: &str) -> Result<u64, ConfigError> {
    let seconds: u64 = value
        .parse()
        .map_err(|_| invalid_value(option, value, "expected a whole number of seconds"))?;

    if seconds == 0 {
        return Err(invalid_value(option, value, "must be at least 1"));
    }

    Ok(seconds)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn args(raw: &[&str]) -> Vec<String> {
        raw.iter().map(|arg| (*arg).to_owned()).collect()
    }

    fn run_config(raw: &[&str]) -> ServiceConfig {
        match parse_args(args(raw)) {
            Ok(Invocation::Run(config)) => config,
            other => panic!("expected a run invocation, got {other:?}"),
        }
    }

    #[test]
    fn no_arguments_uses_defaults() {
        let config = run_config(&[]);

        assert_eq!(config.display, Display::new(DEFAULT_DISPLAY));
        assert_eq!(config.satellite_bin, ServiceConfig::default().satellite_bin);
        assert!(config.satellite_args.is_empty());
        assert_eq!(config.socket_dir, PathBuf::from(DEFAULT_SOCKET_DIR));
        assert_eq!(
            config.ready_timeout,
            Duration::from_secs(DEFAULT_READY_TIMEOUT_SECS)
        );
        assert_eq!(
            config.shutdown_grace,
            Duration::from_secs(DEFAULT_SHUTDOWN_GRACE_SECS)
        );
        assert_eq!(config.ready_file, None);
    }

    #[test]
    fn the_satellite_binary_falls_back_to_the_documented_default() {
        assert_eq!(
            satellite_bin_from(None),
            PathBuf::from(DEFAULT_SATELLITE_BIN)
        );
        assert_eq!(
            satellite_bin_from(Some(OsString::new())),
            PathBuf::from(DEFAULT_SATELLITE_BIN)
        );
        assert_eq!(
            satellite_bin_from(Some(OsString::from("/opt/satellite"))),
            PathBuf::from("/opt/satellite")
        );
    }

    #[test]
    fn every_option_is_parsed_in_the_separated_form() {
        let ready_file = std::env::temp_dir().join("inox-xwayland-ready");

        let config = run_config(&[
            "--display",
            ":42",
            "--satellite",
            "/usr/bin/xwayland-satellite",
            "--satellite-arg",
            "--verbose",
            "--x11-socket-dir",
            "/run/x11",
            "--ready-timeout",
            "3",
            "--shutdown-grace",
            "2",
            "--ready-file",
            ready_file.to_str().expect("utf-8 path"),
        ]);

        assert_eq!(config.display, Display::new(42));
        assert_eq!(
            config.satellite_bin,
            PathBuf::from("/usr/bin/xwayland-satellite")
        );
        assert_eq!(config.satellite_args, vec!["--verbose".to_owned()]);
        assert_eq!(config.socket_dir, PathBuf::from("/run/x11"));
        assert_eq!(config.ready_timeout, Duration::from_secs(3));
        assert_eq!(config.shutdown_grace, Duration::from_secs(2));
        assert_eq!(config.ready_file, Some(ready_file));
    }

    #[test]
    fn every_option_is_parsed_in_the_inline_form() {
        let config = run_config(&[
            "--display=:7",
            "--satellite=/opt/satellite",
            "--satellite-arg=--foo",
            "--x11-socket-dir=/run/sockets",
            "--ready-timeout=4",
            "--shutdown-grace=6",
            "--ready-file=/run/ready",
        ]);

        assert_eq!(config.display, Display::new(7));
        assert_eq!(config.satellite_bin, PathBuf::from("/opt/satellite"));
        assert_eq!(config.satellite_args, vec!["--foo".to_owned()]);
        assert_eq!(config.socket_dir, PathBuf::from("/run/sockets"));
        assert_eq!(config.ready_timeout, Duration::from_secs(4));
        assert_eq!(config.shutdown_grace, Duration::from_secs(6));
        assert_eq!(config.ready_file, Some(PathBuf::from("/run/ready")));
    }

    #[test]
    fn satellite_arguments_are_repeated_in_order() {
        let config = run_config(&[
            "--satellite-arg",
            "--one",
            "--satellite-arg=--two",
            "--satellite-arg",
            "three",
        ]);

        assert_eq!(
            config.satellite_args,
            vec!["--one".to_owned(), "--two".to_owned(), "three".to_owned()]
        );
    }

    #[test]
    fn help_and_version_short_circuit() {
        assert_eq!(parse_args(args(&["--help"])), Ok(Invocation::Help));
        assert_eq!(parse_args(args(&["-h"])), Ok(Invocation::Help));
        assert_eq!(parse_args(args(&["--version"])), Ok(Invocation::Version));
        assert_eq!(parse_args(args(&["-V"])), Ok(Invocation::Version));

        // Options that follow are not consulted once help was requested.
        assert_eq!(
            parse_args(args(&["--display", ":9", "--help"])),
            Ok(Invocation::Help)
        );
    }

    #[test]
    fn unknown_options_and_positional_arguments_are_rejected() {
        assert_eq!(
            parse_args(args(&["--nonsense"])),
            Err(ConfigError::UnknownOption("--nonsense".to_owned()))
        );
        assert_eq!(
            parse_args(args(&["start"])),
            Err(ConfigError::UnexpectedArgument("start".to_owned()))
        );
    }

    #[test]
    fn options_without_a_value_are_rejected() {
        for option in [
            "--display",
            "--satellite",
            "--satellite-arg",
            "--x11-socket-dir",
            "--ready-timeout",
            "--shutdown-grace",
            "--ready-file",
        ] {
            assert_eq!(
                parse_args(args(&[option])),
                Err(ConfigError::MissingValue {
                    option: option.to_owned()
                }),
                "expected `{option}` to require a value"
            );
        }
    }

    #[test]
    fn invalid_displays_are_rejected() {
        for value in ["", "1", ":abc", ":-1", ":10000"] {
            let outcome = parse_args(args(&["--display", value]));

            assert!(
                matches!(outcome, Err(ConfigError::InvalidValue { .. })),
                "expected `{value}` to be rejected, got {outcome:?}"
            );
        }

        assert_eq!(run_config(&["--display", ":0"]).display, Display::new(0));
        assert_eq!(
            run_config(&["--display", ":9999"]).display,
            Display::new(9999)
        );
    }

    #[test]
    fn invalid_timeouts_are_rejected() {
        for option in ["--ready-timeout", "--shutdown-grace"] {
            for value in ["0", "-1", "soon", "1.5"] {
                let outcome = parse_args(args(&[option, value]));

                assert!(
                    matches!(outcome, Err(ConfigError::InvalidValue { .. })),
                    "expected `{option} {value}` to be rejected, got {outcome:?}"
                );
            }
        }
    }

    #[test]
    fn empty_paths_are_rejected() {
        for option in ["--satellite", "--x11-socket-dir", "--ready-file"] {
            assert!(
                matches!(
                    parse_args(args(&[option, ""])),
                    Err(ConfigError::InvalidValue { .. })
                ),
                "expected `{option} \"\"` to be rejected"
            );
        }
    }

    #[test]
    fn help_text_mentions_every_option() {
        for option in [
            "--display",
            "--satellite",
            "--satellite-arg",
            "--x11-socket-dir",
            "--ready-timeout",
            "--shutdown-grace",
            "--ready-file",
            "--help",
            "--version",
        ] {
            assert!(HELP.contains(option), "help text omits `{option}`");
        }
    }
}
