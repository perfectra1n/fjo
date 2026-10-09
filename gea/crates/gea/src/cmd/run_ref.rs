//! How a user names an Actions run, and one of its jobs.
//!
//! # Two numbers, and a URL
//!
//! Every Actions run has two integers, and `gea run list` prints both: `ID` (the database row,
//! 6442) and `NUMBER` (the per-repository counter the web UI used to show, 3623). The API path
//! wants the id. Before this module, `gea run view 3623` was a bare 404 that never said "that is a
//! run number", and a pasted `…/actions/runs/6442` was rejected as "not a number" — the two most
//! natural things to type were both dead ends.
//!
//! So a run reference is one of three shapes, parsed by [`RunRef`]'s `FromStr` (which is what
//! clap's `value_parser!` uses):
//!
//! | you type | means |
//! | --- | --- |
//! | `6442` | an id — and, only if no run has that id, the run whose number it is |
//! | `#3623` | explicitly the run number |
//! | `https://host/o/r/actions/runs/6442[/jobs/33898]` | the id in the URL (Gitea 1.27's `html_url` carries the id), and optionally a job by its id |
//!
//! The bare-number fallback is deliberately **one-way and loud**: the id is tried first, because
//! that is what every `gea` output prints and what the API means; the number is consulted only on
//! a 404, and when it is used a note says so. Guessing silently would be worse than the 404 it
//! replaces — an id and a number that are both valid name two *different* runs.
//!
//! # Jobs, by name
//!
//! `--job` used to take only a numeric job id, which nobody knows. A [`JobRef`] is an id when it
//! is all digits and a name otherwise. Names follow the rule `gea milestone` and `gea project`
//! already set (see `docs/porcelain-conventions.md`): an exact match wins, a unique substring is
//! accepted, and an **ambiguous** name is refused with the candidates listed — never guessed,
//! because a matrix build's `build (linux)` and `build (macos)` are both "build".

// `gea run` uses these now; `run_id_from_url` is for `gea pr checks` and `pick_job`'s generic
// form for `gea ci`, which land separately. Until both do, some of this is reached only from the
// tests.
#![cfg_attr(not(test), allow(dead_code))]

use std::str::FromStr;

use gitea_client::{Api, query};
use gitea_core::error::{Error, ErrorKind, Result};
use gitea_core::types::RepoSlug;
use gitea_model::{ActionWorkflowJob, ActionWorkflowRun};

use crate::cmd::run::int;
use crate::cmd::support;

/// The accepted forms, as one clause. Shared by the parse error and the not-found hint, so the
/// two cannot come to describe different syntaxes.
pub(crate) const RUN_FORMS: &str =
    "expected a run id (`6442`), a run number (`#3623`), or a run URL";

/// How many pages of runs a run-number lookup reads before giving up.
///
/// There is no server-side `run_number` filter, so finding `#N` means paging the runs newest
/// first. The scan stops early as soon as a page's oldest run is numbered below `N` (numbers are
/// monotonic per repository, so `N` cannot be further back), which makes the cap matter only for
/// a run that is very old — and for those the id, which costs no scan at all, is the better
/// handle. The cap turns "read the repository's entire history" into a message saying so.
const NUMBER_SCAN_PAGES: i32 = 20;

/// Runs per page during a run-number scan. Gitea clamps `limit` to `max_response_items`
/// (default 50) without saying so; the scan reads whatever each page holds, so a clamp only
/// costs extra pages and never a wrong answer.
const NUMBER_SCAN_PAGE_SIZE: i32 = 50;

/// A run, as the user named it. See the module docs.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum RunRef {
    /// `6442`: an id, falling back to the run number on a 404.
    Bare(i64),
    /// `#3623`: explicitly the per-repository run number.
    Number(i64),
    /// A run's web URL. `run` is treated like [`RunRef::Bare`].
    Url {
        /// The run segment of the URL.
        run: i64,
        /// `…/jobs/<n>`: the job with that **id**. Gitea 1.27 puts the job's id there, not its
        /// position (measured: a run whose jobs are ids 3 and 4 links them as `jobs/3` and
        /// `jobs/4`), and so do the `target_url`s of the commit statuses Actions posts.
        job: Option<i64>,
        /// The repository the URL names. **Not** used to switch repository — `-R` does that —
        /// only to warn when it differs from the one the command resolved.
        slug: RepoSlug,
    },
}

impl RunRef {
    /// The URL's job id, when the reference was a job URL.
    pub(crate) fn url_job(&self) -> Option<i64> {
        match self {
            RunRef::Url { job, .. } => *job,
            RunRef::Bare(_) | RunRef::Number(_) => None,
        }
    }
}

impl std::fmt::Display for RunRef {
    /// The reference as the user could type it back — what an error or a hint should quote.
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            RunRef::Bare(n) | RunRef::Url { run: n, .. } => write!(f, "{n}"),
            RunRef::Number(n) => write!(f, "#{n}"),
        }
    }
}

impl FromStr for RunRef {
    type Err = String;

    /// The error is a `String` because that is what clap renders as the reason after
    /// `invalid value '…' for '<RUN>'`, and naming every accepted form there is the fix for the
    /// old "is not a number".
    fn from_str(raw: &str) -> std::result::Result<Self, Self::Err> {
        let s = raw.trim();
        if let Some(n) = s.strip_prefix('#') {
            return positive(n).map(RunRef::Number).ok_or_else(|| reject(raw));
        }
        if let Some(n) = positive(s) {
            return Ok(RunRef::Bare(n));
        }
        if let Some(u) = parse_run_url(s) {
            return Ok(RunRef::Url { run: u.run, job: u.job, slug: u.slug });
        }
        Err(reject(raw))
    }
}

fn reject(raw: &str) -> String {
    format!("{raw:?} is not a run: {RUN_FORMS}")
}

/// A strictly positive decimal integer, and nothing else — no sign, no whitespace, no `0x`.
///
/// Stricter than `str::parse::<i64>`, which accepts `+42`: a run reference that parses in a way
/// the user cannot predict is a reference to the wrong run.
fn positive(s: &str) -> Option<i64> {
    if s.is_empty() || !s.bytes().all(|b| b.is_ascii_digit()) {
        return None;
    }
    s.parse::<i64>().ok().filter(|n| *n > 0)
}

/// What a run URL names.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct RunUrl {
    /// The repository in the URL's path.
    pub slug: RepoSlug,
    /// The run segment: the run's id on Gitea 1.27.
    pub run: i64,
    /// The `jobs/<n>` segment, when there was one: the job's id on Gitea 1.27.
    pub job: Option<i64>,
}

/// `https://host[/subpath]/owner/name/actions/runs/6442[/jobs/33898|/attempts/3][/][?…][#…]`,
/// or the same without scheme and host (`/owner/name/actions/runs/6442/jobs/33898`).
///
/// The path-only form is not a nicety: it is what Gitea 1.27 puts in the `target_url` of every
/// commit status its Actions post, so a parser that insisted on `://` recognised no real check
/// as an Actions run, and `gea pr checks` never pointed at `gea ci`.
///
/// Parsed by hand, like `pr::common::parse_url`, rather than through `RepoRef`: the slug sits in
/// front of three more segments, which `RepoRef` would read as the repository's name. Anything
/// after the recognised segments is ignored rather than refused, so a link copied from a deeper
/// page still names its run.
pub(crate) fn parse_run_url(raw: &str) -> Option<RunUrl> {
    let raw = raw.trim();
    let path = match raw.split_once("://") {
        Some((_scheme, rest)) => rest.split_once('/')?.1,
        None => raw.strip_prefix('/')?,
    };
    // The query and the fragment are not part of the path, and a `#L12` anchor on a log line is
    // exactly the kind of thing that gets pasted.
    let path = path.split(['?', '#']).next()?;
    let segs: Vec<&str> = path.split('/').filter(|s| !s.is_empty()).collect();
    let at = segs.windows(2).position(|w| w == ["actions", "runs"])?;
    let owner = segs.get(at.checked_sub(2)?)?;
    let name = segs.get(at - 1)?;
    let run = positive(segs.get(at + 2)?)?;
    let job = match segs.get(at + 3) {
        Some(&"jobs") => Some(positive(segs.get(at + 4)?)?),
        _ => None,
    };
    Some(RunUrl { slug: RepoSlug::new(*owner, *name), run, job })
}

/// The run id in an Actions run URL, or `None` when the URL is not one.
///
/// For `gea pr checks`, whose status checks carry a `target_url` and nothing else: when that URL
/// is an Actions run of the same repository, the check can be followed with `gea ci`. Same parser
/// as [`RunRef`], so the two cannot disagree about which URLs are runs.
pub(crate) fn run_id_from_url(raw: &str) -> Option<i64> {
    parse_run_url(raw).map(|u| u.run)
}

/// Turn a [`RunRef`] into the run it names, in `slug`.
///
/// * [`RunRef::Bare`] and [`RunRef::Url`]: `GET …/actions/runs/{n}`. On a 404 **only**, the run
///   whose `run_number` is `n` is used instead, with one note on stderr saying so. If there is
///   none, the original 404 is returned, after a note naming both forms. Any other error — a 403,
///   a 5xx, a network failure — propagates unchanged: it says nothing about whether `n` was the
///   wrong kind of number, and retrying it as one would only hide it.
/// * [`RunRef::Number`]: the run with that number, or `ResourceNotFound`.
///
/// `term` is for the notes, which follow `support::note`'s rule (a terminal only).
pub(crate) async fn resolve_run(
    api: &Api,
    slug: &RepoSlug,
    r: &RunRef,
) -> Result<ActionWorkflowRun> {
    let id = match r {
        RunRef::Number(n) => {
            return run_by_number(api, slug, *n).await?.ok_or_else(|| number_not_found(slug, *n));
        }
        RunRef::Bare(n) => *n,
        RunRef::Url { run, slug: named, .. } => {
            if named != slug {
                support::warn(&format!(
                    "note: the URL names {named}, but this command is working in {slug}; \
                         looking run {run} up in {slug} (pass -R {named} to use the URL's)"
                ));
            }
            *run
        }
    };

    let miss = match api.run().view(&slug.owner, &slug.name, int(id)?).await {
        Ok(run) => return Ok(run),
        Err(e) if is_miss(&e) => e,
        Err(e) => return Err(e),
    };
    // A failed scan is not reported in place of the 404: the 404 is the answer to what was
    // asked, and the scan was only ever a courtesy on top of it.
    if let Ok(Some(run)) = run_by_number(api, slug, id).await {
        support::warn(&format!("note: no run has id {id}; using run #{id} (id {})", run.id));
        return Ok(run);
    }
    support::warn(&format!(
        "note: {slug} has no run with id {id} and no run numbered #{id}. `gea run list` \
             prints both: pass an ID as it is, or a NUMBER as #N."
    ));
    Err(miss)
}

/// Whether an error is the "no such run" 404, as opposed to anything else.
///
/// Two shapes, because `http::classify` names a 404 by what its repository probe found: with the
/// probe it is `ResourceNotFound` (the repository exists, the run does not), and without one it is
/// `RepoNotFound { probed: false }`. `RepoNotFound { probed: true }` is excluded on purpose — the
/// repository itself is gone, and no run number will be found in it either.
fn is_miss(e: &Error) -> bool {
    matches!(
        e.kind(),
        ErrorKind::ResourceNotFound { .. } | ErrorKind::RepoNotFound { probed: false, .. }
    )
}

fn number_not_found(slug: &RepoSlug, n: i64) -> Error {
    support::not_found("workflow run", format!("#{n}"), slug)
}

/// The run numbered `n`, scanning newest first. `Ok(None)` when it provably does not exist.
///
/// Stops at the first of: a match; a page whose oldest run is numbered below `n` (it would have
/// been on this page or an earlier one); an empty page or the collection's end. Past
/// [`NUMBER_SCAN_PAGES`] it fails with a usage error suggesting the id, rather than reading the
/// whole history of a busy repository one page at a time.
async fn run_by_number(api: &Api, slug: &RepoSlug, n: i64) -> Result<Option<ActionWorkflowRun>> {
    let mut seen: i64 = 0;
    for page in 1..=NUMBER_SCAN_PAGES {
        let q = query::GetWorkflowRunsQuery {
            page: Some(page),
            limit: Some(NUMBER_SCAN_PAGE_SIZE),
            ..Default::default()
        };
        let resp = api.run().list(&slug.owner, &slug.name, &q).await?;
        let got = resp.workflow_runs.len() as i64;
        let oldest = resp.workflow_runs.iter().map(|r| r.run_number).min();
        if let Some(run) = resp.workflow_runs.into_iter().find(|r| r.run_number == n) {
            return Ok(Some(run));
        }
        seen += got;
        if got == 0 || oldest.is_some_and(|o| o < n) || seen >= resp.total_count {
            return Ok(None);
        }
    }
    Err(support::usage(format!(
        "run #{n} is not among the newest {} runs of {slug}, and finding a run number means \
         reading them one page at a time; pass the run's id instead (the ID column of \
         `gea run list`, or the number in the run's URL)",
        i64::from(NUMBER_SCAN_PAGES) * i64::from(NUMBER_SCAN_PAGE_SIZE)
    )))
}

// --------------------------------------------------------------------------------------- jobs

/// A job, as the user named it: `--job 33898` or `--job "Fast Checks"`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum JobRef {
    /// All digits: a job id.
    Id(i64),
    /// Anything else: matched against job names, see [`pick_job`].
    Name(String),
}

impl std::fmt::Display for JobRef {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            JobRef::Id(n) => write!(f, "{n}"),
            JobRef::Name(s) => write!(f, "{s:?}"),
        }
    }
}

impl FromStr for JobRef {
    type Err = String;

    /// A string error for clap, as with [`RunRef`].
    fn from_str(raw: &str) -> std::result::Result<Self, Self::Err> {
        let s = raw.trim();
        if s.is_empty() {
            return Err("expected a job id (`33898`) or a job name (`build`)".to_owned());
        }
        if s.bytes().all(|b| b.is_ascii_digit()) {
            return s.parse::<i64>().map(JobRef::Id).map_err(|_| format!("{s} is not a job id"));
        }
        Ok(JobRef::Name(s.to_owned()))
    }
}

/// The job of `jobs` that `r` names. See [`pick_job`] for the rules.
pub(crate) fn resolve_job<'a>(
    jobs: &'a [ActionWorkflowJob],
    r: &JobRef,
) -> Result<&'a ActionWorkflowJob> {
    pick_job(jobs, |j| j, |j| format!("{} (id {})", j.name, j.id), r)
}

/// [`resolve_job`] over any list that carries jobs — one run's, or several runs' at once.
///
/// Generic so that `gea ci logs --job`, which matches across every run of a commit, applies the
/// same rules and describes its candidates with their workflow (`ci.yml / build (id 11)`)
/// without a second copy of the matching:
///
/// * [`JobRef::Id`]: the job with that id.
/// * [`JobRef::Name`]: a case-insensitive **exact** name match wins; otherwise a unique
///   case-insensitive **substring** match. More than one match at either stage is a usage error
///   listing every candidate, because picking one would be a guess.
///
/// Not found is `ResourceNotFound` (exit 5), listing what *is* there so the next attempt does not
/// need a separate `gea run jobs`.
pub(crate) fn pick_job<'a, T>(
    candidates: &'a [T],
    job_of: impl Fn(&T) -> &ActionWorkflowJob,
    describe: impl Fn(&T) -> String,
    r: &JobRef,
) -> Result<&'a T> {
    let matches: Vec<&T> = match r {
        JobRef::Id(id) => candidates.iter().filter(|c| job_of(c).id.get() == *id).collect(),
        JobRef::Name(name) => {
            let want = name.to_lowercase();
            let exact: Vec<&T> =
                candidates.iter().filter(|c| job_of(c).name.to_lowercase() == want).collect();
            if exact.is_empty() {
                candidates
                    .iter()
                    .filter(|c| job_of(c).name.to_lowercase().contains(&want))
                    .collect()
            } else {
                exact
            }
        }
    };
    match matches.as_slice() {
        [one] => Ok(one),
        [] => Err(job_not_found(&r.to_string(), candidates.iter().map(&describe).collect())),
        many => Err(support::usage(format!(
            "job name {r} matches {} jobs: {}; pass the id to choose one",
            many.len(),
            many.iter().map(|c| describe(c)).collect::<Vec<_>>().join(", ")
        ))),
    }
}

fn job_not_found(what: &str, have: Vec<String>) -> Error {
    let have = if have.is_empty() { "none yet".to_owned() } else { have.join(", ") };
    Error::new(ErrorKind::ResourceNotFound {
        kind: "job",
        id: format!("{what} (the jobs are: {have})"),
        slug: None,
        // Discovered locally: the job listing succeeded and nothing in it matched.
        server_message: None,
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::cmd::support::testing::{self, on, on_fn};
    use gitea_core::http::transport::Canned;
    use gitea_core::types::ids::JobId;
    use std::sync::Arc;

    fn slug() -> RepoSlug {
        RepoSlug::new("o", "r")
    }

    #[test]
    fn every_run_reference_form_parses() {
        let url = |run, job| RunRef::Url { run, job, slug: slug() };
        let cases: &[(&str, RunRef)] = &[
            ("6442", RunRef::Bare(6442)),
            (" 6442 ", RunRef::Bare(6442)),
            ("#3623", RunRef::Number(3623)),
            ("https://git.example.org/o/r/actions/runs/6442", url(6442, None)),
            ("https://git.example.org/o/r/actions/runs/6442/", url(6442, None)),
            ("https://git.example.org/o/r/actions/runs/6442/jobs/33898", url(6442, Some(33898))),
            ("https://git.example.org/o/r/actions/runs/6442/jobs/33898/", url(6442, Some(33898))),
            // The path-only form Gitea puts in an Actions commit status's `target_url`.
            ("/o/r/actions/runs/6442/jobs/12", url(6442, Some(12))),
            ("https://git.example.org/o/r/actions/runs/6442/attempts/3", url(6442, None)),
            ("https://git.example.org/o/r/actions/runs/6442?tab=logs", url(6442, None)),
            ("https://git.example.org/o/r/actions/runs/6442/jobs/1#L12", url(6442, Some(1))),
            // A subpath install: the prefix is not part of the slug.
            ("https://example.org/gitea/o/r/actions/runs/6442", url(6442, None)),
        ];
        for (raw, want) in cases {
            assert_eq!(raw.parse::<RunRef>().as_ref(), Ok(want), "{raw}");
        }
    }

    /// Bug this prevents: the old "is not a number", which never said what *would* be accepted.
    #[test]
    fn a_rejected_run_reference_names_every_accepted_form() {
        for raw in [
            "",
            "#",
            "0",
            "#0",
            "-5",
            "+5",
            "abc",
            "#x",
            "6442x",
            "https://git.example.org/o/r/actions/runs",
            "https://git.example.org/o/r/actions/runs/abc",
            "https://git.example.org/o/r/pulls/4",
            "https://git.example.org/o/r/actions/runs/6442/jobs/x",
            "https://git.example.org/o/r/actions/runs/6442/jobs/0",
            "o/r/actions/runs/6442",
        ] {
            let err = raw.parse::<RunRef>().expect_err(raw);
            assert!(err.contains(RUN_FORMS), "{raw}: {err}");
        }
    }

    #[test]
    fn a_run_reference_prints_as_it_would_be_typed() {
        assert_eq!(RunRef::Bare(6442).to_string(), "6442");
        assert_eq!(RunRef::Number(3623).to_string(), "#3623");
        let u: RunRef = "https://h/o/r/actions/runs/7/jobs/1".parse().unwrap();
        assert_eq!((u.to_string(), u.url_job()), ("7".to_owned(), Some(1)));
    }

    /// For `pr checks`: a check's `target_url` is a run only when it really is an Actions run.
    #[test]
    fn a_run_id_is_read_out_of_a_check_url_and_nothing_else_is() {
        assert_eq!(run_id_from_url("https://h/o/r/actions/runs/6442/jobs/1"), Some(6442));
        // Gitea 1.27 posts Actions statuses with a path-only `target_url`. Bug this prevents:
        // `gea pr checks` treating every real Actions check as external CI, and so never
        // printing its `next: gea ci …` hint (found by the live suite, not by a fixture).
        assert_eq!(run_id_from_url("/o/r/actions/runs/6/jobs/12"), Some(6));
        assert_eq!(run_id_from_url("/gitea/o/r/actions/runs/6"), Some(6));
        assert_eq!(run_id_from_url("https://ci.example.org/builds/6442"), None);
        assert_eq!(run_id_from_url(""), None);
        assert_eq!(
            parse_run_url("https://h/them/proj/actions/runs/9").map(|u| u.slug.to_string()),
            Some("them/proj".to_owned())
        );
    }

    #[test]
    fn job_references_are_ids_when_all_digits_and_names_otherwise() {
        assert_eq!("33898".parse::<JobRef>(), Ok(JobRef::Id(33898)));
        assert_eq!("Fast Checks".parse::<JobRef>(), Ok(JobRef::Name("Fast Checks".into())));
        assert_eq!("build-2".parse::<JobRef>(), Ok(JobRef::Name("build-2".into())));
        assert!("".parse::<JobRef>().is_err());
        assert!("   ".parse::<JobRef>().is_err());
        assert!("99999999999999999999".parse::<JobRef>().is_err());
    }

    // ------------------------------------------------------------------------- resolve_run

    const RUN_6442: &str = r#"{"id":6442,"run_number":3623,"path":"ci.yml@refs/heads/main"}"#;

    #[tokio::test]
    async fn a_bare_id_that_exists_is_one_request() {
        let fake = Arc::new(on(
            testing::transport(),
            "GET",
            "/api/v1/repos/o/r/actions/runs/6442",
            Canned::json(200, RUN_6442),
        ));
        let run =
            resolve_run(&testing::api(fake.clone()), &slug(), &RunRef::Bare(6442)).await.unwrap();
        assert_eq!(run.id.get(), 6442);
        assert_eq!(fake.call_count(), 1, "{:?}", fake.calls());
    }

    /// The reported bug: `gea run view 3623` with the NUMBER column's value was a bare 404.
    #[tokio::test]
    async fn a_bare_number_that_is_no_id_falls_back_to_the_run_number() {
        let fake = Arc::new(on(
            on(
                testing::transport(),
                "GET",
                "/api/v1/repos/o/r/actions/runs/3623",
                Canned::json(404, r#"{"message":"run not found"}"#),
            ),
            "GET",
            "/api/v1/repos/o/r/actions/runs",
            Canned::json(
                200,
                format!(
                    r#"{{"total_count":2,"workflow_runs":[{{"id":6443,"run_number":3624}},{RUN_6442}]}}"#
                ),
            ),
        ));
        let run =
            resolve_run(&testing::api(fake.clone()), &slug(), &RunRef::Bare(3623)).await.unwrap();
        assert_eq!((run.id.get(), run.run_number), (6442, 3623));
        assert_eq!(fake.call_count(), 2, "{:?}", fake.calls());
    }

    /// Neither an id nor a number: the 404 that came back is the one returned, unchanged — not
    /// a "run #N not found" that hides the id lookup having happened at all.
    #[tokio::test]
    async fn a_bare_number_that_is_neither_returns_the_original_404() {
        let fake = Arc::new(on(
            on(
                testing::transport(),
                "GET",
                "/api/v1/repos/o/r/actions/runs/99",
                Canned::json(404, r#"{"message":"run not found"}"#),
            ),
            "GET",
            "/api/v1/repos/o/r/actions/runs",
            Canned::json(200, format!(r#"{{"total_count":1,"workflow_runs":[{RUN_6442}]}}"#)),
        ));
        let err = resolve_run(&testing::api(fake), &slug(), &RunRef::Bare(99)).await.unwrap_err();
        assert_eq!(err.exit_code(), 5, "{err:?}");
        assert!(
            err.ctx.path.as_deref().is_some_and(|p| p.ends_with("/actions/runs/99")),
            "{err:?}"
        );
    }

    /// Bug this prevents: a 403 or a 5xx being "retried" as a run number, so a permissions
    /// problem surfaces as a confusing "no run numbered #N".
    #[tokio::test]
    async fn a_non_404_error_is_not_retried_as_a_run_number() {
        let fake = Arc::new(on(
            testing::transport(),
            "GET",
            "/api/v1/repos/o/r/actions/runs/6442",
            Canned::json(403, r#"{"message":"no"}"#),
        ));
        let err = resolve_run(&testing::api(fake.clone()), &slug(), &RunRef::Bare(6442))
            .await
            .unwrap_err();
        assert_ne!(err.exit_code(), 5, "{err:?}");
        assert_eq!(fake.call_count(), 1, "{:?}", fake.calls());
    }

    /// Numbers are monotonic per repository, so once a page's oldest run is numbered below the
    /// one wanted, it is not further back. Bug this prevents: `#N` for a missing number reading
    /// all twenty pages of a busy repository.
    #[tokio::test]
    async fn a_run_number_scan_stops_once_the_numbers_go_below_it() {
        let fake = Arc::new(on_fn(
            testing::transport(),
            "GET",
            "/api/v1/repos/o/r/actions/runs",
            |c| {
                let page: i64 = c.query_param("page").and_then(|p| p.parse().ok()).unwrap_or(1);
                // Page 1: 120, 119; page 2: 118, 117 ... — 500 runs in all.
                let top = 122 - 2 * page;
                Canned::json(
                    200,
                    format!(
                        r#"{{"total_count":500,"workflow_runs":[{{"id":{a},"run_number":{a}}},{{"id":{b},"run_number":{b}}}]}}"#,
                        a = top,
                        b = top - 1
                    ),
                )
            },
        ));
        let api = testing::api(fake.clone());

        let run = resolve_run(&api, &slug(), &RunRef::Number(117)).await.unwrap();
        assert_eq!(run.run_number, 117);
        assert_eq!(fake.call_count(), 2, "{:?}", fake.calls());

        // Newer than any run: the very first page is already below it, so that page is the
        // whole scan.
        let before = fake.call_count();
        let err = resolve_run(&api, &slug(), &RunRef::Number(1000)).await.unwrap_err();
        assert_eq!(err.exit_code(), 5, "{err}");
        assert!(err.to_string().contains("#1000"), "{err}");
        assert_eq!(fake.call_count() - before, 1, "stopped after the first page");
    }

    /// The cap: a number older than twenty pages is a usage error pointing at the id, not a scan
    /// of the repository's entire history.
    #[tokio::test]
    async fn a_run_number_scan_gives_up_after_twenty_pages() {
        let fake = Arc::new(on_fn(
            testing::transport(),
            "GET",
            "/api/v1/repos/o/r/actions/runs",
            |c| {
                let page: i64 = c.query_param("page").and_then(|p| p.parse().ok()).unwrap_or(1);
                let n = 100_000 - page;
                Canned::json(
                    200,
                    format!(
                        r#"{{"total_count":100000,"workflow_runs":[{{"id":{n},"run_number":{n}}}]}}"#
                    ),
                )
            },
        ));
        let err = resolve_run(&testing::api(fake.clone()), &slug(), &RunRef::Number(5))
            .await
            .unwrap_err();
        assert_eq!(err.exit_code(), 2, "{err}");
        assert!(err.to_string().contains("pass the run's id"), "{err}");
        assert_eq!(fake.call_count(), NUMBER_SCAN_PAGES as usize);
    }

    #[tokio::test]
    async fn a_run_url_is_looked_up_by_its_id() {
        let fake = Arc::new(on(
            testing::transport(),
            "GET",
            "/api/v1/repos/o/r/actions/runs/6442",
            Canned::json(200, RUN_6442),
        ));
        let r: RunRef =
            "https://git.example.org/other/repo/actions/runs/6442/jobs/1".parse().unwrap();
        let run = resolve_run(&testing::api(fake), &slug(), &r).await.unwrap();
        assert_eq!(run.id.get(), 6442);
    }

    // ------------------------------------------------------------------------- resolve_job

    fn job(id: i64, name: &str) -> ActionWorkflowJob {
        ActionWorkflowJob { id: JobId::new(id), name: name.into(), ..Default::default() }
    }

    fn jobs() -> Vec<ActionWorkflowJob> {
        vec![
            job(11, "build"),
            job(12, "build (linux)"),
            job(13, "build (macos)"),
            job(14, "Fast Checks (fmt + static lints)"),
        ]
    }

    /// `build` is both an exact name and a substring of two others. Bug this prevents: the
    /// exact match being refused as ambiguous, which would make a job literally called `build`
    /// unreachable by name.
    #[test]
    fn an_exact_name_beats_substring_matches() {
        let jobs = jobs();
        let name = |s: &str| JobRef::Name(s.into());
        assert_eq!(resolve_job(&jobs, &name("BUILD")).unwrap().id.get(), 11);
        assert_eq!(resolve_job(&jobs, &name("fast checks")).unwrap().id.get(), 14);
        assert_eq!(resolve_job(&jobs, &name("macos")).unwrap().id.get(), 13);
        assert_eq!(resolve_job(&jobs, &JobRef::Id(12)).unwrap().name, "build (linux)");
    }

    /// Never guess: `build (` matches two jobs, and the refusal lists both with their ids.
    #[test]
    fn an_ambiguous_name_is_refused_with_the_candidates() {
        let err = resolve_job(&jobs(), &JobRef::Name("build (".into())).unwrap_err();
        assert_eq!(err.exit_code(), 2, "{err}");
        let msg = err.to_string();
        assert!(msg.contains("build (linux) (id 12)"), "{msg}");
        assert!(msg.contains("build (macos) (id 13)"), "{msg}");
        assert!(msg.contains("pass the id to choose one"), "{msg}");
    }

    #[test]
    fn a_missing_job_is_not_found_and_lists_what_is_there() {
        for r in [JobRef::Id(99), JobRef::Name("deploy".into())] {
            let err = resolve_job(&jobs(), &r).unwrap_err();
            assert_eq!(err.exit_code(), 5, "{err}");
            assert!(err.to_string().contains("Fast Checks (fmt + static lints) (id 14)"), "{err}");
        }
    }

    /// The candidates' description is the caller's, which is what lets `gea ci logs` label them
    /// with their workflow.
    #[test]
    fn candidates_are_described_by_the_caller() {
        let pairs = [("ci.yml", job(1, "lint")), ("release.yml", job(2, "lint"))];
        let err = pick_job(
            &pairs,
            |p| &p.1,
            |p| format!("{} / {} (id {})", p.0, p.1.name, p.1.id),
            &JobRef::Name("lint".into()),
        )
        .unwrap_err();
        assert!(err.to_string().contains("release.yml / lint (id 2)"), "{err}");
    }

    /// Bug this prevents: a pasted job URL acting on the wrong job. Gitea 1.27 links a job as
    /// `…/jobs/<job id>` (measured against a live instance: jobs 3 and 4 of a run are `jobs/3` and
    /// `jobs/4`), so the segment is an id, and treating it as a position picked a different job
    /// or none at all.
    #[test]
    fn a_url_job_segment_is_a_job_id() {
        let u: RunRef = "https://h/o/r/actions/runs/7/jobs/13".parse().unwrap();
        assert_eq!(u.url_job(), Some(13));
        let jobs = jobs();
        assert_eq!(
            resolve_job(&jobs, &JobRef::Id(u.url_job().unwrap())).unwrap().name,
            "build (macos)"
        );
    }
}
