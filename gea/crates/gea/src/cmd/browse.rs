//! `gea browse` — open the repository, or a thing in it, in a browser.
//!
//! Modelled on `gh browse`, and a real improvement on `tea open`, which takes no target flags at
//! all: it can open the repository and nothing else. Every flag here saves a round trip through
//! the web UI's navigation.
//!
//! Three decisions worth recording:
//!
//! * **`-n/--no-browser` prints the URL and opens nothing.** That is what makes the command
//!   usable over SSH, in a container, and in a script (`xdg-open $(gea browse -n)`), and it is
//!   the only mode a test can assert, so it is also how the URL builder is tested.
//! * **The URL is built from the client's web base**, not from a `html_url` we fetched. Every
//!   target below is a documented Gitea web route, so building them locally means `gea browse`
//!   makes **zero API requests** in the common case — instant, and it works while the API is
//!   down. The one exception is a file path with no branch given and no checkout to infer one
//!   from, which needs the repository's default branch.
//! * **A number opens `/issues/{n}`.** Gitea redirects that to `/pulls/{n}` when the index
//!   belongs to a pull request, so one flag-free form covers both — and it matches the fact that
//!   issues and pull requests share one numbering sequence.

use clap::Args as ClapArgs;
use gitea_core::config::SystemEnv;
use gitea_core::error::Result;
use gitea_core::http::encode;
use gitea_core::types::RepoSlug;

use crate::cmd::support::{self, listing as emit};
use crate::global::GlobalOpts;
use crate::runtime::Runtime;

#[derive(Debug, ClapArgs)]
pub struct Args {
    /// An issue or pull request number (`42`), or a file path (`src/main.rs`, `src/main.rs:120`)
    #[arg(value_name = "NUMBER | PATH")]
    pub target: Option<String>,

    /// Open the repository's settings
    #[arg(long, group = "where")]
    pub settings: bool,
    /// Open the wiki
    #[arg(long, group = "where")]
    pub wiki: bool,
    /// Open the releases
    #[arg(long, group = "where")]
    pub releases: bool,
    /// Open the Actions runs
    #[arg(long, group = "where")]
    pub actions: bool,
    /// Open the issues
    #[arg(long, group = "where")]
    pub issues: bool,
    /// Open the pull requests
    #[arg(long, group = "where")]
    pub pulls: bool,

    /// Print the URL instead of opening it
    #[arg(short = 'n', long = "no-browser")]
    pub no_browser: bool,
    /// Browse a branch other than the current one
    #[arg(short = 'b', long, value_name = "BRANCH", conflicts_with = "commit")]
    pub branch: Option<String>,
    /// Browse one commit
    #[arg(short = 'c', long, value_name = "SHA")]
    pub commit: Option<String>,
}

pub fn run(globals: &GlobalOpts, args: &Args) -> Result<()> {
    // `--json` has nothing to select here, and saying so beats printing a URL the user then
    // cannot filter.
    if emit::discover(globals, emit::Fields::None)? {
        return Ok(());
    }
    if let Some(t) = &args.target
        && (args.settings
            || args.wiki
            || args.releases
            || args.actions
            || args.issues
            || args.pulls)
    {
        return Err(support::usage(format!(
            "{t:?} and a target flag are two different destinations; pass one or the other"
        )));
    }

    crate::runtime::block_on(async move {
        let rt = Runtime::new(globals)?;
        let slug = rt.repo(globals)?.slug.clone();
        let root = format!("{}/{}/{}", rt.client().web_base(), slug.owner, slug.name);
        let url = build_url(&root, args, &resolve_ref(&rt, globals, args, &slug).await?)?;
        open_or_print(&rt, &url, args.no_browser)
    })
}

/// Which branch or commit a file path is relative to.
///
/// Order: `--commit`, `--branch`, the branch currently checked out, the repository's default
/// branch. Only the last of these costs a request, and it is reached only when someone browses a
/// file from outside a checkout — otherwise the command stays offline.
async fn resolve_ref(
    rt: &Runtime,
    globals: &GlobalOpts,
    args: &Args,
    slug: &RepoSlug,
) -> Result<Ref> {
    if let Some(sha) = &args.commit {
        return Ok(Ref::Commit(sha.clone()));
    }
    if let Some(b) = &args.branch {
        return Ok(Ref::Branch(b.clone()));
    }
    // Only a file path is ref-relative; every other target is a repository-level page, and
    // shelling out to `git` for one would be pure cost.
    if !looks_like_path(args.target.as_deref()) {
        return Ok(Ref::None);
    }
    if let Ok(Some(branch)) = rt.git().current_branch() {
        return Ok(Ref::Branch(branch));
    }
    let api = gitea_client::Api::new(rt.client().clone());
    let repo = api.repo().get(&slug.owner, &slug.name).await?;
    let _ = globals;
    Ok(Ref::Branch(repo.default_branch))
}

/// What a file path is anchored to.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum Ref {
    Branch(String),
    Commit(String),
    None,
}

/// Build the URL. Pure, so every route below is covered by a test rather than by clicking.
pub(crate) fn build_url(root: &str, args: &Args, at: &Ref) -> Result<String> {
    if args.settings {
        return Ok(format!("{root}/settings"));
    }
    if args.wiki {
        return Ok(format!("{root}/wiki"));
    }
    if args.releases {
        return Ok(format!("{root}/releases"));
    }
    if args.actions {
        return Ok(format!("{root}/actions"));
    }
    if args.issues {
        return Ok(format!("{root}/issues"));
    }
    if args.pulls {
        return Ok(format!("{root}/pulls"));
    }

    match args.target.as_deref() {
        // Issues and pull requests share one numbering sequence, and Gitea redirects
        // `/issues/{n}` to the pull request when that is what `n` is.
        Some(t) if t.chars().all(|c| c.is_ascii_digit()) && !t.is_empty() => {
            Ok(format!("{root}/issues/{t}"))
        }
        Some(t) => {
            let (path, line) = split_line(t);
            let anchor = line.map(|l| format!("#L{l}")).unwrap_or_default();
            let path = encode::path_like(path);
            match at {
                Ref::Commit(sha) => Ok(format!("{root}/src/commit/{sha}/{path}{anchor}")),
                Ref::Branch(b) => {
                    Ok(format!("{root}/src/branch/{}/{path}{anchor}", encode::seg(b)))
                }
                // Unreachable from `run`, which always resolves a ref for a path; a direct
                // caller gets the branchless route rather than a panic.
                Ref::None => Ok(format!("{root}/src/{path}{anchor}")),
            }
        }
        None => match at {
            Ref::Commit(sha) => Ok(format!("{root}/commit/{sha}")),
            Ref::Branch(b) => Ok(format!("{root}/src/branch/{}", encode::seg(b))),
            Ref::None => Ok(root.to_owned()),
        },
    }
}

/// Split `src/main.rs:120` into its path and line number.
///
/// A colon that is not followed by digits is left in the path: Windows-style `C:` cannot appear
/// in a repository path, but a file legitimately can be called `notes:draft.md`.
fn split_line(target: &str) -> (&str, Option<&str>) {
    match target.rsplit_once(':') {
        Some((path, line))
            if !line.is_empty() && line.chars().all(|c| c.is_ascii_digit()) && !path.is_empty() =>
        {
            (path, Some(line))
        }
        _ => (target, None),
    }
}

fn looks_like_path(target: Option<&str>) -> bool {
    match target {
        None => false,
        // A non-empty run of digits is an issue or pull request number; anything else,
        // including the empty string, is treated as a path.
        Some(t) => t.is_empty() || !t.chars().all(|c| c.is_ascii_digit()),
    }
}

/// Open a URL, or print it.
///
/// Shared with `gea run view --web` (and anything else that grows a `-w/--web`) so that the
/// browser preference — `browser` in the config file, then `$BROWSER`, then the platform's
/// default handler — is consulted in exactly one place.
pub(crate) fn open_or_print(rt: &Runtime, url: &str, no_browser: bool) -> Result<()> {
    use std::io::Write;
    if no_browser {
        // stdout, with a newline: this is the command's *result*, meant for `$(…)`.
        let mut out = std::io::stdout().lock();
        writeln!(out, "{url}")?;
        out.flush()?;
        return Ok(());
    }
    support::note(rt.term(), &format!("opening {url}"));
    let browser = rt.config().resolved_browser(Some(rt.host().as_str()), &SystemEnv);
    let result = match &browser {
        Some(b) => open::with(url, b),
        None => open::that(url),
    };
    result.map_err(|e| {
        support::usage(format!(
            "could not open a browser ({e}); pass -n to print the URL instead:\n  {url}"
        ))
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    const ROOT: &str = "https://git.example.org/perf3ct/gea";

    fn args(target: Option<&str>) -> Args {
        Args {
            target: target.map(str::to_owned),
            settings: false,
            wiki: false,
            releases: false,
            actions: false,
            issues: false,
            pulls: false,
            no_browser: true,
            branch: None,
            commit: None,
        }
    }

    fn url(a: &Args, at: &Ref) -> String {
        build_url(ROOT, a, at).unwrap()
    }

    /// Every documented target, in one table. Bug this prevents: a flag that parses and then
    /// opens the repository root, which looks like it worked.
    #[test]
    fn every_target_flag_has_its_own_route() {
        /// One `--flag` setter, so the table below is readable at a glance.
        type SetFlag = fn(&mut Args);

        assert_eq!(url(&args(None), &Ref::None), ROOT);
        let flags: [(SetFlag, &str); 6] = [
            (|a| a.settings = true, "/settings"),
            (|a| a.wiki = true, "/wiki"),
            (|a| a.releases = true, "/releases"),
            (|a| a.actions = true, "/actions"),
            (|a| a.issues = true, "/issues"),
            (|a| a.pulls = true, "/pulls"),
        ];
        for (set, suffix) in flags {
            let mut one = args(None);
            set(&mut one);
            assert_eq!(url(&one, &Ref::None), format!("{ROOT}{suffix}"));
        }
    }

    /// A bare number is an issue *or* a pull request: Gitea redirects, and the two share one
    /// numbering sequence. Bug this prevents: guessing `/pulls/42` for a number that is an issue,
    /// which 404s.
    #[test]
    fn a_number_opens_the_shared_issue_route() {
        assert_eq!(url(&args(Some("42")), &Ref::None), format!("{ROOT}/issues/42"));
    }

    #[test]
    fn a_path_is_anchored_to_a_branch_a_commit_and_a_line() {
        let a = args(Some("crates/gea/src/main.rs"));
        assert_eq!(
            url(&a, &Ref::Branch("main".into())),
            format!("{ROOT}/src/branch/main/crates/gea/src/main.rs")
        );
        assert_eq!(
            url(&a, &Ref::Commit("deadbeef".into())),
            format!("{ROOT}/src/commit/deadbeef/crates/gea/src/main.rs")
        );
        let a = args(Some("src/main.rs:120"));
        assert_eq!(
            url(&a, &Ref::Branch("main".into())),
            format!("{ROOT}/src/branch/main/src/main.rs#L120")
        );
    }

    /// Bug this prevents: percent-encoding a path's slashes (which turns a file URL into a 404),
    /// or *not* encoding a space or `#` in a branch or file name (which truncates the URL).
    #[test]
    fn slashes_survive_encoding_but_spaces_do_not() {
        let a = args(Some("docs/release notes.md"));
        assert_eq!(
            url(&a, &Ref::Branch("feature/new thing".into())),
            format!("{ROOT}/src/branch/feature%2Fnew%20thing/docs/release%20notes.md")
        );
    }

    #[test]
    fn a_line_suffix_is_only_a_line_when_it_is_digits() {
        assert_eq!(split_line("a/b.rs:12"), ("a/b.rs", Some("12")));
        assert_eq!(split_line("notes:draft.md"), ("notes:draft.md", None));
        assert_eq!(split_line("a/b.rs:"), ("a/b.rs:", None));
    }

    /// Bug this prevents: resolving a branch (a `git` subprocess, or an API call for the default
    /// branch) for a target that does not need one — `gea browse --releases` must be offline.
    #[test]
    fn only_a_file_path_needs_a_ref() {
        assert!(looks_like_path(Some("src/main.rs")));
        assert!(!looks_like_path(Some("42")));
        assert!(!looks_like_path(None));
    }

    /// `-n` prints a URL to stdout and opens nothing. The "opens nothing" half cannot be asserted
    /// from in-process (there is no browser to observe), so it is asserted in
    /// `tests/porcelain.rs`, which runs the binary with `BROWSER` pointed at a script that would
    /// leave a file behind.
    #[test]
    fn no_browser_is_the_default_for_these_tests() {
        assert!(args(None).no_browser);
    }
}
