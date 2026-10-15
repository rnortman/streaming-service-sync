//! `sss` — export a Pandora account to a local data repository and migrate
//! it to Spotify.

use std::io::Write;
use std::process::ExitCode;

use clap::Parser;
use streaming_service_sync::cli::{self, Cli};
use streaming_service_sync::secrets::StateRoot;

fn main() -> ExitCode {
    let cli = match Cli::try_parse() {
        Ok(cli) => cli,
        Err(e) => {
            // clap writes `error: …` (usage errors) to stderr and help or
            // version text to stdout. A failed write of either is a failure.
            if let Err(io) = e.print() {
                let _ = writeln!(std::io::stderr(), "error: writing output: {io}");
                return ExitCode::from(1);
            }
            return ExitCode::from(parse_exit_code(&e));
        }
    };
    let mut stdout = std::io::stdout().lock();
    let result = cli::run(
        cli,
        StateRoot::locate,
        &mut std::io::stdin().lock(),
        &mut stdout,
        &mut std::io::stderr(),
    );
    let flushed = stdout
        .flush()
        .map_err(|e| anyhow::Error::new(e).context("writing output"));
    match result.and(flushed) {
        Ok(()) => ExitCode::SUCCESS,
        Err(e) => {
            let _ = writeln!(std::io::stderr(), "error: {e:#}");
            ExitCode::from(1)
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
    use clap::Parser;

    #[test]
    fn usage_error_exits_1() {
        let e = Cli::try_parse_from(["sss", "--bogus"]).unwrap_err();
        assert_eq!(parse_exit_code(&e), 1);
        assert!(e.to_string().starts_with("error: "));
    }

    #[test]
    fn missing_subcommand_exits_1() {
        let e = Cli::try_parse_from(["sss"]).unwrap_err();
        assert_eq!(parse_exit_code(&e), 1);
    }

    #[test]
    fn help_and_version_exit_0() {
        for flag in ["--help", "--version"] {
            let e = Cli::try_parse_from(["sss", flag]).unwrap_err();
            assert_eq!(parse_exit_code(&e), 0, "{flag}");
        }
    }
}
