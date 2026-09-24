//! Terminal detection, copied from `gh` on purpose.
//!
//! Every rule here is observable from a shell script, so "improving" one is a breaking
//! change. `gh` users pipe `gh` into `cut`, set `NO_COLOR` in CI, and set `GH_FORCE_TTY` to
//! get padded output inside `less`. `gea` answers the same knobs with the same semantics so
//! that the muscle memory transfers and so that a wrapper script written against `gh` keeps
//! working when it is repointed at `gea`.
//!
//! Two deliberate implementation choices:
//!
//! * TTY detection uses [`std::io::IsTerminal`], **not** the `is-terminal` crate. `atty` had
//!   a soundness bug, `is-terminal` superseded it, and then the standard library absorbed
//!   the functionality; `deny.toml` bans both so nobody re-adds them.
//! * Environment access goes through the [`Env`] trait rather than [`std::env::var`]. Rust
//!   test binaries run threads in one process, so a test that mutated the real environment
//!   would race every other test in the file. Injecting the environment makes the
//!   `GEA_FORCE_TTY` / `NO_COLOR` / `CLICOLOR_FORCE` matrix testable in parallel.

use std::collections::BTreeMap;
use std::io::IsTerminal;

/// Fallback terminal width. 80 is what `gh`, `git`, and `less` assume, and matching them
/// means a golden test of piped-with-forced-TTY output does not depend on the developer's
/// window size.
pub const DEFAULT_WIDTH: usize = 80;

/// Read-only view of the process environment.
///
/// Exists purely so the detection rules can be unit tested without mutating global state.
pub trait Env {
    fn var(&self, key: &str) -> Option<String>;
}

/// The real process environment.
#[derive(Debug, Clone, Copy, Default)]
pub struct SysEnv;

impl Env for SysEnv {
    fn var(&self, key: &str) -> Option<String> {
        std::env::var(key).ok()
    }
}

/// A fake environment for tests.
#[derive(Debug, Clone, Default)]
pub struct MapEnv(BTreeMap<String, String>);

impl MapEnv {
    pub fn new() -> Self {
        Self::default()
    }

    /// Builder-style setter so a test reads as one expression.
    #[must_use]
    pub fn with(mut self, key: &str, value: &str) -> Self {
        self.0.insert(key.to_string(), value.to_string());
        self
    }
}

impl Env for MapEnv {
    fn var(&self, key: &str) -> Option<String> {
        self.0.get(key).cloned()
    }
}

/// Everything the renderers need to know about where output is going.
///
/// Construct once in `main` and thread it down. Recomputing it per row would call
/// `isatty` and `ioctl(TIOCGWINSZ)` thousands of times in a large table.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Term {
    /// Whether to use the human layout: padded columns, headers, banners, pager.
    /// **When this is false the table renderer emits raw TSV**, which is the contract that
    /// makes `gea pr list | cut -f2` work.
    pub tty: bool,
    /// Usable width in columns. Meaningful even when `tty` is false, because
    /// `GEA_FORCE_TTY=100` asks for padded 100-column output into a pipe.
    pub width: usize,
    /// Whether to emit SGR escapes. Separate from `tty` because `CLICOLOR_FORCE=1` colors a
    /// pipe and `NO_COLOR=1` de-colors a terminal.
    pub color: bool,
    /// Whether the terminal understands OSC 8 hyperlinks. Emitting them blindly leaves
    /// literal `\e]8;;` garbage in terminals that do not, so this is an allowlist.
    pub hyperlinks: bool,
}

impl Default for Term {
    /// The safe default is "a pipe": no padding, no color, no links. If detection is
    /// skipped by mistake, the failure mode is machine-readable output, not corrupted
    /// output.
    fn default() -> Self {
        Self { tty: false, width: DEFAULT_WIDTH, color: false, hyperlinks: false }
    }
}

impl Term {
    /// Detect from the real process: stdout's TTY-ness, the real window size, and the real
    /// environment.
    pub fn detect() -> Self {
        let real_tty = std::io::stdout().is_terminal();
        Self::detect_with(&SysEnv, real_tty, real_width())
    }

    /// The testable core of [`Term::detect`].
    ///
    /// `real_tty` and `real_width` are what the OS says; the environment can then override
    /// both. Order matters: `GEA_FORCE_TTY` is applied first (it can *create* a TTY), then
    /// color, then hyperlink capability, because both of the latter depend on whether we
    /// ended up in TTY mode.
    pub fn detect_with(env: &dyn Env, real_tty: bool, real_width: Option<usize>) -> Self {
        let natural_width = real_width.unwrap_or_else(|| columns_env(env).unwrap_or(DEFAULT_WIDTH));

        let (tty, width) = match force_tty(env) {
            Some(ForceTty::Width(w)) => (true, w),
            Some(ForceTty::Percent(p)) => (true, percent_of(natural_width, p)),
            Some(ForceTty::Plain) => (true, natural_width),
            None => (real_tty, natural_width),
        };

        // `gh`'s exact expression, including the precedence: a forced color wins over a
        // disabled one. Someone who sets both CLICOLOR_FORCE and NO_COLOR asked the more
        // specific question last.
        let color = color_forced(env) || (!color_disabled(env) && tty);

        Self { tty, width: width.max(1), color, hyperlinks: tty && hyperlink_capable(env) }
    }

    /// A `Term` that behaves like a pipe. Use in tests and for `--output <file>`.
    pub fn piped() -> Self {
        Self::default()
    }

    /// A `Term` that behaves like a colorless terminal of the given width. The workhorse for
    /// layout tests, where color escapes would just make goldens unreadable.
    pub fn tty(width: usize) -> Self {
        Self { tty: true, width, color: false, hyperlinks: false }
    }
}

/// The three shapes `GEA_FORCE_TTY` can take, mirroring `GH_FORCE_TTY`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum ForceTty {
    /// `GEA_FORCE_TTY=100` — force TTY mode at exactly 100 columns.
    ///
    /// Note the gotcha, inherited from `GH_FORCE_TTY` deliberately: `GEA_FORCE_TTY=1` is the
    /// *integer* 1, so it forces a one-column terminal rather than acting as a boolean. Use
    /// `GEA_FORCE_TTY=true` for "just force it". Diverging here would break scripts that
    /// already carry `GH_FORCE_TTY=$COLUMNS`-style values.
    Width(usize),
    /// `GEA_FORCE_TTY=80%` — force TTY mode at 80% of the real width.
    Percent(u32),
    /// `GEA_FORCE_TTY=1`, `=true`, `=yes` — force TTY mode, keep the real width.
    Plain,
}

fn force_tty(env: &dyn Env) -> Option<ForceTty> {
    let raw = env.var("GEA_FORCE_TTY")?;
    let v = raw.trim();
    if v.is_empty() {
        // An empty value is treated as unset, so `GEA_FORCE_TTY=` in a wrapper script does
        // not silently force padded output.
        return None;
    }
    if let Some(pct) = v.strip_suffix('%') {
        // A percentage is checked before the integer case; `80%` must not parse as `80`.
        return Some(pct.trim().parse::<u32>().map_or(ForceTty::Plain, ForceTty::Percent));
    }
    if let Ok(w) = v.parse::<usize>() {
        return Some(ForceTty::Width(w));
    }
    Some(ForceTty::Plain)
}

/// Round-half-up percentage, clamped to at least 1 column so `1%` of an 80-column terminal
/// does not produce a zero-width table that panics the layout arithmetic.
fn percent_of(width: usize, pct: u32) -> usize {
    let scaled = width.saturating_mul(pct as usize);
    ((scaled + 50) / 100).max(1)
}

/// `NO_COLOR` (any non-empty value, per no-color.org) or `CLICOLOR=0`.
fn color_disabled(env: &dyn Env) -> bool {
    env.var("NO_COLOR").is_some_and(|v| !v.is_empty())
        || env.var("CLICOLOR").as_deref() == Some("0")
}

/// `CLICOLOR_FORCE` set to anything other than `0`.
fn color_forced(env: &dyn Env) -> bool {
    env.var("CLICOLOR_FORCE").is_some_and(|v| !v.is_empty() && v != "0")
}

/// Terminals known to implement OSC 8. This is an allowlist rather than a feature probe
/// because there is no way to ask a terminal what it supports without writing to it and
/// reading a reply, which would corrupt piped output and hang on terminals that do not
/// answer.
fn hyperlink_capable(env: &dyn Env) -> bool {
    if env.var("FORCE_HYPERLINK").as_deref() == Some("1") {
        return true;
    }
    if env.var("WT_SESSION").is_some_and(|v| !v.is_empty()) {
        return true;
    }
    // VTE gained OSC 8 in 0.50 (VTE_VERSION 5000). Older GNOME Terminals print the escape
    // as literal text, so the comparison must be a real numeric one.
    if env.var("VTE_VERSION").and_then(|v| v.trim().parse::<u32>().ok()).is_some_and(|v| v >= 5000)
    {
        return true;
    }
    matches!(
        env.var("TERM_PROGRAM").as_deref(),
        Some("iTerm.app" | "WezTerm" | "kitty" | "vscode" | "Hyper")
    )
}

/// `COLUMNS`, which is what `stty`-less environments and some CI runners set.
fn columns_env(env: &dyn Env) -> Option<usize> {
    env.var("COLUMNS")?.trim().parse().ok().filter(|w| *w > 0)
}

fn real_width() -> Option<usize> {
    terminal_size::terminal_size().map(|(terminal_size::Width(w), _)| w as usize)
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Bug this prevents: treating `GEA_FORCE_TTY=100` as merely "force" and then padding
    /// to the real window size, so redirected output width depends on the developer's
    /// terminal and goldens differ per machine.
    #[test]
    fn force_tty_integer_is_an_absolute_width() {
        let t = Term::detect_with(&MapEnv::new().with("GEA_FORCE_TTY", "100"), false, Some(37));
        assert!(t.tty);
        assert_eq!(t.width, 100);
    }

    /// Bug this prevents: `80%` parsing as the integer `80` because the `%` was stripped by
    /// a lenient number parser, silently turning "80% of my window" into "80 columns".
    #[test]
    fn force_tty_percent_is_relative_to_the_real_width() {
        let env = MapEnv::new().with("GEA_FORCE_TTY", "50%");
        assert_eq!(Term::detect_with(&env, false, Some(200)).width, 100);
        // and it must not be confused with the absolute form
        let env = MapEnv::new().with("GEA_FORCE_TTY", "80%");
        assert_eq!(Term::detect_with(&env, false, Some(100)).width, 80);
    }

    /// Bug this prevents: a non-numeric force value (`GEA_FORCE_TTY=true`) falling through
    /// to `None` and leaving output in pipe mode, which is the exact opposite of the request.
    #[test]
    fn force_tty_other_value_forces_without_touching_width() {
        for v in ["true", "yes", "always"] {
            let t = Term::detect_with(&MapEnv::new().with("GEA_FORCE_TTY", v), false, Some(42));
            assert!(t.tty, "{v} should force TTY");
            assert_eq!(t.width, 42, "{v} must not override width");
        }
    }

    /// Documents the inherited gotcha rather than letting someone "fix" it: `=1` is the integer
    /// 1, so it forces a one-column terminal. `gh` behaves the same way, and a script written
    /// against `GH_FORCE_TTY` must not change meaning here.
    #[test]
    fn force_tty_one_is_a_width_not_a_boolean() {
        let t = Term::detect_with(&MapEnv::new().with("GEA_FORCE_TTY", "1"), false, Some(42));
        assert!(t.tty);
        assert_eq!(t.width, 1);
    }

    /// Bug this prevents: `GEA_FORCE_TTY=` (set but empty, a very common shell accident)
    /// forcing padded output into a pipe and breaking `cut -f2`.
    #[test]
    fn force_tty_empty_is_unset() {
        let t = Term::detect_with(&MapEnv::new().with("GEA_FORCE_TTY", ""), false, Some(42));
        assert!(!t.tty);
    }

    /// Bug this prevents: getting `gh`'s precedence backwards. `gh` computes
    /// `forced || (!disabled && tty)`, so `CLICOLOR_FORCE` wins over `NO_COLOR`. A script
    /// that sets both is asking for color.
    #[test]
    fn clicolor_force_beats_no_color() {
        let env = MapEnv::new().with("NO_COLOR", "1").with("CLICOLOR_FORCE", "1");
        assert!(Term::detect_with(&env, false, None).color);

        // NO_COLOR alone still wins over a real TTY.
        let env = MapEnv::new().with("NO_COLOR", "1");
        assert!(!Term::detect_with(&env, true, None).color);

        // CLICOLOR_FORCE=0 is not a force.
        let env = MapEnv::new().with("CLICOLOR_FORCE", "0");
        assert!(!Term::detect_with(&env, false, None).color);

        // CLICOLOR=0 disables on a TTY.
        let env = MapEnv::new().with("CLICOLOR", "0");
        assert!(!Term::detect_with(&env, true, None).color);

        // An empty NO_COLOR is not a disable, per no-color.org.
        let env = MapEnv::new().with("NO_COLOR", "");
        assert!(Term::detect_with(&env, true, None).color);
    }

    /// Bug this prevents: colorizing a pipe by default, which puts SGR escapes into TSV
    /// fields and breaks every downstream parser.
    #[test]
    fn pipes_are_colorless_by_default() {
        assert!(!Term::detect_with(&MapEnv::new(), false, None).color);
        assert!(Term::detect_with(&MapEnv::new(), true, None).color);
    }

    /// Bug this prevents: emitting OSC 8 on a terminal that renders it literally, leaving
    /// `\e]8;;https://...` visible in the output.
    #[test]
    fn hyperlinks_need_both_a_tty_and_a_known_terminal() {
        let known = MapEnv::new().with("TERM_PROGRAM", "WezTerm");
        assert!(Term::detect_with(&known, true, None).hyperlinks);
        assert!(!Term::detect_with(&known, false, None).hyperlinks);

        let unknown = MapEnv::new().with("TERM_PROGRAM", "Apple_Terminal");
        assert!(!Term::detect_with(&unknown, true, None).hyperlinks);

        // VTE is a numeric comparison, not a presence check.
        let old_vte = MapEnv::new().with("VTE_VERSION", "4205");
        assert!(!Term::detect_with(&old_vte, true, None).hyperlinks);
        let new_vte = MapEnv::new().with("VTE_VERSION", "5202");
        assert!(Term::detect_with(&new_vte, true, None).hyperlinks);

        let forced = MapEnv::new().with("FORCE_HYPERLINK", "1");
        assert!(Term::detect_with(&forced, true, None).hyperlinks);
    }

    /// Bug this prevents: ignoring `COLUMNS` and defaulting to 80 in environments where
    /// `ioctl(TIOCGWINSZ)` fails (containers, some CI runners), truncating output that had
    /// room.
    #[test]
    fn columns_env_is_the_fallback_before_eighty() {
        let env = MapEnv::new().with("COLUMNS", "120");
        assert_eq!(Term::detect_with(&env, true, None).width, 120);
        assert_eq!(Term::detect_with(&MapEnv::new(), true, None).width, DEFAULT_WIDTH);
        // A real width from the OS wins over COLUMNS, which is often stale.
        assert_eq!(Term::detect_with(&env, true, Some(60)).width, 60);
    }

    /// Bug this prevents: `GEA_FORCE_TTY=0` or `1%` producing width 0, which makes the
    /// column-shrinking arithmetic divide by zero.
    #[test]
    fn width_is_never_zero() {
        let env = MapEnv::new().with("GEA_FORCE_TTY", "0");
        assert_eq!(Term::detect_with(&env, false, None).width, 1);
        let env = MapEnv::new().with("GEA_FORCE_TTY", "1%");
        assert!(Term::detect_with(&env, false, Some(10)).width >= 1);
    }
}
