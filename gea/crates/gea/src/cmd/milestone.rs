//! `gea milestone` — milestones.
//!
//! What this adds over `gea raw issue edit-milestone`: **every subcommand takes a title, not an
//! id.** Gitea's milestone routes are all `/milestones/{id}`, and nobody knows that a
//! milestone called `1.0` is id 7. `list --state all` is searched, so closing a milestone does
//! not make it unreachable, and an *ambiguous* title is refused rather than guessed —
//! Gitea happily allows two milestones called `1.0`, which real testing turned up. See
//! `crate::cmd::issue::shared::milestone_by_title`.
//!
//! `issues` is the other reason this group exists: it is `issue list -m <title>` with the
//! milestone's own progress counters in the banner, which is the question people actually ask
//! ("what is left in 1.0?").

use clap::{Args as ClapArgs, Subcommand};
use futures::StreamExt;
use gitea_core::error::Result;
use gitea_core::types::Timestamp;
use gitea_model::Milestone;

use crate::cmd::issue::shared::{self, Cx, MilestonePatch, Out, State, label_chip, timeago};
use crate::cmd::support;
use crate::global::GlobalOpts;
use crate::output::Table;
use crate::runtime::Runtime;

#[derive(Debug, ClapArgs)]
#[command(long_about = LONG_ABOUT)]
pub struct Args {
    #[command(subcommand)]
    pub cmd: Cmd,
}

const LONG_ABOUT: &str = "\
Manage milestones and list their issues.

Select milestones by title, not ID. Both open and closed milestones are searched.

  gea milestone list -s all
  gea milestone create 1.0 -d 2026-12-31 --description 'first stable release'
  gea milestone issues 1.0
  gea milestone close 1.0";

#[derive(Debug, Subcommand)]
pub enum Cmd {
    /// List milestones
    List(ListArgs),
    /// Create a milestone
    Create(CreateArgs),
    /// Change a milestone's title, description or deadline
    Edit(EditArgs),
    /// Close a milestone
    Close(TitleArgs),
    /// Reopen a closed milestone
    Reopen(TitleArgs),
    /// Delete a milestone
    Delete(DeleteArgs),
    /// List the issues in a milestone
    Issues(IssuesArgs),
}

/// A milestone, by title.
#[derive(Debug, ClapArgs)]
pub struct TitleArgs {
    /// Milestone title
    #[arg(value_name = "TITLE")]
    pub title: String,
}

#[derive(Debug, ClapArgs)]
pub struct ListArgs {
    /// Which milestones to show
    #[arg(short = 's', long, value_enum, default_value_t = State::Open)]
    pub state: State,

    /// Maximum number of milestones (also settable as --limit)
    #[arg(short = 'L', value_name = "N")]
    pub limit: Option<usize>,
}

#[derive(Debug, ClapArgs)]
pub struct CreateArgs {
    /// Milestone title
    #[arg(value_name = "TITLE")]
    pub title: String,

    /// Due date: YYYY-MM-DD, or a full RFC 3339 timestamp
    #[arg(short = 'd', long, value_name = "DATE")]
    pub deadline: Option<String>,

    /// What the milestone is for
    #[arg(long, value_name = "TEXT")]
    pub description: Option<String>,
}

#[derive(Debug, ClapArgs)]
pub struct EditArgs {
    #[command(flatten)]
    pub target: TitleArgs,

    /// New title
    #[arg(long = "title", value_name = "TITLE")]
    pub new_title: Option<String>,

    /// New due date: YYYY-MM-DD, or a full RFC 3339 timestamp
    #[arg(short = 'd', long, value_name = "DATE")]
    pub deadline: Option<String>,

    /// New description
    #[arg(long, value_name = "TEXT")]
    pub description: Option<String>,

    /// Set the state directly, instead of using close/reopen
    #[arg(short = 's', long, value_enum)]
    pub state: Option<State>,
}

#[derive(Debug, ClapArgs)]
pub struct DeleteArgs {
    #[command(flatten)]
    pub target: TitleArgs,

    /// Skip the confirmation
    #[arg(long)]
    pub yes: bool,
}

#[derive(Debug, ClapArgs)]
pub struct IssuesArgs {
    #[command(flatten)]
    pub target: TitleArgs,

    /// Which issues to show
    #[arg(short = 's', long, value_enum, default_value_t = State::Open)]
    pub state: State,

    /// Maximum number of issues (also settable as --limit)
    #[arg(short = 'L', value_name = "N")]
    pub limit: Option<usize>,
}

pub fn run(globals: &GlobalOpts, args: &Args) -> Result<()> {
    // `issues` answers with issues; everything else with milestones. The table has to be right
    // before the first request, because bare `--json` is answered without one.
    let table = match &args.cmd {
        Cmd::Issues(_) => gitea_client::fields::FIELDS_ISSUE,
        _ => gitea_client::fields::FIELDS_MILESTONE,
    };
    let Some(out) = Out::prepare(globals, table)? else { return Ok(()) };

    crate::runtime::block_on(async move {
        let rt = Runtime::new(globals)?;
        let cx = Cx::in_repo(&rt, globals, out)?;
        match &args.cmd {
            Cmd::List(a) => list(&cx, globals, a).await,
            Cmd::Create(a) => create(&cx, a).await,
            Cmd::Edit(a) => edit(&cx, a).await,
            Cmd::Close(a) => set_state(&cx, a, "closed").await,
            Cmd::Reopen(a) => set_state(&cx, a, "open").await,
            Cmd::Delete(a) => delete(&cx, a).await,
            Cmd::Issues(a) => issues(&cx, globals, a).await,
        }
    })
}

// ---------------------------------------------------------------------------------- list

async fn list(cx: &Cx, globals: &GlobalOpts, a: &ListArgs) -> Result<()> {
    let query =
        gitea_client::query::IssueGetMilestonesListQuery::default().with_state(a.state.as_str());
    let cap = support::limit(a.limit, globals);
    let mut stream = cx.api.issue().get_milestones_list(cx.owner()?, cx.name()?, &query).take(cap);

    let mut milestones: Vec<Milestone> = Vec::new();
    while let Some(item) = stream.next().await {
        milestones.push(item?);
    }

    if cx.out.is_machine() {
        return cx.out.machine(support::to_value(&milestones)?, &cx.term);
    }
    if milestones.is_empty() {
        support::note(&cx.term, &format!("no {} milestones in {}", a.state.as_str(), cx.repo()?));
        return cx.out.table(&Table::new(&cx.term));
    }

    // What the capped stream could not say — see `support::total_if_truncated`.
    let total = {
        let (owner, name) = (cx.owner()?, cx.name()?);
        let ops = cx.api.issue();
        support::total_if_truncated(milestones.len(), cap, |p| {
            ops.get_milestones_list_page(owner, name, &query, p)
        })
        .await
    };

    let mut table = Table::new(&cx.term);
    table.headers(["TITLE", "STATE", "OPEN", "CLOSED", "DUE", "UPDATED"]);
    for m in &milestones {
        table.row([
            m.title.clone(),
            m.state.to_string(),
            m.open_issues.to_string(),
            m.closed_issues.to_string(),
            shared::date(m.due_on),
            timeago(m.updated_at),
        ]);
    }
    let n = total.unwrap_or(milestones.len() as u64);
    table.banner(support::banner(
        milestones.len(),
        total,
        &format!("{} milestone{} in {}", a.state.as_str(), support::plural_s(n), cx.repo()?),
    ));
    cx.out.table(&table)
}

// -------------------------------------------------------------------------------- create

async fn create(cx: &Cx, a: &CreateArgs) -> Result<()> {
    let mut body = gitea_model::CreateMilestoneOption {
        title: Some(a.title.clone()),
        description: a.description.clone(),
        ..gitea_model::CreateMilestoneOption::default()
    };
    if let Some(text) = &a.deadline {
        body.due_on = Some(parse_deadline(text)?);
    }
    let milestone = cx.api.issue().create_milestone(cx.owner()?, cx.name()?, &body).await?;
    emit(cx, &milestone, "Created")
}

// ---------------------------------------------------------------------- edit, close, reopen

async fn edit(cx: &Cx, a: &EditArgs) -> Result<()> {
    let mut patch = MilestonePatch {
        title: a.new_title.clone(),
        description: a.description.clone(),
        state: a.state.map(|s| s.as_str().to_owned()),
        ..MilestonePatch::default()
    };
    if let Some(text) = &a.deadline {
        patch.due_on = Some(parse_deadline(text)?);
    }
    if patch.is_empty() {
        return Err(support::usage(
            "nothing to change; pass --title, -d/--deadline, --description or -s/--state",
        ));
    }
    if a.state == Some(State::All) {
        return Err(support::usage("-s/--state on an edit takes open or closed, not all"));
    }

    let existing = shared::milestone_by_title(cx, &a.target.title).await?;
    let milestone = shared::patch_milestone(cx, existing.id, &patch).await?;
    emit(cx, &milestone, "Updated")
}

async fn set_state(cx: &Cx, a: &TitleArgs, state: &str) -> Result<()> {
    let existing = shared::milestone_by_title(cx, &a.title).await?;
    // A sparse patch: `EditMilestoneOption` would also send `description:""`, erasing it, and
    // `title:""`, which Gitea rejects outright.
    let patch = MilestonePatch { state: Some(state.to_owned()), ..MilestonePatch::default() };
    let milestone = shared::patch_milestone(cx, existing.id, &patch).await?;
    emit(cx, &milestone, if state == "closed" { "Closed" } else { "Reopened" })
}

async fn delete(cx: &Cx, a: &DeleteArgs) -> Result<()> {
    let existing = shared::milestone_by_title(cx, &a.target.title).await?;
    cx.confirm(
        &format!(
            "Delete milestone {:?} from {} ({} open, {} closed issue(s) lose it)",
            existing.title,
            cx.repo()?,
            existing.open_issues,
            existing.closed_issues
        ),
        a.yes,
    )?;
    cx.api.issue().delete_milestone(cx.owner()?, cx.name()?, &existing.id.to_string()).await?;
    if cx.out.is_machine() {
        return cx.out.machine(support::to_value(&existing)?, &cx.term);
    }
    cx.out.text(&format!("Deleted milestone {:?}\n", existing.title))
}

// -------------------------------------------------------------------------------- issues

async fn issues(cx: &Cx, globals: &GlobalOpts, a: &IssuesArgs) -> Result<()> {
    // Resolved first, so a mistyped title is "no milestone titled …, it has: …" rather than an
    // empty list that looks like a finished milestone.
    let milestone = shared::milestone_by_title(cx, &a.target.title).await?;

    // `type=issues` for the same reason as `gea issue list`: this endpoint answers with pull
    // requests too. `milestones` takes the title; the id would also work, but the title is what
    // the `Link` header then echoes, which keeps `--paginate` legible in `--debug`.
    let query = gitea_client::query::IssueListIssuesQuery::default()
        .with_state(a.state.as_str())
        .with_type("issues")
        .with_milestones(&milestone.title);
    let cap = support::limit(a.limit, globals);
    let mut stream = cx.api.issue().list_issues(cx.owner()?, cx.name()?, &query).take(cap);

    let mut found = Vec::new();
    while let Some(item) = stream.next().await {
        found.push(item?);
    }

    if cx.out.is_machine() {
        return cx.out.machine(support::to_value(&found)?, &cx.term);
    }
    if found.is_empty() {
        support::note(
            &cx.term,
            &format!("no {} issues in milestone {:?}", a.state.as_str(), milestone.title),
        );
        return cx.out.table(&Table::new(&cx.term));
    }

    let mut table = Table::new(&cx.term);
    table.headers(["NUMBER", "STATE", "TITLE", "LABELS", "UPDATED"]);
    for issue in &found {
        table.row([
            format!("#{}", issue.number),
            issue.state.to_string(),
            issue.title.clone(),
            issue.labels.iter().map(|l| label_chip(&cx.term, l)).collect::<Vec<_>>().join(", "),
            timeago(issue.updated_at),
        ]);
    }
    // The milestone's own counters, which is the number people are after: "2 of 5 done".
    table.banner(format!(
        "Milestone {:?}: {} open, {} closed, due {}",
        milestone.title,
        milestone.open_issues,
        milestone.closed_issues,
        shared::date(milestone.due_on)
    ));
    cx.out.table(&table)
}

// ------------------------------------------------------------------------------- helpers

/// `2026-12-31`, or a full RFC 3339 timestamp.
///
/// A bare date is what people type, and it is not a valid RFC 3339 timestamp — so without this
/// the server answers `parsing time "2026-12-31" as "2006-01-02T15:04:05Z07:00": cannot parse`,
/// which tells a user nothing about what to write instead. Midnight UTC is the interpretation
/// Gitea's own web UI uses for a date picker.
fn parse_deadline(text: &str) -> Result<Timestamp> {
    let trimmed = text.trim();
    if let Ok(ts) = trimmed.parse::<jiff::Timestamp>() {
        return Ok(Timestamp::from_jiff(ts));
    }
    if let Ok(date) = trimmed.parse::<jiff::civil::Date>() {
        let zoned = date
            .to_zoned(jiff::tz::TimeZone::UTC)
            .map_err(|e| support::usage(format!("{trimmed:?} is not a date gea can use: {e}")))?;
        return Ok(Timestamp::from_jiff(zoned.timestamp()));
    }
    Err(support::usage(format!(
        "{trimmed:?} is not a date; write it as YYYY-MM-DD (e.g. 2026-12-31) or as a full RFC \
         3339 timestamp"
    )))
}

fn emit(cx: &Cx, milestone: &Milestone, verb: &str) -> Result<()> {
    if cx.out.is_machine() {
        return cx.out.machine(support::to_value(milestone)?, &cx.term);
    }
    cx.out.text(&format!(
        "{verb} milestone {:?} ({}, due {})\n",
        milestone.title,
        milestone.state,
        shared::date(milestone.due_on)
    ))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::output::Term;
    use gitea_core::http::transport::Canned;
    use gitea_core::http::{Auth, Client, FakeTransport, RetryPolicy};
    use gitea_core::types::RepoSlug;
    use std::sync::{Arc, Mutex};

    macro_rules! method {
        ($name:literal) => {
            $name.parse().expect("a valid HTTP method")
        };
    }

    fn cx(
        fake: Arc<FakeTransport>,
        buf: &Arc<Mutex<Vec<u8>>>,
        globals: &GlobalOpts,
        term: Term,
        table: &'static [gitea_client::meta_types::FieldSpec],
    ) -> Cx {
        let client = Client::builder("https://git.example.org", Auth::token("t"))
            .transport(fake)
            .retry(RetryPolicy { max: 1, ..RetryPolicy::default() })
            .build()
            .expect("a well-formed base URL");
        let out = Out::to_buffer(globals, table, buf);
        Cx::for_test(
            gitea_client::Api::new(client),
            Some(RepoSlug::new("perf3ct", "gea")),
            term,
            out,
        )
    }

    fn text(buf: &Arc<Mutex<Vec<u8>>>) -> String {
        String::from_utf8(buf.lock().unwrap().clone()).expect("utf-8 output")
    }

    const MILESTONES: &str = r#"[
        {"id": 7, "title": "1.0", "state": "open", "open_issues": 2, "closed_issues": 3,
         "due_on": "2026-12-31T00:00:00Z", "description": "first stable release"},
        {"id": 8, "title": "2.0", "state": "open", "open_issues": 0, "closed_issues": 0}
    ]"#;

    fn with_milestones() -> FakeTransport {
        FakeTransport::new()
            .on(
                method!("GET"),
                "/api/v1/settings/api",
                Canned::json(200, r#"{"max_response_items":50}"#),
            )
            .on_sequence(
                method!("GET"),
                "/api/v1/repos/perf3ct/gea/milestones",
                Vec::from([Canned::json(200, MILESTONES), Canned::json(200, "[]")]),
            )
    }

    /// The whole reason this group exists: **a title on the command line becomes an id in the
    /// path.** Bug this prevents: sending the title as `{id}` (a 404, or worse a match against a
    /// numeric title), or requiring users to know that `1.0` is milestone 7.
    #[tokio::test]
    async fn a_title_is_resolved_to_the_id_the_route_wants() {
        let fake = Arc::new(with_milestones().on(
            method!("PATCH"),
            "/api/v1/repos/perf3ct/gea/milestones/7",
            Canned::json(200, r#"{"id":7,"title":"1.0","state":"closed"}"#),
        ));
        let buf = Arc::new(Mutex::new(Vec::new()));
        let globals = GlobalOpts::default();
        let cx =
            cx(fake.clone(), &buf, &globals, Term::piped(), gitea_client::fields::FIELDS_MILESTONE);
        set_state(&cx, &TitleArgs { title: "1.0".to_owned() }, "closed").await.unwrap();

        let patch = fake.calls_to(&method!("PATCH"), "/api/v1/repos/perf3ct/gea/milestones/7");
        assert_eq!(patch.len(), 1, "the id came from the lookup, not from the title");
        // Only the state: `EditMilestoneOption` would also send `description:""`, erasing it.
        assert_eq!(patch[0].body_str(), r#"{"state":"closed"}"#);
        // And the lookup asked for every state, so closing a closed milestone still finds it.
        let lookup = fake
            .calls()
            .into_iter()
            .find(|c| c.path == "/api/v1/repos/perf3ct/gea/milestones")
            .expect("the lookup");
        assert!(lookup.query.contains("state=all"), "{}", lookup.query);
        assert_eq!(text(&buf), "Closed milestone \"1.0\" (closed, due —)\n");
    }

    /// Bug this prevents: resolving an ambiguous title to whichever milestone the server
    /// listed first. Gitea does not enforce unique titles — creating `1.0` twice succeeds —
    /// so "pick the first" would close, edit or delete the wrong milestone and report success.
    #[tokio::test]
    async fn two_milestones_with_one_title_are_refused_rather_than_guessed() {
        let fake = Arc::new(
            FakeTransport::new()
                .on(
                    method!("GET"),
                    "/api/v1/settings/api",
                    Canned::json(200, r#"{"max_response_items":50}"#),
                )
                .on_sequence(
                    method!("GET"),
                    "/api/v1/repos/perf3ct/gea/milestones",
                    Vec::from([
                        Canned::json(
                            200,
                            r#"[{"id":7,"title":"1.0","state":"open"},
                                {"id":8,"title":"1.0","state":"open"}]"#,
                        ),
                        Canned::json(200, "[]"),
                    ]),
                ),
        );
        let buf = Arc::new(Mutex::new(Vec::new()));
        let globals = GlobalOpts::default();
        let cx =
            cx(fake.clone(), &buf, &globals, Term::piped(), gitea_client::fields::FIELDS_MILESTONE);
        let e = set_state(&cx, &TitleArgs { title: "1.0".to_owned() }, "closed").await.unwrap_err();
        assert_eq!(e.exit_code(), 2);
        assert!(e.to_string().contains("ids 7, 8"), "{e}");
        assert!(
            fake.calls().iter().all(|c| c.method.as_str() != "PATCH"),
            "nothing is changed while the title is ambiguous"
        );
    }

    #[tokio::test]
    async fn an_unknown_title_lists_the_titles_that_exist() {
        let fake = Arc::new(with_milestones());
        let buf = Arc::new(Mutex::new(Vec::new()));
        let globals = GlobalOpts::default();
        let cx = cx(fake, &buf, &globals, Term::piped(), gitea_client::fields::FIELDS_MILESTONE);
        let e = set_state(&cx, &TitleArgs { title: "3.0".to_owned() }, "closed").await.unwrap_err();
        // Exit 2: the argument was wrong, and nothing reached the server — so this must not
        // render as "the server rejected the values in this request".
        assert_eq!(e.exit_code(), 2);
        assert!(e.to_string().contains("1.0, 2.0"), "{e}");
    }

    #[tokio::test]
    async fn list_renders_progress_counters() {
        let fake = Arc::new(with_milestones());
        let buf = Arc::new(Mutex::new(Vec::new()));
        let globals = GlobalOpts::default();
        let cx = cx(fake, &buf, &globals, Term::tty(90), gitea_client::fields::FIELDS_MILESTONE);
        let args = ListArgs { state: State::Open, limit: None };
        list(&cx, &globals, &args).await.unwrap();
        insta::assert_snapshot!("list_human", text(&buf));
    }

    #[tokio::test]
    async fn list_json_uses_the_api_field_names() {
        let fake = Arc::new(with_milestones());
        let buf = Arc::new(Mutex::new(Vec::new()));
        let globals = GlobalOpts {
            json: Some("title,open_issues,closed_issues".to_owned()),
            ..GlobalOpts::default()
        };
        let cx = cx(fake, &buf, &globals, Term::piped(), gitea_client::fields::FIELDS_MILESTONE);
        let args = ListArgs { state: State::Open, limit: None };
        list(&cx, &globals, &args).await.unwrap();
        insta::assert_snapshot!("list_json", text(&buf));
    }

    /// `issues` must send the milestone *and* `type=issues`; without the latter it lists pull
    /// requests attached to the milestone as well.
    #[tokio::test]
    async fn milestone_issues_filters_by_title_and_excludes_pull_requests() {
        let fake = Arc::new(with_milestones().on_sequence(
            method!("GET"),
            "/api/v1/repos/perf3ct/gea/issues",
            Vec::from([
                Canned::json(
                    200,
                    r#"[{"id":1,"number":42,"title":"It broke","state":"open","milestone":{"id":7,"title":"1.0"}}]"#,
                ),
                Canned::json(200, "[]"),
            ]),
        ));
        let buf = Arc::new(Mutex::new(Vec::new()));
        let globals = GlobalOpts::default();
        let cx =
            cx(fake.clone(), &buf, &globals, Term::tty(90), gitea_client::fields::FIELDS_ISSUE);
        let args = IssuesArgs {
            target: TitleArgs { title: "1.0".to_owned() },
            state: State::Open,
            limit: None,
        };
        issues(&cx, &globals, &args).await.unwrap();

        let call = fake
            .calls()
            .into_iter()
            .find(|c| c.path == "/api/v1/repos/perf3ct/gea/issues")
            .expect("the issue list request");
        assert!(call.query.contains("milestones=1.0"), "{}", call.query);
        assert!(call.query.contains("type=issues"), "{}", call.query);
        insta::assert_snapshot!("issues_human", text(&buf));
    }

    #[test]
    fn a_bare_date_is_accepted_as_midnight_utc() {
        // Bug this prevents: passing `2026-12-31` straight to the API, which answers with a Go
        // time-parsing error naming a layout string no user has ever seen.
        let ts = parse_deadline("2026-12-31").unwrap();
        assert_eq!(ts.as_jiff().to_string(), "2026-12-31T00:00:00Z");
        // A full timestamp still works, and keeps its time of day.
        let ts = parse_deadline("2026-12-31T13:45:00Z").unwrap();
        assert_eq!(ts.as_jiff().to_string(), "2026-12-31T13:45:00Z");

        let e = parse_deadline("next tuesday").unwrap_err();
        assert_eq!(e.exit_code(), 2);
        assert!(e.to_string().contains("YYYY-MM-DD"), "{e}");
    }

    #[tokio::test]
    async fn edit_with_no_flags_is_a_usage_error_before_any_request() {
        let fake = Arc::new(FakeTransport::new());
        let buf = Arc::new(Mutex::new(Vec::new()));
        let globals = GlobalOpts::default();
        let cx =
            cx(fake.clone(), &buf, &globals, Term::piped(), gitea_client::fields::FIELDS_MILESTONE);
        let args = EditArgs {
            target: TitleArgs { title: "1.0".to_owned() },
            new_title: None,
            deadline: None,
            description: None,
            state: None,
        };
        assert_eq!(edit(&cx, &args).await.unwrap_err().exit_code(), 2);
        assert_eq!(fake.call_count(), 0);
    }
}
