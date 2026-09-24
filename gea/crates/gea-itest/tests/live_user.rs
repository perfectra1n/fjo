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
//!   token is answered 401 by design and `gea` has to fall back to a password. That fallback
//!   reads stdin when there is no terminal, which the shared harness cannot drive — hence
//!   [`gea_stdin`] below.

use std::collections::BTreeSet;
use std::ffi::OsStr;
use std::io::Write;
use std::path::Path;
use std::process::{Command, Stdio};
use std::time::{Duration, Instant};

use gea_itest::{Instance, Run, TestRepo, cover, instance_or_skip};

// --------------------------------------------------------------------------- fixtures

/// A public key whose user id carries [`GPG_EMAIL`], armoured once and pinned here.
///
/// Generating one at test time is not an option: `gpg` is not a dependency of this repository
/// and shelling out to a binary that may not exist would turn a coverage gap into a flake. The
/// private half was never written down, so this is inert — it proves only that we can encode an
/// `armored_public_key` the server will parse.
///
/// Gitea refuses a GPG key none of whose user-id emails is on the account, offering the
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
///
/// It says `fjo` because it is baked into the signed key packet, which was generated for this
/// suite's predecessor; renaming it here without regenerating the key makes every add a 404.
const GPG_EMAIL: &str = "fjo-gpg-itest@example.invalid";

/// The long key id Gitea derives from [`GPG_PUBLIC_KEY`].
///
/// Asserted rather than merely read back, because the fingerprint is computed from the packet
/// bytes: if our encoding of `armored_public_key` ever mangled them, the server would still
/// accept *a* key and only this constant would notice.
const GPG_KEY_ID: &str = "47AAC89E91068EA9";

/// A 1x1 transparent PNG, base64 as `POST /user/avatar` wants it.
const TINY_PNG: &str = "iVBORw0KGgoAAAANSUhEUgAAAAEAAAABCAYAAAAfFcSJAAAADUlEQVR42mP8z8BQDwAEhQGAhKmMIQAAAABJRU5ErkJggg==";

/// A scratch directory that removes itself, for the tests that have to hand `gea` a file path.
struct Scratch(std::path::PathBuf);

impl Scratch {
    fn new(tag: &str) -> Self {
        let d = std::env::temp_dir().join(format!("gea-itest-user-{}-{tag}", std::process::id()));
        let _ = std::fs::remove_dir_all(&d);
        std::fs::create_dir_all(&d).expect("a scratch directory for the key files");
        Self(d)
    }

    /// Write `contents` to `name` inside the directory and return the path, as a `String`
    /// because every use of it is an argument to `gea`.
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
/// A hand-written constant is tempting and wrong: Gitea rejects a key whose blob has already
/// been registered ("Key content has been used as non-deploy key"), so two tests sharing one
/// constant would pass alone and fail together — the worst kind of suite failure. `ssh-keygen`
/// is asserted rather than skipped past, because silently not testing the key routes is the
/// outcome this whole file exists to prevent.
fn ssh_public_key(scratch: &Scratch, tag: &str) -> String {
    let base = scratch.0.join(format!("id_{tag}"));
    let out = Command::new("ssh-keygen")
        .args(["-q", "-t", "ed25519", "-N", "", "-C"])
        .arg(format!("gea-itest-{tag}@example.invalid"))
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

/// Run `gea` with `stdin` piped in.
///
/// The harness's own runners give the child no stdin, which is right for everything else and
/// impossible for the token routes: `/users/{name}/tokens` is `reqBasicAuth()`, so `gea` falls
/// back to asking for a password, and with no terminal it reads one from stdin. Without this the
/// only reachable assertion would be "it complained there was no password", which tests our
/// error message rather than the server.
///
/// The environment is [`Instance::child_env`] verbatim plus [`Instance::HOSTILE_ENV`] cleared,
/// so this differs from `inst.gea(..)` in exactly one respect — the pipe.
fn gea_stdin<I, S>(inst: &Instance, stdin: &str, args: I) -> Run
where
    I: IntoIterator<Item = S>,
    S: AsRef<OsStr>,
{
    let mut cmd = Command::new(gea_itest::gea_bin());
    cmd.args(args).stdin(Stdio::piped()).stdout(Stdio::piped()).stderr(Stdio::piped());
    for (k, v) in inst.child_env() {
        cmd.env(k, v);
    }
    for k in Instance::HOSTILE_ENV {
        cmd.env_remove(k);
    }
    let mut child = cmd.spawn().expect("the gea binary should be executable");
    child
        .stdin
        .as_mut()
        .expect("stdin was piped")
        .write_all(stdin.as_bytes())
        .expect("writing the password to gea's stdin");
    let out = child.wait_with_output().expect("gea should finish");
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
/// Gitea writes notifications and search indexes from queues, so "the mutation landed" and
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

    let me = inst.gea(["user", "view", "--json", "login,id"]);
    me.assert_ok("gea user view");
    let me = me.json();
    assert_eq!(me["login"].as_str(), Some(inst.user.as_str()), "user view named somebody else");

    // Out of band: `user view` must agree with `GET /user`, or the command is reading a
    // different account from the one the token belongs to.
    let current = expect_json("GET /user", inst.api("GET", "user", None));
    assert_eq!(me["id"], current["id"], "user view and GET /user disagree about who we are");

    // The named form goes to `/users/{name}`, and the id has to match — a command that quietly
    // fell back to `/user` would pass every other assertion here.
    let named = inst.gea(["user", "view", &inst.user, "--json", "login,id"]);
    named.assert_ok("gea user view <name>");
    assert_eq!(named.json()["id"], current["id"], "the named lookup found a different account");

    let listed = inst.gea(["user", "list", &inst.user, "--json", "login"]);
    listed.assert_ok("gea user list");
    assert!(
        logins(&listed.json()).contains(&inst.user),
        "user list did not find the account it was asked for: {}",
        listed.stdout
    );

    let searched = inst.gea(["search", "users", &inst.user, "--json", "login"]);
    searched.assert_ok("gea search users");
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

    let before = inst.gea(["raw", "user", "get-user-settings"]);
    before.assert_ok("gea raw user get-user-settings");
    let before = before.json();
    let original_name = before["full_name"].as_str().unwrap_or_default().to_owned();
    let original_desc = before["description"].as_str().unwrap_or_default().to_owned();

    let marker = format!("live-user {}", std::process::id());
    inst.gea(["raw", "user", "update-user-settings", "--description", &marker])
        .assert_ok("gea raw user update-user-settings");

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

    inst.gea(["raw", "user", "update-user-settings", "--description", &original_desc])
        .assert_ok("restoring the description");
}

/// Uploading an avatar and deleting it again must move `avatar_url`, and move it back.
///
/// `POST /user/avatar` answers `204`, so "it worked" is unobservable from the response. Gitea
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

    inst.gea(["raw", "user", "update-avatar", "--image", TINY_PNG])
        .assert_ok("gea raw user update-avatar");
    let uploaded = url_now();
    assert_ne!(uploaded, default_url, "avatar_url did not move, so nothing was stored");

    inst.gea(["raw", "user", "delete-avatar"]).assert_ok("gea raw user delete-avatar");
    assert_eq!(url_now(), default_url, "deleting the avatar did not restore the default");
}

// --------------------------------------------------------------------------- keys

/// The SSH key lifecycle, end to end, including the two listings that are *different endpoints*
/// for the same data: `/user/keys` (authenticated, yours) and `/users/{name}/keys` (public).
///
/// A mock cannot tell those apart, and it cannot tell that the delete worked — `DELETE
/// /user/keys/{id}` answers `204`, so the only evidence is the key being gone from a fresh read.
#[test]
fn an_ssh_key_added_by_gea_appears_in_both_listings_and_is_gone_after_delete() {
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
        inst.gea(["user", "ssh-key", "add", &path, "--title", &title, "--json", "id,key"]);
    created.assert_ok("gea user ssh-key add");
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

    let mine = inst.gea(["user", "ssh-key", "list", "--json", "id,title"]);
    mine.assert_ok("gea user ssh-key list");
    assert!(ids(&mine).contains(&id), "the new key is missing from /user/keys: {}", mine.stdout);

    let public = inst.gea(["user", "ssh-key", "list", &inst.user, "--json", "id,title"]);
    public.assert_ok("gea user ssh-key list <user>");
    assert!(
        ids(&public).contains(&id),
        "the new key is missing from /users/{}/keys, so the named form is reading the wrong \
         endpoint: {}",
        inst.user,
        public.stdout
    );

    inst.gea(["raw", "user", "current-get-key", &id.to_string()])
        .assert_ok("gea raw user current-get-key");

    inst.gea(["user", "ssh-key", "delete", &id.to_string(), "--yes"])
        .assert_ok("gea user ssh-key delete");
    let (code, body) = inst.api("GET", &format!("user/keys/{id}"), None);
    assert_eq!(code, 404, "the key survived a delete that reported success: {body}");
}

/// Gitea refuses a GPG key unless one of its user-id emails is already on the account, so this
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
    // Gitea answers this with **404**, not 422, and puts its real explanation ("None of the
    // emails attached to the GPG key could be found…") in the body. `gea` maps that status to
    // "API endpoint not found" and drops the sentence, so the user is told the route does not
    // exist when in fact their key was rejected for a reason they could act on. That is the
    // failure `CONTRIBUTING.md` names as tea's worst UX bug, reproduced here — and it is why
    // this asserts on the exit code plus an out-of-band read rather than on the message: there
    // is no message left to assert on.
    let refused = inst.gea(["user", "gpg-key", "add", &key_path]);
    refused.assert_code(
        5,
        "gea user gpg-key add, for a key whose user-id email is not on the account",
    );
    let after_refusal = expect_json("GET /user/gpg_keys", inst.api("GET", "user/gpg_keys", None));
    assert!(
        !after_refusal.to_string().contains(GPG_KEY_ID),
        "the command failed but the key was stored anyway: {after_refusal}"
    );

    inst.gea(["raw", "user", "add-email", "--emails", GPG_EMAIL])
        .assert_ok("gea raw user add-email");
    let emails = inst.gea(["raw", "user", "list-emails"]);
    emails.assert_ok("gea raw user list-emails");
    let listed: BTreeSet<String> = emails
        .json()
        .as_array()
        .map(|a| a.iter().filter_map(|e| e["email"].as_str().map(str::to_owned)).collect())
        .unwrap_or_default();
    assert!(listed.contains(GPG_EMAIL), "the added email is not in the listing: {}", emails.stdout);

    let created = inst.gea(["user", "gpg-key", "add", &key_path, "--json", "id,key_id"]);
    created.assert_ok("gea user gpg-key add, once the email is on the account");
    let created = created.json();
    let id = created["id"].as_i64().expect("the server assigned the key an id");
    assert_eq!(
        created["key_id"].as_str(),
        Some(GPG_KEY_ID),
        "the server derived a different key id from the armoured block, so the bytes we sent \
         are not the bytes in this file"
    );

    let mine = inst.gea(["user", "gpg-key", "list", "--json", "id,key_id"]);
    mine.assert_ok("gea user gpg-key list");
    mine.assert_says(GPG_KEY_ID);

    let public = inst.gea(["raw", "user", "list-gpg-keys", &inst.user]);
    public.assert_ok("gea raw user list-gpg-keys");
    public.assert_says(GPG_KEY_ID);

    inst.gea(["raw", "user", "current-get-gpg-key", &id.to_string()])
        .assert_ok("gea raw user current-get-gpg-key")
        .assert_says(GPG_KEY_ID);

    // The token the server wants signed. Only its shape can be asserted — it is random per call.
    let token = inst.gea(["raw", "user", "get-verification-token"]);
    token.assert_ok("gea raw user get-verification-token");
    assert!(
        token.stdout.trim().len() >= 32,
        "the verification token looks too short to be one: {:?}",
        token.stdout
    );

    // Gitea's spec declares no body for this route, so `gea raw` has no flags for it and the
    // body travels whole through `--body-file` — the escape hatch this case is what it is for.
    let verify = scratch.file(
        "verify.json",
        serde_json::json!({
            "key_id": GPG_KEY_ID,
            "armored_signature":
                "-----BEGIN PGP SIGNATURE-----\n\nnot-a-signature\n-----END PGP SIGNATURE-----",
        })
        .to_string()
        .as_str(),
    );
    let bogus = inst.gea(["raw", "user", "verify-gpg-key", "--body-file", verify.as_str()]);
    assert!(
        !bogus.ok(),
        "an invalid signature must not verify a key:\n{}\n{}",
        bogus.stdout,
        bogus.stderr
    );
    bogus.assert_says("signature");

    inst.gea(["user", "gpg-key", "delete", &id.to_string(), "--yes"])
        .assert_ok("gea user gpg-key delete");
    let (code, body) = inst.api("GET", &format!("user/gpg_keys/{id}"), None);
    assert_eq!(code, 404, "the GPG key survived a delete that reported success: {body}");

    inst.gea(["raw", "user", "delete-email", "--emails", GPG_EMAIL])
        .assert_ok("gea raw user delete-email");
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
/// `/users/{name}/tokens` is `reqBasicAuth()`: on Gitea 1.27.3 both the `GET` and the `DELETE`
/// answer a token with `401`, so `gea` has to notice and fall back to a password. That refusal is
/// invisible to a mock, and it is the whole reason `needs_basic_auth` exists.
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
    let password = "gea-itest-token-owner-1";
    let (code, body) = inst.api(
        "POST",
        "admin/users",
        Some(&format!(
            r#"{{"username":"{name}","email":"{name}@example.invalid","password":"{password}","must_change_password":false}}"#
        )),
    );
    assert!((200..300).contains(&code) || code == 422, "could not create {name}: {code}: {body}");

    // Minted out of band with Basic auth, so that this test is about list and delete alone;
    // `creating_a_token_through_gea_hands_back_a_secret_that_authenticates` covers the create.
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

    let listed =
        gea_stdin(inst, password, ["user", "token", "list", "--username", &name, "--json", "name"]);
    listed.assert_ok("gea user token list --username, with the password on stdin");
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

    gea_stdin(
        inst,
        password,
        ["user", "token", "delete", token_name, "--username", &name, "--yes"],
    )
    .assert_ok("gea user token delete, with the password on stdin");

    // Out of band, with the account's password — Gitea refuses any token on this route, the
    // admin's included. The `DELETE` answers 204, so a fresh listing is the only proof.
    let out = Command::new("curl")
        .args(["-sS", "--max-time", "30", "-u", &format!("{name}:{password}")])
        .arg(format!("{}/users/{name}/tokens", inst.api_base()))
        .output()
        .expect("curl should run");
    let left: serde_json::Value =
        serde_json::from_slice(&out.stdout).expect("the token listing is JSON");
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

/// `gea user token create`, read back by using the secret it printed.
///
/// Token creation is another `reqBasicAuth()` route, so the password travels on stdin. The only
/// proof that the create worked is the secret authenticating as the account it was minted for —
/// a mock would hand back whatever string the fixture held. (Forgejo refused this call outright
/// while `gea` sent `"repositories": []`; Gitea's options have no such field.)
#[test]
fn creating_a_token_through_gea_hands_back_a_secret_that_authenticates() {
    let inst = instance_or_skip!();
    cover!(porcelain: ["user token create"], hits: ["userCreateToken"]);

    let name = format!("mktokuser{}", std::process::id());
    let password = "gea-itest-token-owner-2";
    let (code, body) = inst.api(
        "POST",
        "admin/users",
        Some(&format!(
            r#"{{"username":"{name}","email":"{name}@example.invalid","password":"{password}","must_change_password":false}}"#
        )),
    );
    assert!((200..300).contains(&code) || code == 422, "could not create {name}: {code}: {body}");

    let run = gea_stdin(
        inst,
        password,
        ["user", "token", "create", "wanted", "--scope", "read:user", "--username", &name],
    );
    run.assert_ok("gea user token create, with the password on stdin");
    let secret = run
        .stdout
        .lines()
        .find_map(|l| l.strip_prefix("token\t"))
        .unwrap_or_else(|| panic!("no token line in the output:\n{}", run.stdout))
        .trim()
        .to_owned();

    let who = inst.gea_as(&secret, ["api", "user", "--jq", ".login"]);
    who.assert_ok("gea api user, with the freshly minted token");
    assert_eq!(who.stdout.trim(), name, "the token authenticates as somebody else");

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

    inst.gea(["user", "follow", &other.name]).assert_ok("gea user follow");
    // And the other way, so the *followers* listings have something in them too. Without this
    // they are empty on both sides and their assertions would be vacuous.
    inst.gea_as(&other.token, ["user", "follow", &inst.user]).assert_ok("gea user follow, as them");

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
        let run = inst.gea(&args);
        run.assert_ok(&format!("gea {}", args.join(" ")));
    }
    let listed = inst.gea(["raw", "user", "list-following", &inst.user]);
    listed.assert_ok("gea raw user list-following <user>");
    assert!(
        logins(&listed.json()).contains(&other.name),
        "the public following list disagrees with /user/following: {}",
        listed.stdout
    );
    let followers = inst.gea(["raw", "user", "list-followers", &inst.user]);
    followers.assert_ok("gea raw user list-followers <user>");
    assert!(
        logins(&followers.json()).contains(&other.name),
        "the public followers list is missing the account that followed us: {}",
        followers.stdout
    );

    // The yes branch of the boolean-by-status-code protocol.
    inst.gea(["raw", "user", "current-check-following", &other.name])
        .assert_ok("gea raw user current-check-following, while following");
    inst.gea(["raw", "user", "check-following", &inst.user, &other.name])
        .assert_ok("gea raw user check-following, while following");

    inst.gea(["user", "unfollow", &other.name]).assert_ok("gea user unfollow");
    let following = expect_json("GET /user/following", inst.api("GET", "user/following", None));
    assert!(
        !logins(&following).contains(&other.name),
        "the unfollow reported success and changed nothing: {following}"
    );

    // The no branch. Exit 5 is gea's "no such resource", which is how the 404 arrives; asserting
    // on it is what pins the mapping, since the server sends no body to read.
    let gone = inst.gea(["raw", "user", "current-check-following", &other.name]);
    gone.assert_code(5, "gea raw user current-check-following, after unfollowing");
}

/// Blocking and unblocking, checked against the server's own blocklist rather than `gea`'s
/// rendering of it, and `block list` showing the account by login.
#[test]
fn blocking_an_account_lists_it_by_login_and_unblocking_removes_it() {
    let inst = instance_or_skip!();
    let target = inst
        .scoped_user("blocked", &["read:user"])
        .unwrap_or_else(|e| panic!("a second account is needed to block: {e}"));
    cover!(
        porcelain: ["block add", "block list", "block remove"],
        hits: ["userBlockUser", "userUnblockUser", "userListBlocks"]
    );

    let blocked_logins = || -> BTreeSet<String> {
        let blocked = expect_json("GET /user/blocks", inst.api("GET", "user/blocks", None));
        blocked
            .as_array()
            .map(|a| a.iter().filter_map(|u| u["login"].as_str().map(str::to_owned)).collect())
            .unwrap_or_default()
    };

    inst.gea(["block", "add", &target.name, "--note", "itest"]).assert_ok("gea block add");
    assert!(blocked_logins().contains(&target.name), "the block did not land on the server");

    let listed = inst.gea(["block", "list"]);
    listed.assert_ok("gea block list");
    listed.assert_says(&target.name);

    inst.gea(["block", "remove", &target.name]).assert_ok("gea block remove");
    assert!(
        !blocked_logins().contains(&target.name),
        "the unblock reported success and changed nothing"
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

    inst.gea(["user", "star", &repo.slug()]).assert_ok("gea user star");

    let starred = expect_json("GET /user/starred", inst.api("GET", "user/starred", None));
    let names: BTreeSet<String> = starred
        .as_array()
        .map(|a| a.iter().filter_map(|r| r["full_name"].as_str().map(str::to_owned)).collect())
        .unwrap_or_default();
    assert!(names.contains(&repo.slug()), "the star did not land on the server: {starred}");

    let mine = inst.gea(["user", "stars", "--json", "full_name"]);
    mine.assert_ok("gea user stars");
    mine.assert_says(&repo.slug());

    let theirs = inst.gea(["raw", "user", "list-starred", &inst.user, "--limit", "200"]);
    theirs.assert_ok("gea raw user list-starred <user>");
    theirs.assert_says(&repo.name);

    inst.gea(["raw", "user", "current-check-starring", &repo.owner, &repo.name])
        .assert_ok("gea raw user current-check-starring, while starred");

    inst.gea(["user", "unstar", &repo.slug()]).assert_ok("gea user unstar");
    let starred = expect_json("GET /user/starred", inst.api("GET", "user/starred", None));
    let names: BTreeSet<String> = starred
        .as_array()
        .map(|a| a.iter().filter_map(|r| r["full_name"].as_str().map(str::to_owned)).collect())
        .unwrap_or_default();
    assert!(
        !names.contains(&repo.slug()),
        "the unstar reported success and changed nothing: {starred}"
    );

    inst.gea(["raw", "user", "current-check-starring", &repo.owner, &repo.name])
        .assert_code(5, "gea raw user current-check-starring, after unstarring");
}

/// Watching, which Gitea calls a subscription and hangs off the *repository* rather than the
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
    inst.gea(["raw", "user", "current-check-subscription", &repo.owner, &repo.name])
        .assert_ok("gea raw user current-check-subscription, on a freshly created repository");

    inst.gea(["user", "unwatch", &repo.slug()]).assert_ok("gea user unwatch");
    inst.gea(["raw", "user", "current-check-subscription", &repo.owner, &repo.name])
        .assert_code(5, "gea raw user current-check-subscription, after unwatching");

    inst.gea(["user", "watch", &repo.slug()]).assert_ok("gea user watch");
    let sub = expect_json("GET /repos/<slug>/subscription", repo.api("GET", "subscription", None));
    assert_eq!(
        sub["subscribed"],
        serde_json::json!(true),
        "watch reported success but did not subscribe: {sub}"
    );

    let mine = inst.gea(["raw", "user", "current-list-subscriptions", "--limit", "200"]);
    mine.assert_ok("gea raw user current-list-subscriptions");
    mine.assert_says(&repo.name);

    let public = inst.gea(["raw", "user", "list-subscriptions", &inst.user, "--limit", "200"]);
    public.assert_ok("gea raw user list-subscriptions <user>");
    public.assert_says(&repo.name);
}

// --------------------------------------------------------------------------- notifications

/// A notification thread walked through every state the commands can put it in.
///
/// The inbox has to be filled by **somebody else**: Gitea does not notify you about your own
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
        let run = inst.gea(["notification", "list", "-R", &repo.slug(), "--json", "id"]);
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

    inst.gea(["notification", "read", &id_s]).assert_ok("gea notification read <id>");
    assert_eq!(
        thread("unread"),
        serde_json::json!(false),
        "read reported success and changed nothing"
    );

    inst.gea(["notification", "unread", &id_s]).assert_ok("gea notification unread <id>");
    assert_eq!(
        thread("unread"),
        serde_json::json!(true),
        "unread reported success and changed nothing"
    );

    inst.gea(["notification", "pin", &id_s]).assert_ok("gea notification pin <id>");
    assert_eq!(
        thread("pinned"),
        serde_json::json!(true),
        "pin reported success and changed nothing"
    );

    // `notification list` with no `-R` is a different endpoint (`/notifications`, not
    // `/repos/{o}/{r}/notifications`), so the pinned thread has to be findable through it too.
    let pinned =
        inst.gea(["notification", "list", "-s", "pinned", "--limit", "100", "--json", "id"]);
    pinned.assert_ok("gea notification list -s pinned");
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

    inst.gea(["notification", "unpin", &id_s]).assert_ok("gea notification unpin <id>");
    assert_eq!(
        thread("pinned"),
        serde_json::json!(false),
        "unpin reported success and changed nothing"
    );

    inst.gea(["raw", "notify", "get-thread", &id_s]).assert_ok("gea raw notify get-thread");
}

// --------------------------------------------------------------------------- search

/// `search repos --owner` resolves the owner's *name* to a *uid*, because `/repos/search` takes
/// an id — one command, two calls, and the lookup is the half a mock cannot check.
///
/// `search issues` is in the same test because both read from indexes Gitea populates
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
        let run = inst.gea(["search", "repos", &repo.name, "--json", "full_name"]);
        run.ok() && run.stdout.contains(&repo.slug())
    };
    assert!(found_repo(), "a repository that exists was not findable by name");

    // The `--owner` form is the one with the extra lookup. A wrong uid returns an empty list
    // rather than an error, which is why this asserts on the contents and not the exit code.
    let owned =
        inst.gea(["search", "repos", &repo.name, "--owner", &inst.user, "--json", "full_name"]);
    owned.assert_ok("gea search repos --owner");
    owned.assert_says(&repo.slug());

    let found_issue = || {
        let run = inst.gea(["search", "issues", &needle, "--json", "title"]);
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
        "deleteUserSecret"
    ]);

    let var = format!("live_user_var_{}", std::process::id());
    let secret = format!("LIVE_USER_SECRET_{}", std::process::id());

    inst.gea(["raw", "user", "create-user-variable", &var, "--value", "first"])
        .assert_ok("gea raw user create-user-variable");

    let stored = inst.gea(["raw", "user", "get-user-variable", &var]);
    stored.assert_ok("gea raw user get-user-variable");
    assert_eq!(stored.json()["data"].as_str(), Some("first"), "the value did not round trip");

    inst.gea(["raw", "user", "update-user-variable", &var, "--name", &var, "--value", "second"])
        .assert_ok("gea raw user update-user-variable");
    let stored = inst.gea(["raw", "user", "get-user-variable", &var]);
    stored.assert_ok("gea raw user get-user-variable, after the update");
    assert_eq!(
        stored.json()["data"].as_str(),
        Some("second"),
        "the update reported success and changed nothing"
    );

    // Contains, never equals: another test in this binary may own variables of its own.
    let listed = inst.gea(["raw", "user", "get-user-variables-list", "--limit", "200"]);
    listed.assert_ok("gea raw user get-user-variables-list");
    listed.assert_says(&var.to_uppercase());

    inst.gea(["raw", "user", "delete-user-variable", &var])
        .assert_ok("gea raw user delete-user-variable");
    let (code, body) = inst.api("GET", &format!("user/actions/variables/{var}"), None);
    assert_eq!(code, 404, "the variable survived a delete that reported success: {body}");

    // A secret has no read-back route by design, so the delete is the only proof it existed.
    inst.gea(["raw", "user", "update-user-secret", &secret, "--data", "s3cret-value"])
        .assert_ok("gea raw user update-user-secret");
    inst.gea(["raw", "user", "delete-user-secret", &secret])
        .assert_ok("gea raw user delete-user-secret, which can only succeed if the secret existed");
    let (code, body) = inst.api("DELETE", &format!("user/actions/secrets/{secret}"), None);
    assert_eq!(code, 404, "the secret survived a delete that reported success: {body}");
}

/// The user runner lifecycle, which needs no runner process: the registration token is minted
/// through `gea raw`, redeemed the way `act_runner register` would, and the runner is then read,
/// disabled, filtered for and deleted — see [`gea_itest::Instance::drive_runner_lifecycle`].
///
/// Bug this prevents: a `{runner_id}` rendered into the wrong slot, which against a real server
/// is a 404 and against a mock is whatever the fixture says. The job and run listings ride along
/// because an empty answer is a wrapper object with a zero count, not `null` and not a 404.
#[test]
fn a_registered_user_runner_is_listed_until_it_is_deleted() {
    let inst = instance_or_skip!();
    cover!(raw: [
        "userCreateRunnerRegistrationToken", "getUserRunners", "getUserRunner", "updateUserRunner",
        "deleteUserRunner", "getUserWorkflowJobs", "getUserWorkflowRuns",
    ]);

    inst.drive_runner_lifecycle("user", "user", &[]);

    // The harness admin owns every other test binary's repositories, some of which dispatch
    // workflows, so only the shape is asserted: a count beside a list, never `null`.
    for (cmd, key) in
        [("get-user-workflow-jobs", "jobs"), ("get-user-workflow-runs", "workflow_runs")]
    {
        let listed = inst.gea(["raw", "user", cmd]);
        listed.assert_ok(&format!("gea raw user {cmd}"));
        let listed = listed.json();
        assert!(listed["total_count"].is_u64(), "no count in {listed}");
        assert!(listed[key].is_array() || listed[key].is_null(), "no {key} in {listed}");
    }
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

    let created = inst.gea([
        "raw",
        "user",
        "create-hook",
        "--type",
        "gitea",
        "--config",
        &config,
        "--events",
        "push",
        "--active",
    ]);
    created.assert_ok("gea raw user create-hook");
    let created = created.json();
    let id = created["id"].as_i64().expect("the server assigned the hook an id");
    assert_eq!(
        created["config"]["url"].as_str(),
        Some(url.as_str()),
        "the hook's config did not survive encoding: {created}"
    );

    let listed = inst.gea(["raw", "user", "list-hooks", "--limit", "200"]);
    listed.assert_ok("gea raw user list-hooks");
    listed.assert_says(&url);

    let one = inst.gea(["raw", "user", "get-hook", &id.to_string()]);
    one.assert_ok("gea raw user get-hook");
    assert_eq!(
        one.json()["active"],
        serde_json::json!(true),
        "the hook should have been created active"
    );

    inst.gea(["raw", "user", "edit-hook", &id.to_string(), "--active=false"])
        .assert_ok("gea raw user edit-hook");
    let after = expect_json(
        &format!("GET /user/hooks/{id}"),
        inst.api("GET", &format!("user/hooks/{id}"), None),
    );
    assert_eq!(
        after["active"],
        serde_json::json!(false),
        "the edit reported success and changed nothing"
    );

    inst.gea(["raw", "user", "delete-hook", &id.to_string()]).assert_ok("gea raw user delete-hook");
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
        "userGetOauth2Application",
        "userGetOAuth2Application",
        "userUpdateOAuth2Application",
        "userDeleteOAuth2Application"
    ]);

    let name = format!("live-user-app-{}", std::process::id());
    let created = inst.gea([
        "raw",
        "user",
        "create-oauth2-application",
        "--name",
        &name,
        "--redirect-uris",
        "http://app.example.invalid/callback",
        "--confidential-client",
    ]);
    created.assert_ok("gea raw user create-oauth2-application");
    let created = created.json();
    let id = created["id"].as_i64().expect("the server assigned the application an id");
    assert!(
        created["client_secret"].as_str().is_some_and(|s| !s.is_empty()),
        "create must return the client secret; it is the only time it is shown: {created}"
    );

    let one = inst.gea(["raw", "user", "get-oauth2-application", &id.to_string()]);
    one.assert_ok("gea raw user get-oauth2-application");
    let one = one.json();
    assert_eq!(one["name"].as_str(), Some(name.as_str()), "get returned another application");
    assert_eq!(
        one["client_secret"].as_str(),
        Some(""),
        "a read returned a client secret, which contradicts Gitea storing only its hash: {one}"
    );

    let listed = inst.gea(["raw", "user", "list-oauth2-applications", "--limit", "200"]);
    listed.assert_ok("gea raw user list-oauth2-applications");
    listed.assert_says(&name);

    let renamed = format!("{name}-renamed");
    inst.gea([
        "raw",
        "user",
        "update-oauth2-application",
        &id.to_string(),
        "--name",
        &renamed,
        "--redirect-uris",
        "http://app.example.invalid/callback2",
    ])
    .assert_ok("gea raw user update-oauth2-application");
    let after = expect_json(
        &format!("GET /user/applications/oauth2/{id}"),
        inst.api("GET", &format!("user/applications/oauth2/{id}"), None),
    );
    assert_eq!(
        after["name"].as_str(),
        Some(renamed.as_str()),
        "the update reported success and changed nothing"
    );

    inst.gea(["raw", "user", "delete-oauth2-application", &id.to_string()])
        .assert_ok("gea raw user delete-oauth2-application");
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
        let run = inst.gea(&args);
        run.assert_ok(&format!("gea {}", args.join(" ")));
        // Parsed, not merely exited 0: a body we cannot decode is the failure mode that matters
        // for a generated client, and it is invisible to an exit-code check.
        run.json();
    }

    inst.gea(["raw", "user", "tracked-times", &repo.owner, &repo.name, &inst.user])
        .assert_ok("gea raw user tracked-times")
        .json();

    // The two repository listings are different endpoints; both must see a repository this
    // account definitely owns.
    let mine = inst.gea(["raw", "user", "current-list-repos", "--limit", "200"]);
    mine.assert_ok("gea raw user current-list-repos");
    mine.assert_says(&repo.name);

    let public = inst.gea(["raw", "user", "list-repos", &inst.user, "--limit", "200"]);
    public.assert_ok("gea raw user list-repos <user>");
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
    inst.gea_in(outside, ["user", "star", &repo.slug()])
        .assert_ok("gea user star <slug>, from a directory with no git repository");

    let starred = expect_json("GET /user/starred", inst.api("GET", "user/starred", None));
    let names: BTreeSet<String> = starred
        .as_array()
        .map(|a| a.iter().filter_map(|r| r["full_name"].as_str().map(str::to_owned)).collect())
        .unwrap_or_default();
    assert!(names.contains(&repo.slug()), "the star did not land on the server: {starred}");

    inst.api("DELETE", &format!("user/starred/{}", repo.slug()), None);
}
