//! Exit codes, and the one place a failure turns into bytes on stderr.
//!
//! Two rules:
//!
//! 1. **The exit code comes from [`gitea_core::ErrorKind::exit_code`]**, never from a second
//!    mapping here. The table is documented in `docs/output.md` and scripts written against
//!    `gh` depend on 0/1/2/4 agreeing with it; two mappings would drift the moment a variant
//!    was added.
//! 2. **A broken pipe is success.** `gea pr list | head -1` closes stdout under us on
//!    purpose. Combined with restoring `SIGPIPE` in `main`, this is what makes that idiom
//!    silent instead of a panic and a non-zero status.

use std::io::{self, IsTerminal, Write};
use std::process::ExitCode;

use anstyle::{AnsiColor, Style};
use gitea_core::error::{compat, render};
use gitea_core::{Error, ErrorKind};

/// A command failed, either while clap was still parsing or after.
///
/// clap's own errors carry their own rendering (with colour, suggestions, and usage), so they
/// are kept whole rather than flattened into an [`Error`] — remaking that output would be a
/// worse copy of it.
#[derive(Debug)]
pub enum Fail {
    Clap(Box<clap::Error>),
    Run(Error),
}

impl From<clap::Error> for Fail {
    fn from(e: clap::Error) -> Self {
        Fail::Clap(Box::new(e))
    }
}

impl From<Error> for Fail {
    fn from(e: Error) -> Self {
        Fail::Run(e)
    }
}

/// Whether to colour diagnostics, decided from **stderr** rather than stdout: that is where
/// they go, and `gea … | jq` should still get a coloured error.
pub fn color() -> render::Color {
    render::Color::from_tty(io::stderr().is_terminal())
}

/// Print whatever went wrong, drain the compatibility notes, and produce the process's status.
pub fn report(result: Result<(), Fail>) -> ExitCode {
    let color = color();
    let code = match result {
        Ok(()) => 0,
        Err(Fail::Clap(e)) => {
            let _ = e.print();
            // clap uses stderr for real errors and stdout for `--help`/`--version`; the latter
            // is a successful request for information, not a failure.
            if e.use_stderr() { 2 } else { 0 }
        }
        // Nothing is printed for a broken pipe: the reader is gone, and a message about it on
        // stderr is noise in exactly the pipelines where this happens.
        Err(Fail::Run(e)) if is_broken_pipe(&e) => 0,
        Err(Fail::Run(e)) => {
            let _ = writeln!(io::stderr(), "{}", render::render(&e, color));
            u8::try_from(e.exit_code()).unwrap_or(1)
        }
    };

    notes(color);
    // A failed flush here is almost always the same broken pipe; the status is already decided.
    let _ = io::stdout().flush();
    ExitCode::from(code)
}

/// Report a non-fatal problem: a keyring we could not reach, an unreadable `hosts.toml` entry.
///
/// Routed through the *same* [`render`] as a real error, so the advice a variant carries — the
/// `what to do` block — is not lost just because the problem was survivable. Only the headline
/// label differs, and it is replaced by dropping `render`'s first line and writing our own,
/// rather than by styling a second copy of the message.
pub fn warn(kind: &ErrorKind, color: render::Color) {
    let label = match color {
        render::Color::Never => "warning:".to_owned(),
        render::Color::Always => {
            let s = Style::new().bold().fg_color(Some(AnsiColor::Yellow.into()));
            format!("{}warning:{}", s.render(), s.render_reset())
        }
    };
    let full = render::render(&Error::new(clone_kindless(kind)), color);
    let body = full.split_once('\n').map(|(_, rest)| rest).unwrap_or("");
    let mut err = io::stderr().lock();
    let _ = writeln!(err, "{label} {}", render::headline(kind));
    if !body.trim().is_empty() {
        let _ = writeln!(err, "{}", body.trim_end());
    }
}

/// `ErrorKind` is deliberately not `Clone` (it holds an `io::Error`), and [`render::render`]
/// wants an owned [`Error`]. Rather than widen the published API, re-render through the one
/// variant that always survives: for everything else we hand the renderer a `Usage` carrying
/// the headline, which yields the generic remedy, and for the variants that actually appear as
/// warnings we would rather have the real advice — so those are reconstructed.
fn clone_kindless(kind: &ErrorKind) -> ErrorKind {
    match kind {
        ErrorKind::KeyringUnavailable { cause } => {
            ErrorKind::KeyringUnavailable { cause: cause.clone() }
        }
        ErrorKind::CredFilePermissions { path, mode } => {
            ErrorKind::CredFilePermissions { path: path.clone(), mode: *mode }
        }
        ErrorKind::UnknownHost { given, known } => {
            ErrorKind::UnknownHost { given: given.clone(), known: known.clone() }
        }
        other => ErrorKind::Usage(render::headline(other)),
    }
}

/// True when the failure is the reader at the other end of a pipe going away.
pub fn is_broken_pipe(e: &Error) -> bool {
    matches!(e.kind(), ErrorKind::Io(io) if io.kind() == io::ErrorKind::BrokenPipe)
}

/// One grouped note about everything the server sent that this build did not fully understand.
///
/// Never changes the exit code: an unknown enum value round-tripped verbatim is not a failure,
/// it is a newer server, and a CI job must not go red for it.
fn notes(color: render::Color) {
    if compat::is_suppressed() {
        return;
    }
    let notes = compat::drain();
    if notes.is_empty() {
        return;
    }
    let label = match color {
        render::Color::Never => "note:".to_owned(),
        render::Color::Always => {
            let s = Style::new().bold();
            format!("{}note:{}", s.render(), s.render_reset())
        }
    };
    let mut err = io::stderr().lock();
    let (null_rows, newer): (Vec<_>, Vec<_>) =
        notes.iter().partition(|n| matches!(n, compat::Note::NullRows { .. }));
    if !newer.is_empty() {
        let _ = writeln!(
            err,
            "{label} this instance sent {} value{} newer than the API description this build was \
             generated from. They were passed through unchanged.",
            newer.len(),
            if newer.len() == 1 { "" } else { "s" }
        );
    }
    for note in &newer {
        let _ = match note {
            compat::Note::UnknownEnum { type_name, value } => {
                writeln!(err, "      {type_name}: {value:?}")
            }
            compat::Note::Unparsed { what, value } => writeln!(err, "      {what}: {value:?}"),
            compat::Note::NullRows { .. } => Ok(()),
        };
    }
    if !null_rows.is_empty() {
        let _ = writeln!(
            err,
            "{label} the server sent entries it could not render as null, and they were left out \
             of the listing:"
        );
        for note in &null_rows {
            if let compat::Note::NullRows { request } = note {
                let _ = writeln!(err, "      {request}");
            }
        }
    }
    let _ = writeln!(err, "      Silence this with GEA_NO_COMPAT_NOTES=1.");
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Bug this prevents: `gea … | head -1` exiting non-zero, which turns every such pipeline
    /// in a `set -e` script into a failure.
    #[test]
    fn a_broken_pipe_is_success() {
        let e = Error::new(ErrorKind::Io(io::Error::from(io::ErrorKind::BrokenPipe)));
        assert!(is_broken_pipe(&e));
        assert_eq!(format!("{:?}", report(Err(Fail::Run(e)))), format!("{:?}", ExitCode::from(0)));
    }

    #[test]
    fn other_io_errors_are_not_swallowed() {
        let e = Error::new(ErrorKind::Io(io::Error::from(io::ErrorKind::PermissionDenied)));
        assert!(!is_broken_pipe(&e));
    }

    /// Bug this prevents: a warning losing the remedy the renderer would have printed, so a
    /// user told "the operating system keyring is not available" has no idea what to do.
    #[test]
    fn a_keyring_warning_keeps_its_advice() {
        let kind =
            ErrorKind::KeyringUnavailable { cause: gitea_core::error::KeyringCause::NoBackend };
        let rendered = render::render(&Error::new(clone_kindless(&kind)), render::Color::Never);
        assert!(rendered.contains("what to do"), "{rendered}");
        assert!(rendered.contains("GEA_CREDENTIAL_STORE"), "{rendered}");
    }
}
