//! Pagination against a real server.
//!
//! Every termination rule in `gitea-core::http::paginate` keys off response headers that the
//! Gitea specification **does not declare** — so a unit test can only confirm that we agree
//! with our own guess about them. These tests confirm the guess.
//!
//! The rule under the most pressure is (c): "a short page ends the walk" is wrong, because
//! Gitea silently clamps `limit` to `max_response_items`. Ask for 100, receive 50, conclude
//! "done", and lose everything past item 50 — with exit 0 and no warning. That is the highest
//! consequence silent bug available in this tool, so it gets a test against the real clamp
//! rather than a simulated one.

use gea_itest::{TestRepo, cover, instance_or_skip};

/// Comfortably more than one page at both the default page size (30) and the clamp (50), so the
/// walk has to cross a boundary no matter which the server picks.
const ISSUES: usize = 75;

fn seed_issues(repo: &TestRepo<'_>, n: usize) {
    for i in 1..=n {
        let (code, body) =
            repo.api("POST", "issues", Some(&format!(r#"{{"title":"issue number {i}"}}"#)));
        assert!((200..300).contains(&code), "seeding issue {i} failed: HTTP {code}: {body}");
    }
}

/// Does Gitea send the headers the whole design rests on?
///
/// Asserted rather than merely observed: if a future Gitea stops sending `Link`, the walk
/// falls back to the far weaker heuristic in rule (c), and we want to be told.
#[test]
fn gitea_sends_link_and_total_count() {
    let inst = instance_or_skip!();
    let repo = TestRepo::create(inst, "paging-headers");
    seed_issues(&repo, ISSUES);

    let first = inst.api_headers(&format!("repos/{}/issues?limit=50&page=1", repo.slug()));
    let lower = first.to_lowercase();
    assert!(lower.contains("x-total-count:"), "no X-Total-Count on page 1:\n{first}");
    assert!(lower.contains("link:"), "no Link header on page 1:\n{first}");
    assert!(
        lower.contains(r#"rel="next""#),
        "page 1 of {ISSUES} issues should advertise a next page:\n{first}"
    );

    // The last page is what actually terminates the walk: `Link` is still present, but carries
    // only `first`/`prev`. Termination rule (a) depends on exactly this shape.
    let last = inst.api_headers(&format!("repos/{}/issues?limit=50&page=2", repo.slug()));
    let lower = last.to_lowercase();
    assert!(lower.contains("link:"), "the last page should still carry a Link header:\n{last}");
    assert!(
        !lower.contains(r#"rel="next""#),
        "the last page must not advertise a next page, or the walk cannot terminate:\n{last}"
    );
}

/// The clamp is real: asking for more than `max_response_items` returns fewer items *without*
/// saying so, and the `Link` it sends back still echoes the limit we asked for.
#[test]
fn server_clamps_limit_below_what_was_requested() {
    let inst = instance_or_skip!();
    let repo = TestRepo::create(inst, "paging-clamp");
    seed_issues(&repo, ISSUES);

    let (code, body) =
        inst.api("GET", &format!("repos/{}/issues?limit=100&page=1", repo.slug()), None);
    assert_eq!(code, 200, "{body}");
    let items: Vec<serde_json::Value> = serde_json::from_str(&body).expect("an array of issues");

    let (_, caps) = inst.api("GET", "settings/api", None);
    let caps: serde_json::Value = serde_json::from_str(&caps).expect("settings/api is JSON");
    let max = caps["max_response_items"].as_u64().expect("max_response_items");

    assert!(
        (items.len() as u64) <= max && (items.len() as u64) < 100,
        "expected the server to clamp 100 down to {max}, but it returned {}. If Gitea has \
         stopped clamping, termination rule (c) is no longer load-bearing and its comment is \
         now misleading.",
        items.len()
    );
}

/// The property that matters: every item, exactly once, across a real multi-page walk.
///
/// Both spellings are checked because they take different paths through the paginator — an
/// explicit over-max `limit` is the case that trips the naive termination rule.
#[test]
fn paginate_loses_nothing() {
    let inst = instance_or_skip!();
    let repo = TestRepo::create(inst, "paging-walk");
    seed_issues(&repo, ISSUES);

    for query in ["", "?limit=100"] {
        // `--debug` so the walk reports how many pages it took; see `pages_walked` below.
        let run = inst.gea([
            "api",
            &format!("repos/{}/issues{query}", repo.slug()),
            "--paginate",
            "--debug",
            "--jq",
            ".[].number",
        ]);
        run.assert_ok(&format!("gea api --paginate '{query}'"));

        let mut got: Vec<u64> = run.stdout.lines().filter_map(|l| l.trim().parse().ok()).collect();
        let total = got.len();
        got.sort_unstable();
        got.dedup();

        assert_eq!(
            total, ISSUES,
            "--paginate '{query}' returned {total} issues, expected {ISSUES}. \
             Fewer means the walk stopped early and silently dropped items."
        );
        assert_eq!(got.len(), ISSUES, "--paginate '{query}' returned duplicates");
        assert_eq!(got.first().copied(), Some(1));
        assert_eq!(got.last().copied(), Some(ISSUES as u64));

        // The item count alone passed throughout the `?`-collision bug described on
        // `a_limit_typed_into_the_endpoint_governs_the_page_size` below, because the walk still
        // terminated correctly on the `Link` header — it simply ignored the limit and used the
        // server's default page size. The *page* count is what tells the two apart: with
        // `?limit=100` clamped to `max_response_items` the walk is 2 pages, and with the limit
        // silently discarded it is 3.
        assert_eq!(
            pages_walked(&run),
            Some(expected_pages(inst, query)),
            "--paginate '{query}' walked a number of pages that does not match the limit it \
             was given, so the limit did not reach the server:\n{}",
            run.stderr
        );
    }
}

/// How many pages `--debug` says the walk took.
///
/// Read out of the debug summary rather than by counting requests, because there is nowhere to
/// count requests from outside the process — and the summary is the same line a user would be
/// shown when diagnosing this by hand.
fn pages_walked(run: &gea_itest::Run) -> Option<usize> {
    let line = run.stderr.lines().find(|l| l.contains("paginate:"))?;
    let (_, rest) = line.split_once("over ")?;
    let (n, _) = rest.split_once(" page")?;
    n.trim().parse().ok()
}

/// Pages a correct walk of [`ISSUES`] issues takes for a given endpoint query.
///
/// `max_response_items` is asked for rather than assumed: it is configurable, the clamp test
/// above exists precisely because it bites, and a hard-coded 50 here would turn a differently
/// configured instance into a mystery failure in an unrelated assertion.
fn expected_pages(inst: &gea_itest::Instance, query: &str) -> usize {
    let (_, body) = inst.api("GET", "settings/api", None);
    let caps: serde_json::Value = serde_json::from_str(&body).expect("settings/api is JSON");
    let max = caps["max_response_items"].as_u64().expect("max_response_items") as usize;
    let asked = query.strip_prefix("?limit=").and_then(|n| n.parse::<usize>().ok()).unwrap_or(max);
    let per_page = asked.min(max);
    ISSUES.div_ceil(per_page)
}

/// Bug this prevents, and it shipped: an endpoint typed with its own query became
/// `Request::path` whole, `?` and all. Nothing downstream looks inside `path` for a query —
/// `has_query` reads only the structured list — so the paginator's opt-out saw no `limit`, added
/// its own, and the URL grew a second `?`:
///
///     /api/v1/repos/o/r/issues?limit=5?limit=50&page=2
///
/// Everything after the first `?` is then one opaque parameter value, so **neither** limit is
/// honoured and the server falls back to its default page size.
///
/// `?limit=5` rather than a limit near the clamp, because the difference has to be unmissable:
/// against 75 issues a walk that honours it takes 15 pages and a walk that discards it takes 3.
/// Measured on this container with the fix reverted, which is exactly the 3 it reported.
#[test]
fn a_limit_typed_into_the_endpoint_governs_the_page_size() {
    let inst = instance_or_skip!();
    let repo = TestRepo::create(inst, "paging-endpointquery");
    seed_issues(&repo, ISSUES);

    // Without `--paginate` first: one page, of exactly the size that was asked for.
    let run =
        inst.gea(["api", &format!("repos/{}/issues?limit=5", repo.slug()), "--jq", ".[].number"]);
    run.assert_ok("gea api with an endpoint query");
    assert_eq!(
        run.stdout.lines().filter(|l| !l.trim().is_empty()).count(),
        5,
        "a `?limit=5` the user typed must reach the server"
    );

    let run = inst.gea([
        "api",
        &format!("repos/{}/issues?limit=5", repo.slug()),
        "--paginate",
        "--debug",
        "--jq",
        ".[].number",
    ]);
    run.assert_ok("gea api --paginate with an endpoint query");

    let mut got: Vec<u64> = run.stdout.lines().filter_map(|l| l.trim().parse().ok()).collect();
    let total = got.len();
    got.sort_unstable();
    got.dedup();
    assert_eq!(total, ISSUES, "the walk must still collect everything");
    assert_eq!(got.len(), ISSUES, "the walk must not return duplicates");

    assert_eq!(
        pages_walked(&run),
        Some(ISSUES.div_ceil(5)),
        "the user's `?limit=5` did not survive into the walk — it took a different number of \
         pages, which means the paginator sent its own limit alongside a second `?`:\n{}",
        run.stderr
    );
    assert!(
        !run.stderr.contains("issues?limit=5?"),
        "the request URL carries two `?`:\n{}",
        run.stderr
    );
}

/// A `%`-escape the user typed has to reach the server as the bytes they escaped, and no more.
///
/// This guards the *fix*, not the original bug: folding the endpoint's query into the structured
/// list means it is now decoded on the way in and re-encoded on the way out, and a round trip
/// that is not exactly balanced turns `?q=a%20b` — a search for `a b` — into `?q=a%2520b`, a
/// search for the six characters the user typed to avoid the space. The old code passed the raw
/// string through untouched and so could not get this wrong; the new code can, which is why it
/// is pinned here against a real server rather than only in a URL-shape unit test.
///
/// The control is the double-escaped spelling, which must find nothing: without it the
/// assertion would pass on a server that simply ignores `q`.
#[test]
fn a_percent_escape_in_an_endpoint_query_is_not_encoded_twice() {
    let inst = instance_or_skip!();
    let repo = TestRepo::create(inst, "paging-escape");
    let (code, body) = repo.api("POST", "issues", Some(r#"{"title":"needle haystack marker"}"#));
    assert!((200..300).contains(&code), "seeding the issue failed: HTTP {code}: {body}");
    repo.api("POST", "issues", Some(r#"{"title":"unrelated"}"#));

    // Gitea indexes issue titles asynchronously — a search a second after the POST finds
    // nothing — so the ground truth is established out of band first, by polling. Without this
    // the assertions below would be testing the indexer's latency rather than our encoding, and
    // would fail roughly whenever the container was busy.
    let direct = || -> usize {
        let (_, body) = repo.api("GET", "issues?q=needle%20haystack", None);
        serde_json::from_str::<serde_json::Value>(&body)
            .ok()
            .and_then(|v| v.as_array().map(Vec::len))
            .unwrap_or(0)
    };
    let mut waited = 0;
    while direct() != 1 && waited < 60 {
        std::thread::sleep(std::time::Duration::from_millis(500));
        waited += 1;
    }
    assert_eq!(
        direct(),
        1,
        "the server itself never matched `q=needle%20haystack`, so this test cannot say anything \
         about how gea spells it"
    );

    let hits = |query: &str| -> usize {
        let run =
            inst.gea(["api", &format!("repos/{}/issues?{query}", repo.slug()), "--jq", "length"]);
        run.assert_ok(&format!("gea api with '{query}'"));
        run.stdout
            .trim()
            .parse()
            .unwrap_or_else(|_| panic!("expected a count from --jq length, got {:?}", run.stdout))
    };

    assert_eq!(
        hits("q=needle%20haystack"),
        1,
        "`%20` must reach the server as a space, so the search matches the issue whose title \
         contains one — the same match the server just made for the same spelling"
    );
    assert_eq!(
        hits("q=needle%2520haystack"),
        0,
        "`%2520` is the escaped form of `%20` and must stay escaped — a match here would mean \
         the decode step is collapsing an escape the user wrote deliberately"
    );
}
/// `--limit N` is a user cap (termination rule (e)) and must stop the walk early rather than
/// being rounded up to a page boundary.
#[test]
fn user_limit_caps_the_walk() {
    let inst = instance_or_skip!();
    cover!(porcelain: ["issue list"], hits: ["issueListIssues"]);
    let repo = TestRepo::create(inst, "paging-userlimit");
    seed_issues(&repo, ISSUES);

    let run = inst.gea(["issue", "list", "-R", &repo.slug(), "--limit", "35", "--json", "number"]);
    run.assert_ok("gea issue list --limit 35");
    let items = run.json();
    assert_eq!(
        items.as_array().map(Vec::len),
        Some(35),
        "--limit 35 should return exactly 35 items, not a whole number of pages"
    );
}

/// Not every list endpoint paginates the same way, and the differences are invisible to a mock.
///
/// `issues` honours `limit`, clamps it to `max_response_items`, and sends both `Link` and
/// `X-Total-Count`. `labels` sends **no `Link` at all**. Termination rule (a), the authoritative
/// one, is therefore unavailable on `labels` and the walk falls back to the weaker rule (c).
///
/// Recorded as a test because the paginator's correctness argument depends on which of these
/// shapes a given endpoint has, and the specification declares no response headers at all.
#[test]
fn pagination_headers_differ_between_endpoints() {
    let inst = instance_or_skip!();
    let repo = TestRepo::create(inst, "paging-shapes");
    for i in 1..=60 {
        repo.api("POST", "labels", Some(&format!(r#"{{"name":"lbl-{i}","color":"00ff00"}}"#)));
    }

    let headers = inst.api_headers(&format!("repos/{}/labels?limit=5", repo.slug()));
    let lower = headers.to_lowercase();
    assert!(lower.contains("x-total-count:"), "labels should still count: {headers}");
    assert!(
        !lower.contains("link:"),
        "labels has never sent a Link header; if it now does, the paginator can use rule (a) \
         there and this note is out of date:\n{headers}"
    );

    // Gitea honours a bare `limit` on labels (Forgejo ignores it unless `page` is also sent),
    // so both spellings return one page of five. What the walk below relies on is only that the
    // listing terminates without a `Link` header.
    for query in ["limit=5", "page=1&limit=5"] {
        let (_, body) = inst.api("GET", &format!("repos/{}/labels?{query}", repo.slug()), None);
        let page: Vec<serde_json::Value> = serde_json::from_str(&body).expect("an array");
        assert_eq!(page.len(), 5, "`?{query}` on labels returned {} items", page.len());
    }

    // Whatever the endpoint's shape, the generic walk must still collect everything.
    let run = inst.gea([
        "api",
        &format!("repos/{}/labels", repo.slug()),
        "--paginate",
        "--jq",
        ".[].name",
    ]);
    run.assert_ok("gea api --paginate over an endpoint with no Link header");
    assert_eq!(
        run.stdout.lines().filter(|l| !l.trim().is_empty()).count(),
        60,
        "--paginate lost labels on an endpoint that sends no Link header"
    );
}

/// A truncated list has to say it was truncated, and by how much.
///
/// The design calls for gh's `Showing N of M` banner, and `X-Total-Count` supplies the M on
/// every list endpoint. The banner used to read `Showing 30 labels`, with no total, so a user
/// looking at a repository with 60 labels was shown 30 and not told that the other 30 existed.
/// That half is fixed; this pins it.
///
/// `GEA_FORCE_TTY` is not a workaround for the banner being hard to see. The banner is
/// terminal-only *by design*: `docs/porcelain-conventions.md` makes padded columns plus a banner
/// the TTY rendering and headerless TSV with no banner the piped rendering, precisely so
/// `gea label list | cut -f2` works. A test that captured stdout and then complained about the
/// missing banner would be asserting against the contract other tests in this file depend on,
/// so it asks for terminal rendering instead. (The variable takes a width, an `N%`, or any
/// non-empty value.)
#[test]
fn truncated_list_reports_the_total() {
    let inst = instance_or_skip!();
    cover!(porcelain: ["label list"], hits: ["issueListLabels"]);
    let repo = TestRepo::create(inst, "paging-banner");
    for i in 1..=60 {
        repo.api("POST", "labels", Some(&format!(r#"{{"name":"lbl-{i}","color":"00ff00"}}"#)));
    }

    let run = inst.gea(["label", "list", "-R", &repo.slug(), "--json", "name"]);
    run.assert_ok("gea label list");
    let shown = run.json().as_array().map(Vec::len).unwrap_or(0);
    assert!(shown < 60, "this test assumes the default view truncates; it showed {shown}");

    let banner = inst.gea_env(
        std::path::Path::new("."),
        &[("GEA_FORCE_TTY", "80")],
        ["label", "list", "-R", &repo.slug()],
    );
    banner.assert_ok("gea label list (table)");
    assert!(
        banner.stdout.contains(" of 60") || banner.stdout.contains("of 60 "),
        "the banner must say how many were withheld, but it reads:\n{}",
        banner.stdout.lines().next().unwrap_or_default()
    );
}

/// `gea … | head -1` must not panic or print a broken-pipe backtrace.
///
/// Rust sets `SIGPIPE` to `SIG_IGN` before `main`, which turns a closed pipe into an `Err` that
/// surfaces as a panic, exit 101 and a backtrace note. `main` restores the default disposition,
/// so the process is killed by the signal (141) or finishes first (0). Both are correct; what
/// must never happen is 101, or anything on stderr.
///
/// The output has to exceed a pipe buffer (64 KiB) for the signal to land at all, which is why
/// this needs a seeded repository rather than a trivial command.
#[test]
fn sigpipe_is_not_a_panic() {
    let inst = instance_or_skip!();
    let repo = TestRepo::create(inst, "sigpipe");
    seed_issues(&repo, ISSUES);

    // No shell: `${PIPESTATUS[0]}` is a bashism, and on a `dash`-based /bin/sh it would read
    // `head`'s status instead — a test that always passes while testing nothing. Instead the
    // pipe is closed from here, which is exactly what `head` does when it has read enough.
    //
    // The Command is hand-rolled because the pipe has to stay under this test's control, but the
    // environment comes from `child_env()`. Spelling it out here is how this test ended up
    // handing gea a scheme-stripped host and exiting 6 under docker-out-of-docker, long after
    // the harness itself had been fixed.
    let mut child = std::process::Command::new(gea_itest::gea_bin())
        .args(["api", &format!("repos/{}/issues?limit=50", repo.slug()), "--paginate"])
        .stdout(std::process::Stdio::piped())
        .stderr(std::process::Stdio::piped())
        .envs(inst.child_env())
        .spawn()
        .expect("gea should start");

    // Read a little, then drop the read end. The next write past the pipe buffer gets EPIPE.
    let mut stdout = child.stdout.take().expect("piped stdout");
    let mut buf = [0u8; 16];
    let _ = std::io::Read::read(&mut stdout, &mut buf);
    drop(stdout);

    let mut stderr_pipe = child.stderr.take().expect("piped stderr");
    let mut stderr = String::new();
    let _ = std::io::Read::read_to_string(&mut stderr_pipe, &mut stderr);
    let status = child.wait().expect("gea should exit");

    let code = status.code();
    #[cfg(unix)]
    let signal = std::os::unix::process::ExitStatusExt::signal(&status);
    #[cfg(not(unix))]
    let signal: Option<i32> = None;

    assert!(
        code == Some(0) || signal == Some(libc_sigpipe()),
        "expected a clean exit or death by SIGPIPE, got code {code:?} signal {signal:?}. \
         Exit 101 would mean SIGPIPE was left at Rust's SIG_IGN and the write panicked."
    );
    assert!(stderr.trim().is_empty(), "a closed pipe must be silent, but stderr had:\n{stderr}");
}

/// SIGPIPE's number, without taking a dependency on `libc` for one constant.
fn libc_sigpipe() -> i32 {
    13
}
