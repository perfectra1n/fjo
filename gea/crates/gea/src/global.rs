//! The global flags, defined once for both halves of the two-phase parse.
//!
//! `main` parses `argv` twice over, in two very different ways: `gea raw …` gets a
//! hand-built [`clap::Command`] carrying only the named layer-2 subtree, and everything else
//! gets the derive tree. Both roots need the same `--host`, `--jq`, `--json`, … so those
//! arguments are built here with the builder API and read back with [`GlobalOpts::from_matches`]
//! rather than derived. A `#[derive(Args)]` struct would only serve the derive half, and the
//! flags would then have to be written a second time for the layer-2 root — which is exactly
//! how the two halves come to disagree about what `--limit` means.
//!
//! # Why [`args`] takes a set of names already in use
//!
//! Layer 2 exposes the whole API as flags, so its 310 distinct generated flag names include
//! `--repo` (a path parameter on ~200 operations), `--template` (a body field on
//! `repo create`), `--color` (a body field on `label create`), and `--limit` (a query
//! parameter). A global argument is propagated into every subcommand, and clap answers a
//! duplicate long name with a panic — a crash driven by table contents, on a command line a
//! user could reasonably type.
//!
//! So the layer-2 root asks [`taken`] what the subtree already claimed and passes it here. A
//! global whose long name is taken keeps its short (`-R`, `-t`) and loses the long; one with
//! neither is dropped. At that depth the leaf's own meaning is the more specific one anyway:
//! on `gea raw repo get`, `--repo` naming the repository path parameter is what the user
//! meant, and `-R owner/name` still supplies context.

use std::collections::BTreeSet;
use std::path::PathBuf;

use clap::{Arg, ArgAction, ArgMatches, Command, value_parser};
use gitea_core::config::ColorPref;
use gitea_core::types::RepoRef;

// Argument ids are prefixed so they cannot collide with layer 2's (`path:`, `query:`,
// `body:`, `form:`, `gea:`) whatever the generated tables contain.
pub const HOST: &str = "g:host";
pub const LOGIN: &str = "g:login";
pub const REPO: &str = "g:repo";
pub const JSON: &str = "g:json";
pub const JQ: &str = "g:jq";
pub const TEMPLATE: &str = "g:template";
pub const COLOR: &str = "g:color";
pub const SUDO: &str = "g:sudo";
pub const OTP: &str = "g:otp";
pub const DEBUG: &str = "g:debug";
pub const NO_RETRY: &str = "g:no-retry";
pub const MAX_RETRIES: &str = "g:max-retries";
pub const INSECURE_TLS: &str = "g:insecure-skip-tls-verify";
pub const PAGINATE: &str = "g:paginate";
pub const LIMIT: &str = "g:limit";
pub const OUTPUT: &str = "g:output";
pub const FORCE: &str = "g:force";

/// The heading global flags are grouped under in `--help`.
const HEADING: &str = "Global options";

/// Everything every command shares, read out of whichever root parsed the command line.
#[derive(Debug, Default, Clone)]
pub struct GlobalOpts {
    pub host: Option<String>,
    pub login: Option<String>,
    pub repo: Option<RepoRef>,
    /// `Some("")` is a bare `--json`: list the fields and exit. See
    /// [`crate::output::project::resolve`].
    pub json: Option<String>,
    pub jq: Option<String>,
    pub template: Option<String>,
    pub color: Option<ColorPref>,
    pub sudo: Option<String>,
    pub otp: Option<String>,
    pub debug: bool,
    pub no_retry: bool,
    pub max_retries: Option<u32>,
    pub insecure_skip_tls_verify: bool,
    pub paginate: bool,
    pub limit: Option<usize>,
    pub output: Option<PathBuf>,
    pub force: bool,
}

impl GlobalOpts {
    /// Read the globals back out of matches.
    ///
    /// `try_get_*` throughout, never `get_*`: [`args`] may legitimately have dropped an
    /// argument because the layer-2 leaf claimed its name, and `get_one` on an id the command
    /// does not define is a panic.
    pub fn from_matches(m: &ArgMatches) -> Self {
        Self {
            host: one::<String>(m, HOST),
            login: one::<String>(m, LOGIN),
            repo: one::<RepoRef>(m, REPO),
            json: one::<String>(m, JSON),
            jq: one::<String>(m, JQ),
            template: one::<String>(m, TEMPLATE),
            // clap has already validated the value against the allowed list, so an
            // unparseable one here is impossible rather than merely unlikely.
            color: one::<String>(m, COLOR).and_then(|s| s.parse().ok()),
            sudo: one::<String>(m, SUDO),
            otp: one::<String>(m, OTP),
            debug: flag(m, DEBUG),
            no_retry: flag(m, NO_RETRY),
            max_retries: one::<u32>(m, MAX_RETRIES),
            insecure_skip_tls_verify: flag(m, INSECURE_TLS),
            paginate: flag(m, PAGINATE),
            limit: one::<u64>(m, LIMIT).map(|n| n as usize),
            output: one::<PathBuf>(m, OUTPUT),
            force: flag(m, FORCE),
        }
    }

    /// Read the globals from a chain of matches, outermost last, taking the first value found.
    ///
    /// clap propagates a global argument's *value* between levels, but the direction it does so
    /// is an implementation detail, and a global can legally appear at any depth — `gea --jq .
    /// api user` and `gea api user --jq .` must behave identically. Merging across the levels we
    /// descended is a two-line guarantee of that, rather than a bet on which level clap chose.
    pub fn from_chain(chain: &[&ArgMatches]) -> Self {
        let mut out = Self::default();
        for m in chain {
            out = out.or(Self::from_matches(m));
        }
        out
    }

    /// Fill in anything unset from `other`.
    #[must_use]
    fn or(self, other: Self) -> Self {
        Self {
            host: self.host.or(other.host),
            login: self.login.or(other.login),
            repo: self.repo.or(other.repo),
            json: self.json.or(other.json),
            jq: self.jq.or(other.jq),
            template: self.template.or(other.template),
            color: self.color.or(other.color),
            sudo: self.sudo.or(other.sudo),
            otp: self.otp.or(other.otp),
            debug: self.debug || other.debug,
            no_retry: self.no_retry || other.no_retry,
            max_retries: self.max_retries.or(other.max_retries),
            insecure_skip_tls_verify: self.insecure_skip_tls_verify
                || other.insecure_skip_tls_verify,
            paginate: self.paginate || other.paginate,
            limit: self.limit.or(other.limit),
            output: self.output.or(other.output),
            force: self.force || other.force,
        }
    }

    /// The `--json`/`--jq`/`--template` triad, for deciding whether a command should print its
    /// human table.
    pub fn wants_machine_output(&self) -> bool {
        self.json.is_some() || self.jq.is_some() || self.template.is_some()
    }
}

fn one<T>(m: &ArgMatches, id: &str) -> Option<T>
where
    T: std::any::Any + Clone + Send + Sync + 'static,
{
    m.try_get_one::<T>(id).ok().flatten().cloned()
}

fn flag(m: &ArgMatches, id: &str) -> bool {
    m.try_get_one::<bool>(id).ok().flatten().copied().unwrap_or(false)
}

/// Long and short names already claimed anywhere in `cmd`, in the shape [`args`] expects:
/// long names bare, short names with a leading `-` so a one-letter long cannot be mistaken
/// for a short.
pub fn taken(cmd: &Command) -> BTreeSet<String> {
    let mut out = BTreeSet::new();
    walk(cmd, &mut out);
    out
}

fn walk(cmd: &Command, out: &mut BTreeSet<String>) {
    for a in cmd.get_arguments() {
        if let Some(l) = a.get_long() {
            out.insert(l.to_owned());
        }
        if let Some(aliases) = a.get_all_aliases() {
            out.extend(aliases.into_iter().map(str::to_owned));
        }
        if let Some(s) = a.get_short() {
            out.insert(format!("-{s}"));
        }
    }
    for sc in cmd.get_subcommands() {
        walk(sc, out);
    }
}

/// Add every global flag to `cmd`, skipping names `taken` already claims.
pub fn augment(mut cmd: Command, taken: &BTreeSet<String>) -> Command {
    for arg in args(taken) {
        cmd = cmd.arg(arg);
    }
    cmd
}

/// The global arguments. `taken` is empty for the derive root; see the module comment for why
/// the layer-2 root passes a populated one.
pub fn args(taken: &BTreeSet<String>) -> Vec<Arg> {
    let mut out = Vec::new();
    let mut push = |arg: Option<Arg>| {
        if let Some(a) = arg {
            out.push(a.global(true).help_heading(HEADING));
        }
    };

    push(
        named(
            Arg::new(HOST)
                .value_name("HOST")
                .action(ArgAction::Set)
                .help("Gitea server to use [env: GEA_HOST, GITEA_HOST]"),
            "host",
            None,
            taken,
        )
        // `gh` spells this `--hostname`; accepting it hidden costs nothing and saves a
        // "did you mean" round trip for anyone with `gh` in their fingers.
        .map(|a| if taken.contains("hostname") { a } else { a.alias("hostname") }),
    );

    push(named(
        Arg::new(LOGIN)
            .value_name("USER")
            .action(ArgAction::Set)
            .help("Account to use on the selected host [env: GEA_USER, GITEA_USER]"),
        "login",
        None,
        taken,
    ));

    push(named(
        Arg::new(REPO)
            .value_name("OWNER/NAME")
            .action(ArgAction::Set)
            .value_parser(value_parser!(RepoRef))
            .help("Repository to act on: owner/name, host/owner/name, or a URL"),
        "repo",
        Some('R'),
        taken,
    ));

    push(named(
        Arg::new(JSON)
            .value_name("FIELDS")
            .action(ArgAction::Set)
            // Bare `--json` is field discovery, and it is deliberately success rather than a
            // usage error. See `docs/output.md`, divergence 2.
            .num_args(0..=1)
            .default_missing_value("")
            .help("Comma-separated fields to output as JSON; bare --json lists the fields"),
        "json",
        None,
        taken,
    ));

    push(named(
        Arg::new(JQ)
            .value_name("EXPR")
            .action(ArgAction::Set)
            .help("Filter JSON output with a jq expression"),
        "jq",
        Some('q'),
        taken,
    ));

    push(named(
        Arg::new(TEMPLATE)
            .value_name("TEMPLATE")
            .action(ArgAction::Set)
            .help("Format the JSON output with a Go template"),
        "template",
        Some('t'),
        taken,
    ));

    push(named(
        Arg::new(COLOR)
            .value_name("WHEN")
            .action(ArgAction::Set)
            .value_parser(ColorPref::VALUES.to_vec())
            .help("When to colourise output"),
        "color",
        None,
        taken,
    ));

    push(named(
        Arg::new(SUDO)
            .value_name("USER")
            .action(ArgAction::Set)
            .help("Act as another user (admin only)"),
        "sudo",
        None,
        taken,
    ));

    push(named(
        Arg::new(OTP)
            .value_name("CODE")
            .action(ArgAction::Set)
            .help("Two-factor code, sent as X-GITEA-OTP"),
        "otp",
        None,
        taken,
    ));

    push(named(
        Arg::new(DEBUG)
            .action(ArgAction::SetTrue)
            .help("Print context and request details to stderr"),
        "debug",
        None,
        taken,
    ));

    push(named(
        Arg::new(NO_RETRY).action(ArgAction::SetTrue).help("Send each request exactly once"),
        "no-retry",
        None,
        taken,
    ));

    push(named(
        Arg::new(MAX_RETRIES)
            .value_name("N")
            .action(ArgAction::Set)
            .value_parser(value_parser!(u32).range(0..=20))
            .help("Retry a retryable failure at most N times"),
        "max-retries",
        None,
        taken,
    ));

    push(named(
        Arg::new(INSECURE_TLS)
            .action(ArgAction::SetTrue)
            .help("Do not verify the server's TLS certificate"),
        "insecure-skip-tls-verify",
        None,
        taken,
    ));

    push(named(
        Arg::new(PAGINATE).action(ArgAction::SetTrue).help("Follow pagination to the last page"),
        "paginate",
        None,
        taken,
    ));

    push(named(
        Arg::new(LIMIT)
            .value_name("N")
            .action(ArgAction::Set)
            .value_parser(value_parser!(u64).range(1..))
            .help("Stop after N items in total, across all pages"),
        "limit",
        None,
        taken,
    ));

    push(named(
        Arg::new(OUTPUT)
            .value_name("FILE")
            .action(ArgAction::Set)
            .value_parser(value_parser!(PathBuf))
            .value_hint(clap::builder::ValueHint::FilePath)
            .help("Write the response body to FILE instead of stdout"),
        "output",
        None,
        taken,
    ));

    push(named(
        Arg::new(FORCE)
            .action(ArgAction::SetTrue)
            .help("Allow writing binary output to a terminal"),
        "force",
        None,
        taken,
    ));

    out
}

/// Attach whichever of `long`/`short` is still free, or drop the argument if neither is.
///
/// Dropping matters: an `Arg` with no long and no short is a *positional* to clap, which would
/// silently swallow the next word on the command line.
fn named(
    arg: Arg,
    long: &'static str,
    short: Option<char>,
    taken: &BTreeSet<String>,
) -> Option<Arg> {
    let long_free = !taken.contains(long);
    let short_free = short.is_some_and(|s| !taken.contains(&format!("-{s}")));
    if !long_free && !short_free {
        return None;
    }
    let mut arg = arg;
    if long_free {
        arg = arg.long(long);
    }
    if let (Some(s), true) = (short, short_free) {
        arg = arg.short(s);
    }
    Some(arg)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn root(taken: &BTreeSet<String>) -> Command {
        augment(Command::new("gea").no_binary_name(false), taken)
    }

    fn parse(words: &[&str]) -> GlobalOpts {
        let m = root(&BTreeSet::new())
            .try_get_matches_from(words)
            .unwrap_or_else(|e| panic!("{words:?}: {e}"));
        GlobalOpts::from_matches(&m)
    }

    #[test]
    fn bare_json_arrives_as_an_empty_string() {
        // Bug this prevents: wiring `--json` as a plain `Set` argument, which makes bare
        // `--json` a "requires a value" usage error and kills field discovery.
        assert_eq!(parse(&["gea", "--json"]).json.as_deref(), Some(""));
        assert_eq!(parse(&["gea", "--json", "a,b"]).json.as_deref(), Some("a,b"));
        assert_eq!(parse(&["gea"]).json, None);
    }

    #[test]
    fn shorts_and_env_shaped_flags_round_trip() {
        let g = parse(&["gea", "-R", "them/proj", "-q", ".[0]", "-t", "{{.}}", "--limit", "7"]);
        assert_eq!(g.repo.map(|r| r.to_string()).as_deref(), Some("them/proj"));
        assert_eq!(g.jq.as_deref(), Some(".[0]"));
        assert_eq!(g.template.as_deref(), Some("{{.}}"));
        assert_eq!(g.limit, Some(7));
    }

    #[test]
    fn hostname_is_accepted_as_a_hidden_alias_of_host() {
        assert_eq!(parse(&["gea", "--hostname", "x.example"]).host.as_deref(), Some("x.example"));
    }

    /// Bug this prevents: clap panicking on a duplicate long name when a global is propagated
    /// into a layer-2 leaf that already owns that name — `--repo` on the ~200 operations with a
    /// `{repo}` path parameter. The global must lose its long and keep `-R`.
    #[test]
    fn a_taken_long_name_leaves_the_short_behind() {
        let taken: BTreeSet<String> = ["repo".to_owned(), "color".to_owned()].into_iter().collect();
        let args = args(&taken);
        let repo = args.iter().find(|a| a.get_id() == REPO).expect("-R must survive");
        assert_eq!(repo.get_long(), None);
        assert_eq!(repo.get_short(), Some('R'));
        // `--color` has no short, so there is nothing left to keep and it is dropped whole
        // rather than becoming a positional.
        assert!(args.iter().all(|a| a.get_id() != COLOR));
    }

    /// Bug this prevents: `gea api user --jq .login` silently ignoring `--jq` because the value
    /// landed on the subcommand's matches while the caller read the root's (or the reverse).
    #[test]
    fn a_global_is_seen_wherever_on_the_line_it_appears() {
        let root = augment(
            Command::new("gea").subcommand(Command::new("api").arg(Arg::new("e").index(1))),
            &BTreeSet::new(),
        );
        for words in [
            &["gea", "--jq", ".login", "api", "user"][..],
            &["gea", "api", "user", "--jq", ".login"][..],
        ] {
            let m = root.clone().try_get_matches_from(words).unwrap_or_else(|e| panic!("{e}"));
            let sub = m.subcommand_matches("api").expect("api");
            let g = GlobalOpts::from_chain(&[sub, &m]);
            assert_eq!(g.jq.as_deref(), Some(".login"), "{words:?}");
        }
    }

    #[test]
    fn taken_reports_longs_shorts_and_aliases() {
        let cmd = Command::new("x").subcommand(
            Command::new("leaf").arg(Arg::new("a").long("owner").short('o').alias("who")),
        );
        let t = taken(&cmd);
        assert!(t.contains("owner"));
        assert!(t.contains("who"));
        assert!(t.contains("-o"));
    }
}
