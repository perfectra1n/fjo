//! The generated client, driven end to end against a `FakeTransport`.
//!
//! Two things are being tested here that the emitter's own unit tests cannot reach. First, that
//! the generated code *compiles as a caller uses it* — every assertion below is also a
//! type-check of one method shape (typed JSON, `()`, `String`, `(Mime, ByteStream)`,
//! `ItemStream<T>`, a `_page` pair, a multipart upload). Second, that the URLs it builds are the
//! ones the API wants, which is a property of encoding decisions made at generation time and
//! visible only on the wire.
//!
//! No network and no port binding: `FakeTransport` matches `(method, path)` against canned
//! responses, so these run in microseconds.

use std::sync::Arc;

use futures::StreamExt;
use gitea_client::gitea_core::ErrorKind;
use gitea_client::gitea_core::error::classify::infer_scope;
use gitea_client::gitea_core::http::transport::Canned;
use gitea_client::gitea_core::http::{
    Auth, Client, FakeTransport, Paging, Part, Progress, RetryPolicy,
};
use gitea_client::{Api, query};
use reqwest::Method;

fn api(fake: Arc<FakeTransport>) -> Api {
    Api::new(
        Client::builder("https://git.example.org", Auth::token("t"))
            .transport(fake)
            // A capabilities probe would otherwise fire on the first paginated call and 404
            // against a fake that only registered the endpoint under test.
            .retry(RetryPolicy { max: 1, ..RetryPolicy::default() })
            .build()
            .expect("a well-formed base URL"),
    )
}

#[tokio::test]
async fn a_typed_get_deserialises_into_a_generated_model() {
    let fake = Arc::new(FakeTransport::new().on(
        Method::GET,
        "/api/v1/repos/perf3ct/gea/pulls/7",
        Canned::json(200, r#"{"number":7,"title":"Add the client emitter","state":"open"}"#),
    ));
    let pr = api(fake).repo().get_pull_request("perf3ct", "gea", 7).await.unwrap();
    // `number` is the curated `IssueIndex` newtype, not a bare i64: the per-repo counter and the
    // global row id are different things, and mixing them silently operates on another issue.
    assert_eq!(pr.number.to_string(), "7");
    assert_eq!(pr.title, "Add the client emitter");
}

#[tokio::test]
async fn a_body_is_serialised_from_the_generated_option_type() {
    let fake = Arc::new(FakeTransport::new().on(
        Method::POST,
        "/api/v1/repos/perf3ct/gea/pulls",
        Canned::json(201, r#"{"number":8}"#),
    ));
    let body = gitea_client::gitea_model::CreatePullRequestOption {
        title: Some("hi".to_owned()),
        base: Some("main".to_owned()),
        head: Some("topic".to_owned()),
        ..Default::default()
    };
    let pr = api(fake.clone()).repo().create_pull_request("perf3ct", "gea", &body).await.unwrap();
    assert_eq!(pr.number.to_string(), "8");

    let sent = fake.calls()[0].body_str();
    assert!(sent.contains(r#""title":"hi""#), "{sent}");
    assert!(sent.contains(r#""base":"main""#), "{sent}");
    // The whole point of the request-body `Presence` policy: a field the caller never set is
    // *absent* from the JSON. Serialized as `"body":""` instead, this PATCH-shaped payload would
    // blank the description of anything it was sent at.
    assert!(!sent.contains(r#""body""#), "an unset field must not be serialized at all: {sent}");
    assert!(!sent.contains(r#""assignees""#), "an unset Vec must not be serialized: {sent}");
}

#[tokio::test]
async fn a_204_endpoint_returns_unit_without_decoding() {
    let fake = Arc::new(FakeTransport::new().on(
        Method::DELETE,
        "/api/v1/repos/perf3ct/gea",
        Canned::new(204),
    ));
    api(fake).repo().delete("perf3ct", "gea").await.unwrap();
}

/// The `get-contents` 404. `filepath` is path-like, so its slashes are structural: encoded, the
/// request asks for a file literally named `src/main.rs` in the repository root, and every nested
/// file in every repository 404s.
#[tokio::test]
async fn a_path_like_parameter_keeps_its_slashes_on_the_wire() {
    let fake = Arc::new(FakeTransport::new().on(
        Method::GET,
        "/api/v1/repos/perf3ct/gea/contents/src/main.rs",
        Canned::json(200, r#"{"name":"main.rs","path":"src/main.rs"}"#),
    ));
    let out = api(fake.clone())
        .repo()
        .get_contents("perf3ct", "gea", "src/main.rs", &query::RepoGetContentsQuery::default())
        .await
        .unwrap();
    assert_eq!(out.one().expect("a file is the single-entry shape").path, "src/main.rs");
    assert_eq!(fake.calls()[0].path, "/api/v1/repos/perf3ct/gea/contents/src/main.rs");
    // A default query struct sends nothing at all, so no stray `?ref=`.
    assert_eq!(fake.calls()[0].query, "");
}

/// The same route, same method, answering with an **array** because the path named a directory.
///
/// The specification declares only the single-value shape, so a generated
/// `Result<ContentsResponse>` failed here with "invalid type: sequence, expected struct
/// ContentsResponse" — which is why `gea workflow` had to hand-roll the request instead of
/// calling the generated method.
#[tokio::test]
async fn get_contents_decodes_the_directory_shape_too() {
    let fake = Arc::new(FakeTransport::new().on(
        Method::GET,
        "/api/v1/repos/perf3ct/gea/contents/src",
        Canned::json(200, r#"[{"name":"main.rs","path":"src/main.rs"},{"name":"lib.rs"}]"#),
    ));
    let out = api(fake)
        .repo()
        .get_contents("perf3ct", "gea", "src", &query::RepoGetContentsQuery::default())
        .await
        .unwrap();
    assert!(out.one().is_none(), "a directory listing is not a single entry");
    assert_eq!(out.into_vec().len(), 2);
}

/// The mirror-image bug. A single-segment parameter whose value contains `/` must be encoded, or
/// it adds a path segment and matches a *different route* — at best a 404, at worst a request
/// against the wrong object.
#[tokio::test]
async fn a_segment_parameter_containing_a_slash_is_percent_encoded() {
    let fake = Arc::new(FakeTransport::new().on(
        Method::GET,
        "/api/v1/repos/a%2Fb/gea",
        Canned::json(200, r#"{"name":"gea"}"#),
    ));
    api(fake.clone()).repo().get("a/b", "gea").await.unwrap();
    assert_eq!(fake.calls()[0].path, "/api/v1/repos/a%2Fb/gea");
}

/// One of exactly two paths in the spec where two parameters share a segment, separated by a
/// literal `.`. The URL must be `/pulls/7.diff` — not `/pulls/7%2Ediff`, and certainly not
/// `/pulls/7.{diffType}`.
#[tokio::test]
async fn the_dotted_pull_diff_path_renders_a_literal_dot() {
    let fake = Arc::new(FakeTransport::new().on(
        Method::GET,
        "/api/v1/repos/perf3ct/gea/pulls/7.diff",
        Canned::text(200, "diff --git a/x b/x\n"),
    ));
    let diff = api(fake.clone())
        .repo()
        .download_pull_diff_or_patch(
            "perf3ct",
            "gea",
            7,
            "diff",
            &query::RepoDownloadPullDiffOrPatchQuery::default(),
        )
        .await
        .unwrap();
    assert!(diff.starts_with("diff --git"), "{diff}");
    assert_eq!(fake.calls()[0].path, "/api/v1/repos/perf3ct/gea/pulls/7.diff");
    assert_eq!(fake.calls()[0].header("accept"), Some("text/plain"));
}

/// The second dotted path, tested by name so a refactor cannot quietly fix one and break the
/// other.
#[tokio::test]
async fn the_dotted_commit_diff_path_renders_a_literal_dot() {
    let sha = "0123456789abcdef";
    let fake = Arc::new(FakeTransport::new().on(
        Method::GET,
        &format!("/api/v1/repos/perf3ct/gea/git/commits/{sha}.patch"),
        Canned::text(200, "From 0123 Mon Sep 17\n"),
    ));
    let patch = api(fake.clone())
        .repo()
        .download_commit_diff_or_patch("perf3ct", "gea", sha, "patch")
        .await
        .unwrap();
    assert!(patch.starts_with("From 0123"), "{patch}");
}

/// Bug this prevents: `create-runner-registration-token` printing nothing. Gitea's spec puts the
/// token in a response *header* with no body, while the handler answers a JSON object; typed as
/// the spec says, the token was read and thrown away. `overrides.toml [response_type]` keeps it.
#[tokio::test]
async fn a_registration_token_is_read_from_the_body_the_spec_does_not_declare() {
    let fake = Arc::new(FakeTransport::new().on(
        Method::POST,
        "/api/v1/repos/perf3ct/gea/actions/runners/registration-token",
        Canned::json(200, r#"{"token":"AAAA"}"#),
    ));
    let got = api(fake).repo().create_runner_registration_token("perf3ct", "gea").await.unwrap();
    assert_eq!(got["token"], "AAAA");
}

#[tokio::test]
async fn a_binary_endpoint_streams_its_bytes_and_reports_its_media_type() {
    let fake = Arc::new(FakeTransport::new().on(
        Method::GET,
        "/api/v1/repos/perf3ct/gea/archive/main.zip",
        Canned::bytes(200, "application/zip", &b"PK\x03\x04"[..]),
    ));
    let (mime, mut body) = api(fake.clone())
        .repo()
        .get_archive("perf3ct", "gea", "main.zip", &Default::default())
        .await
        .unwrap();
    assert_eq!(mime.essence(), "application/zip");
    let mut bytes = Vec::new();
    while let Some(chunk) = body.next().await {
        bytes.extend_from_slice(&chunk.unwrap());
    }
    assert_eq!(bytes, b"PK\x03\x04");
    // `*/*`: we hand back whatever media type arrived, so a narrower Accept could only earn a 406.
    assert_eq!(fake.calls()[0].header("accept"), Some("*/*"));
}

/// The whole point of the two-method pair. The stream must cross the page boundary: a client that
/// stopped after page one would silently return half the pull requests with exit code 0.
#[tokio::test]
async fn a_paginated_stream_follows_the_link_header_across_pages() {
    let page1 = Canned::json(200, r#"[{"number":1},{"number":2}]"#).with_header(
        "link",
        "<https://git.example.org/api/v1/repos/o/r/pulls?page=2>; rel=\"next\"",
    );
    let page2 = Canned::json(200, r#"[{"number":3}]"#).with_header(
        "link",
        "<https://git.example.org/api/v1/repos/o/r/pulls?page=1>; rel=\"prev\"",
    );
    let fake = Arc::new(
        FakeTransport::new()
            .on_sequence(Method::GET, "/api/v1/repos/o/r/pulls", vec![page1, page2.clone()])
            // The capabilities probe the paginator makes before its first page.
            .on(
                Method::GET,
                "/api/v1/settings/api",
                Canned::json(200, r#"{"max_response_items":50}"#),
            )
            .on(Method::GET, "/api/v1/version", Canned::json(404, "{}")),
    );

    let api = api(fake.clone());
    let numbers: Vec<String> = api
        .repo()
        .list_pull_requests("o", "r", &query::RepoListPullRequestsQuery::default())
        .map(|pr| pr.unwrap().number.to_string())
        .collect()
        .await;
    assert_eq!(numbers, ["1", "2", "3"], "the stream must not stop at the first page");
}

#[tokio::test]
async fn the_page_half_hands_back_the_headers() {
    let fake = Arc::new(FakeTransport::new().on(
        Method::GET,
        "/api/v1/repos/o/r/pulls",
        Canned::json(200, r#"[{"number":1}]"#).with_header("x-total-count", "42"),
    ));
    let (items, info) = api(fake.clone())
        .repo()
        .list_pull_requests_page(
            "o",
            "r",
            &query::RepoListPullRequestsQuery::default().with_page(3),
            Paging { limit: None, per_page: Some(20) },
        )
        .await
        .unwrap();
    assert_eq!(items.len(), 1);
    assert_eq!(info.total_count, Some(42), "x-total-count is why this method exists");
    // The page number comes from the query struct; `Paging::per_page` becomes the page size.
    assert_eq!(fake.calls()[0].query_param("page"), Some("3"));
    assert_eq!(fake.calls()[0].query_param("limit"), Some("20"));
}

/// A stream walks the collection from the beginning, so a page number left on the query struct
/// must not reach the wire: page 5 followed by page 2 would skip items and then repeat them.
#[tokio::test]
async fn the_stream_half_drops_a_page_number_from_the_query() {
    let fake = Arc::new(
        FakeTransport::new()
            .on(Method::GET, "/api/v1/repos/o/r/pulls", Canned::json(200, "[]"))
            .on(Method::GET, "/api/v1/settings/api", Canned::json(200, "{}"))
            .on(Method::GET, "/api/v1/version", Canned::json(404, "{}")),
    );
    let api = api(fake.clone());
    let mut stream = api.repo().list_pull_requests(
        "o",
        "r",
        &query::RepoListPullRequestsQuery::default().with_page(5).with_state("closed"),
    );
    assert!(stream.next().await.is_none());
    let call = fake.calls_to(&Method::GET, "/api/v1/repos/o/r/pulls").remove(0);
    assert_eq!(call.query_param("page"), None, "the paginator owns `page`");
    assert_eq!(call.query_param("state"), Some("closed"), "other filters must survive");
}

#[tokio::test]
async fn a_multipart_upload_posts_to_the_asset_endpoint() {
    let fake = Arc::new(FakeTransport::new().on(
        Method::POST,
        "/api/v1/repos/perf3ct/gea/releases/12/assets",
        Canned::json(201, r#"{"id":3,"name":"gea.tar.gz"}"#),
    ));
    let attachment = Part::bytes("ignored-by-the-generated-method", b"payload".to_vec())
        .with_filename("gea.tar.gz");
    let out = api(fake.clone())
        .repo()
        .create_release_attachment(
            "perf3ct",
            "gea",
            12,
            Some(attachment),
            &query::RepoCreateReleaseAttachmentQuery::default().with_name("gea.tar.gz"),
            Progress::new(),
        )
        .await
        .unwrap();
    assert_eq!(out.name, "gea.tar.gz");
    let call = &fake.calls()[0];
    assert_eq!(call.query_param("name"), Some("gea.tar.gz"));
    // The body never materialises as bytes anywhere in the client. That is the property that
    // keeps `gea release create v1 ./big.iso` at a few hundred kilobytes of RSS instead of the
    // file's size, and `FakeTransport` only records a body for the buffered kinds.
    assert!(call.body.is_none(), "a multipart upload must stream, not buffer");
}

/// `Default` must send nothing. If it sent even one parameter, every caller of every generated
/// list method would be applying a filter it never asked for.
#[test]
fn a_default_query_struct_produces_no_pairs() {
    assert!(query::RepoListPullRequestsQuery::default().to_pairs().is_empty());
    assert!(query::IssueListIssuesQuery::default().to_pairs().is_empty());
    assert!(query::RepoGetContentsQuery::default().to_pairs().is_empty());
}

/// The order is fixed and sorted by wire name. An order that moved between runs would make every
/// snapshot test of a request URL flap.
#[test]
fn to_pairs_is_stable_and_repeats_a_list_key() {
    let q = query::RepoListPullRequestsQuery::default()
        .with_state("open")
        .with_labels(vec![3, 4])
        .with_page(2)
        .with_base_branch("main");
    let pairs = q.to_pairs();
    assert_eq!(
        pairs,
        vec![
            ("base_branch", "main".to_owned()),
            ("labels", "3".to_owned()),
            ("labels", "4".to_owned()),
            ("page", "2".to_owned()),
            ("state", "open".to_owned()),
        ]
    );
    // Twice, because a builder must not mutate anything shared and the order must not depend on
    // insertion order.
    assert_eq!(q.clone().to_pairs(), pairs);
    let same = query::RepoListPullRequestsQuery::default()
        .with_base_branch("main")
        .with_page(2)
        .with_labels(vec![3, 4])
        .with_state("open");
    assert_eq!(same.to_pairs(), pairs);
}

/// Every group is reachable from the facade, and each accessor hands back a distinct type. This is
/// the test that would fail if a spec bump added a group the facade could not reach.
#[test]
fn the_facade_exposes_every_group() {
    let client = Client::builder("https://git.example.org", Auth::None)
        .transport(Arc::new(FakeTransport::new()))
        .build()
        .unwrap();
    let api = Api::new(client);
    // Named individually rather than in a loop: the point is that each one type-checks.
    let _: gitea_client::ops::Repo<'_> = api.repo();
    let _: gitea_client::ops::Issue<'_> = api.issue();
    let _: gitea_client::ops::User<'_> = api.user();
    let _: gitea_client::ops::Org<'_> = api.org();
    let _: gitea_client::ops::Admin<'_> = api.admin();
    let _: gitea_client::ops::Misc<'_> = api.misc();
    let _: gitea_client::ops::Notify<'_> = api.notify();
    let _: gitea_client::ops::Package<'_> = api.package();
    let _: gitea_client::ops::Settings<'_> = api.settings();
    let _: gitea_client::ops::Team<'_> = api.team();
    let _: gitea_client::ops::Topic<'_> = api.topic();
    let _: gitea_client::ops::Job<'_> = api.job();
    let _: gitea_client::ops::Artifact<'_> = api.artifact();
    let _: gitea_client::ops::Git<'_> = api.git();
    let _: gitea_client::ops::Run<'_> = api.run();
    let _: gitea_client::ops::Task<'_> = api.task();
    let _: gitea_client::ops::Workflow<'_> = api.workflow();
}

// ----------------------------------------------------------------- the scope a 403 names
//
// The `needs:` line of an `InsufficientScope` message has two possible sources: `OpMeta::scope`,
// which codegen derives from the specification's `tags[0]` and `overrides.toml` corrects, and
// `error::classify::infer_scope`, which reconstructs the same rule from the method and path at
// runtime. They agree on 469 of the 506 routes, so a test on an ordinary route proves nothing
// about which one answered. Every test below therefore uses a route where they *disagree*.

/// `POST /repos/{o}/{r}/pulls` is tagged `repository` in the specification, so it needs
/// `write:repository` — but its path looks like an issue route, and `infer_scope` says
/// `write:issue`. A user sent to mint `write:issue` for this call gets a second 403.
#[tokio::test]
async fn a_403_names_the_scope_codegen_recorded_not_the_one_inferred_from_the_path() {
    let fake = Arc::new(FakeTransport::new().on(
        Method::POST,
        "/api/v1/repos/perf3ct/gea/pulls",
        // No bracketed list, so `scopes_in_message` finds nothing and the scope has to come from
        // the request itself. Gitea answers exactly this way when the route's own permission
        // check fails rather than the token middleware's.
        Canned::json(403, r#"{"message":"token does not have sufficient scope"}"#),
    ));
    let body = gitea_client::gitea_model::CreatePullRequestOption {
        title: Some("hi".to_owned()),
        ..Default::default()
    };
    let e = api(fake).repo().create_pull_request("perf3ct", "gea", &body).await.unwrap_err();

    let ErrorKind::InsufficientScope { needed, .. } = e.kind() else {
        panic!("expected InsufficientScope, got {:?}", e.kind());
    };
    assert_eq!(needed, &["write:repository".to_owned()]);

    // The discriminating half: the same route, classified without the request's scope, produces
    // a *different* answer. Without this the test would pass on an inferred scope too.
    assert_eq!(
        infer_scope("POST", "/api/v1/repos/perf3ct/gea/pulls").map(|s| s.to_string()),
        Some("write:issue".to_owned()),
        "inference must still disagree here, or this test proves nothing",
    );
}

/// The paginated shape builds its request through `stream_request`, and the `_page` shape through
/// `page_request`. Both rebuild the `Request`, so both are places the scope could be dropped.
#[tokio::test]
async fn a_paginated_call_site_carries_its_scope_too() {
    let fake = Arc::new(FakeTransport::new().on(
        Method::GET,
        "/api/v1/repos/perf3ct/gea/pulls",
        Canned::json(403, r#"{"message":"token does not have sufficient scope"}"#),
    ));
    let q = query::RepoListPullRequestsQuery::default();
    let e = api(fake)
        .repo()
        .list_pull_requests_page("perf3ct", "gea", &q, Paging::default())
        .await
        .unwrap_err();

    let ErrorKind::InsufficientScope { needed, .. } = e.kind() else {
        panic!("expected InsufficientScope, got {:?}", e.kind());
    };
    // `read:repository` from `OpMeta`; `infer_scope` would say `read:issue`.
    assert_eq!(needed, &["read:repository".to_owned()]);
}

/// The hazard that commit 1797de7 restructured the 403 arm to survive, checked from the outside:
/// now that every generated request carries a scope, a 403 that has nothing to do with scopes
/// must still not be reported as one. Sending a user to mint a token for "you are not a
/// collaborator" costs them a trip through the web UI and changes nothing.
#[tokio::test]
async fn a_403_that_is_not_about_scopes_stays_forbidden_even_though_the_scope_is_known() {
    let fake = Arc::new(FakeTransport::new().on(
        Method::POST,
        "/api/v1/repos/perf3ct/gea/pulls",
        Canned::json(403, r#"{"message":"user is not a collaborator on this repository"}"#),
    ));
    let body = gitea_client::gitea_model::CreatePullRequestOption {
        title: Some("hi".to_owned()),
        ..Default::default()
    };
    let e = api(fake).repo().create_pull_request("perf3ct", "gea", &body).await.unwrap_err();
    let ErrorKind::Forbidden { server_message } = e.kind() else {
        panic!("expected Forbidden, got {:?}", e.kind());
    };
    assert!(server_message.contains("collaborator"), "{server_message}");
}

/// The server's own answer still wins over both. It is the only source that is neither derived
/// nor guessed.
#[tokio::test]
async fn a_scope_the_server_names_beats_the_one_the_request_carries() {
    let fake = Arc::new(FakeTransport::new().on(
        Method::POST,
        "/api/v1/repos/perf3ct/gea/pulls",
        Canned::json(
            403,
            r#"{"message":"token does not have at least one of required scope(s): [write:issue]"}"#,
        ),
    ));
    let body = gitea_client::gitea_model::CreatePullRequestOption {
        title: Some("hi".to_owned()),
        ..Default::default()
    };
    let e = api(fake).repo().create_pull_request("perf3ct", "gea", &body).await.unwrap_err();
    let ErrorKind::InsufficientScope { needed, .. } = e.kind() else {
        panic!("expected InsufficientScope, got {:?}", e.kind());
    };
    assert_eq!(needed, &["write:issue".to_owned()], "the server's own list must win");
}
