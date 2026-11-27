//! When a command may ask a question, and what to do with the answer.
//!
//! # The gate
//!
//! **stdin *and* stdout must both be real terminals**, and prompting must not have been switched
//! off. `docs/porcelain-conventions.md` makes that binding, and both halves matter:
//!
//! - stdin alone is not enough. With stdout redirected the question itself is captured into the
//!   file and the user stares at a command that looks hung with no visible prompt.
//! - stdout alone is not enough. `gea repo delete x < /dev/null` in a CI job that allocates a
//!   TTY has a terminal on stdout and nothing on stdin: the prompt blocks until the job times
//!   out. This is the failure the rule exists to prevent, and the reason the check here uses
//!   the *real* `stdout().is_terminal()` and not [`crate::output::Term::tty`] — `GEA_FORCE_TTY`
//!   asks for padded columns in a pipe, which is a rendering request and not a claim that a
//!   human is watching.
//!
//! # Cancellation is not failure
//!
//! Ctrl-C or Ctrl-D at a prompt is [`ErrorKind::Cancelled`] — exit 130, which is what a shell
//! expects from an interrupted interactive program — and not a generic error. A script wrapping
//! `gea` has to be able to tell "the user said no" from "the command broke", so the
//! classification lives in exactly one function here.

use std::io::IsTerminal;

use gitea_core::config::{Config, Prompt};
use gitea_core::error::{Error, ErrorKind, Result};

use super::usage;
use crate::runtime::Runtime;

/// Whether prompting has been switched off by the environment.
///
/// Presence, not a truthy value: someone who exported `GEA_PROMPT_DISABLED=` meant to disable
/// prompting, and for a flag whose failure mode is a hung command, erring towards "fail fast" is
/// the safe direction.
pub fn prompting_disabled() -> bool {
    std::env::var_os("GEA_PROMPT_DISABLED").is_some()
}

/// Both streams are terminals. See the module docs for why both.
pub fn streams_are_terminals() -> bool {
    std::io::stdin().is_terminal() && std::io::stdout().is_terminal()
}

/// The gate, for a command that has no [`Config`] to hand (`auth login` runs before one exists
/// for the host, and the unit-testable helpers take only a terminal description).
pub fn may_prompt() -> bool {
    !prompting_disabled() && streams_are_terminals()
}

/// The gate, honouring `prompt = "disabled"` for this host as well.
pub fn may_prompt_for(config: &Config, host: Option<&str>) -> bool {
    config.prompt(host) != Prompt::Disabled && may_prompt()
}

/// The gate, for a command that has a [`Runtime`].
pub fn can_prompt(rt: &Runtime) -> bool {
    may_prompt_for(rt.config(), Some(rt.host().as_str()))
}

/// Turn an `inquire` outcome into the taxonomy. See the module docs.
pub fn prompted<T>(what: &str, r: inquire::error::InquireResult<T>) -> Result<T> {
    r.map_err(|e| prompt_error(what, e))
}

/// Classify an `inquire` failure.
pub fn prompt_error(what: &str, e: inquire::InquireError) -> Error {
    use inquire::InquireError as E;
    match e {
        E::OperationCanceled | E::OperationInterrupted => Error::new(ErrorKind::Cancelled),
        other => usage(format!("could not prompt for {what}: {other}")),
    }
}

/// The core confirmation.
///
/// `yes` skips it. When there is nobody to ask, `refusal` is returned as a usage error — it must
/// **name `--yes`**, because "this is destructive" with no way forward sends the reader to the
/// manual. Answering "no" is [`ErrorKind::Cancelled`], not an error about the action.
///
/// `may_prompt` is passed in rather than computed: the callers hold four different things that
/// can answer the question (a `Term`, a `Config`, a `Runtime`, a command context), and threading
/// any one of them through here would make the rest reach for it artificially.
pub fn confirm(may_prompt: bool, yes: bool, question: &str, refusal: String) -> Result<()> {
    if yes {
        return Ok(());
    }
    if !may_prompt {
        return Err(usage(refusal));
    }
    let answered =
        prompted("confirmation", inquire::Confirm::new(question).with_default(false).prompt())?;
    if answered { Ok(()) } else { Err(Error::new(ErrorKind::Cancelled)) }
}

/// Confirm something named as a **fragment**: `"delete webhook 4"`, `"purge ada"`.
///
/// The question mark and the refusal sentence are supplied here, which is the whole reason this
/// exists separately from [`confirm_question`]. Five waves each wrote their own wording for this
/// — "is destructive", "cannot be undone", "pass --yes to confirm it non-interactively" — and a
/// tool that phrases the same refusal five ways reads like five tools.
pub fn confirm_action(may_prompt: bool, yes: bool, action: &str) -> Result<()> {
    confirm(
        may_prompt,
        yes,
        &format!("{action}?"),
        format!("{action} is destructive; pass --yes to confirm"),
    )
}

/// Confirm something already phrased as a **whole question**: `"Log ada out of git.example.org?"`.
pub fn confirm_question(may_prompt: bool, yes: bool, question: &str) -> Result<()> {
    confirm(may_prompt, yes, question, format!("{question} Pass --yes to confirm."))
}

/// [`confirm_action`] for a command that has only a terminal description to hand.
///
/// The gate is [`may_prompt`] **and** a real output terminal: the wave this came from checked
/// only `term.tty`, which meant `gea webhook delete 4 < /dev/null` on a TTY-allocating CI runner
/// would sit on a prompt nobody could answer — the exact failure the conventions' two-stream rule
/// exists to prevent.
pub fn confirm_term(term: &crate::output::Term, yes: bool, action: &str) -> Result<()> {
    confirm_action(term.tty && may_prompt(), yes, action)
}

/// [`confirm_action`] for a command that has a [`Runtime`].
pub fn confirm_runtime(rt: &Runtime, yes: bool, action: &str) -> Result<()> {
    confirm_action(can_prompt(rt), yes, action)
}

/// A free-text question with an optional default. Callers check the gate first.
pub fn ask(prompt: &str, default: Option<&str>) -> Result<String> {
    let mut q = inquire::Text::new(prompt);
    if let Some(d) = default {
        q = q.with_default(d);
    }
    prompted(prompt, q.prompt())
}

/// A single-choice picker. Returns the **index** into `options`.
///
/// The index and not the label, because callers pick from a list of structured things whose
/// labels are for reading — matching the answer back by string comparison would break the moment
/// two of them rendered the same way.
pub fn select(prompt: &str, options: &[String]) -> Result<usize> {
    prompted(prompt, inquire::Select::new(prompt, options.to_vec()).raw_prompt())
        .map(|chosen| chosen.index)
}

/// One-of-many, as a picker on a terminal and a usage error naming `flag` otherwise.
pub fn choose(
    may_prompt: bool,
    question: &str,
    flag: &str,
    options: Vec<String>,
) -> Result<String> {
    match options.len() {
        0 => Err(usage(format!("no choices available for {flag}"))),
        // Not ambiguous, so do not ask. Prompting for a single option trains people to hit
        // Enter without reading, which is how the *next* prompt gets answered wrongly.
        1 => Ok(options.into_iter().next().expect("length checked")),
        _ => {
            if !may_prompt {
                return Err(usage(format!(
                    "{question} Select with {flag}. Choices: {}",
                    options.join(", ")
                )));
            }
            prompted(flag, inquire::Select::new(question, options).prompt())
        }
    }
}

/// Read a secret from the terminal without echoing it.
pub fn secret(prompt: &str) -> Result<String> {
    prompted(
        prompt,
        inquire::Password::new(prompt)
            // The value is being sent to a server, not chosen: a confirmation prompt would ask
            // the user to type a secret they are copy-pasting twice.
            .without_confirmation()
            .with_display_mode(inquire::PasswordDisplayMode::Masked)
            .prompt(),
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Bug this prevents: hanging on a confirmation prompt in CI. With nobody to ask, the
    /// refusal has to name `--yes` rather than wait for a keystroke nobody will type.
    #[test]
    fn a_destructive_action_with_nobody_to_ask_names_the_flag_instead_of_prompting() {
        let e = confirm_action(false, false, "delete webhook 4").unwrap_err();
        assert_eq!(e.exit_code(), 2);
        assert!(e.to_string().contains("--yes"), "{e}");
        assert!(e.to_string().contains("delete webhook 4"), "{e}");

        let e = confirm_question(false, false, "Log ada out of git.example.org?").unwrap_err();
        assert_eq!(e.exit_code(), 2);
        assert!(e.to_string().contains("--yes"), "{e}");

        // `--yes` never prompts, whatever the gate says.
        assert!(confirm_action(false, true, "delete webhook 4").is_ok());
        assert!(confirm_action(true, true, "delete webhook 4").is_ok());
    }

    /// Ctrl-C is a cancellation (exit 130), not a failure of the command.
    #[test]
    fn an_interrupted_prompt_is_cancelled_and_anything_else_is_a_usage_error() {
        let cancelled = prompt_error("a name", inquire::InquireError::OperationInterrupted);
        assert_eq!(cancelled.exit_code(), 130);
        let broken = prompt_error("a name", inquire::InquireError::NotTTY);
        assert_eq!(broken.exit_code(), 2);
        assert!(broken.to_string().contains("a name"), "{broken}");
    }

    /// Bug this prevents: `choose` prompting when there is exactly one candidate (which trains
    /// people to hit Enter without reading), and hanging instead of erroring when there is no
    /// terminal to prompt on.
    #[test]
    fn choose_only_asks_when_it_is_genuinely_ambiguous() {
        assert_eq!(choose(false, "Which?", "--host", vec!["a".into()]).unwrap(), "a");
        let e = choose(false, "Which?", "--host", vec!["a".into(), "b".into()]).unwrap_err();
        assert!(e.to_string().contains("--host"), "{e}");
        assert!(e.to_string().contains("a, b"), "{e}");
    }

    /// Bug this prevents: `prompt = "disabled"` in config being ignored, so a user who asked for
    /// every command to fail fast instead gets a blocking question.
    #[test]
    fn disabled_prompting_is_honoured_regardless_of_the_terminal() {
        let dir = tempfile::tempdir().unwrap();
        let mut config = Config::empty_at(dir.path());
        config.set(None, "prompt", "disabled").unwrap();
        assert!(!may_prompt_for(&config, None));
    }
}
