//! The instance-wide corners: packages, settings, templates, markup rendering, notifications,
//! topics and mirrors.
//!
//! What these have in common is that a mock proves almost nothing about them. They are mostly
//! *reads of server state that the server alone decides*: which gitignore templates exist, what
//! `max_response_items` is, how a markup renderer answers. A `FakeTransport` answers with
//! whatever the test author believed, so it agrees with itself by construction.

use std::collections::BTreeSet;
use std::process::Command;

use gea_itest::{Instance, TestRepo, cover, instance_or_skip};

// ---------------------------------------------------------------------------------------------
// Fixtures
// ---------------------------------------------------------------------------------------------

/// Upload one file into Gitea's **generic** package registry.
///
/// Three things here are not obvious and each one cost a round trip to find:
///
/// 1. The registry lives at `/api/packages/…`, **not** `/api/v1/packages/…`. The `v1` path is
///    the read/delete API; the upload is a different surface entirely. So this builds its URL
///    from [`Instance::base_url`] rather than from [`Instance::api_base`].
/// 2. It is a plain `PUT` with the file as the whole body — no multipart, no JSON envelope.
/// 3. The `Content-Type` has to be something other than the form types. Left at curl's default
///    (`application/x-www-form-urlencoded`) Gitea answers `500 request Content-Type isn't
///    multipart/form-data`, which reads like a server fault and is really a missing header.
///
/// Out of band on purpose, like [`Instance::api`]: the fixture that creates the thing under test
/// must not go through the code under test.
fn upload_generic_package(
    inst: &Instance,
    owner: &str,
    name: &str,
    version: &str,
    filename: &str,
    contents: &str,
) {
    let url = format!("{}/api/packages/{owner}/generic/{name}/{version}/{filename}", inst.base_url);
    let out = Command::new("curl")
        .args(["-sS", "-w", "\n%{http_code}", "--max-time", "30", "-X", "PUT"])
        .args(["-H", &format!("Authorization: token {}", inst.token)])
        .args(["-H", "Content-Type: application/octet-stream"])
        .args(["--data-binary", contents])
        .arg(&url)
        .output()
        .expect("curl should run to upload a package");
    let text = String::from_utf8_lossy(&out.stdout);
    let mut lines: Vec<&str> = text.lines().collect();
    let code: i32 = lines.pop().unwrap_or("0").trim().parse().unwrap_or(0);
    assert!(
        (200..300).contains(&code),
        "uploading {name}/{version}/{filename} to the generic registry failed: HTTP {code}: {}",
        lines.join("\n")
    );
}

/// A package name unique to this process, so parallel tests sharing one instance — and one
/// package owner — never see each other's uploads.
fn unique_package_name(inst: &Instance, prefix: &str) -> String {
    // `unique_repo_name` is just "a name nothing else in this process will pick"; the fact that
    // its callers mostly make repositories out of it is incidental. Reusing it keeps the one
    // counter that guarantees uniqueness in one place.
    inst.unique_repo_name(prefix)
}

// ---------------------------------------------------------------------------------------------
// Packages — the generic registry, then every read and both link directions
// ---------------------------------------------------------------------------------------------

/// The whole package surface in one lifecycle: upload, list, read, list files, link to a
/// repository, unlink, delete, confirm gone.
///
/// Every assertion is against server state the upload created, which is the point — a mock
/// cannot tell you that the file listing spells it `size` (Forgejo writes `Size`),
/// nor that `repository` comes back `null` until something links it.
///
/// `link`/`unlink` are the pair a mock is least able to check: they are `POST`s that return
/// nothing useful, so the only evidence they did anything is reading the package back.
#[test]
fn a_generic_package_upload_is_visible_to_every_read_and_survives_link_unlink_and_delete() {
    let inst = instance_or_skip!();
    cover!(raw: [
        "listPackages",
        "getPackage",
        "listPackageFiles",
        "linkPackage",
        "unlinkPackage",
        "listPackageVersions",
        "getLatestPackageVersion",
        "deletePackageVersion",
        "deletePackage",
    ]);

    let pkg = unique_package_name(inst, "rawpkg");
    let body = "generic package payload\n";
    upload_generic_package(inst, &inst.user, &pkg, "1.0.0", "payload.txt", body);
    let repo = TestRepo::create(inst, "pkglink");

    // list-packages is owner-scoped and every test in this file shares the owner, so this
    // asserts presence rather than a count — see this crate's note on test independence.
    let listed = inst.gea(["raw", "package", "list-packages", &inst.user, "--limit", "200"]);
    listed.assert_ok("gea raw package list-packages");
    let names: Vec<String> = listed
        .json()
        .as_array()
        .expect("list-packages returns an array")
        .iter()
        .filter_map(|p| p["name"].as_str().map(str::to_owned))
        .collect();
    assert!(names.contains(&pkg), "the uploaded package is missing from the listing: {names:?}");

    let view = inst.gea(["raw", "package", "get-package", &inst.user, "generic", &pkg, "1.0.0"]);
    view.assert_ok("gea raw package get-package");
    let got = view.json();
    assert_eq!(got["type"], "generic", "the registry type came back wrong: {got}");
    assert_eq!(got["version"], "1.0.0");
    assert!(got["repository"].is_null(), "a fresh package must not be linked to anything: {got}");

    let files =
        inst.gea(["raw", "package", "list-package-files", &inst.user, "generic", &pkg, "1.0.0"]);
    files.assert_ok("gea raw package list-package-files");
    let files = files.json();
    let file = &files[0];
    assert_eq!(file["name"], "payload.txt", "the file listing names the wrong file: {files}");
    assert_eq!(
        file["size"].as_u64(),
        Some(body.len() as u64),
        "the stored size does not match what was uploaded, so the PUT body was mangled: {files}"
    );
    assert!(
        file["sha256"].as_str().is_some_and(|s| s.len() == 64),
        "a package file must carry a sha256 digest: {files}"
    );

    inst.gea(["raw", "package", "link-package", &inst.user, "generic", &pkg, &repo.name])
        .assert_ok("gea raw package link-package");
    // Read back out of band: `link-package` answers with nothing, so its own exit code is not
    // evidence that the server attached anything.
    let (code, linked) =
        inst.api("GET", &format!("packages/{}/generic/{pkg}/1.0.0", inst.user), None);
    assert_eq!(code, 200, "{linked}");
    let linked: serde_json::Value = serde_json::from_str(&linked).expect("a package");
    assert_eq!(
        linked["repository"]["full_name"].as_str(),
        Some(repo.slug().as_str()),
        "link-package reported success but the package is not attached to the repository"
    );

    inst.gea(["raw", "package", "unlink-package", &inst.user, "generic", &pkg])
        .assert_ok("gea raw package unlink-package");
    let (_, unlinked) =
        inst.api("GET", &format!("packages/{}/generic/{pkg}/1.0.0", inst.user), None);
    let unlinked: serde_json::Value = serde_json::from_str(&unlinked).expect("a package");
    assert!(
        unlinked["repository"].is_null(),
        "unlink-package reported success but the link is still there: {unlinked}"
    );

    // A second version, so the two deletes — one version, then the whole package — each have
    // something the other would not have removed.
    upload_generic_package(inst, &inst.user, &pkg, "1.1.0", "payload.txt", body);
    let versions =
        inst.gea(["raw", "package", "list-package-versions", &inst.user, "generic", &pkg]);
    versions.assert_ok("gea raw package list-package-versions");
    let listed: BTreeSet<String> = versions
        .json()
        .as_array()
        .map(|a| a.iter().filter_map(|p| p["version"].as_str().map(str::to_owned)).collect())
        .unwrap_or_default();
    assert_eq!(listed, BTreeSet::from(["1.0.0".to_owned(), "1.1.0".to_owned()]), "{listed:?}");
    let latest =
        inst.gea(["raw", "package", "get-latest-package-version", &inst.user, "generic", &pkg]);
    latest.assert_ok("gea raw package get-latest-package-version");
    assert_eq!(latest.json()["version"], "1.1.0", "{}", latest.stdout);

    inst.gea(["raw", "package", "delete-package-version", &inst.user, "generic", &pkg, "1.0.0"])
        .assert_ok("gea raw package delete-package-version");
    let (code, _) = inst.api("GET", &format!("packages/{}/generic/{pkg}/1.0.0", inst.user), None);
    assert_eq!(code, 404, "the package version is still readable after delete-package-version");
    let (code, _) = inst.api("GET", &format!("packages/{}/generic/{pkg}/1.1.0", inst.user), None);
    assert_eq!(code, 200, "deleting one version took another with it");

    inst.gea(["raw", "package", "delete-package", &inst.user, "generic", &pkg])
        .assert_ok("gea raw package delete-package");
    let (code, _) = inst.api("GET", &format!("packages/{}/generic/{pkg}/1.1.0", inst.user), None);
    assert_eq!(code, 404, "a version survived deleting the whole package");
}

/// The package porcelain's whole value is the inference: `gea package view NAME` works out the
/// owner, the registry type and the version for you.
///
/// That inference is three extra round trips a mock decides the answers to. Here it is driven
/// against a registry whose contents the test put there, so an inference that picks the wrong
/// package shows up as the wrong version rather than as a passing test.
///
/// Two versions are uploaded deliberately: with only one, "infer the version" and "take the
/// only thing you found" are indistinguishable, and the naming below asserts which happened.
#[test]
fn the_package_porcelain_infers_owner_type_and_version_from_the_registry() {
    let inst = instance_or_skip!();
    cover!(
        porcelain: ["package list", "package view", "package files", "package delete"],
        hits: ["listPackages", "getPackage", "listPackageFiles", "deletePackage"],
    );

    let pkg = unique_package_name(inst, "porcpkg");
    upload_generic_package(inst, &inst.user, &pkg, "1.0.0", "one.txt", "first\n");
    upload_generic_package(inst, &inst.user, &pkg, "2.0.0", "two.txt", "second payload\n");

    let listed = inst.gea(["package", "list", "--limit", "200", "--json", "name,version"]);
    listed.assert_ok("gea package list");
    let rows = listed.json();
    let mine: Vec<&serde_json::Value> = rows
        .as_array()
        .expect("package list --json is an array")
        .iter()
        .filter(|r| r["name"].as_str() == Some(pkg.as_str()))
        .collect();
    assert_eq!(mine.len(), 2, "both uploaded versions should be listed: {rows}");

    // An explicit version and type: this is the path that reaches `getPackage`, where the
    // inferring path resolves everything out of the listing instead.
    let view = inst.gea([
        "package",
        "view",
        &pkg,
        "2.0.0",
        "--type",
        "generic",
        "--owner",
        &inst.user,
        "--json",
        "name,version,type",
    ]);
    view.assert_ok("gea package view with an explicit version");
    let v = view.json();
    assert_eq!(v["version"], "2.0.0", "view resolved the wrong version: {v}");
    assert_eq!(v["type"], "generic");

    let files = inst.gea(["package", "files", &pkg, "2.0.0", "--json", "name,size"]);
    files.assert_ok("gea package files");
    let files = files.json();
    assert_eq!(
        files[0]["name"], "two.txt",
        "`package files` resolved the wrong version's files: {files}"
    );

    inst.gea(["package", "delete", &pkg, "1.0.0", "--yes"]).assert_ok("gea package delete");
    // Out of band, and asserting on *both* versions: a delete that removed the whole package
    // rather than the named version would still leave 1.0.0 gone.
    let (gone, _) = inst.api("GET", &format!("packages/{}/generic/{pkg}/1.0.0", inst.user), None);
    let (kept, _) = inst.api("GET", &format!("packages/{}/generic/{pkg}/2.0.0", inst.user), None);
    assert_eq!(gone, 404, "`package delete 1.0.0` left the version behind");
    assert_eq!(kept, 200, "`package delete 1.0.0` took 2.0.0 with it");

    let _ = inst.api("DELETE", &format!("packages/{}/generic/{pkg}/2.0.0", inst.user), None);
}

// ---------------------------------------------------------------------------------------------
// Settings
// ---------------------------------------------------------------------------------------------

/// The four `/settings/*` documents, each checked for a field the spec says is there.
///
/// These are the endpoints whose whole content is the server's own configuration, so a mock
/// test of them is a tautology. The specific risk they guard is a field being renamed or moved
/// by a Gitea release: `max_response_items` in particular is what every `--limit` in `gea` is
/// silently clamped to, so losing it degrades pagination everywhere without an error.
#[test]
fn the_four_settings_documents_carry_the_fields_the_spec_names() {
    let inst = instance_or_skip!();
    cover!(raw: [
        "getGeneralAPISettings",
        "getGeneralAttachmentSettings",
        "getGeneralRepositorySettings",
        "getGeneralUISettings",
    ]);

    let api = inst.gea(["raw", "settings", "get-general-api-settings"]);
    api.assert_ok("gea raw settings get-general-api-settings");
    let api = api.json();
    assert!(
        api["max_response_items"].as_u64().is_some_and(|n| n > 0),
        "every --limit is clamped to max_response_items, so it must be a positive number: {api}"
    );
    assert!(api["default_paging_num"].as_u64().is_some(), "{api}");

    let att = inst.gea(["raw", "settings", "get-general-attachment-settings"]);
    att.assert_ok("gea raw settings get-general-attachment-settings");
    let att = att.json();
    assert!(att["enabled"].is_boolean(), "attachment settings must say whether they are on: {att}");
    assert!(
        att["allowed_types"].as_str().is_some_and(|s| s.contains(".png")),
        "the default allow-list should mention a common type: {att}"
    );

    let repo = inst.gea(["raw", "settings", "get-general-repository-settings"]);
    repo.assert_ok("gea raw settings get-general-repository-settings");
    let repo = repo.json();
    assert!(repo["mirrors_disabled"].is_boolean(), "{repo}");
    assert!(repo["http_git_disabled"].is_boolean(), "{repo}");

    let ui = inst.gea(["raw", "settings", "get-general-ui-settings"]);
    ui.assert_ok("gea raw settings get-general-ui-settings");
    let ui = ui.json();
    assert!(
        ui["default_theme"].as_str().is_some_and(|s| !s.is_empty()),
        "the UI settings must name a default theme: {ui}"
    );
    assert!(
        ui["allowed_reactions"].as_array().is_some_and(|a| !a.is_empty()),
        "the reaction list is what `gea issue react` validates against: {ui}"
    );
}

/// `gea nodeinfo limits` exists so a user can find the page-size ceiling without reading the
/// swagger. It must report the server's number, not one baked into `gea`.
///
/// Cross-checked against the raw document out of band: the porcelain formats
/// `default_max_blob_size` as `10 MiB`, and a formatter that ignored its input entirely would
/// still print something plausible.
#[test]
fn nodeinfo_limits_reports_the_servers_own_page_size_ceiling() {
    let inst = instance_or_skip!();
    cover!(porcelain: ["nodeinfo limits"], hits: ["getGeneralAPISettings"]);

    let (code, raw) = inst.api("GET", "settings/api", None);
    assert_eq!(code, 200, "{raw}");
    let raw: serde_json::Value = serde_json::from_str(&raw).expect("the API settings document");
    let ceiling = raw["max_response_items"].as_u64().expect("a numeric max_response_items");

    let run = inst.gea(["nodeinfo", "limits"]);
    run.assert_ok("gea nodeinfo limits");
    run.assert_says("max_response_items");
    run.assert_says(&ceiling.to_string());
}

/// `gea nodeinfo` identifies the server from `/version` — which a real Gitea answers without a
/// NodeInfo document, unlike Forgejo's discovery route — and must not attach the non-Gitea note to
/// a plain Gitea.
#[test]
fn nodeinfo_identifies_a_plain_gitea_by_its_version_and_does_not_warn() {
    let inst = instance_or_skip!();
    // No `cover!`: `gea nodeinfo` is a group whose one inventoried leaf is `nodeinfo limits`
    // (covered above), and `getVersion` is covered through `raw` below.

    let (code, raw) = inst.api("GET", "version", None);
    assert_eq!(code, 200, "{raw}");
    let raw: serde_json::Value = serde_json::from_str(&raw).expect("the version document");
    let version = raw["version"].as_str().expect("a version string").to_owned();
    assert!(!version.contains("+gitea-"), "the test instance is Gitea, not Forgejo: {version}");

    // Piped output is one TSV line: software, version, max_response_items.
    let run = inst.gea(["nodeinfo"]);
    run.assert_ok("gea nodeinfo");
    let line = run.stdout.lines().next().unwrap_or_default().to_owned();
    let cols: Vec<&str> = line.split('\t').collect();
    assert_eq!(cols.first().copied(), Some("gitea"), "{line}");
    assert_eq!(cols.get(1).copied(), Some(version.as_str()), "{line}");
    assert!(!run.stderr.contains("Forgejo"), "a plain Gitea was warned about: {}", run.stderr);

    let json = inst.gea(["nodeinfo", "--json", "version"]);
    json.assert_ok("gea nodeinfo --json version");
    assert_eq!(json.json()["version"], serde_json::json!(version));
}

// ---------------------------------------------------------------------------------------------
// Templates, version, signing keys, markup
// ---------------------------------------------------------------------------------------------

/// Each template catalogue lists names, and each listed name must be fetchable.
///
/// The bug this prevents is a listing whose entries cannot be used: `/licenses` returns objects
/// with a `key`, `/gitignore/templates` returns bare strings, and `/label/templates` returns
/// bare strings too. Fetching an entry taken *from the listing* is what proves the two halves
/// agree — a mock would have used whatever identifier the test author guessed.
#[test]
fn every_template_catalogue_lists_names_that_can_be_fetched_back_by_name() {
    let inst = instance_or_skip!();
    cover!(raw: [
        "listGitignoresTemplates",
        "getGitignoreTemplateInfo",
        "listLicenseTemplates",
        "getLicenseTemplateInfo",
        "listLabelTemplates",
        "getLabelTemplateInfo",
    ]);

    let gitignores = inst.gea(["raw", "misc", "list-gitignores-templates"]);
    gitignores.assert_ok("gea raw misc list-gitignores-templates");
    let gitignores = gitignores.json();
    let names: Vec<&str> =
        gitignores.as_array().expect("an array").iter().filter_map(|v| v.as_str()).collect();
    assert!(
        names.contains(&"Rust"),
        "a Gitea ships hundreds of gitignore templates and Rust is one of them: {names:?}"
    );
    let one = inst.gea(["raw", "misc", "get-gitignore-template-info", "Rust"]);
    one.assert_ok("gea raw misc get-gitignore-template-info");
    let one = one.json();
    assert_eq!(one["name"], "Rust", "the fetched template is not the one asked for: {one}");
    assert!(
        one["source"].as_str().is_some_and(|s| s.contains("target")),
        "a gitignore template must carry its own text: {one}"
    );

    let licenses = inst.gea(["raw", "misc", "list-license-templates", "--limit", "500"]);
    licenses.assert_ok("gea raw misc list-license-templates");
    let licenses = licenses.json();
    let key = licenses
        .as_array()
        .expect("an array")
        .iter()
        .find_map(|l| l["key"].as_str())
        .expect("the licence catalogue is never empty")
        .to_owned();
    let one = inst.gea(["raw", "misc", "get-license-template-info", &key]);
    one.assert_ok("gea raw misc get-license-template-info");
    let one = one.json();
    assert_eq!(one["key"], key.as_str(), "the fetched licence is not the one listed: {one}");
    assert!(
        one["body"].as_str().is_some_and(|b| !b.is_empty()),
        "a licence template must carry its text: {one}"
    );

    let labels = inst.gea(["raw", "misc", "list-label-templates"]);
    labels.assert_ok("gea raw misc list-label-templates");
    let labels = labels.json();
    let set = labels
        .as_array()
        .expect("an array")
        .iter()
        .find_map(serde_json::Value::as_str)
        .expect("at least one label template")
        .to_owned();
    let one = inst.gea(["raw", "misc", "get-label-template-info", &set]);
    one.assert_ok("gea raw misc get-label-template-info");
    let one = one.json();
    let first = &one[0];
    assert!(
        first["name"].as_str().is_some_and(|n| !n.is_empty()),
        "a label template entry must have a name: {one}"
    );
    assert!(
        first["color"].as_str().is_some_and(|c| c.len() == 6),
        "Gitea returns label colours as bare six-digit hex, with no leading '#': {one}"
    );
}

/// `/version`, `/signing-key.gpg` and `/signing-key.pub` — the instance-level reads that answer
/// outside JSON when they succeed.
///
/// A default container has no signing key of either kind, and Gitea's handler answers both with
/// a `404` carrying "no signing key" rather than an empty body. That is a documented answer from
/// a registered route, so the assertion is on the exit code and the request path rather than on
/// the server's wording, per this repository's rule about error strings. `getVersion` is the
/// JSON control.
#[test]
fn the_version_and_signing_key_endpoints_answer_outside_json() {
    let inst = instance_or_skip!();
    cover!(raw: ["getVersion", "getSigningKey", "getSigningKeySSH"]);

    let version = inst.gea(["raw", "misc", "get-version"]);
    version.assert_ok("gea raw misc get-version");
    let version = version.json();
    assert!(
        version["version"].as_str().is_some_and(|v| v.contains('.')),
        "the version endpoint must report a dotted version: {version}"
    );

    for (command, path) in
        [("get-signing-key", "/signing-key.gpg"), ("get-signing-key-ssh", "/signing-key.pub")]
    {
        let key = inst.gea(["raw", "misc", command]);
        key.assert_code(5, &format!("gea raw misc {command} without a configured key"));
        key.assert_says(path);
    }
}

/// The three markup renderers, each asked to turn a heading into HTML.
///
/// # Why `render-markdown-raw` is asserted more weakly than the other two
///
/// Its request body is `text/plain`, and `gea raw` has no way to send one. `--body-file` runs
/// every body through `serde_json::from_str` (`crates/gea-raw/src/bodyfile.rs`), the plan
/// carries a `serde_json::Value`, and the serializer emits JSON regardless of the declared
/// content type. So the only body reachable from the command line is a **JSON-encoded string**:
/// `--body-file` holding `"# x"` sends the five bytes `"# x"` — quotes included — under
/// `content-type: text/plain`, and Gitea faithfully renders the quotes.
///
/// That is a real defect, found here and not by any mock: a `FakeTransport` test asserts that
/// the body is the `Value` we built, which it is. The test therefore asserts what is true today
/// — the endpoint answers with HTML containing the payload — and this comment is the record of
/// what it *should* assert once a plain-text body can be sent: `# x` becoming an `<h1>`, exactly
/// as `render-markdown` does below.
#[test]
fn the_markup_renderers_turn_a_heading_into_real_html() {
    let inst = instance_or_skip!();
    cover!(raw: ["renderMarkdown", "renderMarkup", "renderMarkdownRaw"]);

    let md =
        inst.gea(["raw", "misc", "render-markdown", "--text", "# hello", "--mode", "markdown"]);
    md.assert_ok("gea raw misc render-markdown");
    assert!(
        md.stdout.contains("<h1") && md.stdout.contains("hello"),
        "`# hello` must render as an h1, not be echoed back: {:?}",
        md.stdout
    );

    let markup = inst.gea([
        "raw",
        "misc",
        "render-markup",
        "--text",
        "# heading",
        "--mode",
        "markdown",
        "--context",
        "/",
    ]);
    markup.assert_ok("gea raw misc render-markup");
    assert!(
        markup.stdout.contains("<h1") && markup.stdout.contains("heading"),
        "render-markup in markdown mode must produce the same h1 render-markdown does: {:?}",
        markup.stdout
    );

    // See the doc comment: a JSON string is the only body shape `gea raw` can produce, so the
    // assertion is "HTML came back carrying the payload", not "the markdown was interpreted".
    let scratch = std::env::temp_dir().join(format!("gea-itest-rawmd-{}.json", std::process::id()));
    std::fs::write(&scratch, "\"raw renderer payload\"").expect("write the body file");
    let raw =
        inst.gea(["raw", "misc", "render-markdown-raw", "--body-file", &scratch.to_string_lossy()]);
    let _ = std::fs::remove_file(&scratch);
    raw.assert_ok("gea raw misc render-markdown-raw");
    assert!(
        raw.stdout.contains("<p") && raw.stdout.contains("raw renderer payload"),
        "the raw renderer must answer with HTML carrying the text it was given: {:?}",
        raw.stdout
    );
}

// ---------------------------------------------------------------------------------------------
// Topics
// ---------------------------------------------------------------------------------------------

/// A topic attached to a repository must be findable through the instance-wide topic search,
/// through both `gea raw topic search` and `gea search topics`.
///
/// The topic is attached out of band, so this tests the *search*, not the writing. `repo_count`
/// is the assertion that matters: the search index is populated asynchronously in some Gitea
/// configurations, and a search that returned a topic with no repositories behind it would mean
/// the index and the repository disagree.
#[test]
fn a_topic_on_a_repository_is_findable_through_the_instance_wide_topic_search() {
    let inst = instance_or_skip!();
    cover!(raw: ["topicSearch"]);
    cover!(porcelain: ["search topics"], hits: ["topicSearch"]);

    let repo = TestRepo::create(inst, "topicsearch");
    // Gitea topics are lowercase, and this one has to be unique across the instance so a
    // parallel test's topic cannot satisfy the assertions below.
    let topic = format!("geaitest{}t", std::process::id());
    let (code, body) = repo.api("PUT", &format!("topics/{topic}"), None);
    assert!((200..300).contains(&code), "could not attach the topic: HTTP {code}: {body}");

    let raw = inst.gea(["raw", "topic", "search", "--q", &topic]);
    raw.assert_ok("gea raw topic search");
    let found = raw.json();
    let hit = found["topics"]
        .as_array()
        .expect("topicSearch answers with a `topics` array")
        .iter()
        .find(|t| t["topic_name"].as_str() == Some(topic.as_str()))
        .unwrap_or_else(|| panic!("the topic just attached is not in the search results: {found}"));
    assert_eq!(
        hit["repo_count"].as_u64(),
        Some(1),
        "the search found the topic but claims no repository carries it: {found}"
    );

    let porcelain = inst.gea(["search", "topics", &topic, "--json", "topic_name,repo_count"]);
    porcelain.assert_ok("gea search topics");
    let rows = porcelain.json();
    assert!(
        rows.as_array()
            .expect("search topics --json is an array")
            .iter()
            .any(|t| t["topic_name"].as_str() == Some(topic.as_str())),
        "the porcelain search did not find the topic the raw search did: {rows}"
    );
}

// ---------------------------------------------------------------------------------------------
// Notifications
// ---------------------------------------------------------------------------------------------

/// All seven notification operations, walked as one story: somebody else files issues, the
/// inbox fills, one thread is read individually, the rest of the repository is marked read, and
/// finally the account-wide mark clears what is left.
///
/// # Why this is one test rather than seven
///
/// `notifyReadList` is `PUT /notifications` with no scope — it marks **every** unread thread on
/// the account. Two tests doing that in parallel against the shared instance would each destroy
/// the other's fixture. Keeping the whole flow in one test makes the account-wide mark the last
/// thing that happens, with nothing left to race.
///
/// # Why a second account
///
/// Gitea does not notify you about your own actions, so an admin-only test of this would be a
/// test of an empty list. The repository is made public so the second account can file issues
/// in it without a collaborator grant.
///
/// # Why the polling
///
/// Notifications are written from a queue, so they are not there the instant the issue is
/// created — measured at over a second on an idle container. A fixed sleep is either flaky or
/// slow; polling is neither.
#[test]
fn the_notification_endpoints_walk_a_thread_from_unread_to_read() {
    const THREADS: usize = 3;

    let inst = instance_or_skip!();
    cover!(raw: [
        "notifyGetList",
        "notifyGetRepoList",
        "notifyGetThread",
        "notifyNewAvailable",
        "notifyReadList",
        "notifyReadRepoList",
        "notifyReadThread",
    ]);

    let reporter = inst.scoped_user("miscnotifier", &["all"]).expect(
        "a second account is needed to fill the inbox: Gitea never notifies you \
                 about your own actions",
    );
    let repo = TestRepo::create_initialized(inst, "notify");
    repo.api("PATCH", "", Some(r#"{"private":false}"#));

    for i in 1..=THREADS {
        let (code, body) = inst.api_as(
            &reporter.token,
            "POST",
            &format!("repos/{}/issues", repo.slug()),
            Some(&format!(r#"{{"title":"notify fixture {i}"}}"#)),
        );
        assert!((200..300).contains(&code), "seeding issue {i} failed: HTTP {code}: {body}");
    }

    let repo_threads = || -> Vec<serde_json::Value> {
        let run = inst.gea([
            "raw",
            "notify",
            "get-repo-list",
            &repo.owner,
            &repo.name,
            "--status-types",
            "unread",
        ]);
        run.assert_ok("gea raw notify get-repo-list");
        run.json().as_array().cloned().unwrap_or_default()
    };

    let mut waited = 0;
    while repo_threads().len() < THREADS && waited < 60 {
        std::thread::sleep(std::time::Duration::from_millis(500));
        waited += 1;
    }
    let threads = repo_threads();
    assert_eq!(
        threads.len(),
        THREADS,
        "the instance never delivered {THREADS} notifications, so everything below would have \
         been asserted against an empty inbox"
    );
    assert!(
        threads.iter().all(|t| t["subject"]["type"] == "Issue"),
        "every seeded thread is about an issue: {threads:?}"
    );

    let available = inst.gea(["raw", "notify", "new-available"]);
    available.assert_ok("gea raw notify new-available");
    assert!(
        available.json()["new"].as_u64().is_some_and(|n| n >= THREADS as u64),
        "notifications/new must count at least the {THREADS} threads just seen: {}",
        available.stdout
    );

    let global =
        inst.gea(["raw", "notify", "get-list", "--status-types", "unread", "--limit", "100"]);
    global.assert_ok("gea raw notify get-list");
    let global = global.json();
    let ids: Vec<i64> =
        global.as_array().expect("an array").iter().filter_map(|t| t["id"].as_i64()).collect();
    for t in &threads {
        let id = t["id"].as_i64().expect("a thread id");
        assert!(ids.contains(&id), "thread {id} is in the repository list but not the global one");
    }

    let first = threads[0]["id"].as_i64().expect("a thread id").to_string();
    let thread = inst.gea(["raw", "notify", "get-thread", &first]);
    thread.assert_ok("gea raw notify get-thread");
    let thread = thread.json();
    assert_eq!(thread["unread"], serde_json::Value::Bool(true), "a fresh thread is unread");
    assert_eq!(
        thread["repository"]["full_name"].as_str(),
        Some(repo.slug().as_str()),
        "get-thread returned a thread belonging to another repository: {thread}"
    );

    inst.gea(["raw", "notify", "read-thread", &first]).assert_ok("gea raw notify read-thread");
    // Out of band: the PATCH answers with the thread it changed, so reading its own reply back
    // would prove nothing about what was stored.
    let (code, after) = inst.api("GET", &format!("notifications/threads/{first}"), None);
    assert_eq!(code, 200, "{after}");
    let after: serde_json::Value = serde_json::from_str(&after).expect("a thread");
    assert_eq!(
        after["unread"],
        serde_json::Value::Bool(false),
        "read-thread reported success but the thread is still unread: {after}"
    );

    inst.gea(["raw", "notify", "read-repo-list", &repo.owner, &repo.name])
        .assert_ok("gea raw notify read-repo-list");
    let (_, left) =
        inst.api("GET", &format!("repos/{}/notifications?status-types=unread", repo.slug()), None);
    let left: serde_json::Value = serde_json::from_str(&left).unwrap_or_default();
    assert_eq!(
        left.as_array().map(Vec::len),
        Some(0),
        "read-repo-list left unread threads behind: {left}"
    );

    // Account-wide, and therefore last. Nothing else in this file creates notifications for the
    // admin — every other test acts as the admin, and Gitea does not notify you about your
    // own actions — so this cannot pull a fixture out from under a parallel test.
    inst.gea(["raw", "notify", "read-list"]).assert_ok("gea raw notify read-list");
    let (_, new) = inst.api("GET", "notifications/new", None);
    let new: serde_json::Value = serde_json::from_str(&new).expect("a counter");
    assert_eq!(
        new["new"].as_u64(),
        Some(0),
        "read-list reported success but unread notifications remain: {new}"
    );
}

// ---------------------------------------------------------------------------------------------
// Mirrors
// ---------------------------------------------------------------------------------------------

/// A remote address that Gitea will accept but nothing will ever answer.
///
/// Gitea runs `IsMigrateURLAllowed` over the address, and its default allow-list is "external"
/// — which excludes loopback, RFC 1918 **and** the RFC 5737 documentation ranges. So TEST-NET-2,
/// the natural choice for an address that can never answer, is refused with `401 Permission
/// denied`: a 401 about the *mirror target* that reads exactly like a bad token. A real hostname
/// would need DNS, and the container has no outbound network guarantee.
///
/// The harness sets `[migrations] ALLOW_LOCALNETWORKS = true` (see `crates/gea-itest/src/lib.rs`),
/// which puts loopback on the allow-list. Port 9 is `discard`, closed in the container, so a sync
/// attempt is refused immediately and no packet ever leaves it.
const UNROUTABLE_MIRROR: &str = "https://127.0.0.1:9/example/mirror.git";

/// A push mirror survives a round trip: added with options, listed, shown by `status`, removed.
///
/// The interval is the part worth checking against a real server: `--interval 8h` has to arrive
/// as Gitea's `8h0m0s`, and a mock would have accepted whatever shape the command sent. Everything is read back with `repo.api`, because
/// `mirror add` answers with the object it created and believing that reply would be believing
/// the code under test.
#[test]
fn a_push_mirror_keeps_its_interval_through_a_round_trip() {
    let inst = instance_or_skip!();
    cover!(
        porcelain: ["mirror add", "mirror list", "mirror status", "mirror delete"],
        hits: ["repoAddPushMirror", "repoListPushMirrors", "repoDeletePushMirror", "repoGet"],
    );

    let repo = TestRepo::create_initialized(inst, "mirror");

    inst.gea(["mirror", "add", UNROUTABLE_MIRROR, "--interval", "8h", "-R", &repo.slug()])
        .assert_ok("gea mirror add");

    let (code, body) = repo.api("GET", "push_mirrors", None);
    assert_eq!(code, 200, "{body}");
    let mirrors: serde_json::Value = serde_json::from_str(&body).expect("a mirror list");
    let stored = &mirrors[0];
    assert_eq!(stored["remote_address"], UNROUTABLE_MIRROR, "the address was rewritten: {body}");
    assert_eq!(
        stored["interval"], "8h0m0s",
        "`--interval 8h` must reach the server as Gitea's own duration spelling: {body}"
    );
    let remote = stored["remote_name"].as_str().expect("a remote name").to_owned();

    let listed = inst.gea(["mirror", "list", "-R", &repo.slug(), "--json", "remote_name,interval"]);
    listed.assert_ok("gea mirror list");
    let listed = listed.json();
    assert_eq!(
        listed[0]["remote_name"].as_str(),
        Some(remote.as_str()),
        "`mirror list` does not show the mirror that was just added: {listed}"
    );

    let status = inst.gea(["mirror", "status", "-R", &repo.slug()]);
    status.assert_ok("gea mirror status");
    status.assert_says(&remote);
    status.assert_says("127.0.0.1:9");

    inst.gea(["mirror", "delete", &remote, "--yes", "-R", &repo.slug()])
        .assert_ok("gea mirror delete");
    let (_, after) = repo.api("GET", "push_mirrors", None);
    let after: serde_json::Value = serde_json::from_str(&after).unwrap_or_default();
    assert_eq!(
        after.as_array().map(Vec::len),
        Some(0),
        "`mirror delete` reported success but the mirror is still configured: {after}"
    );
}

/// `gea mirror sync` decides which of two endpoints to call by reading the repository first,
/// and refuses when neither applies.
///
/// # Why `--push` on a repository with no push mirrors
///
/// `POST …/push_mirrors-sync` is **synchronous** in Gitea: with a mirror configured it runs
/// the push inline, and against an address nothing answers that is 132 seconds of TCP timeout —
/// measured — which is past this crate's 180s per-test kill and would report as an anonymous
/// hang rather than as anything about mirrors. With no mirrors configured the same endpoint
/// answers in milliseconds, so this drives the real route without buying the stall. What is
/// being tested is the routing decision and the request, which is the part `gea` owns.
///
/// The second half is the refusal: without a flag, a repository that is neither a pull mirror
/// nor has push mirrors must be told so as a usage error, not sent to an endpoint that would
/// quietly do nothing.
#[test]
fn mirror_sync_reaches_the_push_endpoint_and_refuses_when_there_is_no_mirror_at_all() {
    let inst = instance_or_skip!();
    cover!(
        porcelain: ["mirror sync"],
        hits: ["repoPushMirrorSync", "repoListPushMirrors", "repoGet"],
    );

    let repo = TestRepo::create_initialized(inst, "mirrorsync");

    // `GEA_FORCE_TTY` because the confirmation is a `porcelain::note`, which is deliberately
    // terminal-only: piped output carries data, not chatter. Without it this test would have to
    // settle for the exit code, and exit 0 alone cannot tell "the POST was made" apart from "the
    // command decided there was nothing to do and said so quietly".
    let forced = inst.gea_env(
        std::path::Path::new("."),
        &[("GEA_FORCE_TTY", "1")],
        ["mirror", "sync", "--push", "-R", &repo.slug()],
    );
    forced.assert_ok("gea mirror sync --push");
    forced.assert_says("Queued a push");

    let refused = inst.gea(["mirror", "sync", "-R", &repo.slug()]);
    refused.assert_code(2, "gea mirror sync on a repository with no mirrors");
    refused.assert_says("no mirrors to sync");

    let pull_only = inst.gea(["mirror", "sync", "--pull", "-R", &repo.slug()]);
    pull_only.assert_code(2, "gea mirror sync --pull on a repository that is not a pull mirror");
    pull_only.assert_says("not a pull mirror");
}
