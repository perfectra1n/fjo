//! Proves the harness itself works: a container boots, an admin exists, and the token we
//! minted is accepted by the API.
//!
//! If this fails, every other integration test is meaningless, so it is worth having
//! separately from the tests that exercise `gea`.

use std::process::Command;

use gea_itest::instance_or_skip;

/// `curl` rather than reqwest: this test deliberately does not depend on our own HTTP stack,
/// so a bug in `gitea-core` cannot make the harness look broken.
fn get(url: &str, token: &str) -> (i32, String) {
    let out = Command::new("curl")
        .args(["-sS", "-w", "\n%{http_code}", "--max-time", "10"])
        .args(["-H", &format!("Authorization: token {token}")])
        .arg(url)
        .output()
        .expect("curl should run");
    let body = String::from_utf8_lossy(&out.stdout);
    let mut lines: Vec<&str> = body.lines().collect();
    let code = lines.pop().unwrap_or("0").trim().parse().unwrap_or(0);
    (code, lines.join("\n"))
}

#[test]
fn harness_boots_gitea_and_mints_a_working_token() {
    let inst = instance_or_skip!();

    // Unauthenticated: proves the server is really Gitea and reachable.
    let (code, body) = get(&format!("{}/version", inst.api_base()), "");
    assert_eq!(code, 200, "GET /version failed: {body}\nlogs:\n{}", inst.logs());
    assert!(body.contains("version"), "unexpected /version body: {body}");

    // Authenticated: proves the minted token is accepted, which is the part most likely to
    // break if Gitea changes its admin CLI.
    let (code, body) = get(&format!("{}/user", inst.api_base()), &inst.token);
    assert_eq!(code, 200, "GET /user with our token failed: {body}");
    assert!(body.contains(&inst.user), "token belongs to someone unexpected: {body}");

    // The token must have real privileges, not just read access to itself.
    let (code, _) = get(&format!("{}/admin/users", inst.api_base()), &inst.token);
    assert_eq!(code, 200, "admin scope missing from the minted token");
}

#[test]
fn version_reports_the_release_we_generated_from() {
    let inst = instance_or_skip!();
    let (code, body) = get(&format!("{}/version", inst.api_base()), "");
    assert_eq!(code, 200);

    // Not an equality assertion: the point is to make a mismatch *visible* in test output
    // rather than to fail when someone deliberately tests against a newer server.
    if !body.contains("16.0") {
        eprintln!(
            "note: instance reports {body}, but the vendored spec is Gitea 1.27.3. \
             Endpoint differences are expected."
        );
    }
}

/// The URL the *server* hands back has to name the address the tests can actually reach.
///
/// Gitea builds `clone_url` out of `ROOT_URL`, which the harness has to decide before there is
/// a container to ask where it is reachable (see `Instance::boot`). When that guess is wrong —
/// a remote `DOCKER_HOST`, or a sibling container where the daemon answers on the bridge gateway
/// — every URL the API returns points somewhere that does not resolve from here, and the first
/// symptom is `gea repo clone` hanging a long way from the cause.
///
/// This is the test that would have caught that: it takes the server's own `clone_url` and makes
/// git talk to it. Nothing else in the suite does — `TestRepo::push_url` builds its URL from
/// `base_url` rather than from the API, so the whole suite can pass with `ROOT_URL` wrong.
#[test]
fn the_clone_url_the_server_hands_back_is_reachable() {
    let inst = instance_or_skip!();
    let repo = gea_itest::TestRepo::create_initialized(inst, "root-url");

    let (code, body) = get(&format!("{}/repos/{}", inst.api_base(), repo.slug()), &inst.token);
    assert_eq!(code, 200, "could not read the repository back: {body}");
    let v: serde_json::Value = serde_json::from_str(&body).expect("the repo reply is JSON");
    let clone_url = v["clone_url"].as_str().expect("a repository has a clone_url").to_owned();

    let want = format!("{}/", inst.base_url);
    assert!(
        clone_url.starts_with(&want),
        "the server says this repository is at {clone_url}, but the tests reach it at \
         {}. ROOT_URL and the address in use have diverged, so every URL the API hands \
         back names somewhere unreachable.",
        inst.base_url
    );

    // Not just string-equal: actually make git speak to it, which is what a user would do.
    let authed = clone_url.replacen("http://", &format!("http://{}:{}@", inst.user, inst.token), 1);
    let out = Command::new("git")
        .args(["ls-remote", "--heads", &authed])
        .env("GIT_TERMINAL_PROMPT", "0")
        .output()
        .expect("git should run");
    assert!(
        out.status.success(),
        "git could not reach the clone_url the server gave us ({clone_url})\n{}",
        String::from_utf8_lossy(&out.stderr)
    );
}
