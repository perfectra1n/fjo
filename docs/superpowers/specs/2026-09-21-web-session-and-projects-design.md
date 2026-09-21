# Layer 0: a web session for the routes Forgejo has no API for

Status: design, approved in conversation 2026-09-21. Verified against Forgejo **16.0.5** by reading
`routers/web/web.go`, `routers/web/repo/projects.go`, `routers/web/shared/project/column.go`,
`routers/web/auth/auth.go`, `templates/projects/view.tmpl` and `web_src/js/features/repo-projects.js`
at tag `v16.0.5`, and by probing `code.perfectra1n.com`. The routes and markup below were then **confirmed
empirically** by driving a 16.0.5 container: creating a board, adding a column, assigning issues
and moving cards, all with plain `curl` and no CSRF token. Two facts in this document's first
draft were wrong, and are corrected throughout rather than quietly overwritten:

* the session cookie is named **`session`**, not Gitea's `i_like_gitea`; and
* the web forms take **lowercase** field names (`title`, `content`, `template_type`,
  `card_type`, `user_name`, `password`, `remember`, `passcode`), not the capitalised Go struct
  field names declared in `services/forms/repo_form.go`.

## The problem

Forgejo's Projects (kanban boards) have no REST API. The served spec has zero `project` paths, the
obvious `/api/v1/...projects` routes 404, and the web routes answer a valid site-admin API token with
`303 → /user/login`. The upstream implementation ([forgejo#9384](https://codeberg.org/forgejo/forgejo/pulls/9384))
is blocked behind #9380 with no date.

Every layer `fjo` has descends from `spec/forgejo-v16.0.5.json`, so nothing in the tool can reach a
board today, and the bijection test in `crates/forgejo-client/src/generated/meta/invariants.rs`
correctly refuses a hand-written operation. Projects are the first customer; issue content-history
(`GET /{o}/{r}/issues/{n}/content-history/*`, JSON) is the second, and any future web-only surface is
the third.

## What Forgejo's web layer actually is (16.0.5)

These facts drive every decision below.

- **Writes are JSON endpoints.** Every mutating project route ends in `ctx.JSONOK()`. The two that
  matter take JSON bodies, exactly what the browser sends:
  - `POST /{o}/{r}/projects/{id}/{columnID}/move` — `{"issues":[{"issueID":n,"sorting":i},…]}`,
    where `issueID` is the issue's **internal database id**, not its per-repo number
  - `POST /{o}/{r}/projects/{id}/move` — `{"columns":[{"columnID":n,"sorting":i},…]}`
  - `POST /{o}/{r}/issues/projects?id={projectID}` — form `issue_ids` as a **comma-joined**
    list (`issue_ids=1,2,3`), not repeated parameters; issues land in the board's default column
  - `POST …/projects/new` (form `title`, `content`, `template_type`, `card_type` — where
    `template_type` 0=none, 1=basic kanban, 2=bug triage, and 1 creates Backlog/To Do/In
    Progress/Done); `POST …/projects/{id}` (add column: `title`, `sorting`, `color`);
    `PUT`/`DELETE …/projects/{id}/{columnID}`;
    `POST …/projects/{id}/{open|close|delete}`; `POST …/projects/{id}/{columnID}/default`.
  - The same tree exists for org and user boards under `/{owner}/-/projects/…`.
- **Reads are HTML.** `ViewProject` renders `templates/projects/view.tmpl`; there is no JSON board
  read. The template is built for the JS to drive, so it carries `id="board_{{.ID}}"`,
  `data-url="{{$.Link}}/{{.ID}}"`, `data-sorting`, `data-project`, and one card per issue linking to
  `/{o}/{r}/issues/{index}`.
- **There is no CSRF token.** `web.go:250` installs Go's `http.NewCrossOriginProtection()`. It decides
  by `Sec-Fetch-Site` / `Origin`; a request carrying neither "is assumed to be either same-origin or
  non-browser and is allowed" (Go docs). `GET`/`HEAD`/`OPTIONS` are never checked. The login form has
  no `_csrf` field for the same reason.
- **Authentication is a token, not a password.** `POST /user/login` with `remember=on` sets a
  `persistent` cookie whose value is a long-term authorization token (`auth_token` table, purpose
  `long_term_authorization`, lifetime `LOGIN_REMEMBER_DAYS`, default 31). `GET /user/login` with that
  cookie runs `autoSignIn()` (`auth.go:155`) and mints a fresh `session` cookie. The token is
  not rotated on auto-sign-in.
- **2FA.** TOTP → `303 → /user/two_factor`, a second form post (`passcode`), after which the
  `persistent` cookie is issued. WebAuthn → `303 → /user/webauthn`; there is no headless completion.
- **Unauthenticated is a `303`, never a `401`.** Any web route answers `303 → /user/login` when the
  session is missing or dead. A client that follows redirects would see a `200` login page and
  believe the command succeeded.

## What this adds

Three things, at three layers, plus one storage change.

```
fjo web <path>           layer 0   web-root, cookie-authenticated     any route under /
fjo api <path>           layer 1   generated from nothing             any path under /api/v1
fjo raw <group> <op>     layer 2   generated                          506 operations
fjo <noun> <verb>        layer 3   hand-written porcelain             + fjo project
```

Layers 1–3 are pinned to the vendored spec. Layer 0 is pinned to Forgejo's **source** at the same
tag, and the pin is enforced the same way: the live suite runs against
`codeberg.org/forgejo/forgejo:16.0.5`, and every `fjo project` leaf is `cover!`'d so the
`coverage-check` ratchet refuses one that was never driven. `docs/layers.md` gains the row above and
says this plainly.

### `fjo auth login --with-password`

Mirrors `--with-token`: the password is read from stdin, or from a hidden prompt when stdin is a TTY.
It is never on argv, never stored, and — per the `auth/common.rs` rule — never bound to a named
`String`. The result is a **web session** credential stored *beside* any API token for the same login.

1. `POST /user/login` form `user_name`, `password`, `remember=on`, redirects off.
2. Classify:
   - `303` with `Set-Cookie: persistent=…; Max-Age=N` — success. **Expiry is `now + Max-Age`**, read
     off the header, never assumed to be 31 days.
   - `303 → /user/two_factor` — `POST /user/two_factor` with `passcode` from the global `--otp`, or
     a TTY prompt. The `persistent` cookie arrives on *this* response.
   - `303 → /user/webauthn` — refuse: "this account requires WebAuthn, which cannot be completed
     without a browser; enrol TOTP alongside it, or use a dedicated bot account".
   - `200` — the login failed; the page's `flash-error` text ("Username or password is incorrect")
     is the error's `problem:` line.
3. Verify before saving, as `auth login` does for tokens: `GET /user/login` with only the
   `persistent` cookie must answer `303` away from the login page. Then store.

`auth status` shows both credentials per login (`web session: keyring, until 2026-10-22`).
`auth logout` clears both. `fjo auth token --web` prints the current session cookie for a `curl`.

### `fjo web <path>` — layer 0

`fjo api`'s exact flag set (`-X`, `-f`/`--raw-field`, `-F`/`--field`, `--input`, `-H`, `-i`,
`--verbose`), the same `{owner}`/`{repo}` substitution, the same `--json`/`--jq`/`--template`
pipeline. The path is rooted at `/` rather than `/api/v1`. A JSON response goes through the
pipeline; anything else is written through raw, and `--json` on a non-JSON body is a usage error
that says so. `--paginate` is refused (web routes do not carry `Link` headers).

```
fjo web POST AtvikSecurity/VulnCorp/projects/3/12/move --input cards.json
fjo web GET  AtvikSecurity/VulnCorp/projects/3 > board.html
```

`fjo web` never warns about server versions. It is the escape hatch.

### `fjo project` — layer 3

Every verb takes **titles and issue numbers**, never ids: the routes are all `/projects/{id}/{columnID}`
and nobody knows those numbers. An ambiguous title is refused, not guessed — Forgejo allows two
projects called `Roadmap`, as it allows two milestones called `1.0`.

```
fjo project list [-s open|closed|all] [--owner ORG]
fjo project view <title> [-w]
fjo project create <title> [--description TEXT] [--template none|basic-kanban|bug-triage]
fjo project close|reopen|delete <title>
fjo project column add    <project> <title> [--color '#rrggbb']
fjo project column edit   <project> <title> [--title NEW] [--color …]
fjo project column delete <project> <title>
fjo project column move   <project> <title> --before <other> | --after <other> | --first | --last
fjo project card add  <issue-number> --project <title> [--column <title>]
fjo project card move <issue-number> --to <column>
```

- `-R`/cwd select the repository board; `--owner` selects the org/user board at `/{owner}/-/projects`.
  Same verbs, one flag.
- `card move 42 --to Done` needs no `--project`: an issue sits on at most one board per repository,
  so the board is found from the issue. This is the CI use case in one line.
- `view` renders columns → cards as `#42  Title`, titles taken from the board HTML (no extra
  calls). `--json` yields `{id,title,state,columns:[{id,title,color,cards:[{number,title}]}]}`.
- `-w` opens the board in the browser, the fixed meaning of `--web` in `porcelain-conventions.md`.
- **Interactive fallback.** A web command with no session and a TTY on stdin prompts for the
  password, stores the session, and continues. Without a TTY the error names the command to run.

What each verb earns over `fjo web`, per the conventions: context inference (repo, board from
issue), orchestration (title → id resolution, HTML read then JSON write), interactive fallback,
and rendering.

### `fjo auth export --web` / `fjo auth import --web`

A web session is worth moving between machines, because signing in again on every CI run defeats
the point of a credential that lasts a month.

**What moves is the remember token**, not the session cookie. The session lapses in about a day
(`SESSION_LIFE_TIME`, default 86400), so exporting only that yields a job that works this
afternoon and fails tomorrow. The whole document goes, and the receiving machine mints its own
sessions from it for the remember token's remaining life.

```
fjo auth export --web | gh secret set FJO_WEB_SESSION     # provision CI
FJO_WEB_SESSION=... fjo project card move 42 --to Done    # CI needs no import step
fjo auth import --web < session.json                      # or store it on a workstation
```

`--web` is the only mode. API tokens are deliberately **not** exportable: `FJO_TOKEN` with a
scoped PAT created in the web UI is already the better CI story, and an exported PAT would lose
the scope record `Login.scopes` keeps — which is the only reason an `InsufficientScope` error can
say what the token actually has.

Two guards, both because of what this credential is. It authenticates the **whole account** for
~31 days and cannot be scoped the way a PAT can:

* export refuses to write to a terminal without `--force`, reusing the global flag that already
  means exactly that. A full-account credential scrolling into terminal scrollback is how one
  ends up in a screen recording;
* export prints one line to **stderr** — never stdout, which must stay a clean pipe — naming what
  the value is, when it lapses, and that a dedicated bot account is the right thing for CI.

Import verifies before storing, the same contract `auth login` has: a document that cannot mint a
session is refused rather than filed away to fail later somewhere else.

No `import` is needed for CI. The store precedence is unchanged — env, then file, then keyring —
so `FJO_WEB_SESSION` is read directly. `import` exists for a workstation where the keyring is
wanted instead.

## The session lifecycle

Principle: **never guess server configuration; observe and self-heal.** `SESSION_LIFE_TIME` and
`LOGIN_REMEMBER_DAYS` are unknowable from a client; the only signal used is the server's own
`303 → /user/login`.

Stored credential (one JSON document, the `StoredOauth` pattern — a long-lived token minting a
short-lived one):

```json
{"v":1,"kind":"web-session","user":"perfectra1n","remember":"…",
 "remember_expires_at":"2026-10-22T08:24:00Z","session":"…"}
```

The first draft carried a `session_minted_at`, and it was dropped in implementation because
nothing may read it: a client cannot know the server's `SESSION_LIFE_TIME`, so a minted-at
timestamp could only feed the local guess this design exists to refuse.

`WebSession::ensure`, run by every web request:

1. Send with the cached `session` cookie. No TTL guess.
2. On `303 → /user/login`: re-mint **once** — `GET /user/login` with the `persistent` cookie, capture
   the new `session` cookie, persist it, retry the request **once**. This covers session expiry, a
   server restart under `PROVIDER = memory`, and a session revoked from the UI.
3. If the re-mint is itself bounced, the remember token is dead (expired, "log out everywhere",
   password changed). Fail with `web session for <host> has expired — run
   `fjo auth login --with-password``, classified as an authentication failure like a `401`.
4. When `remember_expires_at` is within three days, print one stderr line naming the same command.
   This is the one failure that cannot self-heal, so it is announced ahead of time; a cron job's
   log then shows the warning before it shows the failure.
5. Two concurrent processes may each mint a session. Forgejo allows many sessions; the credential
   write is last-writer-wins and either winner is valid.

The persist-before-use rule from `oauth_refresh.rs` applies verbatim: the new session is written
before the retried request is sent.

## Transport rules

All structural — none is a convention a caller has to remember.

- `WebClient` owns a **second `ReqwestTransport` with redirects off**. reqwest's redirect policy is
  per-client, the API client must keep following, and a followed `303` discards the very
  `Set-Cookie` and the very auth signal this layer depends on.
- It is built **without the API credential**, the way `Client::anonymous` is. A web session can
  never be aimed at a host that harvests the token, and the host-pinning check from
  `Client::web_request` (prefix + `/`/`?`/`#` boundary) applies to every URL.
- It sends **no `Origin` and no `Sec-Fetch-Site`**. A hermetic test asserts their absence on every
  request the client composes, because that absence is the whole `CrossOriginProtection` contract.
- `Cookie` values pass through `http/redact.rs` in `--debug` traces, error facts and panics.
- The existing `RetryPolicy` applies to replayable bodies. The JSON `move` bodies *set* an order,
  so a network-level retry is idempotent.
- Bodies are `Body::Form` for the login and column/project forms, `Body::Json` for moves — both
  variants exist.

## Credential storage

`CredStore::{get,set,delete}` gain a `Slot` parameter, `Slot::Api` (today's behaviour) or
`Slot::Web`. The three stores key it as:

| Store | `Slot::Api` (unchanged) | `Slot::Web` |
| --- | --- | --- |
| keyring | account `{login}@{host}` (unchanged, forever) | account `web:{login}@{host}` |
| file (`hosts.toml`) | `Login.token` | `Login.web_session` (new field, `opt_secret`) |
| env | `FORGEJO_TOKEN` | `FJO_WEB_SESSION` — the JSON document, for CI |

`Login` is a typed struct with no catch-all, so **an older `fjo` saving `hosts.toml` drops
`web_session`**. The consequence is one re-login, not corruption, and `auth status` reports the slot
as empty rather than broken. This is documented on the field, next to the existing note on `kind`.

`Credentials::store` for `Slot::Web` does not call `hosts.add_login` with a new identity; the
login already exists (or is created with no token, exactly as today when only a web session is
held). `forget` clears both slots.

## Board parsing

One function, `parse_board(html: &str) -> Result<Board>`, in `crates/fjo/src/cmd/project/parse.rs`.
It scans for the template's hooks (`id="board_N"`, the column `data-url`, the `data-sorting`, the
column title element, and each card's `/issues/{index}` link and title) with `regex`, which is
**already in the dependency graph**; `scraper`/`html5ever` would cost ~1.5–2 MB against a
`SIZE_BUDGET` with 384 KiB of headroom and are not added. HTML entities in titles are decoded for
the five named entities and numeric references; nothing else appears in a title.

The parser is tested against **fixture HTML captured from the 16.0.5 image**
(`crates/fjo/tests/fixtures/projects/view-16.0.5.html`) as an `insta` snapshot, and the live suite
parses a real board it just built. A parse failure is an error whose facts include `verified against: 16.0.5`
(`forgejo_core::web::VERIFIED_AGAINST`) and the server's own version, fetched from
`/api/v1/version` **only on this failure path**. There is no version warning on success: a warning about a risk that did not
materialise is noise, and layers 1–3 do not editorialise about the spec pin either.

## Failure handling

| Situation | Behaviour |
| --- | --- |
| No web session, TTY | Prompt for password, store, continue |
| No web session, no TTY | Error naming `fjo auth login --with-password`; auth exit code |
| Session dead, remember token live | Re-mint once, retry once, silent |
| Remember token dead | Error naming the login command; auth exit code |
| Remember token expiring ≤ 3 days | One stderr line, command proceeds |
| Bad password | Forgejo's flash-error text as `problem:` |
| TOTP required | Second leg with `--otp` or prompt |
| WebAuthn required | Refuse with the bot-account / TOTP advice |
| Ambiguous project or column title | Refuse, list the matches, ask for `--id` (the one place an id is accepted) |
| Board HTML does not parse | Error with server version vs verified version |
| `fjo web` gets HTML with `--json` | Usage error: "this route answered HTML; drop --json or add -i" |
| Server answers `403` JSON `{message}` | The message as `problem:`, permission exit code |

## What is deliberately not done

- No `Auth::Cookie` variant. `Credentials::apply` is one-header-per-request; a session mint does
  not fit it, and it would attach the cookie to `/api/v1` calls.
- No new crate. The session and client are a `forgejo_core::web` module beside `oauth/`, which
  they mirror; the parser is porcelain code. Reconsider a `forgejo-web` crate when a second
  scraped surface arrives.
- No new dependencies. Binary growth is tens of KiB; startup is unaffected (nothing here runs
  before argument parsing).
- No caching of `GET /api/v1/version`, no background renewal, no multi-repo board views.
- Content-history commands. They are the natural second customer of `fjo web` and are a separate,
  smaller spec.

## Verification

Three planes, matching `docs/ratchets.md` and the coverage design:

1. **Hermetic** (`mise run test`): `FakeTransport` + `Canned::html` drive the session state machine
   (fresh → dead → re-mint → retry; dead remember token), the four login outcomes, the
   header-absence assertion, redaction, and `parse_board` against the fixture.
2. **Live** (`mise run itest`, Docker): against the pinned image, using `Instance::web_password()`:
   `auth login --with-password` end to end; `project create/column add/card add/card move/view`
   round-trip, asserting the board through both `view --json` and the HTML the server serves; a
   re-mint test that invalidates the session server-side (`DELETE` the session row via the
   container, or `PROVIDER = memory` restart) and asserts silent recovery; `fjo web` posting a raw
   move. Attach mode (`FJO_TEST_HOST`, no password) skips these. Every leaf is `cover!`'d.
3. **Pin**: the image tag in `crates/fjo-itest/src/lib.rs` is the pin. A Forgejo bump that changes
   a route or the template turns the live suite red before it reaches a user.

Ratchets touched: `coverage-check` (new leaves must be driven), `budget-check` (no dep growth),
`panic-check`/`complexity-check` unchanged budgets.

## Retirement plan

When upstream #9384 lands and `cargo xtask update-spec` generates `raw project …`, `fjo project`
re-targets to layer 2 and `parse.rs` and its fixture are deleted. `fjo web`, the session, and the
storage slot stay: they exist for every web-only surface, not for this one.

## Files

```
crates/forgejo-core/src/web/{mod,stored,login,session,client}.rs     new
crates/forgejo-core/src/config/secrets.rs                             Slot on CredStore
crates/forgejo-core/src/config/hosts.rs                               Login.web_session
crates/fjo/src/web.rs                                                 layer 0 (shares api/fields.rs)
crates/fjo/src/cmd/project/{mod,parse,resolve,view}.rs                new
crates/fjo/src/cmd/auth/{login,status,logout,token}.rs                --with-password, both slots
crates/fjo/src/cmd/mod.rs, main.rs                                    register `project`, `web`
crates/fjo/tests/fixtures/projects/view-16.0.5.html                   captured fixture
crates/fjo-itest/tests/live_projects.rs                               new
docs/layers.md, docs/porcelain-conventions.md                         layer 0 row; `--owner`
```

## Scope note

This is one implementation plan. Content-history, and any decision to prefer a server-side
project API when one is detected, are follow-ups and are not designed here.
