//! `sss` — export a Pandora account to a local data repository and migrate
//! it to Spotify.

use std::io::Write;
use std::process::ExitCode;

use clap::Parser;

/// Export a Pandora account to a local data repository and migrate it to
/// Spotify.
#[derive(Debug, Parser)]
#[command(name = "sss", version, about)]
struct Cli {}

fn main() -> ExitCode {
    match Cli::try_parse() {
        Ok(Cli {}) => ExitCode::SUCCESS,
        Err(e) => {
            // clap writes `error: …` (usage errors) to stderr and help or
            // version text to stdout. A failed write of either is a failure.
            if let Err(io) = e.print() {
                let _ = writeln!(std::io::stderr(), "error: writing output: {io}");
                return ExitCode::from(1);
            }
            ExitCode::from(parse_exit_code(&e))
        }
    }
}

/// Exit status for a command line clap did not accept: 1 for a usage error,
/// 0 for `--help` and `--version`.
fn parse_exit_code(e: &clap::Error) -> u8 {
    if e.use_stderr() { 1 } else { 0 }
}

#[cfg(test)]
mod tests {
    use super::{Cli, parse_exit_code};
    use clap::{CommandFactory, Parser};

    #[test]
    fn cli_definition_is_valid() {
        Cli::command().debug_assert();
    }

    #[test]
    fn usage_error_exits_1() {
        let e = Cli::try_parse_from(["sss", "--bogus"]).unwrap_err();
        assert_eq!(parse_exit_code(&e), 1);
        assert!(e.to_string().starts_with("error: "));
    }

    #[test]
    fn help_and_version_exit_0() {
        for flag in ["--help", "--version"] {
            let e = Cli::try_parse_from(["sss", flag]).unwrap_err();
            assert_eq!(parse_exit_code(&e), 0, "{flag}");
        }
    }
}
