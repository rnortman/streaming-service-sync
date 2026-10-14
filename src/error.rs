//! Error kinds: the decision a program driving `sss` makes on a failure —
//! sign in again, wait and retry, or fix something — carried by a typed
//! error in the `anyhow` chain.

use std::fmt;

use serde::Serialize;

use crate::pandora::client::SessionRejected;
use crate::pandora::export::{ExportIncomplete, ExportStopped};

/// What kind of failure ended a command.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum Kind {
    /// A stored session is missing, or the service refused it or the
    /// credential given to sign in.
    Auth,
    /// The service did not answer, after every retry.
    Unavailable,
    /// An export run ended with failed steps.
    Incomplete,
    /// What was pasted or piped is unusable.
    Input,
    /// The data repository, its configuration or the environment is not
    /// usable as it stands.
    Config,
    /// The command is not built yet.
    NotImplemented,
    /// Anything else: I/O, parsing, a failed write.
    Other,
}

/// An error with a [`Kind`]. Its text is `message`; its
/// [`std::error::Error::source`], when it wraps one, is `source`.
#[derive(Debug)]
pub struct Failure {
    pub kind: Kind,
    pub message: String,
    pub source: Option<anyhow::Error>,
}

impl Failure {
    pub fn new(kind: Kind, message: impl Into<String>) -> Self {
        Self {
            kind,
            message: message.into(),
            source: None,
        }
    }
}

impl fmt::Display for Failure {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.message)
    }
}

impl std::error::Error for Failure {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        self.source
            .as_ref()
            .map(|e| e.as_ref() as &(dyn std::error::Error + 'static))
    }
}

/// Gives the error of a `Result` a [`Kind`], as `anyhow::Context` gives it
/// a context: the error becomes the source of a [`Failure`] whose message
/// is `message()`.
pub trait WithKind<T> {
    fn with_kind<M: Into<String>>(
        self,
        kind: Kind,
        message: impl FnOnce() -> M,
    ) -> anyhow::Result<T>;
}

impl<T, E: Into<anyhow::Error>> WithKind<T> for Result<T, E> {
    fn with_kind<M: Into<String>>(
        self,
        kind: Kind,
        message: impl FnOnce() -> M,
    ) -> anyhow::Result<T> {
        self.map_err(|e| {
            anyhow::Error::new(Failure {
                kind,
                message: message().into(),
                source: Some(e.into()),
            })
        })
    }
}

/// Returns early with a [`Failure`] of the given kind, like `anyhow::bail!`.
#[macro_export]
macro_rules! fail {
    ($kind:expr, $($arg:tt)+) => {
        return Err(::anyhow::Error::new($crate::error::Failure::new($kind, format!($($arg)+))))
    };
}

/// The kind of `err`: that of the first error in its chain that carries
/// one, else [`Kind::Other`].
pub fn kind(err: &anyhow::Error) -> Kind {
    carried_kind(err).unwrap_or(Kind::Other)
}

fn carried_kind(err: &anyhow::Error) -> Option<Kind> {
    for e in err.chain() {
        if let Some(f) = e.downcast_ref::<Failure>() {
            return Some(f.kind);
        } else if e.is::<SessionRejected>() {
            return Some(Kind::Auth);
        } else if e.is::<ExportIncomplete>() {
            return Some(Kind::Incomplete);
        } else if let Some(stopped) = e.downcast_ref::<ExportStopped>() {
            // Its own place in the chain stands for the outermost error of
            // `source`, which the chain therefore never yields.
            return carried_kind(&stopped.source);
        }
    }
    None
}

/// The export run `err` concerns, when it stopped or ended incomplete.
pub fn export_run(err: &anyhow::Error) -> Option<ExportRun<'_>> {
    err.chain().find_map(|e| {
        if let Some(s) = e.downcast_ref::<ExportStopped>() {
            Some(ExportRun::Stopped(s))
        } else {
            e.downcast_ref::<ExportIncomplete>()
                .map(ExportRun::Incomplete)
        }
    })
}

/// An export run found in an error chain by [`export_run`].
#[derive(Debug, Clone, Copy)]
pub enum ExportRun<'a> {
    Stopped(&'a ExportStopped),
    Incomplete(&'a ExportIncomplete),
}

#[cfg(test)]
mod tests {
    use super::*;
    use anyhow::{Context, anyhow};

    fn failing() -> anyhow::Result<()> {
        fail!(Kind::Input, "no token given {}", 1);
    }

    #[test]
    fn kind_is_found_through_contexts() {
        let err = failing().context("inner").context("outer").unwrap_err();
        assert_eq!(kind(&err), Kind::Input);
        assert_eq!(format!("{err:#}"), "outer: inner: no token given 1");
    }

    #[test]
    fn a_kind_given_to_an_error_keeps_its_chain() {
        let cause: anyhow::Result<()> = Err(anyhow!(SessionRejected {
            detail: "HTTP 401".into()
        })
        .context("reading"));
        let err = cause
            .with_kind(Kind::Unavailable, || "POST x: no response")
            .context("outer")
            .unwrap_err();
        assert_eq!(kind(&err), Kind::Unavailable);
        assert_eq!(
            format!("{err:#}"),
            "outer: POST x: no response: reading: Pandora rejected the session (HTTP 401); \
             re-run `sss pandora auth`"
        );
        assert!(
            err.chain().any(|e| e.is::<SessionRejected>()),
            "the wrapped error stays in the chain"
        );
    }

    #[test]
    fn a_rejected_session_is_auth_and_a_plain_error_is_other() {
        let err = anyhow!(SessionRejected {
            detail: "HTTP 401".into()
        })
        .context("verifying the token");
        assert_eq!(kind(&err), Kind::Auth);
        assert_eq!(kind(&anyhow!("disk full")), Kind::Other);
    }

    #[test]
    fn kinds_serialize_in_snake_case() {
        assert_eq!(
            serde_json::to_string(&Kind::NotImplemented).unwrap(),
            "\"not_implemented\""
        );
        assert_eq!(serde_json::to_string(&Kind::Auth).unwrap(), "\"auth\"");
    }
}
