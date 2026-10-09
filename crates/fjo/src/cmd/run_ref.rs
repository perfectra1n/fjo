//! How a user names an Actions run, and one of its jobs.
//!
//! # Two numbers, and a URL
//!
//! Every Actions run has two integers, and `fjo run list` prints both: `ID` (the database row,
//! 6442, which the API path wants) and `INDEX` (the per-repository counter, 3623 — the run's
//! *number*). Before this module, `fjo run view 3623` was a bare 404 that never said "that is a
//! run number", and a pasted `…/actions/runs/3623` was rejected as "not a number" — the two most
//! natural things to type were both dead ends.
//!
//! So a run reference is one of three shapes, parsed by [`RunRef`]'s `FromStr` (which is what
//! clap's `value_parser!` uses):
//!
//! | you type | means |
//! | --- | --- |
//! | `6442` | an id — and, only if no run has that id, the run whose number it is |
//! | `#3623` | explicitly the run number |
//! | `https://host/o/r/actions/runs/3623[/jobs/1]` | the run **number** in the URL, and optionally a job by its 0-based **position** |
//!
//! The URL row is where Forgejo and Gitea part ways, and both halves were measured on Forgejo
//! 16.0.5 rather than assumed: a run's `html_url` and the `target_url` of every commit status its
//! jobs post carry `index_in_repo`, not the id (run id 2, number 1 links as `runs/1`, and
//! `GET …/actions/runs/1` is then a 404), and `/jobs/<n>` is the job's position in the run (jobs
//! with ids 4 and 5 link as `jobs/0` and `jobs/1`). Gitea 1.27 puts ids in both places, so the
//! `gea` port of this module reads the same URL the other way round.
//!
//! The bare-number fallback is deliberately **one-way and loud**: the id is tried first, because
//! that is what every `fjo` output prints in its ID column and what the API means; the number is
//! consulted only on a 404, and when it is used a warning says so. Guessing silently would be
//! worse than the 404 it replaces — an id and a number that are both valid name two *different*
//! runs.
//!
//! # Jobs, by name
//!
//! `--job` used to take only a numeric job id, which nobody knows. A [`JobRef`] is an id when it
//! is all digits and a name otherwise. Names follow the rule `fjo milestone` and `fjo project`
//! already set (see `docs/porcelain-conventions.md`): an exact match wins, a unique substring is
//! accepted, and an **ambiguous** name is refused with the candidates listed — never guessed,
//! because a matrix build's `build (linux)` and `build (macos)` are both "build".

use std::str::FromStr;

use forgejo_client::{Api, query};
use forgejo_core::error::{Error, ErrorKind, Result};
use forgejo_core::types::RepoSlug;
use forgejo_model::{ActionRun, ActionRunJob};

use crate::cmd::support;

/// The accepted forms, as one clause. Shared by the parse error and the not-found hint, so the
/// two cannot come to describe different syntaxes.
pub(crate) const RUN_FORMS: &str =
    "expected a run id (`6442`), a run number (`#3623`), or a run URL";

/// A run, as the user named it. See the module docs.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum RunRef {
    /// `6442`: an id, falling back to the run number on a 404.
    Bare(i64),
    /// `#3623`: explicitly the per-repository run number (`index_in_repo`).
    Number(i64),
    /// A run's web URL. `run` is the run **number**, resolved like [`RunRef::Number`].
    Url {
        /// The run segment of the URL: the run's `index_in_repo` on Forgejo.
        run: i64,
        /// `…/jobs/<n>`: the job at that 0-based **position** in the run, not a job id.
        job: Option<usize>,
        /// The repository the URL names. **Not** used to switch repository — `-R` does that —
        /// only to warn when it differs from the one the command resolved.
        slug: RepoSlug,
    },
}

impl RunRef {
    /// The URL's job position, when the reference was a job URL.
    pub(crate) fn url_job(&self) -> Option<usize> {
        match self {
            RunRef::Url { job, .. } => *job,
            RunRef::Bare(_) | RunRef::Number(_) => None,
        }
    }
}

impl std::fmt::Display for RunRef {
    /// The reference as the user could type it back — what an error or a hint should quote. A URL
    /// prints as the number it carries, since that is what it means.
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            RunRef::Bare(n) => write!(f, "{n}"),
            RunRef::Number(n) | RunRef::Url { run: n, .. } => write!(f, "#{n}"),
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

/// A job position: a decimal integer, zero included — the first job is `jobs/0`.
fn position(s: &str) -> Option<usize> {
    if s.is_empty() || !s.bytes().all(|b| b.is_ascii_digit()) {
        return None;
    }
    s.parse::<usize>().ok()
}

/// What a run URL names.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct RunUrl {
    /// The repository in the URL's path.
    pub slug: RepoSlug,
    /// The run segment: the run's number (`index_in_repo`) on Forgejo.
    pub run: i64,
    /// The `jobs/<n>` segment, when there was one: the job's 0-based position on Forgejo.
    pub job: Option<usize>,
}

/// `https://host[/subpath]/owner/name/actions/runs/3623[/jobs/1|/attempts/3][/][?…][#…]`, or
/// the same without scheme and host (`/owner/name/actions/runs/3623/jobs/1`).
///
/// The path-only form is not a nicety: it is what Forgejo puts in the `target_url` of every
/// commit status its Actions post, so a parser that insisted on `://` recognised no real check
/// as an Actions run, and `fjo pr checks` never pointed at `fjo ci`.
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
        Some(&"jobs") => Some(position(segs.get(at + 4)?)?),
        _ => None,
    };
    Some(RunUrl { slug: RepoSlug::new(*owner, *name), run, job })
}

/// Turn a [`RunRef`] into the run it names, in `slug`.
///
/// * [`RunRef::Bare`]: `GET …/actions/runs/{n}`. On a 404 **only**, the run whose number is `n`
///   is used instead, with one warning on stderr saying so. If there is none, the original 404
///   is returned, after a warning naming both forms. Any other error — a 403, a 5xx, a network
///   failure — propagates unchanged: it says nothing about whether `n` was the wrong kind of
///   number, and retrying it as one would only hide it.
/// * [`RunRef::Number`] and [`RunRef::Url`]: the run with that number, or `ResourceNotFound`.
pub(crate) async fn resolve_run(api: &Api, slug: &RepoSlug, r: &RunRef) -> Result<ActionRun> {
    let id = match r {
        RunRef::Number(n) => {
            return run_by_number(api, slug, *n).await?.ok_or_else(|| number_not_found(slug, *n));
        }
        RunRef::Url { run, slug: named, .. } => {
            if named != slug {
                support::warn(&format!(
                    "note: the URL names {named}, but this command is working in {slug}; \
                     looking run #{run} up in {slug} (pass -R {named} to use the URL's)"
                ));
            }
            return run_by_number(api, slug, *run)
                .await?
                .ok_or_else(|| number_not_found(slug, *run));
        }
        RunRef::Bare(n) => *n,
    };

    let miss = match api.run().view(&slug.owner, &slug.name, id).await {
        Ok(run) => return Ok(run),
        Err(e) if is_miss(&e) => e,
        Err(e) => return Err(e),
    };
    // A failed lookup is not reported in place of the 404: the 404 is the answer to what was
    // asked, and the lookup was only ever a courtesy on top of it.
    if let Ok(Some(run)) = run_by_number(api, slug, id).await {
        support::warn(&format!("note: no run has id {id}; using run #{id} (id {})", run.id));
        return Ok(run);
    }
    support::warn(&format!(
        "note: {slug} has no run with id {id} and no run numbered #{id}. `fjo run list` \
         prints both: pass an ID as it is, or an INDEX as #N."
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

/// The run numbered `n`, or `Ok(None)` when there is none.
///
/// One request: Forgejo's run listing filters on `run_number` server-side, which Gitea's does
/// not (the `gea` port pages the whole list newest first to find one). The answer is still
/// checked against `index_in_repo`, so a server that ignored the filter — and so answered with
/// its newest run — reads as "not found" rather than as the wrong run.
async fn run_by_number(api: &Api, slug: &RepoSlug, n: i64) -> Result<Option<ActionRun>> {
    let q = query::ListActionRunsQuery { run_number: Some(n), ..Default::default() };
    let resp = api.run().list(&slug.owner, &slug.name, &q).await?;
    Ok(resp.workflow_runs.into_iter().find(|r| r.index_in_repo == n))
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
pub(crate) fn resolve_job<'a>(jobs: &'a [ActionRunJob], r: &JobRef) -> Result<&'a ActionRunJob> {
    pick_job(jobs, |j| j, describe_job, r)
}

/// `build (id 11)`: how a job is named in a not-found or ambiguity message.
pub(crate) fn describe_job(job: &ActionRunJob) -> String {
    format!("{} (id {})", job.name, job.id)
}

/// The job at `pos` in a run's job listing — what a pasted `…/jobs/<pos>` URL names.
///
/// The listing is in the run's own job order (measured: ids 4 and 5, linked as `jobs/0` and
/// `jobs/1`, come back in that order), so the position indexes it directly. Past the end is
/// `ResourceNotFound`, listing the jobs that are there.
pub(crate) fn job_at(jobs: &[ActionRunJob], pos: usize) -> Result<&ActionRunJob> {
    jobs.get(pos).ok_or_else(|| {
        job_not_found(&format!("at position {pos}"), jobs.iter().map(describe_job).collect())
    })
}

/// [`resolve_job`] over any list that carries jobs — one run's, or several runs' at once.
///
/// Generic so that `fjo ci logs --job`, which matches across every run of a commit, applies the
/// same rules and describes its candidates with their workflow (`ci.yml / build (id 11)`)
/// without a second copy of the matching:
///
/// * [`JobRef::Id`]: the job with that id.
/// * [`JobRef::Name`]: a case-insensitive **exact** name match wins; otherwise a unique
///   case-insensitive **substring** match. More than one match at either stage is a usage error
///   listing every candidate, because picking one would be a guess.
///
/// Not found is `ResourceNotFound` (exit 5), listing what *is* there so the next attempt does not
/// need a separate `fjo run jobs`.
pub(crate) fn pick_job<'a, T>(
    candidates: &'a [T],
    job_of: impl Fn(&T) -> &ActionRunJob,
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
    use crate::cmd::support::testing::{self, on};
    use forgejo_core::http::transport::Canned;
    use forgejo_core::types::ids::JobId;
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
            ("https://git.example.org/o/r/actions/runs/3623", url(3623, None)),
            ("https://git.example.org/o/r/actions/runs/3623/", url(3623, None)),
            ("https://git.example.org/o/r/actions/runs/3623/jobs/1", url(3623, Some(1))),
            ("https://git.example.org/o/r/actions/runs/3623/jobs/1/", url(3623, Some(1))),
            // The first job is position 0, which a positive-only parse would refuse.
            ("https://git.example.org/o/r/actions/runs/3623/jobs/0", url(3623, Some(0))),
            // The path-only form Forgejo puts in an Actions commit status's `target_url`.
            ("/o/r/actions/runs/6/jobs/1", url(6, Some(1))),
            ("https://git.example.org/o/r/actions/runs/3623/attempts/3", url(3623, None)),
            ("https://git.example.org/o/r/actions/runs/3623?tab=logs", url(3623, None)),
            ("https://git.example.org/o/r/actions/runs/3623/jobs/1#L12", url(3623, Some(1))),
            // A subpath install: the prefix is not part of the slug.
            ("https://example.org/forgejo/o/r/actions/runs/3623", url(3623, None)),
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
            "https://git.example.org/o/r/actions/runs/0",
            "https://git.example.org/o/r/pulls/4",
            "https://git.example.org/o/r/actions/runs/6442/jobs/x",
            "https://git.example.org/o/r/actions/runs/6442/jobs/-1",
            "o/r/actions/runs/6442",
        ] {
            let err = raw.parse::<RunRef>().expect_err(raw);
            assert!(err.contains(RUN_FORMS), "{raw}: {err}");
        }
    }

    /// A URL prints as the number it carries, because on Forgejo that is what it is.
    #[test]
    fn a_run_reference_prints_as_it_would_be_typed() {
        assert_eq!(RunRef::Bare(6442).to_string(), "6442");
        assert_eq!(RunRef::Number(3623).to_string(), "#3623");
        let u: RunRef = "https://h/o/r/actions/runs/7/jobs/1".parse().unwrap();
        assert_eq!((u.to_string(), u.url_job()), ("#7".to_owned(), Some(1)));
    }

    /// For `pr checks`: a check's `target_url` is a run only when it really is an Actions run.
    #[test]
    fn a_run_url_is_read_out_of_a_check_url_and_nothing_else_is() {
        let run = |raw: &str| parse_run_url(raw).map(|u| u.run);
        assert_eq!(run("https://h/o/r/actions/runs/6442/jobs/1"), Some(6442));
        // Forgejo posts Actions statuses with a path-only `target_url`. Bug this prevents:
        // `fjo pr checks` treating every real Actions check as external CI, and so never
        // printing its `next: fjo ci …` hint.
        assert_eq!(run("/o/r/actions/runs/6/jobs/0"), Some(6));
        assert_eq!(run("/forgejo/o/r/actions/runs/6"), Some(6));
        assert_eq!(run("https://ci.example.org/builds/6442"), None);
        assert_eq!(run(""), None);
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

    const RUN_6442: &str = r#"{"id":6442,"index_in_repo":3623,"workflow_id":"ci.yml"}"#;

    fn by_number(body: impl Into<String>) -> Canned {
        Canned::json(200, body.into())
    }

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

    /// The reported bug: `fjo run view 3623` with the INDEX column's value was a bare 404. And
    /// the number is looked up with the server's `run_number` filter, in one request.
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
            by_number(format!(r#"{{"total_count":1,"workflow_runs":[{RUN_6442}]}}"#)),
        ));
        let run =
            resolve_run(&testing::api(fake.clone()), &slug(), &RunRef::Bare(3623)).await.unwrap();
        assert_eq!((run.id.get(), run.index_in_repo), (6442, 3623));
        let listed = testing::calls(&fake, "GET", "/api/v1/repos/o/r/actions/runs");
        assert_eq!(listed.len(), 1, "{:?}", fake.calls());
        assert_eq!(listed[0].query_param("run_number"), Some("3623"));
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
            by_number(r#"{"total_count":0,"workflow_runs":[]}"#),
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

    /// A server that ignores `run_number` answers with its newest runs. Bug this prevents:
    /// `#5` silently resolving to whatever run happened to be first on that page.
    #[tokio::test]
    async fn a_run_number_the_server_did_not_filter_on_is_not_found_rather_than_wrong() {
        let fake = Arc::new(on(
            testing::transport(),
            "GET",
            "/api/v1/repos/o/r/actions/runs",
            by_number(format!(r#"{{"total_count":1,"workflow_runs":[{RUN_6442}]}}"#)),
        ));
        let err = resolve_run(&testing::api(fake), &slug(), &RunRef::Number(5)).await.unwrap_err();
        assert_eq!(err.exit_code(), 5, "{err}");
        assert!(err.to_string().contains("#5"), "{err}");
    }

    /// The Forgejo-specific half of the design: a pasted run URL carries the run's *number*, so
    /// it is looked up through the `run_number` filter and never as `GET …/actions/runs/{n}`,
    /// which would address a different run — or, measured, a 404.
    #[tokio::test]
    async fn a_run_url_is_looked_up_by_its_number_never_as_an_id() {
        let fake = Arc::new(on(
            testing::transport(),
            "GET",
            "/api/v1/repos/o/r/actions/runs",
            by_number(format!(r#"{{"total_count":1,"workflow_runs":[{RUN_6442}]}}"#)),
        ));
        let r: RunRef =
            "https://git.example.org/other/repo/actions/runs/3623/jobs/1".parse().unwrap();
        let run = resolve_run(&testing::api(fake.clone()), &slug(), &r).await.unwrap();
        assert_eq!(run.id.get(), 6442);
        assert_eq!(fake.call_count(), 1, "{:?}", fake.calls());
        assert!(testing::calls(&fake, "GET", "/api/v1/repos/o/r/actions/runs/3623").is_empty());
    }

    // ------------------------------------------------------------------------- resolve_job

    fn job(id: i64, name: &str) -> ActionRunJob {
        ActionRunJob { id: JobId::new(id), name: name.into(), ..Default::default() }
    }

    fn jobs() -> Vec<ActionRunJob> {
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

    /// The candidates' description is the caller's, which is what lets `fjo ci logs` label them
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

    /// Bug this prevents: a pasted job URL acting on the wrong job. Forgejo links a job as
    /// `…/jobs/<position>` (measured: a run whose jobs are ids 4 and 5 links them as `jobs/0` and
    /// `jobs/1`), so treating the segment as an id — which is right on Gitea — picks a different
    /// job or none at all here.
    #[test]
    fn a_url_job_segment_is_a_position_not_an_id() {
        let u: RunRef = "https://h/o/r/actions/runs/7/jobs/2".parse().unwrap();
        let jobs = jobs();
        assert_eq!(job_at(&jobs, u.url_job().unwrap()).unwrap().name, "build (macos)");
        assert_eq!(job_at(&jobs, 0).unwrap().id.get(), 11);
        let err = job_at(&jobs, 4).unwrap_err();
        assert_eq!(err.exit_code(), 5, "{err}");
        assert!(err.to_string().contains("at position 4"), "{err}");
    }
}
