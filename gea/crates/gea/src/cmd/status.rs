//! `gea status` — the cross-repository dashboard `tea` conspicuously lacks.
//!
//! Every other list command in `gea` is scoped to one repository, which means the question people
//! actually start their day with — *what is waiting for me anywhere?* — needs one command per
//! repository and a lot of scrolling. `/repos/issues/search` answers it in a single call because it
//! searches every repository the token can see, and the notification endpoint supplies what
//! happened since you last looked.
//!
//! This earns its place as a porcelain command on two counts from
//! `docs/porcelain-conventions.md`: it is four API calls behind one word, and its rendering — four
//! labelled sections of aligned rows — is materially better than four JSON arrays.
//!
//! # Why the responses stay as `serde_json::Value`
//!
//! Not laziness, and not a preference for untyped code. Forgejo 16.0.4 (measured for fjo, which gea was ported from) sends
//! `"assignees": null` for an unassigned issue, while the generated
//! `gitea_model::Issue::assignees` is a `Vec<User>` carrying only `#[serde(default)]` — and
//! `serde`'s `default` fills in a *missing* field, not an explicit `null`. So
//! `Issue::search_issues_page` fails to decode against a real instance the moment any result is
//! unassigned:
//!
//! ```text
//! error: the response did not look the way this build expects
//!   at:       /0/assignees
//!   expected: invalid type: null, expected a sequence
//! ```
//!
//! Found by running this command against Forgejo 16.0.4 (measured for fjo, which gea was ported from). The proper fix belongs in the model
//! emitter — a nullable array needs `deserialize_with` mapping `null` to empty — and it is *not*
//! local to this command: it breaks every typed decode of an `Issue`. Until it lands, this command
//! walks the JSON itself, which also means `--json` hands back the API's own objects untouched,
//! exactly as `docs/output.md` promises.

use std::io::Write;

use clap::Args as ClapArgs;
use gitea_core::Result;
use gitea_core::http::{Client, Request};
use serde_json::{Value, json};

use crate::api::paginate;
use crate::cmd::support;
use crate::cmd::support::machine::Triad;
use crate::global::GlobalOpts;
use crate::output::template::funcs::timeago;
use crate::output::{Table, Term, project::FieldKind, project::FieldSpec};

/// `--json` selects whole sections, because that is what the top-level keys are.
///
/// `--json review_requests --jq '.review_requests[].html_url'` is the intended shape; a section is
/// an array of the API's own issue and notification objects, unmodified, so every field name inside
/// one is Gitea's (see `docs/output.md`).
const FIELDS: &[FieldSpec] = &[
    FieldSpec {
        name: "assigned_issues",
        kind: FieldKind::Array(&FieldKind::Json),
        doc: "open issues assigned to you, anywhere",
    },
    FieldSpec {
        name: "assigned_pull_requests",
        kind: FieldKind::Array(&FieldKind::Json),
        doc: "open pull requests assigned to you",
    },
    FieldSpec {
        name: "review_requests",
        kind: FieldKind::Array(&FieldKind::Json),
        doc: "pull requests waiting for your review",
    },
    FieldSpec {
        name: "activity",
        kind: FieldKind::Array(&FieldKind::Json),
        doc: "unread notification threads",
    },
];

/// `-L/--limit`'s default, per `docs/porcelain-conventions.md`.
const DEFAULT_LIMIT: usize = 30;

#[derive(Debug, ClapArgs)]
#[command(after_long_help = LONG_HELP)]
pub struct Args {
    /// Leave a repository out; repeatable
    #[arg(short = 'e', long = "exclude", value_name = "OWNER/NAME")]
    pub exclude: Vec<String>,

    /// Only look at repositories owned by this user or organization
    #[arg(short = 'o', long = "org", value_name = "OWNER")]
    pub org: Option<String>,
}

const LONG_HELP: &str = "\
Show assigned issues, assigned pull requests, review requests, and unread notifications.

--limit applies separately to each section (default 30). No checkout is required.

  gea status
  gea status --org my-team
  gea status -e noisy/repo -e other/repo
  gea status --json review_requests --jq '.review_requests[].html_url'";

/// One dashboard, assembled. Sections hold the API's own objects — see the module comment.
struct Dashboard {
    assigned_issues: Vec<Value>,
    assigned_pulls: Vec<Value>,
    review_requests: Vec<Value>,
    activity: Vec<Value>,
}

impl Dashboard {
    fn empty() -> Self {
        Self {
            assigned_issues: Vec::new(),
            assigned_pulls: Vec::new(),
            review_requests: Vec::new(),
            activity: Vec::new(),
        }
    }

    fn to_json(&self) -> Value {
        json!({
            "assigned_issues": self.assigned_issues,
            "assigned_pull_requests": self.assigned_pulls,
            "review_requests": self.review_requests,
            "activity": self.activity,
        })
    }

    fn is_empty(&self) -> bool {
        self.assigned_issues.is_empty()
            && self.assigned_pulls.is_empty()
            && self.review_requests.is_empty()
            && self.activity.is_empty()
    }

    /// Drop everything from an excluded repository, in every section.
    fn exclude(&mut self, excluded: &[String]) {
        for section in [
            &mut self.assigned_issues,
            &mut self.assigned_pulls,
            &mut self.review_requests,
            &mut self.activity,
        ] {
            section.retain(|item| !is_excluded(item, excluded));
        }
    }
}

pub fn run(globals: &GlobalOpts, args: &Args) -> Result<()> {
    // Field discovery answers before any request, and so does a malformed `--exclude`: a typo must
    // not be reported as a configuration or authentication problem.
    let Some(machine) = Triad::for_local_table(globals, FIELDS)? else { return Ok(()) };
    let excluded = parse_excludes(&args.exclude)?;
    let limit = globals.limit.unwrap_or(DEFAULT_LIMIT);

    let mut board = Dashboard::empty();
    let mut term = Term::detect();

    // `board` and `term` are filled by reference: `runtime::block_on` is typed
    // `Future<Output = Result<()>>` and borrows nothing, so an out-parameter is the whole
    // adaptation needed.
    crate::runtime::block_on(async {
        let rt = crate::runtime::Runtime::new(globals)?;
        term = *rt.term();
        board = fetch(rt.client(), args.org.as_deref(), limit).await?;
        Ok(())
    })?;

    board.exclude(&excluded);

    let mut out = support::writer(globals)?;
    if machine.is_explicit() {
        machine.pipeline().render(board.to_json(), &term, &mut out)?;
        out.flush()?;
        return Ok(());
    }

    // Emptiness is success. A clear inbox is the good outcome, and exiting non-zero for it would
    // break `gea status && echo all clear`.
    if board.is_empty() {
        support::note(
            &term,
            "no assigned issues, pull requests, review requests, or unread notifications",
        );
    }
    write_human(&board, &term, &mut *out)?;
    out.flush()?;
    Ok(())
}

/// All four sections, concurrently.
///
/// `try_join` rather than four sequential `await`s: they are independent reads, and on a self-hosted
/// instance across a WAN the difference is four round trips against one. The runtime is
/// single-threaded, which is irrelevant here — this is I/O concurrency, not parallelism.
///
/// Each section goes through [`paginate::walk`] rather than one plain request, because Gitea
/// silently clamps `limit` to `max_response_items` (50 by default): asking for `--limit 200` and
/// stopping at the first short page is the data-loss bug `gitea_core::http::paginate` documents at
/// length.
/// Takes a bare [`Client`] rather than the [`crate::runtime::Runtime`] it comes from, so a
/// `FakeTransport` test can assert the four requests this builds without a network or a config file.
async fn fetch(client: &Client, org: Option<&str>, limit: usize) -> Result<Dashboard> {
    use gitea_client::query::{IssueSearchIssuesQuery, NotifyGetListQuery};

    let search = |q: IssueSearchIssuesQuery| {
        let mut q = q.with_state("open");
        if let Some(owner) = org {
            q = q.with_owner(owner);
        }
        // `apply` is the generated query struct's own serializer, so the parameter names and their
        // encoding are the API's rather than ours.
        q.apply(Request::get("/repos/issues/search"))
    };
    let assigned = IssueSearchIssuesQuery::default().with_assigned(true);
    let requested = IssueSearchIssuesQuery::default().with_review_requested(true);

    let assigned_issues = search(assigned.clone().with_type("issues"));
    let assigned_pulls = search(assigned.with_type("pulls"));
    let review_requests = search(requested.with_type("pulls"));
    let activity = NotifyGetListQuery::default().apply(Request::get("/notifications"));

    let (a, b, c, d) = futures::try_join!(
        paginate::walk(client, &assigned_issues, Some(limit)),
        paginate::walk(client, &assigned_pulls, Some(limit)),
        paginate::walk(client, &review_requests, Some(limit)),
        paginate::walk(client, &activity, Some(limit)),
    )?;

    let items = |pages: paginate::Pages| match paginate::flatten(pages.pages) {
        Value::Array(items) => items,
        // Both endpoints answer with arrays; anything else is a shape worth showing rather than
        // silently dropping.
        other => vec![other],
    };
    Ok(Dashboard {
        assigned_issues: items(a),
        assigned_pulls: items(b),
        review_requests: items(c),
        activity: items(d),
    })
}

/// `-e owner/name`, validated.
///
/// Checked up front rather than compared loosely later: `-e cli` silently matching nothing is the
/// kind of flag that looks like it worked, and the user then wonders why the repository is still
/// listed.
fn parse_excludes(raw: &[String]) -> Result<Vec<String>> {
    let mut out = Vec::with_capacity(raw.len());
    for spec in raw {
        let spec = spec.trim();
        let ok = spec.split('/').count() == 2 && spec.split('/').all(|p| !p.is_empty());
        if !ok {
            return Err(support::usage(format!(
                "--exclude wants owner/name, and {spec:?} is not that"
            )));
        }
        out.push(spec.to_owned());
    }
    Ok(out)
}

/// Whether an item belongs to an excluded repository.
///
/// Compared against `repository.full_name` exactly. A prefix match would make `-e noisy/repo` also
/// hide `noisy/repo-two`, which is a different repository.
fn is_excluded(item: &Value, excluded: &[String]) -> bool {
    match item["repository"]["full_name"].as_str() {
        Some(name) => excluded.iter().any(|e| e == name),
        None => false,
    }
}

fn write_human(board: &Dashboard, term: &Term, out: &mut dyn Write) -> std::io::Result<()> {
    let mut first = true;
    let mut section = |title: &str, rows: Vec<[String; 3]>| -> std::io::Result<()> {
        // A section with nothing in it is still printed, with a dash. Omitting it would make the
        // layout jump around between runs, and "no review requests" is information.
        if !first {
            writeln!(out)?;
        }
        first = false;
        writeln!(out, "{title}")?;
        if rows.is_empty() {
            writeln!(out, "  -")?;
            return Ok(());
        }
        let mut table = Table::new(term);
        for row in rows {
            table.row(row);
        }
        // Indented by two, matching `gh status`, so the section headings stand out on a terminal.
        for line in table.render_to_string().lines() {
            writeln!(out, "  {line}")?;
        }
        Ok(())
    };

    section("Assigned Issues", board.assigned_issues.iter().map(issue_row).collect())?;
    section("Assigned Pull Requests", board.assigned_pulls.iter().map(issue_row).collect())?;
    section("Review Requests", board.review_requests.iter().map(issue_row).collect())?;
    section("Recent Activity", board.activity.iter().map(activity_row).collect())?;
    Ok(())
}

/// `owner/repo#42`, the title, and how long ago it moved.
///
/// The repository is part of the *first* column rather than a column of its own because the whole
/// point of this command is that rows come from different repositories, and a reader scanning it
/// needs `owner/repo#42` as one token they can paste into `gea issue view`.
fn issue_row(item: &Value) -> [String; 3] {
    let repo = item["repository"]["full_name"].as_str().unwrap_or_default();
    let number = item["number"].as_i64().map(|n| n.to_string()).unwrap_or_default();
    [
        format!("{repo}#{number}"),
        item["title"].as_str().unwrap_or_default().to_owned(),
        timeago(item["updated_at"].as_str().unwrap_or_default()),
    ]
}

/// A notification thread. The subject's *type* is shown next to the repository because `issue` and
/// `pull` are what tell a reader which command to reach for next.
fn activity_row(item: &Value) -> [String; 3] {
    let repo = item["repository"]["full_name"].as_str().unwrap_or_default();
    let kind = item["subject"]["type"].as_str().unwrap_or_default();
    let title = item["subject"]["title"].as_str().unwrap_or_default();
    [
        match (repo.is_empty(), kind.is_empty()) {
            (true, _) => kind.to_owned(),
            (false, true) => repo.to_owned(),
            (false, false) => format!("{repo} ({kind})"),
        },
        title.to_owned(),
        timeago(item["updated_at"].as_str().unwrap_or_default()),
    ]
}

#[cfg(test)]
mod tests {
    use super::*;

    fn issue(full_name: &str, number: i64, title: &str) -> Value {
        json!({
            "number": number,
            "title": title,
            // The `null` that broke the typed decode, kept here on purpose so this test is also a
            // regression net for the workaround the module comment describes.
            "assignees": Value::Null,
            "repository": { "full_name": full_name, "name": "r", "owner": "o", "id": 1 },
        })
    }

    /// Bug this prevents: `-e cli` (an owner with no repository name) matching nothing while
    /// looking like it worked, so the user keeps seeing rows they thought they had excluded.
    #[test]
    fn exclude_insists_on_owner_slash_name() {
        assert_eq!(parse_excludes(&["a/b".to_owned()]).unwrap(), ["a/b"]);
        for bad in ["a", "a/", "/b", "a/b/c", ""] {
            assert!(parse_excludes(&[bad.to_owned()]).is_err(), "{bad:?} should be refused");
        }
    }

    #[test]
    fn exclusion_matches_the_full_name_and_nothing_else() {
        let excluded = vec!["noisy/repo".to_owned()];
        assert!(is_excluded(&issue("noisy/repo", 1, "x"), &excluded));
        // A prefix must not match: `noisy/repo-two` is a different repository.
        assert!(!is_excluded(&issue("noisy/repo-two", 1, "x"), &excluded));
        assert!(!is_excluded(&issue("other/repo", 1, "x"), &excluded));
        // An item with no repository at all is never excluded, rather than always excluded.
        assert!(!is_excluded(&json!({ "number": 1 }), &excluded));
    }

    /// Exclusion has to reach *every* section. Filtering only the issue list would leave the
    /// excluded repository's pull requests and notifications on screen, which reads as the flag
    /// half-working.
    #[test]
    fn exclusion_covers_every_section() {
        let mut board = Dashboard {
            assigned_issues: vec![issue("noisy/repo", 1, "a")],
            assigned_pulls: vec![issue("noisy/repo", 2, "b")],
            review_requests: vec![issue("noisy/repo", 3, "c")],
            activity: vec![json!({
                "repository": { "full_name": "noisy/repo" },
                "subject": { "type": "issue", "title": "d" },
            })],
        };
        board.exclude(&["noisy/repo".to_owned()]);
        assert!(board.is_empty(), "{}", board.to_json());
    }

    /// Bug this prevents: dropping the repository from a row. Every other list command in gea is
    /// single-repository, so a bare `#42` here is genuinely ambiguous — this is the one view where
    /// rows come from different repositories.
    #[test]
    fn a_row_names_its_repository() {
        let row = issue_row(&issue("them/proj", 42, "fix it"));
        assert_eq!(row[0], "them/proj#42");
        assert_eq!(row[1], "fix it");
    }

    /// Bug this prevents: a notification row that says only "issue", with no way to tell which
    /// repository it came from.
    #[test]
    fn an_activity_row_names_the_repository_and_the_subject_kind() {
        let row = activity_row(&json!({
            "repository": { "full_name": "them/proj" },
            "subject": { "type": "pull", "title": "needs your review" },
        }));
        assert_eq!(row[0], "them/proj (pull)");
        assert_eq!(row[1], "needs your review");
        // A thread with no repository still renders rather than producing an empty first column.
        assert_eq!(activity_row(&json!({ "subject": { "type": "issue" } }))[0], "issue");
    }

    /// Bug this prevents: `--json` and the section list drifting apart, so `--json activity`
    /// rejects a key the payload has.
    #[test]
    fn the_json_object_emits_exactly_the_declared_sections() {
        let mut board = Dashboard::empty();
        board.assigned_issues.push(issue("a/b", 1, "t"));
        let v = board.to_json();
        let keys: Vec<&str> = v.as_object().unwrap().keys().map(String::as_str).collect();
        assert_eq!(keys, FIELDS.iter().map(|f| f.name).collect::<Vec<_>>());
        // Section contents are the API's own objects, so their field names are Gitea's.
        assert!(v["assigned_issues"][0]["number"].is_number(), "{v}");
    }

    /// An empty section is still printed. Otherwise the layout moves between runs and the reader
    /// cannot tell "no review requests" from "the review-request call failed".
    #[test]
    fn empty_sections_are_shown_as_a_dash() {
        let mut buf = Vec::new();
        write_human(&Dashboard::empty(), &Term::tty(100), &mut buf).unwrap();
        let text = String::from_utf8(buf).unwrap();
        for heading in
            ["Assigned Issues", "Assigned Pull Requests", "Review Requests", "Recent Activity"]
        {
            assert!(text.contains(heading), "{heading} missing from:\n{text}");
        }
        assert_eq!(text.matches("  -").count(), 4, "{text}");
    }
}
