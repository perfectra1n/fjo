//! The account itself: keys, tokens, emails, follows, stars, watches, blocks and notifications.
//!
//! Almost every route here answers with something other than a JSON body, and that is exactly
//! what a `FakeTransport` cannot check because the fake decides for itself what came back. Three
//! shapes recur:
//!
//! * **204 vs 404 as a boolean.** `GET /user/following/{name}`, `/user/starred/{o}/{r}` and
//!   `/repos/{o}/{r}/subscription` answer `204 No Content` for yes and `404` for no. Nothing in
//!   the response says which question was asked, so a client that maps 404 to "the user does not
//!   exist" reports the wrong thing and a mock never notices.
//! * **Empty 204 mutations.** Follow, unfollow, block, unblock, star, unstar, every variable and
//!   secret write, and both email writes return no body. The only evidence they did anything is
//!   a second, independent read — so every mutation below is read back with `inst.api`, never
//!   trusted because the command exited 0.
//! * **Routes that refuse a token on purpose.** `/users/{name}/tokens` is `reqBasicAuth()`, so a
//!   token is answered 401 by design and `fjo` has to fall back to a password. That fallback
//!   reads stdin when there is no terminal, which the shared harness cannot drive — hence
//!   [`fjo_stdin`] below.
//!
//! One test here does not use the shared instance. `/user/activitypub/follow` is gated behind
//! `[federation] ENABLED`, which the shared instance must not set — with federation on, Forgejo
//! 16.0.4 answers `PUT /user/starred/{owner}/{repo}` with a 500, which would silently break
//! starring for every suite in the repository. So that one test asks `fjo_itest::federated()`
//! for a second container, and records nothing when there is not one.

use std::collections::BTreeSet;
use std::ffi::OsStr;
use std::io::Write;
use std::path::Path;
use std::process::{Command, Stdio};
use std::time::{Duration, Instant};

use fjo_itest::{Instance, Run, TestRepo, cover, instance_or_skip};

// --------------------------------------------------------------------------- fixtures

/// A public key whose user id carries [`GPG_EMAIL`], armoured once and pinned here.
///
/// Generating one at test time is not an option: `gpg` is not a dependency of this repository
/// and shelling out to a binary that may not exist would turn a coverage gap into a flake. The
/// private half was never written down, so this is inert — it proves only that we can encode an
/// `armored_public_key` the server will parse.
///
/// Forgejo refuses a GPG key none of whose user-id emails is on the account, offering the
/// verification-token route instead — which is why the test below adds the address first. That
/// refusal is itself a server behaviour no mock would have told us about.
const GPG_PUBLIC_KEY: &str = "\
-----BEGIN PGP PUBLIC KEY BLOCK-----

xsBNBGVT8QABCACbohjeK/j4oisGYxGnzyNvGRUItuAiO2KZ5TRA91XB+cvTU+vQ
fhtPkvdW5CElaQSMniuP8a+mMWo981Q5VYt8cqTlc4cljPciyrdf0cyfZXVoLzuj
0JAx+FR8VQM5AmVnMe8vz1S1dZp9OB9GkzbKlTO/LiB/ZQlNsF60fUomNbGI55wE
BnHGkE9cZl1QkBoql/BZCQTEmGCduBBT0drKL7ugQWNrXW+pMhuYI9FXJQ28exdI
pbPxRYv7XjwtBP+s2aees6o8Iq9ziDjokUMMRfI9BXoyD+uVx99KDINGyymQClcD
6eFxv2NI1raevucYsWS5Wkh8I5PpjmjrvmwlABEBAAHNLmZqbyBsaXZlIGl0ZXN0
IDxmam8tZ3BnLWl0ZXN0QGV4YW1wbGUuaW52YWxpZD7CwF8EEwEIAAkFAmVT8QAC
GwMACgkQR6rInpEGjqkYGgf/QU8YhtW3ch9vJRhprWsjtAq591++6aI1irGwQDxr
9pwQBlppuZ3LDmXAIdFjZpEzg+RyLJ67x74IG0JCXz4GVpIKKSCPxL3LCZUFlqCn
68VhRxvSgqA7JFDlhq+Cn8oC4Isz4qtf6C+BCqBdQ7TqALD/ESDnazbrLG9rmQX8
p5/1osECqXz29ZHXZxLZPycJC23a9/eQ1pfStb4a0tdcH09161ptguEV/A+0ctd5
kNwnWWiL+gAv6F1WUpdRIvyvdv7acx62MX+Oeq1pH9PiAGWmmJTrDRaQrSOXW0Bm
VNbNrAJcH1fNSoEho51VmU7fN13NXgycAmGXc639xS1Ahg==
=4x35
-----END PGP PUBLIC KEY BLOCK-----
";

/// The address in [`GPG_PUBLIC_KEY`]'s user id. Must be on the account before the key is added.
const GPG_EMAIL: &str = "fjo-gpg-itest@example.invalid";

/// The long key id Forgejo derives from [`GPG_PUBLIC_KEY`].
///
/// Asserted rather than merely read back, because the fingerprint is computed from the packet
/// bytes: if our encoding of `armored_public_key` ever mangled them, the server would still
/// accept *a* key and only this constant would notice.
const GPG_KEY_ID: &str = "47AAC89E91068EA9";

/// A 1x1 transparent PNG, base64 as `POST /user/avatar` wants it.
const TINY_PNG: &str = "iVBORw0KGgoAAAANSUhEUgAAAAEAAAABCAYAAAAfFcSJAAAADUlEQVR42mP8z8BQDwAEhQGAhKmMIQAAAABJRU5ErkJggg==";

/// A scratch directory that removes itself, for the tests that have to hand `fjo` a file path.
struct Scratch(std::path::PathBuf);

impl Scratch {
    fn new(tag: &str) -> Self {
        let d = std::env::temp_dir().join(format!("fjo-itest-user-{}-{tag}", std::process::id()));
        let _ = std::fs::remove_dir_all(&d);
        std::fs::create_dir_all(&d).expect("a scratch directory for the key files");
        Self(d)
    }

    /// Write `contents` to `name` inside the directory and return the path, as a `String`
    /// because every use of it is an argument to `fjo`.
    fn file(&self, name: &str, contents: &str) -> String {
        let p = self.0.join(name);
        std::fs::write(&p, contents)
            .unwrap_or_else(|e| panic!("could not write the fixture {}: {e}", p.display()));
        p.to_string_lossy().into_owned()
    }
}

impl Drop for Scratch {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.0);
    }
}

/// An SSH public key that the server will actually parse, generated for this test alone.
///
/// A hand-written constant is tempting and wrong: Forgejo rejects a key whose blob has already
/// been registered ("Key content has been used as non-deploy key"), so two tests sharing one
/// constant would pass alone and fail together — the worst kind of suite failure. `ssh-keygen`
/// is asserted rather than skipped past, because silently not testing the key routes is the
/// outcome this whole file exists to prevent.
fn ssh_public_key(scratch: &Scratch, tag: &str) -> String {
    let base = scratch.0.join(format!("id_{tag}"));
    let out = Command::new("ssh-keygen")
        .args(["-q", "-t", "ed25519", "-N", "", "-C"])
        .arg(format!("fjo-itest-{tag}@example.invalid"))
        .arg("-f")
        .arg(&base)
        .stdin(Stdio::null())
        .output()
        .unwrap_or_else(|e| {
            panic!(
                "ssh-keygen is needed to generate a key the server will parse, and it could not \
                 be run: {e}. Install openssh-client (or openssh) and re-run; skipping would \
                 leave the SSH key routes untested while reporting success."
            )
        });
    assert!(
        out.status.success(),
        "ssh-keygen failed\n--- stdout ---\n{}\n--- stderr ---\n{}",
        String::from_utf8_lossy(&out.stdout),
        String::from_utf8_lossy(&out.stderr)
    );
    std::fs::read_to_string(base.with_extension("pub"))
        .expect("ssh-keygen wrote a .pub next to the private key")
        .trim()
        .to_owned()
}

/// Run `fjo` with `stdin` piped in.
///
/// The harness's own runners give the child no stdin, which is right for everything else and
/// impossible for the token routes: `/users/{name}/tokens` is `reqBasicAuth()`, so `fjo` falls
/// back to asking for a password, and with no terminal it reads one from stdin. Without this the
/// only reachable assertion would be "it complained there was no password", which tests our
/// error message rather than the server.
///
/// The environment is [`Instance::child_env`] verbatim plus [`Instance::HOSTILE_ENV`] cleared,
/// so this differs from `inst.fjo(..)` in exactly one respect — the pipe.
fn fjo_stdin<I, S>(inst: &Instance, stdin: &str, args: I) -> Run
where
    I: IntoIterator<Item = S>,
    S: AsRef<OsStr>,
{
    let mut cmd = Command::new(fjo_itest::fjo_bin());
    cmd.args(args).stdin(Stdio::piped()).stdout(Stdio::piped()).stderr(Stdio::piped());
    for (k, v) in inst.child_env() {
        cmd.env(k, v);
    }
    for k in Instance::HOSTILE_ENV {
        cmd.env_remove(k);
    }
    let mut child = cmd.spawn().expect("the fjo binary should be executable");
    child
        .stdin
        .as_mut()
        .expect("stdin was piped")
        .write_all(stdin.as_bytes())
        .expect("writing the password to fjo's stdin");
    let out = child.wait_with_output().expect("fjo should finish");
    Run {
        code: out.status.code(),
        stdout: String::from_utf8_lossy(&out.stdout).into_owned(),
        stderr: String::from_utf8_lossy(&out.stderr).into_owned(),
    }
}

/// Parse a `(status, body)` pair from [`Instance::api`], failing with the body when it is not 2xx.
#[track_caller]
fn expect_json(what: &str, (code, body): (i32, String)) -> serde_json::Value {
    assert!((200..300).contains(&code), "{what}: HTTP {code}: {body}");
    serde_json::from_str(&body)
        .unwrap_or_else(|e| panic!("{what}: reply was not JSON ({e}): {body}"))
}

/// The `login` of every user in a JSON array, as a set — the shape most assertions here want.
fn logins(v: &serde_json::Value) -> BTreeSet<String> {
    v.as_array()
        .map(|a| a.iter().filter_map(|u| u["login"].as_str().map(str::to_owned)).collect())
        .unwrap_or_default()
}

/// Poll `f` until it answers `true`, or give up after `secs`.
///
/// Forgejo writes notifications and search indexes from queues, so "the mutation landed" and
/// "a later read can see it" are different moments. A fixed sleep is either flaky or slow; this
/// is neither.
fn wait_until(secs: u64, mut f: impl FnMut() -> bool) -> bool {
    let deadline = Instant::now() + Duration::from_secs(secs);
    loop {
        if f() {
            return true;
        }
        if Instant::now() >= deadline {
            return false;
        }
        std::thread::sleep(Duration::from_millis(250));
    }
}

// --------------------------------------------------------------------------- identity

/// `user view` with no argument, `user view <name>` and `user list` are three different
/// endpoints wearing one command, and only the first is authenticated-as-you.
///
/// The bug this catches is `@me` resolving to the literal string `@me` in the path — which a
/// mock answers happily because it was told to, and a real server answers with a 404 for a user
/// nobody has.
#[test]
fn viewing_yourself_and_a_named_user_reach_two_different_endpoints() {
    let inst = instance_or_skip!();
    cover!(
        porcelain: ["user view", "user list", "search users"],
        hits: ["userGetCurrent", "userGet", "userSearch"]
    );

    let me = inst.fjo(["user", "view", "--json", "login,id"]);
    me.assert_ok("fjo user view");
    let me = me.json();
    assert_eq!(me["login"].as_str(), Some(inst.user.as_str()), "user view named somebody else");

    // Out of band: `user view` must agree with `GET /user`, or the command is reading a
    // different account from the one the token belongs to.
    let current = expect_json("GET /user", inst.api("GET", "user", None));
    assert_eq!(me["id"], current["id"], "user view and GET /user disagree about who we are");

    // The named form goes to `/users/{name}`, and the id has to match — a command that quietly
    // fell back to `/user` would pass every other assertion here.
    let named = inst.fjo(["user", "view", &inst.user, "--json", "login,id"]);
    named.assert_ok("fjo user view <name>");
    assert_eq!(named.json()["id"], current["id"], "the named lookup found a different account");

    let listed = inst.fjo(["user", "list", &inst.user, "--json", "login"]);
    listed.assert_ok("fjo user list");
    assert!(
        logins(&listed.json()).contains(&inst.user),
        "user list did not find the account it was asked for: {}",
        listed.stdout
    );

    let searched = inst.fjo(["search", "users", &inst.user, "--json", "login"]);
    searched.assert_ok("fjo search users");
    assert!(
        logins(&searched.json()).contains(&inst.user),
        "search users did not find the account it was asked for: {}",
        searched.stdout
    );
}

/// `raw user get-user-settings` / `update-user-settings` are the only writable view of the
/// profile, and `PATCH` here is a merge — sending one field must not blank the others.
///
/// Kept to a single test and restored at the end: these are account-wide, so two tests writing
/// them in parallel would each see the other's value.
#[test]
fn patching_one_profile_setting_leaves_the_others_alone() {
    let inst = instance_or_skip!();
    cover!(raw: ["getUserSettings", "updateUserSettings"]);

    let before = inst.fjo(["raw", "user", "get-user-settings"]);
    before.assert_ok("fjo raw user get-user-settings");
    let before = before.json();
    let original_name = before["full_name"].as_str().unwrap_or_default().to_owned();
    let original_desc = before["description"].as_str().unwrap_or_default().to_owned();

    let marker = format!("live-user {}", std::process::id());
    inst.fjo(["raw", "user", "update-user-settings", "--description", &marker])
        .assert_ok("fjo raw user update-user-settings");

    // Read back out of band. The command echoes the server's reply, so asserting on its own
    // stdout would only prove the server echoed what we sent.
    let after = expect_json("GET /user/settings", inst.api("GET", "user/settings", None));
    assert_eq!(
        after["description"].as_str(),
        Some(marker.as_str()),
        "the description did not stick"
    );
    assert_eq!(
        after["full_name"].as_str().unwrap_or_default(),
        original_name,
        "PATCHing the description also rewrote full_name, so this route replaces rather than \
         merges and every other field is being silently blanked"
    );

    inst.fjo(["raw", "user", "update-user-settings", "--description", &original_desc])
        .assert_ok("restoring the description");
}

/// Uploading an avatar and deleting it again must move `avatar_url`, and move it back.
///
/// `POST /user/avatar` answers `204`, so "it worked" is unobservable from the response. Forgejo
/// serves the default avatar from a hash of the email and a custom one from a hash of the file,
/// which makes `avatar_url` the out-of-band witness.
#[test]
fn uploading_an_avatar_changes_the_url_and_deleting_it_restores_the_default() {
    let inst = instance_or_skip!();
    cover!(raw: ["userUpdateAvatar", "userDeleteAvatar"]);

    let url_now = || {
        expect_json("GET /user", inst.api("GET", "user", None))["avatar_url"]
            .as_str()
            .unwrap_or_default()
            .to_owned()
    };
    let default_url = url_now();

    inst.fjo(["raw", "user", "update-avatar", "--image", TINY_PNG])
        .assert_ok("fjo raw user update-avatar");
    let uploaded = url_now();
    assert_ne!(uploaded, default_url, "avatar_url did not move, so nothing was stored");

    inst.fjo(["raw", "user", "delete-avatar"]).assert_ok("fjo raw user delete-avatar");
    assert_eq!(url_now(), default_url, "deleting the avatar did not restore the default");
}

// --------------------------------------------------------------------------- keys

/// The SSH key lifecycle, end to end, including the two listings that are *different endpoints*
/// for the same data: `/user/keys` (authenticated, yours) and `/users/{name}/keys` (public).
///
/// A mock cannot tell those apart, and it cannot tell that the delete worked — `DELETE
/// /user/keys/{id}` answers `204`, so the only evidence is the key being gone from a fresh read.
#[test]
fn an_ssh_key_added_by_fjo_appears_in_both_listings_and_is_gone_after_delete() {
    let inst = instance_or_skip!();
    cover!(
        porcelain: ["user ssh-key add", "user ssh-key list", "user ssh-key delete"],
        hits: [
            "userCurrentPostKey",
            "userCurrentListKeys",
            "userCurrentGetKey",
            "userCurrentDeleteKey",
            "userListKeys"
        ]
    );

    let scratch = Scratch::new("ssh");
    let pubkey = ssh_public_key(&scratch, "added");
    let path = scratch.file("added.pub", &pubkey);
    let title = format!("live-user-{}", std::process::id());

    let created =
        inst.fjo(["user", "ssh-key", "add", &path, "--title", &title, "--json", "id,key"]);
    created.assert_ok("fjo user ssh-key add");
    let id = created.json()["id"].as_i64().expect("the server assigned the key an id");

    // Out of band, and comparing the *key material*: a title round trip would still pass if the
    // body had sent the wrong field, which is precisely the mistake `--title` invites.
    let stored = expect_json(
        &format!("GET /user/keys/{id}"),
        inst.api("GET", &format!("user/keys/{id}"), None),
    );
    assert_eq!(
        stored["key"].as_str().map(str::trim),
        Some(pubkey.as_str()),
        "the server stored different key material from the file it was given"
    );

    let ids = |run: &Run| -> BTreeSet<i64> {
        run.json()
            .as_array()
            .map(|a| a.iter().filter_map(|k| k["id"].as_i64()).collect())
            .unwrap_or_default()
    };

    let mine = inst.fjo(["user", "ssh-key", "list", "--json", "id,title"]);
    mine.assert_ok("fjo user ssh-key list");
    assert!(ids(&mine).contains(&id), "the new key is missing from /user/keys: {}", mine.stdout);

    let public = inst.fjo(["user", "ssh-key", "list", &inst.user, "--json", "id,title"]);
    public.assert_ok("fjo user ssh-key list <user>");
    assert!(
        ids(&public).contains(&id),
        "the new key is missing from /users/{}/keys, so the named form is reading the wrong \
         endpoint: {}",
        inst.user,
        public.stdout
    );

    inst.fjo(["raw", "user", "current-get-key", &id.to_string()])
        .assert_ok("fjo raw user current-get-key");

    inst.fjo(["user", "ssh-key", "delete", &id.to_string(), "--yes"])
        .assert_ok("fjo user ssh-key delete");
    let (code, body) = inst.api("GET", &format!("user/keys/{id}"), None);
    assert_eq!(code, 404, "the key survived a delete that reported success: {body}");
}

/// Forgejo refuses a GPG key unless one of its user-id emails is already on the account, so this
/// walks the email routes and the key routes together.
///
/// That refusal is the finding: nothing in the specification says `POST /user/gpg_keys` depends
/// on `POST /user/emails` having run first, and a `FakeTransport` would accept the key with no
/// email at all. The same test pins `key_id`, which is derived from the packet bytes — if our
/// encoding of `armored_public_key` ever mangled them the server would still store *a* key.
///
/// `verify-gpg-key` is driven with a deliberately invalid signature: producing a real one needs
/// the private half, which this repository does not carry. A refusal still proves the path, the
/// method and the body encoding reach the handler, which is the part a mock cannot confirm.
#[test]
fn a_gpg_key_is_only_accepted_once_its_user_id_email_is_on_the_account() {
    let inst = instance_or_skip!();
    cover!(
        porcelain: ["user gpg-key add", "user gpg-key list", "user gpg-key delete"],
        hits: [
            "userAddEmail",
            "userListEmails",
            "userDeleteEmail",
            "userCurrentPostGPGKey",
            "userCurrentListGPGKeys",
            "userCurrentGetGPGKey",
            "userCurrentDeleteGPGKey",
            "userListGPGKeys",
            "getVerificationToken",
            "userVerifyGPGKey"
        ]
    );

    let scratch = Scratch::new("gpg");
    let key_path = scratch.file("key.asc", GPG_PUBLIC_KEY);

    // A previous run against a persistent instance may have left either behind; clear both
    // rather than fail on somebody else's debris.
    let existing = expect_json("GET /user/gpg_keys", inst.api("GET", "user/gpg_keys", None));
    for k in existing.as_array().cloned().unwrap_or_default() {
        if k["key_id"].as_str() == Some(GPG_KEY_ID) {
            let id = k["id"].as_i64().unwrap_or_default();
            inst.api("DELETE", &format!("user/gpg_keys/{id}"), None);
        }
    }

    // The refusal, before the email exists.
    //
    // Forgejo answers this with **404**, not 422, and puts its real explanation ("None of the
    // emails attached to the GPG key could be found…") in the body. `fjo` maps that status to
    // "API endpoint not found" and drops the sentence, so the user is told the route does not
    // exist when in fact their key was rejected for a reason they could act on. That is the
    // failure `CONTRIBUTING.md` names as tea's worst UX bug, reproduced here — and it is why
    // this asserts on the exit code plus an out-of-band read rather than on the message: there
    // is no message left to assert on.
    let refused = inst.fjo(["user", "gpg-key", "add", &key_path]);
    refused.assert_code(
        5,
        "fjo user gpg-key add, for a key whose user-id email is not on the account",
    );
    let after_refusal = expect_json("GET /user/gpg_keys", inst.api("GET", "user/gpg_keys", None));
    assert!(
        !after_refusal.to_string().contains(GPG_KEY_ID),
        "the command failed but the key was stored anyway: {after_refusal}"
    );

    inst.fjo(["raw", "user", "add-email", "--emails", GPG_EMAIL])
        .assert_ok("fjo raw user add-email");
    let emails = inst.fjo(["raw", "user", "list-emails"]);
    emails.assert_ok("fjo raw user list-emails");
    let listed: BTreeSet<String> = emails
        .json()
        .as_array()
        .map(|a| a.iter().filter_map(|e| e["email"].as_str().map(str::to_owned)).collect())
        .unwrap_or_default();
    assert!(listed.contains(GPG_EMAIL), "the added email is not in the listing: {}", emails.stdout);

    let created = inst.fjo(["user", "gpg-key", "add", &key_path, "--json", "id,key_id"]);
    created.assert_ok("fjo user gpg-key add, once the email is on the account");
    let created = created.json();
    let id = created["id"].as_i64().expect("the server assigned the key an id");
    assert_eq!(
        created["key_id"].as_str(),
        Some(GPG_KEY_ID),
        "the server derived a different key id from the armoured block, so the bytes we sent \
         are not the bytes in this file"
    );

    let mine = inst.fjo(["user", "gpg-key", "list", "--json", "id,key_id"]);
    mine.assert_ok("fjo user gpg-key list");
    mine.assert_says(GPG_KEY_ID);

    let public = inst.fjo(["raw", "user", "list-gpg-keys", &inst.user]);
    public.assert_ok("fjo raw user list-gpg-keys");
    public.assert_says(GPG_KEY_ID);

    inst.fjo(["raw", "user", "current-get-gpg-key", &id.to_string()])
        .assert_ok("fjo raw user current-get-gpg-key")
        .assert_says(GPG_KEY_ID);

    // The token the server wants signed. Only its shape can be asserted — it is random per call.
    let token = inst.fjo(["raw", "user", "get-verification-token"]);
    token.assert_ok("fjo raw user get-verification-token");
    assert!(
        token.stdout.trim().len() >= 32,
        "the verification token looks too short to be one: {:?}",
        token.stdout
    );

    let bogus = inst.fjo([
        "raw",
        "user",
        "verify-gpg-key",
        "--key-id",
        GPG_KEY_ID,
        "--armored-signature",
        "-----BEGIN PGP SIGNATURE-----\n\nnot-a-signature\n-----END PGP SIGNATURE-----",
    ]);
    assert!(
        !bogus.ok(),
        "an invalid signature must not verify a key:\n{}\n{}",
        bogus.stdout,
        bogus.stderr
    );
    bogus.assert_says("signature");

    inst.fjo(["user", "gpg-key", "delete", &id.to_string(), "--yes"])
        .assert_ok("fjo user gpg-key delete");
    let (code, body) = inst.api("GET", &format!("user/gpg_keys/{id}"), None);
    assert_eq!(code, 404, "the GPG key survived a delete that reported success: {body}");

    inst.fjo(["raw", "user", "delete-email", "--emails", GPG_EMAIL])
        .assert_ok("fjo raw user delete-email");
    let after = expect_json("GET /user/emails", inst.api("GET", "user/emails", None));
    let left: BTreeSet<String> = after
        .as_array()
        .map(|a| a.iter().filter_map(|e| e["email"].as_str().map(str::to_owned)).collect())
        .unwrap_or_default();
    assert!(
        !left.contains(GPG_EMAIL),
        "the email survived a delete that reported success: {after}"
    );
}

// --------------------------------------------------------------------------- tokens

/// `user token list` and `user token delete`, against the routes that refuse a token by design.
///
/// `/users/{name}/tokens` is `reqBasicAuth()`: the `GET` happens to accept a token on Forgejo
/// 16.0.4 while the `DELETE` answers `401`, so `fjo` has to notice and fall back to a password.
/// That asymmetry is invisible to a mock, and it is the whole reason `needs_basic_auth` exists.
///
/// A dedicated account is created here so the password is one this test chose, rather than a
/// constant borrowed from the harness's private internals.
#[test]
fn deleting_a_token_falls_back_to_a_password_when_the_route_refuses_the_token() {
    let inst = instance_or_skip!();
    cover!(
        porcelain: ["user token list", "user token delete"],
        hits: ["userGetTokens", "userDeleteAccessToken"]
    );

    let name = format!("tokuser{}", std::process::id());
    let password = "fjo-itest-token-owner-1";
    let (code, body) = inst.api(
        "POST",
        "admin/users",
        Some(&format!(
            r#"{{"username":"{name}","email":"{name}@example.invalid","password":"{password}","must_change_password":false}}"#
        )),
    );
    assert!((200..300).contains(&code) || code == 422, "could not create {name}: {code}: {body}");

    // Minted out of band with Basic auth, so this test of `list` and `delete` does not also
    // depend on `create`, which `a_token_created_through_fjo_authenticates_as_its_account` drives.
    let token_name = "doomed";
    let out = Command::new("curl")
        .args(["-sS", "-w", "\n%{http_code}", "--max-time", "30"])
        .args(["-u", &format!("{name}:{password}")])
        .args(["-X", "POST", "-H", "Content-Type: application/json"])
        .args(["-d", &format!(r#"{{"name":"{token_name}","scopes":["read:user"]}}"#)])
        .arg(format!("{}/users/{name}/tokens", inst.api_base()))
        .output()
        .expect("curl should run");
    let minted = String::from_utf8_lossy(&out.stdout);
    assert!(
        minted.trim_end().ends_with("201"),
        "could not mint a token to delete, so this test would prove nothing: {minted}"
    );

    let listed = inst.fjo(["user", "token", "list", "--username", &name, "--json", "name"]);
    listed.assert_ok("fjo user token list --username");
    let names: BTreeSet<String> = listed
        .json()
        .as_array()
        .map(|a| a.iter().filter_map(|t| t["name"].as_str().map(str::to_owned)).collect())
        .unwrap_or_default();
    assert!(
        names.contains(token_name),
        "the minted token is not in the listing: {}",
        listed.stdout
    );

    fjo_stdin(
        inst,
        password,
        ["user", "token", "delete", token_name, "--username", &name, "--yes"],
    )
    .assert_ok("fjo user token delete, with the password on stdin");

    // Out of band, as the admin: the `DELETE` answers 204, so a fresh listing is the only proof.
    let left = expect_json(
        &format!("GET /users/{name}/tokens"),
        inst.api("GET", &format!("users/{name}/tokens"), None),
    );
    let left: BTreeSet<String> = left
        .as_array()
        .map(|a| a.iter().filter_map(|t| t["name"].as_str().map(str::to_owned)).collect())
        .unwrap_or_default();
    assert!(
        !left.contains(token_name),
        "the token survived a delete that reported success: {left:?}"
    );

    inst.api("DELETE", &format!("admin/users/{name}"), None);
}

/// `fjo user token create`, end to end: the token it prints must exist, carry the scope asked
/// for, and authenticate as the account.
///
/// This replaces a test that pinned a live defect: the command used to send `"repositories": []`,
/// which Forgejo reads as "limited to these repositories" and refuses with `400 Access tokens with
/// specified repositories must have at least one repository`, so it could not create a token on
/// any server. A mock had been told the call succeeded, so nothing else noticed. The token is
/// proved by using it, because a 201 with an unusable secret would look the same on stdout.
#[test]
fn a_token_created_through_fjo_authenticates_as_its_account() {
    let inst = instance_or_skip!();
    cover!(porcelain: ["user token create"], hits: ["userCreateToken"]);

    let name = format!("mktokuser{}", std::process::id());
    let password = "fjo-itest-token-owner-2";
    let (code, body) = inst.api(
        "POST",
        "admin/users",
        Some(&format!(
            r#"{{"username":"{name}","email":"{name}@example.invalid","password":"{password}","must_change_password":false}}"#
        )),
    );
    assert!((200..300).contains(&code) || code == 422, "could not create {name}: {code}: {body}");

    let run = fjo_stdin(
        inst,
        password,
        [
            "user",
            "token",
            "create",
            "wanted",
            "--scope",
            "read:user",
            "--username",
            &name,
            "--json",
            "name,sha1,scopes",
        ],
    );
    run.assert_ok("fjo user token create, with the password on stdin");
    let created = run.json();
    assert_eq!(created["name"], "wanted", "{created}");
    assert_eq!(created["scopes"], serde_json::json!(["read:user"]), "{created}");
    let secret = created["sha1"].as_str().filter(|s| !s.is_empty());
    let secret = secret.unwrap_or_else(|| panic!("no token secret was printed: {created}"));

    let (code, body) = inst.api_as(secret, "GET", "user", None);
    assert_eq!(code, 200, "the printed token does not authenticate: {body}");
    let me: serde_json::Value = serde_json::from_str(&body).expect("a user");
    assert_eq!(me["login"], name.as_str(), "the token belongs to someone else: {body}");

    inst.api("DELETE", &format!("admin/users/{name}"), None);
}

// --------------------------------------------------------------------------- following

/// Following is asymmetric — `/user/following` and `/user/followers` are different lists — and
/// the `check` routes answer `204` for yes and `404` for no with no body either way.
///
/// That 204/404 protocol is the bug this catches: a client that treats 404 as "no such user"
/// turns "you are not following them" into an error, and a mock that was told to return 204
/// never exercises the other branch. Both branches are driven here, before and after the
/// unfollow.
#[test]
fn following_is_visible_from_both_sides_and_the_check_routes_answer_204_then_404() {
    let inst = instance_or_skip!();
    let other = inst
        .scoped_user("followee", &["read:user", "write:user"])
        .unwrap_or_else(|e| panic!("a second account is needed to follow: {e}"));
    cover!(
        porcelain: ["user follow", "user unfollow"],
        hits: [
            "userCurrentPutFollow",
            "userCurrentDeleteFollow",
            "userCurrentListFollowing",
            "userCurrentListFollowers",
            "userListFollowing",
            "userListFollowers",
            "userCurrentCheckFollowing",
            "userCheckFollowing"
        ]
    );

    inst.fjo(["user", "follow", &other.name]).assert_ok("fjo user follow");
    // And the other way, so the *followers* listings have something in them too. Without this
    // they are empty on both sides and their assertions would be vacuous.
    inst.fjo_as(&other.token, ["user", "follow", &inst.user]).assert_ok("fjo user follow, as them");

    // Out of band first: the command answers 204, so its exit code says nothing.
    let following = expect_json("GET /user/following", inst.api("GET", "user/following", None));
    assert!(
        logins(&following).contains(&other.name),
        "the follow did not land on the server: {following}"
    );
    let their_followers = expect_json(
        "GET /users/<other>/followers",
        inst.api("GET", &format!("users/{}/followers", other.name), None),
    );
    assert!(
        logins(&their_followers).contains(&inst.user),
        "we follow them but do not appear in their followers, so the two lists disagree: \
         {their_followers}"
    );

    for args in [
        vec!["raw", "user", "current-list-following"],
        vec!["raw", "user", "current-list-followers"],
    ] {
        let run = inst.fjo(&args);
        run.assert_ok(&format!("fjo {}", args.join(" ")));
    }
    let listed = inst.fjo(["raw", "user", "list-following", &inst.user]);
    listed.assert_ok("fjo raw user list-following <user>");
    assert!(
        logins(&listed.json()).contains(&other.name),
        "the public following list disagrees with /user/following: {}",
        listed.stdout
    );
    let followers = inst.fjo(["raw", "user", "list-followers", &inst.user]);
    followers.assert_ok("fjo raw user list-followers <user>");
    assert!(
        logins(&followers.json()).contains(&other.name),
        "the public followers list is missing the account that followed us: {}",
        followers.stdout
    );

    // The yes branch of the boolean-by-status-code protocol.
    inst.fjo(["raw", "user", "current-check-following", &other.name])
        .assert_ok("fjo raw user current-check-following, while following");
    inst.fjo(["raw", "user", "check-following", &inst.user, &other.name])
        .assert_ok("fjo raw user check-following, while following");

    inst.fjo(["user", "unfollow", &other.name]).assert_ok("fjo user unfollow");
    let following = expect_json("GET /user/following", inst.api("GET", "user/following", None));
    assert!(
        !logins(&following).contains(&other.name),
        "the unfollow reported success and changed nothing: {following}"
    );

    // The no branch. Exit 5 is fjo's "no such resource", which is how the 404 arrives; asserting
    // on it is what pins the mapping, since the server sends no body to read.
    let gone = inst.fjo(["raw", "user", "current-check-following", &other.name]);
    gone.assert_code(5, "fjo raw user current-check-following, after unfollowing");
}

/// Blocking is two `PUT`s with no bodies and one listing that reports *ids*, not logins — so
/// `block list` has to resolve each id through the user search to print anything a human can use.
///
/// The resolution is the part worth testing live: a mock would return whatever login it was
/// told to, including for an id the server never issued.
#[test]
fn blocking_an_account_lists_it_by_login_and_unblocking_removes_it() {
    let inst = instance_or_skip!();
    let target = inst
        .scoped_user("blocked", &["read:user"])
        .unwrap_or_else(|e| panic!("a second account is needed to block: {e}"));
    cover!(
        porcelain: ["block add", "block list", "block remove"],
        hits: ["userBlockUser", "userUnblockUser", "userListBlockedUsers", "userSearch"]
    );

    let their_id = expect_json(
        "GET /users/<target>",
        inst.api("GET", &format!("users/{}", target.name), None),
    )["id"]
        .as_i64()
        .expect("the second account has an id");

    inst.fjo(["block", "add", &target.name]).assert_ok("fjo block add");

    let blocked = expect_json("GET /user/list_blocked", inst.api("GET", "user/list_blocked", None));
    let ids: BTreeSet<i64> = blocked
        .as_array()
        .map(|a| a.iter().filter_map(|b| b["block_id"].as_i64()).collect())
        .unwrap_or_default();
    assert!(ids.contains(&their_id), "the block did not land on the server: {blocked}");

    // The listing must show the login, not the bare id — that resolution is the only reason
    // `block list` makes more than one call.
    let listed = inst.fjo(["block", "list"]);
    listed.assert_ok("fjo block list");
    listed.assert_says(&target.name);

    inst.fjo(["block", "remove", &target.name]).assert_ok("fjo block remove");
    let blocked = expect_json("GET /user/list_blocked", inst.api("GET", "user/list_blocked", None));
    let ids: BTreeSet<i64> = blocked
        .as_array()
        .map(|a| a.iter().filter_map(|b| b["block_id"].as_i64()).collect())
        .unwrap_or_default();
    assert!(
        !ids.contains(&their_id),
        "the unblock reported success and changed nothing: {blocked}"
    );
}

// --------------------------------------------------------------------------- stars and watches

/// Starring: a `PUT` with no body, three listings, and the same 204/404 boolean as following.
///
/// `user stars` and `raw user list-starred <name>` are different endpoints — `/user/starred`
/// against `/users/{name}/starred` — so a command that used the wrong one would still print a
/// plausible list. Asserting the repository appears in both is what separates them.
#[test]
fn starring_a_repository_shows_up_in_both_star_listings_until_it_is_unstarred() {
    let inst = instance_or_skip!();
    let repo = TestRepo::create_initialized(inst, "star");
    cover!(
        porcelain: ["user star", "user stars", "user unstar"],
        hits: [
            "userCurrentPutStar",
            "userCurrentDeleteStar",
            "userCurrentListStarred",
            "userListStarred",
            "userCurrentCheckStarring"
        ]
    );

    inst.fjo(["user", "star", &repo.slug()]).assert_ok("fjo user star");

    let starred = expect_json("GET /user/starred", inst.api("GET", "user/starred", None));
    let names: BTreeSet<String> = starred
        .as_array()
        .map(|a| a.iter().filter_map(|r| r["full_name"].as_str().map(str::to_owned)).collect())
        .unwrap_or_default();
    assert!(names.contains(&repo.slug()), "the star did not land on the server: {starred}");

    let mine = inst.fjo(["user", "stars", "--json", "full_name"]);
    mine.assert_ok("fjo user stars");
    mine.assert_says(&repo.slug());

    let theirs = inst.fjo(["raw", "user", "list-starred", &inst.user, "--limit", "200"]);
    theirs.assert_ok("fjo raw user list-starred <user>");
    theirs.assert_says(&repo.name);

    inst.fjo(["raw", "user", "current-check-starring", &repo.owner, &repo.name])
        .assert_ok("fjo raw user current-check-starring, while starred");

    inst.fjo(["user", "unstar", &repo.slug()]).assert_ok("fjo user unstar");
    let starred = expect_json("GET /user/starred", inst.api("GET", "user/starred", None));
    let names: BTreeSet<String> = starred
        .as_array()
        .map(|a| a.iter().filter_map(|r| r["full_name"].as_str().map(str::to_owned)).collect())
        .unwrap_or_default();
    assert!(
        !names.contains(&repo.slug()),
        "the unstar reported success and changed nothing: {starred}"
    );

    inst.fjo(["raw", "user", "current-check-starring", &repo.owner, &repo.name])
        .assert_code(5, "fjo raw user current-check-starring, after unstarring");
}

/// Watching, which Forgejo calls a subscription and hangs off the *repository* rather than the
/// user: `PUT /repos/{o}/{r}/subscription`.
///
/// Creating a repository subscribes its owner automatically, so this unwatches first — otherwise
/// the `watch` below would be a no-op that passes whatever it did. The check route answers a
/// body here (unlike stars and follows), so both its status and its `subscribed` field matter.
#[test]
fn unwatching_and_rewatching_a_repository_moves_the_subscription_both_ways() {
    let inst = instance_or_skip!();
    let repo = TestRepo::create_initialized(inst, "watch");
    cover!(
        porcelain: ["user watch", "user unwatch"],
        hits: [
            "userCurrentPutSubscription",
            "userCurrentDeleteSubscription",
            "userCurrentCheckSubscription",
            "userCurrentListSubscriptions",
            "userListSubscriptions"
        ]
    );

    // The owner is subscribed on creation, which is the state this test has to undo first.
    inst.fjo(["raw", "user", "current-check-subscription", &repo.owner, &repo.name])
        .assert_ok("fjo raw user current-check-subscription, on a freshly created repository");

    inst.fjo(["user", "unwatch", &repo.slug()]).assert_ok("fjo user unwatch");
    inst.fjo(["raw", "user", "current-check-subscription", &repo.owner, &repo.name])
        .assert_code(5, "fjo raw user current-check-subscription, after unwatching");

    inst.fjo(["user", "watch", &repo.slug()]).assert_ok("fjo user watch");
    let sub = expect_json("GET /repos/<slug>/subscription", repo.api("GET", "subscription", None));
    assert_eq!(
        sub["subscribed"],
        serde_json::json!(true),
        "watch reported success but did not subscribe: {sub}"
    );

    let mine = inst.fjo(["raw", "user", "current-list-subscriptions", "--limit", "200"]);
    mine.assert_ok("fjo raw user current-list-subscriptions");
    mine.assert_says(&repo.name);

    let public = inst.fjo(["raw", "user", "list-subscriptions", &inst.user, "--limit", "200"]);
    public.assert_ok("fjo raw user list-subscriptions <user>");
    public.assert_says(&repo.name);
}

// --------------------------------------------------------------------------- notifications

/// A notification thread walked through every state the commands can put it in.
///
/// The inbox has to be filled by **somebody else**: Forgejo does not notify you about your own
/// actions, so a single-account test of these commands is a test of an empty list. Delivery is
/// queued, so the arrival is polled rather than slept on.
///
/// Everything is scoped with `-R` and acts on explicit ids, because notifications belong to the
/// account rather than to the repository — a bare `notification read` would mark threads another
/// test in this binary is still waiting for.
#[test]
fn a_notification_thread_walks_unread_read_pinned_and_back() {
    let inst = instance_or_skip!();
    let reporter = inst
        .scoped_user("notifier", &["all"])
        .unwrap_or_else(|e| panic!("a second account is needed to fill the inbox: {e}"));
    let repo = TestRepo::create_initialized(inst, "notif-state");
    // Public, so the second account can open an issue without a collaborator grant.
    repo.api("PATCH", "", Some(r#"{"private":false}"#));
    cover!(
        porcelain: [
            "notification list",
            "notification read",
            "notification unread",
            "notification pin",
            "notification unpin"
        ],
        hits: ["notifyGetList", "notifyGetRepoList", "notifyReadThread", "notifyGetThread"]
    );

    let (code, body) = inst.api_as(
        &reporter.token,
        "POST",
        &format!("repos/{}/issues", repo.slug()),
        Some(r#"{"title":"a thread to walk"}"#),
    );
    assert!((200..300).contains(&code), "seeding the issue failed: HTTP {code}: {body}");

    let listed_ids = || -> Vec<i64> {
        let run = inst.fjo(["notification", "list", "-R", &repo.slug(), "--json", "id"]);
        run.json()
            .as_array()
            .map(|a| a.iter().filter_map(|n| n["id"].as_i64()).collect())
            .unwrap_or_default()
    };
    assert!(
        wait_until(60, || !listed_ids().is_empty()),
        "the instance never delivered a notification, so this test would have proved nothing \
         about changing one's state.\n{}",
        inst.logs()
    );
    let id = listed_ids()[0];
    let id_s = id.to_string();

    // Out of band, every time: each of these commands answers with the thread it just patched,
    // so reading the command's own output would only prove the server echoed itself.
    let thread = |field: &str| -> serde_json::Value {
        expect_json(
            &format!("GET /notifications/threads/{id}"),
            inst.api("GET", &format!("notifications/threads/{id}"), None),
        )[field]
            .clone()
    };
    assert_eq!(thread("unread"), serde_json::json!(true), "a fresh thread should be unread");

    inst.fjo(["notification", "read", &id_s]).assert_ok("fjo notification read <id>");
    assert_eq!(
        thread("unread"),
        serde_json::json!(false),
        "read reported success and changed nothing"
    );

    inst.fjo(["notification", "unread", &id_s]).assert_ok("fjo notification unread <id>");
    assert_eq!(
        thread("unread"),
        serde_json::json!(true),
        "unread reported success and changed nothing"
    );

    inst.fjo(["notification", "pin", &id_s]).assert_ok("fjo notification pin <id>");
    assert_eq!(
        thread("pinned"),
        serde_json::json!(true),
        "pin reported success and changed nothing"
    );

    // `notification list` with no `-R` is a different endpoint (`/notifications`, not
    // `/repos/{o}/{r}/notifications`), so the pinned thread has to be findable through it too.
    let pinned =
        inst.fjo(["notification", "list", "-s", "pinned", "--limit", "100", "--json", "id"]);
    pinned.assert_ok("fjo notification list -s pinned");
    let pinned_ids: BTreeSet<i64> = pinned
        .json()
        .as_array()
        .map(|a| a.iter().filter_map(|n| n["id"].as_i64()).collect())
        .unwrap_or_default();
    assert!(
        pinned_ids.contains(&id),
        "the pinned thread is missing from the account-wide listing, so the two list endpoints \
         disagree: {}",
        pinned.stdout
    );

    inst.fjo(["notification", "unpin", &id_s]).assert_ok("fjo notification unpin <id>");
    assert_eq!(
        thread("pinned"),
        serde_json::json!(false),
        "unpin reported success and changed nothing"
    );

    inst.fjo(["raw", "notify", "get-thread", &id_s]).assert_ok("fjo raw notify get-thread");
}

// --------------------------------------------------------------------------- search

/// `search repos --owner` resolves the owner's *name* to a *uid*, because `/repos/search` takes
/// an id — one command, two calls, and the lookup is the half a mock cannot check.
///
/// `search issues` is in the same test because both read from indexes Forgejo populates
/// asynchronously; polling once for the pair keeps that wait to a single test.
#[test]
fn searching_finds_a_repository_by_owner_and_an_issue_by_title() {
    let inst = instance_or_skip!();
    let repo = TestRepo::create_initialized(inst, "searchable");
    cover!(
        porcelain: ["search repos", "search issues"],
        hits: ["repoSearch", "issueSearchIssues", "userGet"]
    );

    let needle = format!("needle{}", std::process::id());
    let (code, body) = repo.api("POST", "issues", Some(&format!(r#"{{"title":"{needle}"}}"#)));
    assert!((200..300).contains(&code), "seeding the issue failed: HTTP {code}: {body}");

    let found_repo = || {
        let run = inst.fjo(["search", "repos", &repo.name, "--json", "full_name"]);
        run.ok() && run.stdout.contains(&repo.slug())
    };
    assert!(found_repo(), "a repository that exists was not findable by name");

    // The `--owner` form is the one with the extra lookup. A wrong uid returns an empty list
    // rather than an error, which is why this asserts on the contents and not the exit code.
    let owned =
        inst.fjo(["search", "repos", &repo.name, "--owner", &inst.user, "--json", "full_name"]);
    owned.assert_ok("fjo search repos --owner");
    owned.assert_says(&repo.slug());

    let found_issue = || {
        let run = inst.fjo(["search", "issues", &needle, "--json", "title"]);
        run.ok() && run.stdout.contains(&needle)
    };
    assert!(
        wait_until(60, found_issue),
        "the issue never became searchable, so `search issues` could not be tested"
    );
}

// --------------------------------------------------------------------------- actions surface

/// Variables and secrets on the *account*, which are separate endpoints from the repository and
/// organization ones despite the identical shapes.
///
/// Three things here only a real server shows: the name is upper-cased on the way in, `POST`
/// answers `204` while `PUT` on a secret answers `201`, and a variable update needs its own name
/// echoed in the body. Every one of those is a decision the specification does not record.
#[test]
fn a_user_variable_and_secret_survive_a_full_create_update_delete_cycle() {
    let inst = instance_or_skip!();
    cover!(raw: [
        "createUserVariable",
        "getUserVariable",
        "updateUserVariable",
        "getUserVariablesList",
        "deleteUserVariable",
        "updateUserSecret",
        "userListActionsSecrets",
        "deleteUserSecret"
    ]);

    let var = format!("live_user_var_{}", std::process::id());
    let secret = format!("LIVE_USER_SECRET_{}", std::process::id());

    inst.fjo(["raw", "user", "create-user-variable", &var, "--value", "first"])
        .assert_ok("fjo raw user create-user-variable");

    let stored = inst.fjo(["raw", "user", "get-user-variable", &var]);
    stored.assert_ok("fjo raw user get-user-variable");
    assert_eq!(stored.json()["data"].as_str(), Some("first"), "the value did not round trip");

    inst.fjo(["raw", "user", "update-user-variable", &var, "--name", &var, "--value", "second"])
        .assert_ok("fjo raw user update-user-variable");
    let stored = inst.fjo(["raw", "user", "get-user-variable", &var]);
    stored.assert_ok("fjo raw user get-user-variable, after the update");
    assert_eq!(
        stored.json()["data"].as_str(),
        Some("second"),
        "the update reported success and changed nothing"
    );

    // Contains, never equals: another test in this binary may own variables of its own.
    let listed = inst.fjo(["raw", "user", "get-user-variables-list", "--limit", "200"]);
    listed.assert_ok("fjo raw user get-user-variables-list");
    listed.assert_says(&var.to_uppercase());

    inst.fjo(["raw", "user", "delete-user-variable", &var])
        .assert_ok("fjo raw user delete-user-variable");
    let (code, body) = inst.api("GET", &format!("user/actions/variables/{var}"), None);
    assert_eq!(code, 404, "the variable survived a delete that reported success: {body}");

    // A secret has no read-back route by design. Since v17 the account's secrets can at least be
    // *listed* — names only, never values — so the listing proves it exists and the delete
    // proves it is gone.
    inst.fjo(["raw", "user", "update-user-secret", &secret, "--data", "s3cret-value"])
        .assert_ok("fjo raw user update-user-secret");
    let listed = inst.fjo(["raw", "user", "list-actions-secrets", "--limit", "50"]);
    listed.assert_ok("fjo raw user list-actions-secrets");
    listed.assert_says(&secret);
    assert!(
        !listed.stdout.contains("s3cret-value"),
        "listing the account's secrets returned a secret's value:\n{}",
        listed.stdout
    );
    inst.fjo(["raw", "user", "delete-user-secret", &secret])
        .assert_ok("fjo raw user delete-user-secret, which can only succeed if the secret existed");
    let (code, body) = inst.api("DELETE", &format!("user/actions/secrets/{secret}"), None);
    assert_eq!(code, 404, "the secret survived a delete that reported success: {body}");
    let listed = inst.fjo(["raw", "user", "list-actions-secrets", "--limit", "50"]);
    listed.assert_ok("fjo raw user list-actions-secrets, after the delete");
    assert!(
        !listed.stdout.contains(&secret),
        "a deleted secret is still listed:\n{}",
        listed.stdout
    );
}

/// Registering a runner against the account, then taking it away again.
///
/// The registration token is a `GET` that mints a credential, which is worth driving on its own:
/// a mock returns a plausible string for it and the real route is the only thing that can say
/// whether the path is right. The runner's own token is never asserted on — it authenticates.
#[test]
fn a_registered_user_runner_is_listed_until_it_is_deleted() {
    let inst = instance_or_skip!();
    cover!(raw: [
        "userGetRunnerRegistrationToken",
        "registerUserRunner",
        "getUserRunners",
        "getUserRunner",
        "deleteUserRunner",
        "userSearchRunJobs"
    ]);

    let reg = inst.fjo(["raw", "user", "get-runner-registration-token", "--json", "token"]);
    reg.assert_ok("fjo raw user get-runner-registration-token");
    assert!(
        reg.json()["token"].as_str().is_some_and(|t| !t.is_empty()),
        "the registration token came back empty, so nothing could register with it"
    );

    let name = format!("live-user-runner-{}", std::process::id());
    let created =
        inst.fjo(["raw", "user", "register-user-runner", "--name", &name, "--json", "id,uuid"]);
    created.assert_ok("fjo raw user register-user-runner");
    let id = created.json()["id"].as_i64().expect("the server assigned the runner an id");

    let listed = inst.fjo(["raw", "user", "get-user-runners", "--limit", "200"]);
    listed.assert_ok("fjo raw user get-user-runners");
    listed.assert_says(&name);

    let one = inst.fjo(["raw", "user", "get-user-runner", &id.to_string()]);
    one.assert_ok("fjo raw user get-user-runner");
    assert_eq!(
        one.json()["name"].as_str(),
        Some(name.as_str()),
        "get-user-runner returned another runner"
    );

    // Empty on this instance, which is fine: the assertion is that the route exists and the
    // query encoding is accepted, not that a job ran.
    inst.fjo(["raw", "user", "search-run-jobs"]).assert_ok("fjo raw user search-run-jobs");

    inst.fjo(["raw", "user", "delete-user-runner", &id.to_string()])
        .assert_ok("fjo raw user delete-user-runner");
    let (code, body) = inst.api("GET", &format!("user/actions/runners/{id}"), None);
    assert_eq!(code, 404, "the runner survived a delete that reported success: {body}");
}

// --------------------------------------------------------------------------- hooks and apps

/// The account-level webhook lifecycle.
///
/// `config` is a free-form object the specification types as `map[string]string`, so the one
/// thing worth proving live is that the object we encode comes back with its `url` intact —
/// a mock would echo whatever it was handed, including a JSON-encoded string of an object.
#[test]
fn a_user_webhook_keeps_its_config_through_create_edit_and_delete() {
    let inst = instance_or_skip!();
    cover!(raw: ["userCreateHook", "userListHooks", "userGetHook", "userEditHook", "userDeleteHook"]);

    let url = format!("http://hook-{}.example.invalid/endpoint", std::process::id());
    let config = format!(r#"{{"url":"{url}","content_type":"json"}}"#);

    let created = inst.fjo([
        "raw",
        "user",
        "create-hook",
        "--type",
        "forgejo",
        "--config",
        &config,
        "--events",
        "push",
        "--active",
    ]);
    created.assert_ok("fjo raw user create-hook");
    let created = created.json();
    let id = created["id"].as_i64().expect("the server assigned the hook an id");
    assert_eq!(
        created["config"]["url"].as_str(),
        Some(url.as_str()),
        "the hook's config did not survive encoding: {created}"
    );

    let listed = inst.fjo(["raw", "user", "list-hooks", "--limit", "200"]);
    listed.assert_ok("fjo raw user list-hooks");
    listed.assert_says(&url);

    let one = inst.fjo(["raw", "user", "get-hook", &id.to_string()]);
    one.assert_ok("fjo raw user get-hook");
    assert_eq!(
        one.json()["active"],
        serde_json::json!(true),
        "the hook should have been created active"
    );

    inst.fjo(["raw", "user", "edit-hook", &id.to_string(), "--active=false"])
        .assert_ok("fjo raw user edit-hook");
    let after = expect_json(
        &format!("GET /user/hooks/{id}"),
        inst.api("GET", &format!("user/hooks/{id}"), None),
    );
    assert_eq!(
        after["active"],
        serde_json::json!(false),
        "the edit reported success and changed nothing"
    );

    inst.fjo(["raw", "user", "delete-hook", &id.to_string()]).assert_ok("fjo raw user delete-hook");
    let (code, body) = inst.api("GET", &format!("user/hooks/{id}"), None);
    assert_eq!(code, 404, "the hook survived a delete that reported success: {body}");
}

/// The OAuth2 application lifecycle, including the detail that makes it worth driving live:
/// the client secret is returned **only** by `create` and `update`, and is blank on every read.
///
/// A mock cannot show that, and a client that assumed the secret was always present would print
/// an empty string as if it were one.
#[test]
fn an_oauth2_application_returns_its_secret_only_when_it_is_written() {
    let inst = instance_or_skip!();
    cover!(raw: [
        "userCreateOAuth2Application",
        "userGetOAuth2Applications",
        "userGetOAuth2Application",
        "userUpdateOAuth2Application",
        "userDeleteOAuth2Application"
    ]);

    let name = format!("live-user-app-{}", std::process::id());
    let created = inst.fjo([
        "raw",
        "user",
        "create-oauth2-application",
        "--name",
        &name,
        "--redirect-uris",
        "http://app.example.invalid/callback",
        "--confidential-client",
    ]);
    created.assert_ok("fjo raw user create-oauth2-application");
    let created = created.json();
    let id = created["id"].as_i64().expect("the server assigned the application an id");
    assert!(
        created["client_secret"].as_str().is_some_and(|s| !s.is_empty()),
        "create must return the client secret; it is the only time it is shown: {created}"
    );

    let one = inst.fjo(["raw", "user", "get-oauth2-application", &id.to_string()]);
    one.assert_ok("fjo raw user get-oauth2-application");
    let one = one.json();
    assert_eq!(one["name"].as_str(), Some(name.as_str()), "get returned another application");
    assert_eq!(
        one["client_secret"].as_str(),
        Some(""),
        "a read returned a client secret, which contradicts Forgejo storing only its hash: {one}"
    );

    let listed = inst.fjo(["raw", "user", "list-oauth2-applications", "--limit", "200"]);
    listed.assert_ok("fjo raw user list-oauth2-applications");
    listed.assert_says(&name);

    let renamed = format!("{name}-renamed");
    inst.fjo([
        "raw",
        "user",
        "update-oauth2-application",
        &id.to_string(),
        "--name",
        &renamed,
        "--redirect-uris",
        "http://app.example.invalid/callback2",
    ])
    .assert_ok("fjo raw user update-oauth2-application");
    let after = expect_json(
        &format!("GET /user/applications/oauth2/{id}"),
        inst.api("GET", &format!("user/applications/oauth2/{id}"), None),
    );
    assert_eq!(
        after["name"].as_str(),
        Some(renamed.as_str()),
        "the update reported success and changed nothing"
    );

    inst.fjo(["raw", "user", "delete-oauth2-application", &id.to_string()])
        .assert_ok("fjo raw user delete-oauth2-application");
    let (code, body) = inst.api("GET", &format!("user/applications/oauth2/{id}"), None);
    assert_eq!(code, 404, "the application survived a delete that reported success: {body}");
}

// --------------------------------------------------------------------------- reports

/// The read-only account reports, driven together because each is a single `GET` whose only
/// failure mode is the path or the query encoding being wrong.
///
/// They are worth a live test anyway: `heatmap` and `activities/feeds` both return arrays of
/// objects with time fields, and `tracked-times` exists in two spellings — one on the account
/// and one under a repository — that a generator can easily wire to the same place.
#[test]
fn the_read_only_account_reports_all_answer_and_parse() {
    let inst = instance_or_skip!();
    let repo = TestRepo::create_initialized(inst, "reports");
    cover!(raw: [
        "userGetHeatmapData",
        "userListActivityFeeds",
        "userListTeams",
        "userGetStopWatches",
        "userCurrentTrackedTimes",
        "userTrackedTimes",
        "userCurrentListRepos",
        "userListRepos"
    ]);

    for args in [
        vec!["raw", "user", "get-heatmap-data", inst.user.as_str()],
        vec!["raw", "user", "list-activity-feeds", inst.user.as_str()],
        vec!["raw", "user", "list-teams"],
        vec!["raw", "user", "get-stop-watches"],
        vec!["raw", "user", "current-tracked-times"],
    ] {
        let run = inst.fjo(&args);
        run.assert_ok(&format!("fjo {}", args.join(" ")));
        // Parsed, not merely exited 0: a body we cannot decode is the failure mode that matters
        // for a generated client, and it is invisible to an exit-code check.
        run.json();
    }

    inst.fjo(["raw", "user", "tracked-times", &repo.owner, &repo.name, &inst.user])
        .assert_ok("fjo raw user tracked-times")
        .json();

    // The two repository listings are different endpoints; both must see a repository this
    // account definitely owns.
    let mine = inst.fjo(["raw", "user", "current-list-repos", "--limit", "200"]);
    mine.assert_ok("fjo raw user current-list-repos");
    mine.assert_says(&repo.name);

    let public = inst.fjo(["raw", "user", "list-repos", &inst.user, "--limit", "200"]);
    public.assert_ok("fjo raw user list-repos <user>");
    public.assert_says(&repo.name);
}

/// The commands that reach for git context still work from a directory that is not a repository.
///
/// `user star` and `user watch` default to "the repository you are in", and the failure this
/// guards is the opposite of the obvious one: not that the default is missing, but that a
/// command given an explicit slug still consults git and fails outside a checkout.
#[test]
fn an_explicit_slug_wins_over_git_context_outside_a_checkout() {
    let inst = instance_or_skip!();
    let repo = TestRepo::create_initialized(inst, "nogit");
    cover!(porcelain: ["user star"], hits: ["userCurrentPutStar"]);

    let scratch = Scratch::new("nogit");
    let outside: &Path = &scratch.0;
    inst.fjo_in(outside, ["user", "star", &repo.slug()])
        .assert_ok("fjo user star <slug>, from a directory with no git repository");

    let starred = expect_json("GET /user/starred", inst.api("GET", "user/starred", None));
    let names: BTreeSet<String> = starred
        .as_array()
        .map(|a| a.iter().filter_map(|r| r["full_name"].as_str().map(str::to_owned)).collect())
        .unwrap_or_default();
    assert!(names.contains(&repo.slug()), "the star did not land on the server: {starred}");

    inst.api("DELETE", &format!("user/starred/{}", repo.slug()), None);
}

// --------------------------------------------------------------------------- quota

/// A repository owned by somebody other than the admin, deleted when the test ends.
///
/// [`TestRepo`] always creates as the harness's own account, and the quota tests need a
/// repository whose *bytes are billed to a fresh user* — so this is the same idea with the
/// owner's token. `Drop` rather than an explicit call, so a failed assertion still tears down.
struct OwnedRepo<'a> {
    inst: &'a Instance,
    token: String,
    slug: String,
}

impl<'a> OwnedRepo<'a> {
    fn create(inst: &'a Instance, token: &str, owner: &str, name: &str) -> Self {
        let (code, body) = inst.api_as(
            token,
            "POST",
            "user/repos",
            Some(&format!(
                r#"{{"name":"{name}","private":true,"auto_init":true,"default_branch":"main"}}"#
            )),
        );
        assert!((200..300).contains(&code), "could not create {owner}/{name}: {code}: {body}");
        Self { inst, token: token.to_owned(), slug: format!("{owner}/{name}") }
    }
}

impl Drop for OwnedRepo<'_> {
    fn drop(&mut self) {
        let _ = self.inst.api_as(&self.token, "DELETE", &format!("repos/{}", self.slug), None);
    }
}

/// Attach a file to an issue, which is the cheapest way to make a byte count appear under
/// `used.size.assets.attachments`.
///
/// `curl` rather than [`Instance::api`] because this route is `multipart/form-data` and the
/// harness's helper only sends JSON. Out of band either way, which is the point.
fn upload_issue_attachment(inst: &Instance, token: &str, slug: &str, index: u64, file: &str) {
    let out = Command::new("curl")
        .args(["-sS", "-w", "\n%{http_code}", "--max-time", "30"])
        .args(["-H", &format!("Authorization: token {token}")])
        .args(["-X", "POST", "-F", &format!("attachment=@{file}")])
        .arg(format!("{}/repos/{slug}/issues/{index}/assets", inst.api_base()))
        .output()
        .expect("curl should run");
    let text = String::from_utf8_lossy(&out.stdout);
    assert!(
        text.trim_end().ends_with("201"),
        "could not attach {file} to {slug}#{index}, so the quota assertion below would be \
         measuring nothing: {text}"
    );
}

/// Every leaf byte count under `used.size`, so "all zero" can be asserted without naming each
/// one and silently missing the one that was not.
fn size_leaves(quota: &serde_json::Value) -> Vec<(String, u64)> {
    fn walk(prefix: &str, v: &serde_json::Value, out: &mut Vec<(String, u64)>) {
        match v {
            serde_json::Value::Object(map) => {
                for (k, child) in map {
                    let path = if prefix.is_empty() { k.clone() } else { format!("{prefix}.{k}") };
                    walk(&path, child, out);
                }
            }
            serde_json::Value::Number(n) => out.push((prefix.to_owned(), n.as_u64().unwrap_or(0))),
            _ => {}
        }
    }
    let mut out = Vec::new();
    walk("", &quota["used"]["size"], &mut out);
    out
}

/// Quota accounting, measured on an account that nothing else in this binary touches.
///
/// The figures have to be read on a *fresh* user: the admin account shares this container with
/// every other test in the file, so "used is zero" there would be a race rather than an
/// assertion. A second account makes the numbers exact — and exactness is the point, because a
/// `200` alone would pass just as happily against a handler that returns a fixed zero.
///
/// So this asserts three things no mock could: that a new account's every size leaf is `0`, that
/// creating a private repository moves `repos.private` and leaves `repos.public` alone (the
/// breakdown is real, not one number copied into several fields), and that an attachment adds
/// *exactly* the bytes uploaded.
///
/// It also pins the empty-collection shape. Forgejo hands back `[]` here, not `null` — a
/// distinction that matters because another suite found empty reaction collections arriving as
/// `null`, which decodes to a different thing entirely.
#[test]
fn a_fresh_accounts_quota_starts_at_zero_and_then_counts_exactly_what_it_stores() {
    let inst = instance_or_skip!();
    let owner = inst
        .scoped_user("quotaowner", &["all"])
        .unwrap_or_else(|e| panic!("a fresh account is needed to measure quota on: {e}"));
    cover!(raw: [
        "userGetQuota",
        "userListQuotaArtifacts",
        "userListQuotaAttachments",
        "userListQuotaPackages"
    ]);

    let quota = || -> serde_json::Value {
        let run = inst.fjo_as(&owner.token, ["raw", "user", "get-quota"]);
        run.assert_ok("fjo raw user get-quota");
        run.json()
    };

    let fresh = quota();
    let leaves = size_leaves(&fresh);
    assert!(
        !leaves.is_empty(),
        "no numeric size fields were found at all, so the assertions below would be vacuous: \
         {fresh}"
    );
    for (path, used) in &leaves {
        assert_eq!(*used, 0, "a brand new account already reports {used} bytes at {path}: {fresh}");
    }
    assert!(fresh["groups"].is_array(), "groups should be a list, even when empty: {fresh}");

    // Empty listings, before anything exists: `[]` and not `null`.
    for op in ["list-quota-artifacts", "list-quota-attachments", "list-quota-packages"] {
        let run = inst.fjo_as(&owner.token, ["raw", "user", op]);
        run.assert_ok(&format!("fjo raw user {op}"));
        let v = run.json();
        assert!(
            v.is_array(),
            "fjo raw user {op} returned {v} on an empty account. An empty collection must arrive \
             as [], not null: the two decode to different things and only one of them can be \
             iterated."
        );
        assert_eq!(v.as_array().map(Vec::len), Some(0), "a fresh account should own nothing: {v}");
    }

    let repo =
        OwnedRepo::create(inst, &owner.token, &owner.name, &format!("quota{}", std::process::id()));

    let with_repo = quota();
    let repos = &with_repo["used"]["size"]["repos"];
    assert!(
        repos["private"].as_u64().is_some_and(|n| n > 0),
        "a private repository was created and the private figure did not move: {with_repo}"
    );
    assert_eq!(
        repos["public"].as_u64(),
        Some(0),
        "a *private* repository was counted against the public figure, so the breakdown is not \
         per-visibility: {with_repo}"
    );

    // An attachment of known length, so the number can be checked rather than merely watched.
    let scratch = Scratch::new("quota");
    let payload = "quota accounting is measured in bytes\n";
    let file = scratch.file("attachment.txt", payload);
    let (code, body) = inst.api_as(
        &owner.token,
        "POST",
        &format!("repos/{}/issues", repo.slug),
        Some(r#"{"title":"carries an attachment"}"#),
    );
    assert!((200..300).contains(&code), "seeding the issue failed: HTTP {code}: {body}");
    upload_issue_attachment(inst, &owner.token, &repo.slug, 1, &file);

    let with_asset = quota();
    assert_eq!(
        with_asset["used"]["size"]["assets"]["attachments"]["issues"].as_u64(),
        Some(payload.len() as u64),
        "the issue-attachment figure is not the number of bytes that were uploaded ({}): {}",
        payload.len(),
        with_asset
    );
    assert_eq!(
        with_asset["used"]["size"]["assets"]["attachments"]["releases"].as_u64(),
        Some(0),
        "an *issue* attachment was billed to releases: {with_asset}"
    );

    // And now the listing has an element, which is the only way to learn that its shape decodes.
    let listed = inst.fjo_as(&owner.token, ["raw", "user", "list-quota-attachments"]);
    listed.assert_ok("fjo raw user list-quota-attachments, with one attachment");
    let listed = listed.json();
    let first = listed
        .as_array()
        .and_then(|a| a.first())
        .unwrap_or_else(|| panic!("the attachment is missing from the listing: {listed}"));
    assert_eq!(first["name"].as_str(), Some("attachment.txt"), "wrong attachment listed: {listed}");
    assert_eq!(
        first["size"].as_u64(),
        Some(payload.len() as u64),
        "the listed size disagrees with the file that was uploaded: {listed}"
    );
}

/// `GET /user/quota/check` answers a **bare JSON boolean**, not an object wrapping one.
///
/// That is the whole finding: a generated client that expected `{"result": true}` would fail to
/// decode a body that is literally `true`, and no mock would ever have said so. The unknown
/// subject is driven too, because a `true` on its own cannot show that `subject` was transported
/// at all — the server naming the value back is what proves the query parameter arrived under
/// its wire name.
#[test]
fn checking_a_quota_subject_answers_a_bare_boolean_and_names_an_unknown_one_back() {
    let inst = instance_or_skip!();
    cover!(raw: ["userCheckQuota"]);

    let ok = inst.fjo(["raw", "user", "check-quota", "--subject", "size:all"]);
    ok.assert_ok("fjo raw user check-quota --subject size:all");
    let answer = ok.json();
    assert!(
        answer.is_boolean(),
        "check-quota returned {answer}, but the route answers a bare boolean. A client that \
         expected an object here would fail to decode every successful call."
    );

    let bogus = inst.fjo(["raw", "user", "check-quota", "--subject", "not-a-quota-subject"]);
    bogus.assert_code(1, "fjo raw user check-quota, with a subject the server does not know");
    bogus.assert_says("not-a-quota-subject");
}

// --------------------------------------------------------------------------- federation

/// A federating instance, or `None` when there is no Docker — mirroring `instance_or_skip!`.
///
/// `instance_or_skip!` only knows about [`fjo_itest::shared`], and the ActivityPub routes are
/// gated behind `[federation] ENABLED`, which the shared instance deliberately does not set:
/// with federation on, Forgejo 16.0.4 answers `PUT /user/starred/{owner}/{repo}` with a 500
/// (`StarRepo: client: invalid host for HostMatcher: nil client host(s)`), which would break
/// starring for every suite. So federation gets its own container and this gets its own skip.
fn federated_or_skip() -> Option<&'static Instance> {
    match fjo_itest::federated() {
        Ok(Some(i)) => Some(i),
        Ok(None) => {
            let reason = fjo_itest::unavailable_reason();
            if std::env::var_os("FJO_ITEST_REQUIRE").is_some() {
                panic!(
                    "FJO_ITEST_REQUIRE is set but no federating instance is available: {reason}"
                );
            }
            println!("SKIPPED (federated): {reason}");
            eprintln!("SKIPPED (federated): {reason}");
            None
        }
        Err(e) => panic!("could not obtain a federating Forgejo instance: {e}"),
    }
}

/// `POST /user/activitypub/follow` reaches a handler when federation is on, and the handler
/// tells us what it did with the `target` we sent.
///
/// **It cannot be made to succeed here, and this test does not pretend otherwise.** Following a
/// remote actor makes Forgejo fetch that instance's `/.well-known/nodeinfo` first, so a green
/// path needs a second *federating and reachable* instance — two containers that can resolve
/// each other, which is a different piece of harness from the one that exists.
///
/// What it does prove is everything short of that, and none of it was reachable while the route
/// answered a router `404`: the route is registered, the method and path are right, and the
/// `target` body field arrives under its wire name — proven because the refusal quotes back the
/// host we sent it. The operation is claimed on that basis, and on nothing more.
///
/// The target is a TEST-NET-1 address on purpose. `192.0.2.0/24` is reserved for documentation,
/// so it never resolves and never routes; Forgejo's outbound allowlist refuses it at dial time
/// without a DNS lookup or a connection attempt, which makes the refusal instant and identical
/// on every machine. A hostname would make this test depend on the resolver.
#[test]
fn the_activitypub_follow_route_reaches_a_handler_and_quotes_the_target_back() {
    let Some(fed) = federated_or_skip() else { return };

    // `federated()` hands back the *attached* instance when `FJO_TEST_HOST` is set, and whether
    // somebody else's server federates is a fact to be discovered rather than arranged. Probing
    // out of band and returning before `cover!` is the same contract `instance_or_skip!` keeps:
    // a test that did not run records nothing.
    let (code, _) = fed.api("GET", "activitypub/actor", None);
    if code == 404 {
        let msg = "this instance does not federate ([federation] ENABLED is off), so \
                   /user/activitypub/follow is not registered and driving it would measure the \
                   router rather than the handler";
        println!("SKIPPED (federation off): {msg}");
        eprintln!("SKIPPED (federation off): {msg}");
        return;
    }
    assert_eq!(code, 200, "GET /activitypub/actor answered {code}, which is neither on nor off");

    cover!(raw: ["userCurrentActivityPubFollow"]);

    let target = "http://192.0.2.10:3000/api/v1/activitypub/user-id/1";
    let run = fed.fjo(["raw", "user", "current-activity-pub-follow", "--target", target]);

    // Exit 1 (the server refused) rather than 5 (no such resource, which is how a router 404
    // surfaces). That difference *is* the assertion: it separates "the route exists and
    // declined" from "the route is not there".
    run.assert_code(
        1,
        "fjo raw user current-activity-pub-follow, against an unreachable documentation address",
    );
    assert!(
        !run.stderr.contains("API endpoint not found"),
        "the route answered as if it were unregistered, so this test is measuring the router \
         rather than the handler:\n{}",
        run.stderr
    );
    // The host we sent, quoted back by the handler — the only available proof that `target`
    // was transported under the right name and read.
    run.assert_says("192.0.2.10:3000");
}
