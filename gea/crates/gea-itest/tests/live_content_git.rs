//! Repository contents, git plumbing, releases, wikis, webhooks, deploy keys and git hooks,
//! driven against a real Gitea.
//!
//! # Why this file exists rather than more mock tests
//!
//! Three classes of defect live here and none of them is reachable with a `FakeTransport`:
//!
//! * **Path encoding.** `GET /repos/{owner}/{repo}/contents/{filepath}` is called with values
//!   like `src/deep/nested/main.rs`. `gitea_client::meta_types::PathEncoding` exists solely
//!   to keep that `/` unencoded, and its doc comment says why: `src%2Fmain.rs` is a 404 for
//!   every nested file in every repository. A mock answers whatever URL it is handed, so it
//!   confirms the encoder agrees with itself. Only a server can say the URL was right.
//! * **Semantics the specification does not state.** `GET /commits/{sha}/pull` reads as "the
//!   pull request for this commit"; the server means "the pull request this *merge commit*
//!   closed", and answers 404 for the head commit of an open one. The spec says neither.
//! * **Whether an operation is reachable at all.** The git-hook endpoints are behind
//!   `DISABLE_GIT_HOOKS`, which Gitea ships switched on. A mock would show four working
//!   commands; the server shows four 403s.
//!
//! # Byte-producing operations
//!
//! `repoGetArchive`, `repoGetRawFile` and `repoGetRawFileOrLFS` are `Produces::Bytes`: streamed,
//! and refused to a terminal without `--output` (`gea::output::guard_binary`). The refusal is
//! keyed on the response's media type *and* on there being a terminal, so a test asserting it
//! has to ask for one with `GEA_FORCE_TTY` — under plain `cargo test` stdout is a pipe and
//! nothing is refused.

use std::path::{Path, PathBuf};

use gea_itest::{TestRepo, cover, instance_or_skip};

// ------------------------------------------------------------------------------- fixtures

/// A scratch directory that cleans up after itself, for the tests that write files.
///
/// Named per test as well as per process: these run on parallel threads against one container,
/// so two tests sharing a directory would race over the same asset filenames.
struct Scratch(PathBuf);

impl Scratch {
    fn new(tag: &str) -> Self {
        let d = std::env::temp_dir().join(format!("gea-itest-cg-{}-{tag}", std::process::id()));
        let _ = std::fs::remove_dir_all(&d);
        std::fs::create_dir_all(&d).expect("create the scratch directory");
        Self(d)
    }
    fn join(&self, name: &str) -> PathBuf {
        self.0.join(name)
    }
}

impl Drop for Scratch {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.0);
    }
}

/// Standard, padded base64 — what `contents`, `git/blobs` and `wiki` all speak.
///
/// Hand-rolled because `gea-itest` has no base64 dependency and cannot grow one: this file is
/// written under an isolation rule that forbids touching `Cargo.toml`. Fourteen lines is a
/// cheaper price than a dependency, and `gitea-core` reached the same conclusion for the
/// same reason.
fn b64(input: &[u8]) -> String {
    const ALPHABET: &[u8] = b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789+/";
    let mut out = String::with_capacity(input.len().div_ceil(3) * 4);
    for chunk in input.chunks(3) {
        let b = [chunk[0], *chunk.get(1).unwrap_or(&0), *chunk.get(2).unwrap_or(&0)];
        let n = u32::from(b[0]) << 16 | u32::from(b[1]) << 8 | u32::from(b[2]);
        for i in 0..4 {
            if i <= chunk.len() {
                out.push(ALPHABET[(n >> (18 - 6 * i) & 0x3f) as usize] as char);
            } else {
                out.push('=');
            }
        }
    }
    out
}

/// A deploy key generated once for this file.
///
/// A literal rather than a call to `ssh-keygen`: Gitea only has to accept the armored text,
/// the private half is worthless (it opens a throwaway container's private repository and
/// nothing else), and not shelling out keeps the test independent of what the CI image ships.
const DEPLOY_KEY: &str = "ssh-ed25519 AAAAC3NzaC1lZDI1NTE5AAAAII4biqlvq1jAyhaWDAhkslndl15r0e2xcWbjK6h5CPrj \
     gea-itest-content-git";

/// Commit `contents` at `path` on the default branch, returning the commit sha.
///
/// Goes through `gea raw repo create-file` rather than the API so that the operation under
/// test is the one doing the seeding — a fixture that used `curl` would leave `repoCreateFile`
/// exercised only by the one test that names it.
fn seed_file(
    inst: &gea_itest::Instance,
    repo: &TestRepo<'_>,
    path: &str,
    contents: &str,
) -> String {
    let run = inst.gea([
        "raw",
        "repo",
        "create-file",
        &repo.owner,
        &repo.name,
        path,
        "--content",
        &b64(contents.as_bytes()),
        "--message",
        &format!("add {path}"),
        "--branch",
        "main",
    ]);
    run.assert_ok(&format!("gea raw repo create-file {path}"));
    run.json()["commit"]["sha"]
        .as_str()
        .unwrap_or_else(|| panic!("create-file should report a commit sha: {}", run.stdout))
        .to_owned()
}

// --------------------------------------------------------------- contents and path encoding

/// The single highest-value assertion in this file: a file at a nested path is reachable, by
/// that path, through every route that takes one.
///
/// `PathEncoding::PathLike` exists for exactly these four operations, and getting it wrong is
/// not a subtle degradation — `src%2Fdeep%2Fnested%2Fmain.rs` is a 404 for every nested file in
/// every repository, which is nearly every file anybody stores. A `FakeTransport` cannot catch
/// it because the mock is handed the encoded URL and answers it regardless; the encoder and the
/// assertion would agree with each other and both be wrong.
///
/// Three levels deep on purpose. One `/` could be passed by an encoder that special-cases the
/// first separator, and the directory listing below (`get-contents` on `src/deep`) only
/// distinguishes a real directory walk from a lucky prefix match when there is something under
/// it.
#[test]
fn a_nested_file_is_reachable_by_its_slashed_path_through_contents_raw_and_media() {
    let inst = instance_or_skip!();
    cover!(raw: [
        "repoCreateFile",
        "repoGetContents",
        "repoGetContentsList",
        "repoGetRawFile",
        "repoGetRawFileOrLFS",
        "repoUpdateFile",
        "repoDeleteFile"
    ]);
    let repo = TestRepo::create_initialized(inst, "contents-nested");
    let scratch = Scratch::new("contents");
    let path = "src/deep/nested/main.rs";
    let body = "fn main() { println!(\"deep\"); }\n";

    seed_file(inst, &repo, path, body);

    // 1. contents/{filepath}: the metadata route.
    let got = inst.gea(["raw", "repo", "get-contents", &repo.owner, &repo.name, path]);
    got.assert_ok("gea raw repo get-contents on a nested path");
    let v = got.json();
    assert_eq!(v["path"], path, "the server echoed a different path than we asked for");
    assert_eq!(v["type"], "file");
    assert_eq!(
        v["content"].as_str(),
        Some(b64(body.as_bytes()).as_str()),
        "the nested file's contents did not survive the round trip"
    );

    // 2. contents/{filepath} on a *directory*, which is the same route answering with an array
    //    instead of an object. Both depths are asked for, because the listing is one level deep:
    //    `src/deep` must show the directory under it and not the file two levels down.
    let listing = |p: &str| -> Vec<String> {
        let run = inst.gea(["raw", "repo", "get-contents", &repo.owner, &repo.name, p]);
        run.assert_ok(&format!("gea raw repo get-contents on the directory {p}"));
        run.json()
            .as_array()
            .unwrap_or_else(|| panic!("a directory answers with an array: {}", run.stdout))
            .iter()
            .map(|e| e["path"].as_str().unwrap_or_default().to_owned())
            .collect()
    };
    assert_eq!(
        listing("src/deep"),
        vec!["src/deep/nested".to_owned()],
        "a directory listing is one level deep, and its entries carry full paths"
    );
    assert_eq!(
        listing("src/deep/nested"),
        vec![path.to_owned()],
        "the deepest directory did not list the file in it"
    );

    // 3. raw/{filepath} and media/{filepath}: the two byte routes. Both are `Produces::Bytes`,
    //    so both are driven with `--output` and the bytes compared, not the exit code.
    for (op, name) in [("get-raw-file", "raw.txt"), ("get-raw-file-or-lfs", "media.txt")] {
        let out = scratch.join(name);
        inst.gea([
            "raw",
            "repo",
            op,
            &repo.owner,
            &repo.name,
            path,
            "--output",
            &out.to_string_lossy(),
        ])
        .assert_ok(&format!("gea raw repo {op} --output"));
        let bytes = std::fs::read(&out).expect("the streamed file should exist");
        assert_eq!(
            String::from_utf8_lossy(&bytes),
            body,
            "{op} streamed different bytes than were committed"
        );
    }

    // 4. The root listing, which takes no filepath at all, so it is the control that proves the
    //    nesting above came from the parameter rather than from the repository being flat.
    let root = inst.gea(["raw", "repo", "get-contents-list", &repo.owner, &repo.name]);
    root.assert_ok("gea raw repo get-contents-list");
    let names: Vec<String> = root
        .json()
        .as_array()
        .expect("the root listing is an array")
        .iter()
        .map(|e| e["path"].as_str().unwrap_or_default().to_owned())
        .collect();
    assert!(names.contains(&"src".to_owned()), "the root listing lost src: {names:?}");
    assert!(
        !names.contains(&path.to_owned()),
        "the root listing is not recursive, so it must not contain {path}: {names:?}"
    );

    // 5. PUT and DELETE take the same slashed filepath, and both need the blob sha, so this is
    //    also the check that `get-contents` handed back a sha the server will accept back.
    let sha = v["sha"].as_str().expect("a file has a sha").to_owned();
    let updated = "fn main() { println!(\"deeper\"); }\n";
    inst.gea([
        "raw",
        "repo",
        "update-file",
        &repo.owner,
        &repo.name,
        path,
        "--content",
        &b64(updated.as_bytes()),
        "--sha",
        &sha,
        "--message",
        "update the nested file",
    ])
    .assert_ok("gea raw repo update-file on a nested path");

    let after = inst.gea(["raw", "repo", "get-contents", &repo.owner, &repo.name, path]);
    after.assert_ok("gea raw repo get-contents after the update");
    let after = after.json();
    assert_eq!(
        after["content"].as_str(),
        Some(b64(updated.as_bytes()).as_str()),
        "the update did not reach the nested path"
    );

    inst.gea([
        "raw",
        "repo",
        "delete-file",
        &repo.owner,
        &repo.name,
        path,
        "--sha",
        after["sha"].as_str().expect("a sha"),
        "--message",
        "remove the nested file",
    ])
    .assert_ok("gea raw repo delete-file on a nested path");

    // Out of band: the command exiting 0 is not evidence the file is gone.
    let (code, body) = repo.api("GET", "contents/src/deep/nested/main.rs", None);
    assert_eq!(code, 404, "the nested file should be gone, but the server still serves it: {body}");
}

/// `POST /contents` commits several paths in one go, and the whole point is that it is **one**
/// commit. A per-file loop would produce the same final tree and a different history, so the
/// assertion is on the commit sha being shared, not on the files existing.
///
/// Driven through `--body-file` because `files` is a nested array: `ValueTy::Json` is exactly
/// the case the flag surface cannot express, and this is the only live test that proves the
/// `--body-file` path composes with a real server rather than with our own parser.
#[test]
fn change_files_puts_every_path_in_a_single_commit() {
    let inst = instance_or_skip!();
    cover!(raw: ["repoChangeFiles"]);
    let repo = TestRepo::create_initialized(inst, "change-files");
    let scratch = Scratch::new("changefiles");

    // The wire field is `content`, not `content_base64`: the spec's x-go-name is ContentBase64
    // but the JSON key is `content`. Getting that wrong is silent — the server commits an empty
    // file and answers 201 — which is why the assertions below read the bytes back.
    let plan = serde_json::json!({
        "message": "two files at once",
        "branch": "main",
        "files": [
            { "operation": "create", "path": "pkg/one.txt", "content": b64(b"one") },
            { "operation": "create", "path": "pkg/deep/two.txt", "content": b64(b"two") },
        ]
    });
    let plan_file = scratch.join("change.json");
    std::fs::write(&plan_file, plan.to_string()).expect("write the body file");

    let run = inst.gea([
        "raw",
        "repo",
        "change-files",
        &repo.owner,
        &repo.name,
        "--body-file",
        &plan_file.to_string_lossy(),
    ]);
    run.assert_ok("gea raw repo change-files");
    let v = run.json();
    let files = v["files"].as_array().expect("change-files reports the files it wrote");
    assert_eq!(files.len(), 2, "both files should be reported: {}", run.stdout);

    // Out of band, and reading the *bytes*: a wrong body field name still returns 201 with both
    // paths listed, and produces two empty files.
    for (path, want) in [("pkg/one.txt", "one"), ("pkg/deep/two.txt", "two")] {
        let (code, body) = repo.api("GET", &format!("contents/{path}"), None);
        assert_eq!(code, 200, "{path} is missing: {body}");
        let got: serde_json::Value = serde_json::from_str(&body).expect("contents JSON");
        assert_eq!(
            got["content"].as_str(),
            Some(b64(want.as_bytes()).as_str()),
            "{path} was committed empty, so the request body field name is wrong"
        );
    }

    let shas: Vec<&str> =
        files.iter().filter_map(|f| f["last_commit_sha"].as_str()).collect::<Vec<_>>();
    assert_eq!(shas.len(), 2, "both files should name the commit that created them: {v}");
    assert_eq!(shas[0], shas[1], "the two files landed in different commits: {v}");
}

// ----------------------------------------------------------------------- byte-stream guards

/// A `Produces::Bytes` operation must reach a file intact and must refuse a terminal.
///
/// Both halves matter and they fail in opposite directions. Without the guard, a few kilobytes
/// of zip interpreted as terminal input leaves a session needing `reset`; with the guard placed
/// wrongly — say before the media type is known, or applied to a `--output` destination — the
/// operation becomes unusable. So this drives the same command twice and asserts each outcome.
///
/// `GEA_FORCE_TTY` is required for the refusal: `guard_binary` only fires when there is a
/// terminal, and under `cargo test` stdout is a pipe. It is not a way of making the assertion
/// convenient — without it the test would pass vacuously against a guard that never ran.
///
/// The archive is used rather than `get-raw-file` deliberately. Gitea serves a `.rs` file as
/// text, and the guard lets text through on purpose, so a raw-file refusal is not something the
/// server would ever produce. `application/zip` is.
#[test]
fn an_archive_streams_to_a_file_and_is_refused_to_a_terminal() {
    let inst = instance_or_skip!();
    cover!(raw: ["repoGetArchive"]);
    let repo = TestRepo::create_initialized(inst, "archive");
    let scratch = Scratch::new("archive");
    seed_file(inst, &repo, "src/deep/nested/main.rs", "fn main() {}\n");

    let zip = scratch.join("repo.zip");
    inst.gea([
        "raw",
        "repo",
        "get-archive",
        &repo.owner,
        &repo.name,
        "main.zip",
        "--output",
        &zip.to_string_lossy(),
    ])
    .assert_ok("gea raw repo get-archive --output");

    let bytes = std::fs::read(&zip).expect("the archive should have been written");
    assert!(
        bytes.starts_with(b"PK\x03\x04"),
        "the archive does not begin with the zip magic, so what arrived is not a zip: {:?}",
        &bytes[..bytes.len().min(16)]
    );
    // The nested path is in the archive's central directory as literal text, which is the
    // cheapest way to prove the bytes are this repository's archive and not an error page.
    assert!(
        bytes.windows(22).any(|w| w == b"src/deep/nested/main.rs"[..22].as_ref()),
        "the archive does not mention the nested file that was committed to it"
    );

    // tar.gz takes the same parameter with a different suffix, and the suffix is part of the
    // path rather than a query parameter — an encoder that treated `.` specially would break it.
    let tgz = scratch.join("repo.tar.gz");
    inst.gea([
        "raw",
        "repo",
        "get-archive",
        &repo.owner,
        &repo.name,
        "main.tar.gz",
        "--output",
        &tgz.to_string_lossy(),
    ])
    .assert_ok("gea raw repo get-archive main.tar.gz");
    let gz = std::fs::read(&tgz).expect("the tarball should have been written");
    assert_eq!(&gz[..2], b"\x1f\x8b", "the tar.gz does not begin with the gzip magic");

    // And the refusal, with a terminal and no destination.
    let refused = inst.gea_env(
        Path::new("."),
        &[("GEA_FORCE_TTY", "80")],
        ["raw", "repo", "get-archive", &repo.owner, &repo.name, "main.zip"],
    );
    assert!(!refused.ok(), "a zip should not be written to a terminal: {}", refused.stdout.len());
    refused.assert_says("--output");
}

// ----------------------------------------------------------------------- branches and refs

/// A branch name containing `/` — the overwhelmingly common convention — has to survive being
/// created, listed, read back, renamed, deleted, and looked up through `git/refs/{ref}`.
///
/// `ref` on `repoListGitRefs` is the other `PathEncoding::PathLike` parameter, and the one with
/// two slashes in play at once: the value `heads/feat/deep/one` is itself a path *and* names a
/// path. Encoding it would answer 404, or worse, silently match a different ref.
///
/// The rename is asserted from both ends — the new name resolves and the old one does not —
/// because `PATCH /branches/{branch}` answers 204 with no body, so its exit code says nothing
/// about what the server did.
#[test]
fn a_branch_whose_name_contains_slashes_survives_the_whole_lifecycle() {
    let inst = instance_or_skip!();
    cover!(raw: [
        "repoCreateBranch",
        "repoListBranches",
        "repoGetBranch",
        "repoRenameBranch",
        "repoDeleteBranch",
        "repoListGitRefs",
        "repoListAllGitRefs"
    ]);
    let repo = TestRepo::create_initialized(inst, "branch-slash");
    let branch = "feat/deep/one";
    let renamed = "feat/deep/two";

    let created = inst.gea([
        "raw",
        "repo",
        "create-branch",
        &repo.owner,
        &repo.name,
        "--new-branch-name",
        branch,
        "--old-branch-name",
        "main",
    ]);
    created.assert_ok("gea raw repo create-branch with a slashed name");
    assert_eq!(created.json()["name"], branch, "the server named the branch something else");

    let listed = inst.gea(["raw", "repo", "list-branches", &repo.owner, &repo.name]);
    listed.assert_ok("gea raw repo list-branches");
    let names: Vec<String> = listed
        .json()
        .as_array()
        .expect("branches are an array")
        .iter()
        .map(|b| b["name"].as_str().unwrap_or_default().to_owned())
        .collect();
    assert!(names.contains(&branch.to_owned()), "the slashed branch is missing: {names:?}");

    let one = inst.gea(["raw", "repo", "get-branch", &repo.owner, &repo.name, branch]);
    one.assert_ok("gea raw repo get-branch with a slashed name");
    assert_eq!(one.json()["name"], branch);

    // The PathLike `ref` proof. `heads/feat/deep/one` is three separators inside one parameter.
    let refs = inst.gea([
        "raw",
        "repo",
        "list-git-refs",
        &repo.owner,
        &repo.name,
        &format!("heads/{branch}"),
    ]);
    refs.assert_ok("gea raw repo list-git-refs on a slashed ref");
    let found: Vec<String> = refs
        .json()
        .as_array()
        .expect("refs are an array")
        .iter()
        .map(|r| r["ref"].as_str().unwrap_or_default().to_owned())
        .collect();
    assert_eq!(
        found,
        vec![format!("refs/heads/{branch}")],
        "git/refs/{{ref}} did not resolve the slashed ref to exactly one branch"
    );

    let all = inst.gea(["raw", "repo", "list-all-git-refs", &repo.owner, &repo.name]);
    all.assert_ok("gea raw repo list-all-git-refs");
    let all_refs: Vec<String> = all
        .json()
        .as_array()
        .expect("refs are an array")
        .iter()
        .map(|r| r["ref"].as_str().unwrap_or_default().to_owned())
        .collect();
    assert!(
        all_refs.contains(&format!("refs/heads/{branch}")),
        "the unfiltered ref listing lost the slashed branch: {all_refs:?}"
    );

    inst.gea(["raw", "repo", "rename-branch", &repo.owner, &repo.name, branch, "--name", renamed])
        .assert_ok("gea raw repo rename-branch");

    // 204, so both directions have to be read back out of band.
    let (code, _) = repo.api("GET", &format!("branches/{renamed}"), None);
    assert_eq!(code, 200, "the renamed branch is not there");
    let (code, _) = repo.api("GET", &format!("branches/{branch}"), None);
    assert_eq!(code, 404, "the old branch name still resolves, so nothing was renamed");

    inst.gea(["raw", "repo", "delete-branch", &repo.owner, &repo.name, renamed])
        .assert_ok("gea raw repo delete-branch");
    let (code, _) = repo.api("GET", &format!("branches/{renamed}"), None);
    assert_eq!(code, 404, "the branch survived its own deletion");
}

// ------------------------------------------------------------------------ tags and plumbing

/// An annotated tag is two objects — the tag and the commit it points at — and the API exposes
/// them through two unrelated routes. `POST /tags` returns the *tag* object's sha, and
/// `git/tags/{sha}` is the only route that will accept it; handing it a commit sha answers 404.
///
/// So this asserts the sha `repoCreateTag` hands back is the one `GetAnnotatedTag` resolves,
/// and that the message survives. A mock cannot check that: it would return whichever sha the
/// fixture author typed, for whichever route.
#[test]
fn an_annotated_tag_is_readable_as_a_tag_and_as_a_git_object() {
    let inst = instance_or_skip!();
    cover!(raw: ["repoCreateTag", "repoListTags", "repoGetTag", "GetAnnotatedTag", "repoDeleteTag"]);
    let repo = TestRepo::create_initialized(inst, "tags");
    let message = "annotated by the integration suite";

    let made = inst.gea([
        "raw",
        "repo",
        "create-tag",
        &repo.owner,
        &repo.name,
        "--tag-name",
        "v0.9.0",
        "--message",
        message,
        "--target",
        "main",
    ]);
    made.assert_ok("gea raw repo create-tag");
    let tag_sha = made.json()["id"].as_str().expect("a tag reports its object id").to_owned();

    let listed = inst.gea(["raw", "repo", "list-tags", &repo.owner, &repo.name]);
    listed.assert_ok("gea raw repo list-tags");
    let names: Vec<String> = listed
        .json()
        .as_array()
        .expect("tags are an array")
        .iter()
        .map(|t| t["name"].as_str().unwrap_or_default().to_owned())
        .collect();
    assert_eq!(names, vec!["v0.9.0".to_owned()], "the tag listing is wrong: {names:?}");

    let one = inst.gea(["raw", "repo", "get-tag", &repo.owner, &repo.name, "v0.9.0"]);
    one.assert_ok("gea raw repo get-tag");
    assert_eq!(
        one.json()["id"].as_str(),
        Some(tag_sha.as_str()),
        "get-tag and create-tag disagree about the tag object's sha"
    );

    let obj = inst.gea(["raw", "git", "annotated-tag", &repo.owner, &repo.name, &tag_sha]);
    obj.assert_ok("gea raw git annotated-tag");
    let obj = obj.json();
    assert_eq!(obj["tag"], "v0.9.0");
    assert!(
        obj["message"].as_str().unwrap_or_default().contains(message),
        "the annotation message was lost: {obj}"
    );

    inst.gea(["raw", "repo", "delete-tag", &repo.owner, &repo.name, "v0.9.0"])
        .assert_ok("gea raw repo delete-tag");
    let (code, _) = repo.api("GET", "tags/v0.9.0", None);
    assert_eq!(code, 404, "the tag survived its own deletion");
}

/// The plumbing routes chained the way a client actually walks them: commit -> tree -> blob.
///
/// Each sha is taken from the previous answer rather than from a fixture, which is the property
/// that matters. A mock supplies both ends of every hop, so it proves the calls were made and
/// nothing about whether the shas the server hands out are the shas the next route accepts.
///
/// `git/blobs` (plural) takes a **comma-separated query parameter**, not a repeated flag, and
/// is the only operation in this group that does — a list encoded as repeated `shas=` would
/// come back short rather than failing.
#[test]
fn a_commit_leads_to_its_tree_and_its_tree_to_its_blobs() {
    let inst = instance_or_skip!();
    cover!(raw: [
        "repoGetAllCommits",
        "repoGetSingleCommit",
        "GetTree",
        "GetBlob",
        "repoDownloadCommitDiffOrPatch"
    ]);
    let repo = TestRepo::create_initialized(inst, "plumbing");
    let path = "src/deep/nested/main.rs";
    let body = "fn main() { println!(\"plumbing\"); }\n";
    let head = seed_file(inst, &repo, path, body);

    let log = inst.gea(["raw", "repo", "get-all-commits", &repo.owner, &repo.name, "--limit", "5"]);
    log.assert_ok("gea raw repo get-all-commits");
    let shas: Vec<String> = log
        .json()
        .as_array()
        .expect("commits are an array")
        .iter()
        .map(|c| c["sha"].as_str().unwrap_or_default().to_owned())
        .collect();
    assert_eq!(shas.first().map(String::as_str), Some(head.as_str()), "the log's head is wrong");

    let commit = inst.gea(["raw", "repo", "get-single-commit", &repo.owner, &repo.name, &head]);
    commit.assert_ok("gea raw repo get-single-commit");
    let commit = commit.json();
    assert_eq!(commit["sha"], head);
    let tree_sha =
        commit["commit"]["tree"]["sha"].as_str().expect("a commit names its tree").to_owned();

    // Recursive, so the nested path appears as a full path rather than as a lone directory
    // entry — which is also the only way to reach the blob without three more round trips.
    let tree = inst.gea([
        "raw",
        "git",
        "tree",
        &repo.owner,
        &repo.name,
        &tree_sha,
        "--recursive=true",
        "--limit",
        "100",
    ]);
    tree.assert_ok("gea raw git tree --recursive");
    let entries = tree.json();
    let entries = entries["tree"].as_array().expect("a tree has entries");
    let blob = entries
        .iter()
        .find(|e| e["path"] == path)
        .unwrap_or_else(|| panic!("the tree does not contain {path}: {entries:?}"));
    let blob_sha = blob["sha"].as_str().expect("a tree entry has a sha").to_owned();

    let one = inst.gea(["raw", "git", "blob", &repo.owner, &repo.name, &blob_sha]);
    one.assert_ok("gea raw git blob");
    let one = one.json();
    assert_eq!(one["encoding"], "base64");
    assert_eq!(
        one["content"].as_str(),
        Some(b64(body.as_bytes()).as_str()),
        "the blob reached through the tree is not the file that was committed"
    );

    // `.{diffType}` is a *path suffix*, not a query parameter, and the response is plain text
    // rather than JSON — so this is also the check that `--jq` is correctly refused for it.
    let diff = inst.gea([
        "raw",
        "repo",
        "download-commit-diff-or-patch",
        &repo.owner,
        &repo.name,
        &head,
        "--diff-type",
        "diff",
    ]);
    diff.assert_ok("gea raw repo download-commit-diff-or-patch diff");
    assert!(
        diff.stdout.contains(&format!("b/{path}")),
        "the diff does not mention the file the commit added: {}",
        diff.stdout
    );

    let patch = inst.gea([
        "raw",
        "repo",
        "download-commit-diff-or-patch",
        &repo.owner,
        &repo.name,
        &head,
        "--diff-type",
        "patch",
    ]);
    patch.assert_ok("gea raw repo download-commit-diff-or-patch patch");
    assert!(
        patch.stdout.starts_with(&format!("From {head}")),
        "a patch is a mailbox and must open with the commit it is for: {}",
        patch.stdout.lines().next().unwrap_or_default()
    );
}

/// Commit statuses are readable one by one and as a rollup, and the two shapes differ in a way
/// that is easy to get backwards: an individual status carries `status`, the combined view
/// carries `state`. A decoder that expected `state` on both would read every status as null.
///
/// The status is seeded out of band (`repoCreateStatus` belongs to another group), so what is
/// under test here is purely the two read routes.
///
/// It also records a real divergence between the vendored specification and the server. Both
/// operations declare `ref` as `PathEncoding::PathLike`, and `git/refs/{ref}` genuinely honours
/// that — but `commits/{ref}/status` does not: a branch named `feat/x` produces
/// `commits/feat/x/status`, which Gitea's router cannot parse, and the answer is 404. The
/// encoding is right and the server disagrees with its own specification, which is exactly the
/// class of finding a mock cannot produce.
#[test]
fn commit_statuses_are_readable_individually_and_combined() {
    let inst = instance_or_skip!();
    cover!(raw: ["repoListStatusesByRef", "repoGetCombinedStatusByRef"]);
    let repo = TestRepo::create_initialized(inst, "statuses");
    let head = seed_file(inst, &repo, "built.txt", "something to build\n");

    let (code, body) = repo.api(
        "POST",
        &format!("statuses/{head}"),
        Some(r#"{"state":"success","context":"itest/ci","description":"green"}"#),
    );
    assert!((200..300).contains(&code), "could not seed a commit status: HTTP {code}: {body}");

    let list = inst.gea(["raw", "repo", "list-statuses-by-ref", &repo.owner, &repo.name, &head]);
    list.assert_ok("gea raw repo list-statuses-by-ref");
    let rows = list.json();
    let rows = rows.as_array().expect("statuses are an array");
    assert_eq!(rows.len(), 1, "exactly the seeded status should be listed: {}", list.stdout);
    assert_eq!(rows[0]["context"], "itest/ci");
    assert_eq!(
        rows[0]["status"], "success",
        "an individual status carries `status`, not `state`: {}",
        list.stdout
    );

    let combined =
        inst.gea(["raw", "repo", "get-combined-status-by-ref", &repo.owner, &repo.name, &head]);
    combined.assert_ok("gea raw repo get-combined-status-by-ref");
    let combined = combined.json();
    assert_eq!(combined["state"], "success", "the combined view carries `state`: {combined}");
    assert_eq!(combined["total_count"], 1);
    assert_eq!(combined["sha"], head);

    // The same route reached by branch name rather than by sha, which is the form the parameter
    // is documented for and the form a caller is most likely to use.
    let by_branch =
        inst.gea(["raw", "repo", "get-combined-status-by-ref", &repo.owner, &repo.name, "main"]);
    by_branch.assert_ok("gea raw repo get-combined-status-by-ref by branch name");
    assert_eq!(by_branch.json()["sha"], head, "the branch resolved to a different commit");
}

/// `GET /commits/{sha}/pull` does not mean "the pull request containing this commit". Gitea
/// looks the commit up as a **merge commit**, so the head commit of an open pull request — the
/// obvious thing to pass — answers 404, and only the sha in `merge_commit_sha` works.
///
/// Nothing in the specification says that; its summary is "Get the pull request of the commit".
/// A `FakeTransport` test would return a pull request for whatever sha the fixture used and
/// would enshrine the wrong reading. Both halves are asserted here so the asymmetry is on the
/// record rather than rediscovered.
#[test]
fn a_commits_pull_request_is_found_from_the_merge_commit_and_not_from_the_head() {
    let inst = instance_or_skip!();
    cover!(raw: ["repoGetCommitPullRequest"]);
    let repo = TestRepo::create_initialized(inst, "commit-pr");

    let (code, body) = repo.api(
        "POST",
        "branches",
        Some(r#"{"new_branch_name":"topic","old_branch_name":"main"}"#),
    );
    assert!((200..300).contains(&code), "could not branch: HTTP {code}: {body}");
    let head = {
        let (code, body) = repo.api(
            "POST",
            "contents/merged.txt",
            Some(&format!(
                r#"{{"content":"{}","message":"a change to merge","branch":"topic"}}"#,
                b64(b"merged content\n")
            )),
        );
        assert!((200..300).contains(&code), "could not commit: HTTP {code}: {body}");
        let v: serde_json::Value = serde_json::from_str(&body).expect("a file response");
        v["commit"]["sha"].as_str().expect("a commit sha").to_owned()
    };

    let (code, body) =
        repo.api("POST", "pulls", Some(r#"{"title":"merge me","head":"topic","base":"main"}"#));
    assert!((200..300).contains(&code), "could not open a pull request: HTTP {code}: {body}");
    let pr: serde_json::Value = serde_json::from_str(&body).expect("a pull request");
    let number = pr["number"].as_u64().expect("a pull request has a number");

    // The head commit of an open pull request: the intuitive argument, and a 404.
    let open = inst.gea(["raw", "repo", "get-commit-pull-request", &repo.owner, &repo.name, &head]);
    assert!(
        !open.ok(),
        "the head commit of an *open* pull request resolved, which contradicts what this route \
         means; if Gitea changed, this test is the place that records the old behaviour: {}",
        open.stdout
    );
    open.assert_says("404");

    let (code, body) = repo.merge_pull(number, "merge");
    assert!((200..300).contains(&code), "could not merge: HTTP {code}: {body}");

    // The merge commit is written asynchronously, so poll rather than sleep a fixed amount.
    let mut merge_sha = String::new();
    for _ in 0..30 {
        let (_, body) = repo.api("GET", &format!("pulls/{number}"), None);
        let v: serde_json::Value = serde_json::from_str(&body).unwrap_or_default();
        if let Some(sha) = v["merge_commit_sha"].as_str().filter(|s| !s.is_empty()) {
            merge_sha = sha.to_owned();
            break;
        }
        std::thread::sleep(std::time::Duration::from_millis(200));
    }
    assert!(!merge_sha.is_empty(), "the merged pull request never reported a merge commit");

    let found =
        inst.gea(["raw", "repo", "get-commit-pull-request", &repo.owner, &repo.name, &merge_sha]);
    found.assert_ok("gea raw repo get-commit-pull-request on the merge commit");
    assert_eq!(
        found.json()["number"].as_u64(),
        Some(number),
        "the merge commit resolved to a different pull request: {}",
        found.stdout
    );
}

// ------------------------------------------------------------------------------- releases

/// Every way of addressing a release and its assets, in one lifecycle.
///
/// Gitea exposes a release under three keys — its id, its tag, and "latest" — and an asset
/// under a fourth. A client that mixes them up produces a 404 the user reads as "the release is
/// gone". `repoGetLatestRelease` is the one worth being careful with: it is not "the most
/// recently created release", it excludes drafts and prereleases, so the assertion below is
/// made while the release is still published and repeated after it is marked a prerelease.
///
/// `porcelain.rs` already proves `gea release create` uploads a large asset with its bytes
/// intact. This is the layer-2 half — the addressing and the metadata edits — and deliberately
/// uses a small asset so it is not a second copy of that test.
#[test]
fn a_release_is_reachable_by_id_by_tag_and_as_the_latest_one() {
    let inst = instance_or_skip!();
    cover!(raw: [
        "repoCreateRelease",
        "repoListReleases",
        "repoGetRelease",
        "repoGetReleaseByTag",
        "repoGetLatestRelease",
        "repoEditRelease",
        "repoCreateReleaseAttachment",
        "repoListReleaseAttachments",
        "repoGetReleaseAttachment",
        "repoEditReleaseAttachment",
        "repoDeleteReleaseAttachment",
        "repoDeleteRelease"
    ]);
    let repo = TestRepo::create_initialized(inst, "release-raw");
    let scratch = Scratch::new("release-raw");
    let asset = scratch.join("notes.txt");
    let payload = b"raw release asset payload";
    std::fs::write(&asset, payload).expect("write the asset");

    let made = inst.gea([
        "raw",
        "repo",
        "create-release",
        &repo.owner,
        &repo.name,
        "--tag-name",
        "v1.0.0",
        "--name",
        "One Point Oh",
        "--body",
        "the first one",
        "--target-commitish",
        "main",
    ]);
    made.assert_ok("gea raw repo create-release");
    let id = made.json()["id"].as_u64().expect("a release has an id").to_string();

    let by_id = inst.gea(["raw", "repo", "get-release", &repo.owner, &repo.name, &id]);
    by_id.assert_ok("gea raw repo get-release by id");
    assert_eq!(by_id.json()["tag_name"], "v1.0.0");

    let by_tag = inst.gea(["raw", "repo", "get-release-by-tag", &repo.owner, &repo.name, "v1.0.0"]);
    by_tag.assert_ok("gea raw repo get-release-by-tag");
    assert_eq!(
        by_tag.json()["id"].as_u64().map(|n| n.to_string()),
        Some(id.clone()),
        "the tag and the id name different releases"
    );

    let latest = inst.gea(["raw", "repo", "get-latest-release", &repo.owner, &repo.name]);
    latest.assert_ok("gea raw repo get-latest-release");
    assert_eq!(latest.json()["tag_name"], "v1.0.0");

    let listed = inst.gea(["raw", "repo", "list-releases", &repo.owner, &repo.name]);
    listed.assert_ok("gea raw repo list-releases");
    let tags: Vec<String> = listed
        .json()
        .as_array()
        .expect("releases are an array")
        .iter()
        .map(|r| r["tag_name"].as_str().unwrap_or_default().to_owned())
        .collect();
    assert_eq!(tags, vec!["v1.0.0".to_owned()], "the release listing is wrong: {tags:?}");

    // The asset: uploaded as multipart with the name in the *query string*, which is the one
    // place in this group where a parameter does not travel in the path or the body.
    let attached = inst.gea([
        "raw",
        "repo",
        "create-release-attachment",
        &repo.owner,
        &repo.name,
        &id,
        "--attachment",
        &asset.to_string_lossy(),
        "--name",
        "notes.txt",
    ]);
    attached.assert_ok("gea raw repo create-release-attachment");
    let attached = attached.json();
    let attachment_id = attached["id"].as_u64().expect("an attachment has an id").to_string();
    assert_eq!(
        attached["size"].as_u64(),
        Some(payload.len() as u64),
        "the uploaded size does not match the file, so the multipart body was truncated"
    );

    let assets =
        inst.gea(["raw", "repo", "list-release-attachments", &repo.owner, &repo.name, &id]);
    assets.assert_ok("gea raw repo list-release-attachments");
    let names: Vec<String> = assets
        .json()
        .as_array()
        .expect("attachments are an array")
        .iter()
        .map(|a| a["name"].as_str().unwrap_or_default().to_owned())
        .collect();
    assert_eq!(names, vec!["notes.txt".to_owned()], "the attachment listing is wrong: {names:?}");

    let one = inst.gea([
        "raw",
        "repo",
        "get-release-attachment",
        &repo.owner,
        &repo.name,
        &id,
        &attachment_id,
    ]);
    one.assert_ok("gea raw repo get-release-attachment");
    // This route answers with *metadata*, not bytes — the bytes live under the web root at
    // `browser_download_url`. `gea release download` is what follows that, and it is covered in
    // the porcelain test below; asserting the field is present here is what keeps the two
    // halves honestly separated.
    assert_eq!(one.json()["name"], "notes.txt");
    assert!(
        one.json()["browser_download_url"].as_str().unwrap_or_default().contains("/attachments/"),
        "an attachment must carry a web download URL: {}",
        one.stdout
    );

    inst.gea([
        "raw",
        "repo",
        "edit-release-attachment",
        &repo.owner,
        &repo.name,
        &id,
        &attachment_id,
        "--name",
        "release-notes.txt",
    ])
    .assert_ok("gea raw repo edit-release-attachment");
    let (code, body) = repo.api("GET", &format!("releases/{id}/assets/{attachment_id}"), None);
    assert_eq!(code, 200, "{body}");
    let renamed: serde_json::Value = serde_json::from_str(&body).expect("an attachment");
    assert_eq!(renamed["name"], "release-notes.txt", "the asset was not renamed: {body}");

    // Marking it a prerelease must take it out of `latest`, which is the behaviour most likely
    // to be assumed away.
    inst.gea([
        "raw",
        "repo",
        "edit-release",
        &repo.owner,
        &repo.name,
        &id,
        "--name",
        "One Point Oh (rc)",
        "--prerelease=true",
    ])
    .assert_ok("gea raw repo edit-release");
    let (code, body) = repo.api("GET", &format!("releases/{id}"), None);
    assert_eq!(code, 200, "{body}");
    let edited: serde_json::Value = serde_json::from_str(&body).expect("a release");
    assert_eq!(edited["name"], "One Point Oh (rc)");
    assert_eq!(edited["prerelease"], true);
    let (code, _) = repo.api("GET", "releases/latest", None);
    assert_eq!(code, 404, "a prerelease must not be served as the latest release");

    inst.gea([
        "raw",
        "repo",
        "delete-release-attachment",
        &repo.owner,
        &repo.name,
        &id,
        &attachment_id,
    ])
    .assert_ok("gea raw repo delete-release-attachment");
    let (code, _) = repo.api("GET", &format!("releases/{id}/assets/{attachment_id}"), None);
    assert_eq!(code, 404, "the attachment survived its own deletion");

    inst.gea(["raw", "repo", "delete-release", &repo.owner, &repo.name, &id])
        .assert_ok("gea raw repo delete-release");
    let (code, _) = repo.api("GET", &format!("releases/{id}"), None);
    assert_eq!(code, 404, "the release survived its own deletion");
}

/// `DELETE /releases/tags/{tag}` removes the release and leaves the git tag standing.
///
/// The two are separate objects and it is easy to believe otherwise — `gea release delete` has
/// a `--cleanup-tag` flag precisely because deleting the release does not delete the tag. This
/// asserts the tag is still there afterwards, which is the half a mock has no opinion about.
#[test]
fn deleting_a_release_by_tag_leaves_the_git_tag_behind() {
    let inst = instance_or_skip!();
    cover!(raw: ["repoDeleteReleaseByTag"]);
    let repo = TestRepo::create_initialized(inst, "release-bytag");

    let (code, body) = repo.api("POST", "tags", Some(r#"{"tag_name":"v2.0.0","target":"main"}"#));
    assert!((200..300).contains(&code), "could not create the tag: HTTP {code}: {body}");
    let (code, body) = repo.api("POST", "releases", Some(r#"{"tag_name":"v2.0.0","name":"Two"}"#));
    assert!((200..300).contains(&code), "could not create the release: HTTP {code}: {body}");

    inst.gea(["raw", "repo", "delete-release-by-tag", &repo.owner, &repo.name, "v2.0.0"])
        .assert_ok("gea raw repo delete-release-by-tag");

    let (code, _) = repo.api("GET", "releases/tags/v2.0.0", None);
    assert_eq!(code, 404, "the release survived deletion by tag");
    let (code, body) = repo.api("GET", "tags/v2.0.0", None);
    assert_eq!(
        code, 200,
        "deleting a release must not delete its git tag — that is what --cleanup-tag is for: \
         {body}"
    );
}

/// `gea release` end to end: create with an asset, read it back, edit it, upload another, pull
/// them down again, and take both the asset and the release away.
///
/// Every leaf here is several API calls that a mock decides the answers to for itself. The two
/// that only a server settles:
///
/// * **`download` does not use the API.** There is no route that returns an asset's bytes, so
///   the command follows `browser_download_url` under the instance's *web* root. That is a
///   different host path, a different auth surface, and the only way to know it works is to
///   compare the bytes that come back with the bytes that went up.
/// * **`upload --clobber` is delete-then-upload**, not a replace, so a failure between the two
///   loses the asset. Asserting the final contents is what distinguishes the two outcomes.
#[test]
fn the_release_porcelain_drives_a_whole_lifecycle_including_the_web_download() {
    let inst = instance_or_skip!();
    cover!(
        porcelain: [
            "release create",
            "release list",
            "release view",
            "release edit",
            "release upload",
            "release download",
            "release delete-asset",
            "release delete"
        ],
        hits: [
            "repoCreateRelease",
            "repoListReleases",
            "repoGetReleaseByTag",
            "repoEditRelease",
            "repoCreateReleaseAttachment",
            "repoDeleteReleaseAttachment",
            "repoDeleteRelease",
            "repoGetArchive"
        ]
    );
    let repo = TestRepo::create_initialized(inst, "release-porc");
    let scratch = Scratch::new("release-porc");
    let first = scratch.join("alpha.txt");
    let second = scratch.join("beta.bin");
    let alpha = b"alpha payload for the release";
    // Not text, so a download path that decoded or line-ended the body would be caught.
    let beta: Vec<u8> = (0..4096u32).map(|i| (i % 251) as u8).collect();
    std::fs::write(&first, alpha).expect("write alpha");
    std::fs::write(&second, &beta).expect("write beta");

    inst.gea([
        "release",
        "create",
        "v1.0.0",
        "-R",
        &repo.slug(),
        "--title",
        "One",
        "--notes",
        "first release",
        &first.to_string_lossy(),
    ])
    .assert_ok("gea release create");

    let listed = inst.gea(["release", "list", "-R", &repo.slug(), "--json", "tag_name,draft"]);
    listed.assert_ok("gea release list --json");
    let rows = listed.json();
    let rows = rows.as_array().expect("release list --json is an array");
    assert_eq!(rows.len(), 1, "exactly one release should be listed: {}", listed.stdout);
    assert_eq!(rows[0]["tag_name"], "v1.0.0");

    let viewed = inst.gea(["release", "view", "v1.0.0", "-R", &repo.slug(), "--json", "name,body"]);
    viewed.assert_ok("gea release view --json");
    assert_eq!(viewed.json()["name"], "One");

    inst.gea([
        "release",
        "edit",
        "v1.0.0",
        "-R",
        &repo.slug(),
        "--title",
        "One, revised",
        "--notes",
        "second thoughts",
    ])
    .assert_ok("gea release edit");
    // Out of band: `edit` reads by tag and writes by id, so a mismatch between the two would
    // report success having patched a different release.
    let (code, body) = repo.api("GET", "releases/tags/v1.0.0", None);
    assert_eq!(code, 200, "{body}");
    let rel: serde_json::Value = serde_json::from_str(&body).expect("a release");
    assert_eq!(rel["name"], "One, revised", "the edit did not reach the release: {body}");
    assert_eq!(rel["body"], "second thoughts");

    inst.gea(["release", "upload", "v1.0.0", &second.to_string_lossy(), "-R", &repo.slug()])
        .assert_ok("gea release upload");

    let dir = scratch.join("downloaded");
    inst.gea(["release", "download", "v1.0.0", "-R", &repo.slug(), "-D", &dir.to_string_lossy()])
        .assert_ok("gea release download");
    assert_eq!(
        std::fs::read(dir.join("alpha.txt")).expect("alpha.txt should have been downloaded"),
        alpha,
        "the text asset did not come back byte for byte from the web root"
    );
    assert_eq!(
        std::fs::read(dir.join("beta.bin")).expect("beta.bin should have been downloaded"),
        beta,
        "the binary asset did not come back byte for byte from the web root"
    );

    // `-A` takes the other branch entirely: the source archive, through the API's
    // `repoGetArchive`, rather than an asset through the web root.
    let arc = scratch.join("archive");
    inst.gea([
        "release",
        "download",
        "v1.0.0",
        "-R",
        &repo.slug(),
        "-A",
        "tar.gz",
        "-D",
        &arc.to_string_lossy(),
    ])
    .assert_ok("gea release download -A tar.gz");
    let tarball = std::fs::read(arc.join("v1.0.0.tar.gz")).expect("the source archive");
    assert_eq!(&tarball[..2], b"\x1f\x8b", "the source archive is not gzip");

    inst.gea(["release", "delete-asset", "v1.0.0", "beta.bin", "-R", &repo.slug(), "--yes"])
        .assert_ok("gea release delete-asset");
    let (code, body) = repo.api("GET", "releases/tags/v1.0.0", None);
    assert_eq!(code, 200, "{body}");
    let rel: serde_json::Value = serde_json::from_str(&body).expect("a release");
    let left: Vec<String> = rel["assets"]
        .as_array()
        .cloned()
        .unwrap_or_default()
        .iter()
        .map(|a| a["name"].as_str().unwrap_or_default().to_owned())
        .collect();
    assert_eq!(left, vec!["alpha.txt".to_owned()], "the wrong asset was deleted: {left:?}");

    inst.gea(["release", "delete", "v1.0.0", "-R", &repo.slug(), "--yes"])
        .assert_ok("gea release delete");
    let (code, _) = repo.api("GET", "releases/tags/v1.0.0", None);
    assert_eq!(code, 404, "the release survived its own deletion");
}

// ----------------------------------------------------------------------------------- wiki

/// `gea wiki` end to end, with `--title` supplied on the edit.
///
/// The title is given even though the page is not being renamed, because omitting it does not
/// mean "leave it alone" on the server — see the test below. Passing it is what a caller must
/// do today for the page to keep its name, so this is the lifecycle that actually works.
///
/// Wikis are a second git repository that Gitea creates lazily on the first write, so nothing
/// here can be seeded over the API first: the create *is* the fixture. That also makes the
/// revision count a real assertion — two revisions means create and edit each produced a commit
/// in that repository rather than one overwriting the other.
#[test]
fn the_wiki_porcelain_creates_reads_edits_and_deletes_a_page() {
    let inst = instance_or_skip!();
    cover!(
        porcelain: ["wiki create", "wiki list", "wiki view", "wiki edit", "wiki revisions", "wiki delete"],
        hits: [
            "repoCreateWikiPage",
            "repoGetWikiPages",
            "repoGetWikiPage",
            "repoEditWikiPage",
            "repoGetWikiPageRevisions",
            "repoDeleteWikiPage"
        ]
    );
    let repo = TestRepo::create_initialized(inst, "wiki-porc");

    inst.gea(["wiki", "create", "Getting Started", "-b", "first body", "-R", &repo.slug()])
        .assert_ok("gea wiki create");

    let listed = inst.gea(["wiki", "list", "-R", &repo.slug(), "--json", "title"]);
    listed.assert_ok("gea wiki list --json");
    let titles: Vec<String> = listed
        .json()
        .as_array()
        .expect("wiki list --json is an array")
        .iter()
        .map(|p| p["title"].as_str().unwrap_or_default().to_owned())
        .collect();
    assert_eq!(titles, vec!["Getting Started".to_owned()], "the page listing is wrong: {titles:?}");

    // A title with a space and its dashed web form must both reach the same page: `wire_name`
    // does that translation, and only the server can say the translation is the one it wants.
    for name in ["Getting Started", "Getting-Started"] {
        let viewed = inst.gea(["wiki", "view", name, "-R", &repo.slug()]);
        viewed.assert_ok(&format!("gea wiki view {name:?}"));
        assert!(
            viewed.stdout.contains("first body"),
            "viewing {name:?} did not print the page body: {}",
            viewed.stdout
        );
    }

    inst.gea([
        "wiki",
        "edit",
        "Getting-Started",
        "-b",
        "second body",
        "--title",
        "Getting Started",
        "--message",
        "revise the page",
        "-R",
        &repo.slug(),
    ])
    .assert_ok("gea wiki edit");

    let (code, body) = repo.api("GET", "wiki/page/Getting-Started", None);
    assert_eq!(code, 200, "the page is not where it was left: {body}");
    let page: serde_json::Value = serde_json::from_str(&body).expect("a wiki page");
    assert_eq!(page["title"], "Getting Started", "the edit renamed the page: {body}");
    assert_eq!(
        page["content_base64"].as_str(),
        Some(b64(b"second body").as_str()),
        "the new body did not reach the page: {body}"
    );

    let revs = inst.gea(["wiki", "revisions", "Getting-Started", "-R", &repo.slug()]);
    revs.assert_ok("gea wiki revisions");
    assert!(
        revs.stdout.contains("revise the page"),
        "the history does not mention the edit's commit message: {}",
        revs.stdout
    );
    let (code, body) = repo.api("GET", "wiki/revisions/Getting-Started", None);
    assert_eq!(code, 200, "{body}");
    let hist: serde_json::Value = serde_json::from_str(&body).expect("a revision list");
    assert_eq!(
        hist["count"].as_u64(),
        Some(2),
        "create and edit should be two commits in the wiki repository: {body}"
    );

    inst.gea(["wiki", "delete", "Getting-Started", "--yes", "-R", &repo.slug()])
        .assert_ok("gea wiki delete");
    let (code, _) = repo.api("GET", "wiki/page/Getting-Started", None);
    assert_eq!(code, 404, "the page survived its own deletion");
}

/// A defect, recorded rather than asserted away: `gea wiki edit -b <text>` with no `--title`
/// renames the page to `unnamed`.
///
/// `cmd/wiki/mod.rs` builds the PATCH body with `title: args.title.clone()` and a comment
/// saying "Omitted means keep unchanged". Gitea does not agree. `EditWikiPage` runs the
/// submitted title through its web-path normaliser unconditionally, and an absent title
/// normalises to `unnamed` — so the most ordinary invocation of the command, changing a page's
/// text, silently destroys its name, prints `Updated unnamed`, and exits 0.
///
/// This is precisely the class of bug the live plane exists for: a `FakeTransport` returns the
/// page the fixture author wrote, so it confirms the comment rather than the server. The fix is
/// for `edit` to send the current title when the user did not ask for a rename — the command
/// already fetches the page first, so the value is in hand. Until then this test is the record
/// of what the server does, and it will fail the moment the fix lands, which is the point: the
/// person fixing it should be the one to delete it.
#[test]
fn editing_a_wiki_page_without_a_title_renames_it_to_unnamed() {
    let inst = instance_or_skip!();
    cover!(porcelain: ["wiki edit"], hits: ["repoEditWikiPage"]);
    let repo = TestRepo::create_initialized(inst, "wiki-rename");

    inst.gea(["wiki", "create", "Keep My Name", "-b", "v1", "-R", &repo.slug()])
        .assert_ok("gea wiki create");

    inst.gea(["wiki", "edit", "Keep-My-Name", "-b", "v2", "-R", &repo.slug()])
        .assert_ok("gea wiki edit with no --title");

    let (code, body) = repo.api("GET", "wiki/pages", None);
    assert_eq!(code, 200, "{body}");
    let pages: Vec<String> = serde_json::from_str::<serde_json::Value>(&body)
        .expect("a page listing")
        .as_array()
        .expect("pages are an array")
        .iter()
        .map(|p| p["title"].as_str().unwrap_or_default().to_owned())
        .collect();
    assert_eq!(
        pages,
        vec!["unnamed".to_owned()],
        "the page kept its name, which means `gea wiki edit` now sends the current title. \
         That is the correct behaviour — delete this test and the defect note above it."
    );
}

// ------------------------------------------------------------------------------- webhooks

/// `gea webhook` end to end against a URL that deliberately does not resolve.
///
/// `example.invalid` is reserved and unroutable, which is the point: a webhook's target is never
/// contacted at create time, and `webhook test` only asks the server to **queue** a delivery. It
/// therefore answers 204 and exits 0 even though nothing can ever receive the payload. That is
/// worth pinning down, because the opposite assumption — that `test` reports whether the
/// endpoint answered — is the natural one and would make this test flaky by design.
///
/// `webhook edit --add-event` is read-modify-write: the command fetches the hook, unions the
/// event set and PATCHes it back. A mock decides for itself what the fetch returns, so it can
/// never show that Gitea expands a coarse event name (`pull_request`) into the family of
/// fine-grained ones it stores. The assertion below is that the previously-set events survive
/// that expansion, which is the property a naive replace would break.
#[test]
fn the_webhook_porcelain_creates_reads_edits_tests_and_deletes_a_hook() {
    let inst = instance_or_skip!();
    cover!(
        porcelain: ["webhook create", "webhook list", "webhook view", "webhook edit", "webhook test", "webhook delete"],
        hits: [
            "repoCreateHook",
            "repoListHooks",
            "repoGetHook",
            "repoEditHook",
            "repoTestHook",
            "repoDeleteHook"
        ]
    );
    let repo = TestRepo::create_initialized(inst, "webhook");

    let made = inst.gea([
        "webhook",
        "create",
        "--url",
        "http://example.invalid/hook",
        "-e",
        "push",
        "-R",
        &repo.slug(),
        "--json",
        "id,type,active",
    ]);
    made.assert_ok("gea webhook create");
    let made = made.json();
    assert_eq!(made["type"], "gitea");
    assert_eq!(made["active"], true, "a hook is created switched on unless --inactive");
    let id = made["id"].as_u64().expect("a hook has an id").to_string();

    let listed = inst.gea(["webhook", "list", "-R", &repo.slug(), "--json", "id"]);
    listed.assert_ok("gea webhook list --json");
    let ids: Vec<String> = listed
        .json()
        .as_array()
        .expect("hooks are an array")
        .iter()
        .filter_map(|h| h["id"].as_u64())
        .map(|n| n.to_string())
        .collect();
    assert_eq!(ids, vec![id.clone()], "the hook listing is wrong: {ids:?}");

    let viewed = inst.gea(["webhook", "view", &id, "-R", &repo.slug()]);
    viewed.assert_ok("gea webhook view");
    assert!(
        viewed.stdout.contains("http://example.invalid/hook"),
        "the view does not show the hook's URL, which is the thing it exists to show: {}",
        viewed.stdout
    );

    inst.gea([
        "webhook",
        "edit",
        &id,
        "-R",
        &repo.slug(),
        "--branch-filter",
        "main",
        "--add-event",
        "issues",
    ])
    .assert_ok("gea webhook edit");

    let (code, body) = repo.api("GET", &format!("hooks/{id}"), None);
    assert_eq!(code, 200, "{body}");
    let hook: serde_json::Value = serde_json::from_str(&body).expect("a hook");
    let events: Vec<String> = hook["events"]
        .as_array()
        .cloned()
        .unwrap_or_default()
        .iter()
        .map(|e| e.as_str().unwrap_or_default().to_owned())
        .collect();
    assert!(events.contains(&"issues".to_owned()), "the added event is missing: {events:?}");
    assert!(
        events.contains(&"push".to_owned()),
        "adding an event dropped the one the hook already had: {events:?}"
    );
    assert_eq!(hook["branch_filter"], "main", "the branch filter was not stored: {body}");

    // 204 and no body. The target is unroutable and that is deliberate — the server queues the
    // delivery rather than performing it, so this must succeed regardless.
    inst.gea(["webhook", "test", &id, "-R", &repo.slug()])
        .assert_ok("gea webhook test against an unroutable URL");

    inst.gea(["webhook", "delete", &id, "-R", &repo.slug(), "--yes"])
        .assert_ok("gea webhook delete");
    let (code, _) = repo.api("GET", &format!("hooks/{id}"), None);
    assert_eq!(code, 404, "the hook survived its own deletion");
}

// ---------------------------------------------------------------------------- deploy keys

/// `gea deploy-key` end to end, including the part the flag names invert.
///
/// The API field is `read_only` and the flags are `--read-only` / `--allow-write`, so the
/// command has to send `read_only: false` for `--allow-write`. Getting a negated boolean
/// backwards produces a key that works, is listed, and quietly grants the opposite access —
/// which no exit code reveals. Both settings are therefore read back over the API.
#[test]
fn a_deploy_key_is_added_read_back_and_removed_with_the_access_it_was_given() {
    let inst = instance_or_skip!();
    cover!(
        porcelain: ["deploy-key add", "deploy-key list", "deploy-key view", "deploy-key delete"],
        hits: ["repoCreateKey", "repoListKeys", "repoGetKey", "repoDeleteKey"]
    );
    let repo = TestRepo::create_initialized(inst, "deploy-key");
    let scratch = Scratch::new("deploy-key");
    let pubkey = scratch.join("id_ed25519.pub");
    std::fs::write(&pubkey, format!("{DEPLOY_KEY}\n")).expect("write the public key");

    let added = inst.gea([
        "deploy-key",
        "add",
        &pubkey.to_string_lossy(),
        "--title",
        "itest content-git key",
        "-R",
        &repo.slug(),
        "--json",
        "id,title,read_only",
    ]);
    added.assert_ok("gea deploy-key add");
    let added = added.json();
    assert_eq!(added["title"], "itest content-git key");
    assert_eq!(added["read_only"], true, "a deploy key must default to read-only: {added}");
    let id = added["id"].as_u64().expect("a key has an id").to_string();

    let listed = inst.gea(["deploy-key", "list", "-R", &repo.slug(), "--json", "id,title"]);
    listed.assert_ok("gea deploy-key list --json");
    let ids: Vec<String> = listed
        .json()
        .as_array()
        .expect("keys are an array")
        .iter()
        .filter_map(|k| k["id"].as_u64())
        .map(|n| n.to_string())
        .collect();
    assert_eq!(ids, vec![id.clone()], "the key listing is wrong: {ids:?}");

    let viewed = inst.gea(["deploy-key", "view", &id, "-R", &repo.slug()]);
    viewed.assert_ok("gea deploy-key view");
    assert!(
        viewed.stdout.contains("read-only"),
        "the view must say what the key may do: {}",
        viewed.stdout
    );

    // Out of band, because the rendered word "read-only" is our own text and proves nothing
    // about what the server stored.
    let (code, body) = repo.api("GET", &format!("keys/{id}"), None);
    assert_eq!(code, 200, "{body}");
    let key: serde_json::Value = serde_json::from_str(&body).expect("a deploy key");
    assert_eq!(key["read_only"], true, "the server stored the opposite access: {body}");
    assert!(
        key["key"].as_str().unwrap_or_default().starts_with("ssh-ed25519 "),
        "the armored key did not survive the round trip: {body}"
    );

    inst.gea(["deploy-key", "delete", &id, "-R", &repo.slug(), "--yes"])
        .assert_ok("gea deploy-key delete");
    let (code, _) = repo.api("GET", &format!("keys/{id}"), None);
    assert_eq!(code, 404, "the deploy key survived its own deletion");
}

// ------------------------------------------------------------------------------ git hooks

/// Every `git-hook` leaf against an instance that ships server-side hooks switched off.
///
/// Gitea gates `/repos/{owner}/{repo}/hooks/git` behind `security.DISABLE_GIT_HOOKS`, which
/// defaults to on, so a server-side git hook cannot be read or written even by an instance
/// admin. The harness does not override it, which makes this the default configuration almost
/// every user will meet.
///
/// That is worth a test rather than an exemption. What is proven is that the four commands that
/// do make a request **reach the server and are refused by it**, rather than failing locally, on
/// a route we guessed, or with a message that hides the reason. A mock would show four working
/// commands, which is the opposite of the truth for a stock instance. The fifth leaf, `delete`,
/// is the one that must *not* reach the server; it is asserted at the end.
///
/// The assertion is on the exit code and on `403`, never on Gitea's wording: "must be allowed
/// to edit Git hooks" is a string the next release is free to change.
///
/// An instance with `DISABLE_GIT_HOOKS=false` would answer 200 here. If that configuration is
/// ever added to the harness, this test is the one to split rather than to delete — the refusal
/// path is still the one most users are on.
#[test]
fn git_hook_commands_reach_the_server_and_are_refused_while_git_hooks_are_disabled() {
    let inst = instance_or_skip!();
    cover!(
        porcelain: [
            "git-hook list",
            "git-hook view",
            "git-hook edit",
            "git-hook disable",
            "git-hook delete"
        ],
        hits: ["repoListGitHooks", "repoGetGitHook", "repoEditGitHook", "repoDeleteGitHook"]
    );
    let repo = TestRepo::create_initialized(inst, "git-hook");
    let scratch = Scratch::new("git-hook");
    let script = scratch.join("pre-receive");
    std::fs::write(&script, "#!/bin/sh\nexit 0\n").expect("write the hook script");

    // Each of the four verbs, and each one is a different HTTP method on a different route, so
    // a routing mistake in any one of them would show here as a 404 rather than a 403.
    let attempts: [(&str, Vec<String>); 4] = [
        ("git-hook list", vec!["git-hook".into(), "list".into()]),
        ("git-hook view", vec!["git-hook".into(), "view".into(), "pre-receive".into()]),
        (
            "git-hook edit",
            vec![
                "git-hook".into(),
                "edit".into(),
                "pre-receive".into(),
                "-F".into(),
                script.to_string_lossy().into_owned(),
            ],
        ),
        (
            "git-hook disable",
            vec!["git-hook".into(), "disable".into(), "pre-receive".into(), "--yes".into()],
        ),
    ];

    for (what, mut args) in attempts {
        args.extend(["-R".to_owned(), repo.slug()]);
        let run = inst.gea(args);
        assert!(
            !run.ok(),
            "`gea {what}` succeeded against an instance with git hooks disabled, which means \
             either the harness now enables them or the command is not reaching the server: {}",
            run.stdout
        );
        run.assert_says("403");
    }

    // Out of band, to prove the refusal is the server's and names the route we think it does.
    let (code, body) = repo.api("GET", "hooks/git", None);
    assert_eq!(
        code, 403,
        "the git-hook listing route answered {code}, not the 403 the commands reported: {body}"
    );

    // `git-hook delete` is a hidden fifth leaf that exists only to refuse. Gitea's git hooks
    // are a fixed set, and its `DELETE` route empties a hook's script rather than removing the
    // hook, so the command answers with a usage error naming `disable` instead of repeating the
    // route's name. The evidence that it never reaches the server is that it does *not* report
    // the 403 every other verb here does — on this instance any request to that route would.
    let refused = inst.gea(["git-hook", "delete", "pre-receive", "-R", &repo.slug()]);
    refused.assert_code(2, "gea git-hook delete");
    refused.assert_says("disable");
    assert!(
        !refused.stderr.contains("403"),
        "`git-hook delete` reached the server; it is supposed to refuse before sending \
         anything: {}",
        refused.stderr
    );
}

// --------------------------------------------------------------- Gitea's newer content reads

/// The newer content routes, against a file whose bytes this test chose.
///
/// `contents-ext` returns metadata alone unless `includes` names more, `file-contents` is the
/// same batch read spelled twice — a `GET` carrying its JSON body in a query parameter, and a
/// `POST` carrying it as a body — and `licenses` is the repository's detected licence list. All
/// are shapes a mock would have agreed with by construction; only the server shows the content
/// arrives where the documentation says it does.
#[test]
fn the_extended_and_batch_content_routes_return_the_file_that_was_committed() {
    let inst = instance_or_skip!();
    cover!(raw: [
        "repoGetContentsExt",
        "repoGetFileContents",
        "repoGetFileContentsPost",
        "repoGetLicenses",
    ]);
    let repo = TestRepo::create_initialized(inst, "content-ext");
    let path = "docs/notes/readme.txt";
    let body = "the extended content routes\n";
    seed_file(inst, &repo, path, body);

    let bare = inst.gea(["raw", "repo", "get-contents-ext", &repo.owner, &repo.name, path]);
    bare.assert_ok("gea raw repo get-contents-ext");
    let with = inst.gea([
        "raw",
        "repo",
        "get-contents-ext",
        &repo.owner,
        &repo.name,
        path,
        "--includes",
        "file_content",
    ]);
    with.assert_ok("gea raw repo get-contents-ext --includes file_content");
    let with = with.json();
    assert_eq!(
        with["file_contents"]["content"].as_str(),
        Some(b64(body.as_bytes()).as_str()),
        "`includes=file_content` must carry the committed bytes: {with}"
    );
    assert!(
        bare.json()["file_contents"]["content"].is_null(),
        "without `includes` the route is documented to return metadata only: {}",
        bare.stdout
    );

    let wanted = format!(r#"{{"files":["{path}"]}}"#);
    let by_get =
        inst.gea(["raw", "repo", "get-file-contents", &repo.owner, &repo.name, "--body", &wanted]);
    by_get.assert_ok("gea raw repo get-file-contents");
    let by_post = inst.gea([
        "raw",
        "repo",
        "get-file-contents-post",
        &repo.owner,
        &repo.name,
        "--files",
        path,
    ]);
    by_post.assert_ok("gea raw repo get-file-contents-post");
    for (how, run) in [("GET", &by_get), ("POST", &by_post)] {
        let rows = run.json();
        assert_eq!(
            rows[0]["content"].as_str(),
            Some(b64(body.as_bytes()).as_str()),
            "the {how} spelling of file-contents lost the file: {rows}"
        );
    }

    let licenses = inst.gea(["raw", "repo", "get-licenses", &repo.owner, &repo.name]);
    licenses.assert_ok("gea raw repo get-licenses");
    assert!(licenses.json().is_array(), "the licence list is an array: {}", licenses.stdout);
}

/// `PUT /branches/{branch}` moves a branch to another commit — a fast-forward unless `force` is
/// sent, and guarded by `old_commit_id` when that is. Both the move and the guard are asserted:
/// a stale `old_commit_id` must be refused, which is the property that makes the route safe to
/// script.
#[test]
fn a_branch_is_fast_forwarded_to_a_newer_commit_only_from_the_tip_it_names() {
    let inst = instance_or_skip!();
    cover!(raw: ["repoUpdateBranch"]);
    let repo = TestRepo::create_initialized(inst, "branch-ff");
    let tip = |branch: &str| -> String {
        let (code, body) = repo.api("GET", &format!("branches/{branch}"), None);
        assert_eq!(code, 200, "{body}");
        let b: serde_json::Value = serde_json::from_str(&body).expect("a branch");
        b["commit"]["id"].as_str().expect("a commit id").to_owned()
    };

    let (code, body) = repo.api("POST", "branches", Some(r#"{"new_branch_name":"behind"}"#));
    assert!((200..300).contains(&code), "{body}");
    let old = tip("behind");
    let new = seed_file(inst, &repo, "ahead.txt", "a commit `behind` does not have\n");
    assert_ne!(old, new);

    let stale = inst.gea([
        "raw",
        "repo",
        "update-branch",
        &repo.owner,
        &repo.name,
        "behind",
        "--new-commit-id",
        &new,
        "--old-commit-id",
        &new,
    ]);
    assert!(!stale.ok(), "a stale old_commit_id was accepted:\n{}", stale.stdout);
    assert_eq!(tip("behind"), old, "a refused update still moved the branch");

    inst.gea([
        "raw",
        "repo",
        "update-branch",
        &repo.owner,
        &repo.name,
        "behind",
        "--new-commit-id",
        &new,
        "--old-commit-id",
        &old,
    ])
    .assert_ok("gea raw repo update-branch");
    assert_eq!(tip("behind"), new, "the branch did not move to the commit named");
}

/// A git note is readable through the API once one is pushed — and only pushing can make one,
/// since Gitea has no route that writes notes.
#[test]
fn a_pushed_git_note_is_readable_by_its_commit() {
    let inst = instance_or_skip!();
    cover!(raw: ["repoGetNote"]);
    let repo = TestRepo::create_initialized(inst, "git-note");
    let scratch = Scratch::new("note");
    let checkout = scratch.join("clone");
    repo.clone_to(&checkout);
    let sha = gea_itest::git(&checkout, &["rev-parse", "HEAD"]).trim().to_owned();
    gea_itest::git(&checkout, &["notes", "add", "-m", "a note from the suite", &sha]);
    gea_itest::git(&checkout, &["push", "--quiet", "origin", "refs/notes/commits"]);

    let note = inst.gea(["raw", "repo", "get-note", &repo.owner, &repo.name, &sha]);
    note.assert_ok("gea raw repo get-note");
    let note = note.json();
    assert_eq!(note["message"].as_str().map(str::trim), Some("a note from the suite"), "{note}");
    // `commit` in Gitea's answer is the commit on `refs/notes/commits` that carries the note, not
    // the annotated one, so the message is the assertion.
}
