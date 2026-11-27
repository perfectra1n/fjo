# OAuth login

`gea auth login --web` obtains a credential through the browser instead of asking the user to paste a personal access token. It is opt-in: bare `gea auth login` still prompts for a token, and `GEA_TOKEN` / `GITEA_TOKEN` are unchanged.

```
gea auth login --host gitea.com --web
gea auth login --host git.example.org --web --no-browser   # over SSH
```

## Why it exists

A token login sends the user to a settings page to tick scopes, copy a secret, and paste it into a terminal. The secret then never expires, so a leaked one is leaked forever. An OAuth session expires in an hour, renews itself, and never crosses the clipboard.

It also works on instances behind an external identity provider without gea knowing anything about it. The browser round trip goes to Gitea, which delegates to Authentik or Keycloak or whatever else and delegates back. A token minted by that provider directly would **not** work — Gitea's API only accepts credentials Gitea itself issued.

## What Gitea implements

Every item here was established by reading Gitea's source or measuring a running instance, and several are not what someone who knows OAuth would assume.

| | |
| --- | --- |
| Grants | `authorization_code` and `refresh_token` only |
| Device flow | **None** |
| PKCE | `S256`, and **mandatory** for public clients |
| Authorize | `/login/oauth/authorize` |
| Token | `/login/oauth/access_token` — note, not `/token` |
| Discovery | `/.well-known/openid-configuration` |
| Access token | one hour by default (`ACCESS_TOKEN_EXPIRATION_TIME`) |
| Refresh token | 730 hours, about a month (`REFRESH_TOKEN_EXPIRATION_TIME`), and **rotated on every refresh** |
| Scopes | optional; a token requested without one is granted `all`, so it can do anything the user can |
| `scope` parameter | optional, so gea omits it and receives no `id_token` |
| Auth header | `Authorization: Bearer <jwt>`, where a personal access token uses `Authorization: token <hex>` |

Three of these shape the implementation more than the rest.

**The redirect URI must carry no path.** Gitea compares redirect URIs by exact string after uppercasing and trimming one trailing slash. For a public client on `http` at a loopback IP it first strips the *port* and compares again — but not the path. The built-in applications register `http://127.0.0.1`, so `http://127.0.0.1:45231` matches and `http://127.0.0.1:45231/callback` does not. Appending the `/callback` that most OAuth guides use breaks every login with a generic `redirect_uri_mismatch` that says nothing about paths. `127.0.0.1` and not `localhost`, too: the loopback special case parses the host as an IP address, and a name is not one.

**Refresh tokens rotate.** Every refresh mints a new one, and whether the previous one keeps working depends on the instance's `INVALIDATE_REFRESH_TOKENS`. So gea writes the new refresh token *before* using the access token that came with it: if that write fails and the token is used anyway, the session works for an hour and is then unrecoverable, because the token that would have renewed it was never recorded.

**There is no device flow.** A machine with no browser cannot complete a self-contained login. `--no-browser` prints the URL, the user opens it wherever they have a browser, Gitea redirects to `127.0.0.1` *there* and fails to connect, and the user pastes the resulting URL back. That is the whole of the headless story until Gitea implements the device grant.

## Which client ID

By default, Gitea's built-in `git-credential-oauth` application, which every stock instance registers with `http://127.0.0.1` as a redirect URI and no client secret. That means a login against an unmodified instance needs no setup.

It is not gea's own ID, and **the consent screen will say "git-credential-oauth"**. Borrowing it is a deliberate trade: zero-setup login anywhere, at the cost of a consent screen naming the wrong tool. The long-term fix is an upstream change adding `gea` to Gitea's `BuiltinApplications()`.

An administrator can switch the built-in applications off through `[oauth2] DEFAULT_APPLICATIONS`. For an instance that registers its own application instead:

```
gea auth login --host git.example.org --web --client-id <ID>
gea config set oauth_client_id <ID> --host git.example.org   # remembered per host
```

## How the credential is stored

As one JSON document in the usual credential store — the OS keyring, or `hosts.toml` at mode 0600 — under the same `(host, login)` key a token would use. It holds the access token, the refresh token, the expiry, the client ID and the token endpoint.

One document rather than several fields because those values must agree and all change together on every refresh. Split across two backends, a rotation is two writes with no transaction, and a crash in the gap leaves a refresh token the server has already invalidated beside an access token that still works. `hosts.toml` records only an advisory `kind = "oauth2"`, which nothing depends on.

**An older gea cannot read one.** It will send the document as though it were a token and get a 401 whose advice is to log in again. That is deliberate: shaping the document so an old build half-works would trade a loud, correct error for a credential that functions for an hour and then fails with no explanation.

## Lifetime, and what to use in CI

Sessions are renewed automatically, five minutes before the access token lapses, and lapse for good after about thirty days. `gea auth status` shows which kind of credential a login holds and when it next renews.

**CI should use a token, not an OAuth session.** A token created in the web UI does not expire and needs no browser. `gea auth login --with-token < token.txt`, or `GEA_TOKEN` in the environment.

## What is tested, and what is not

`crates/gea-itest/tests/live_oauth.rs` drives the whole flow against a real Gitea, including the consent click. A stand-in browser signs in, posts the grant, and fetches the redirect, so gea's own PKCE, loopback listener, state check, token exchange and storage all run for real; the stored session is then spent on live API calls to prove Gitea accepts it as `Bearer`.

This needs no HTML parsing, which is why it is worth having. Gitea 1.27.3's sign-in form carries no CSRF token, and neither does the grant form — every field the grant form submits is a value that was already in the authorize URL. So there is no markup dependency to rot.

The redirect-URI rule is pinned directly: with a session, an authorize request naming `http://127.0.0.1:<port>` answers `200` and one naming `http://127.0.0.1:<port>/callback` answers `400`. Checking it needs the session, because `reqSignIn` runs before the handler and bounces every unauthenticated request to `/user/login` whatever it asks for — a good redirect URI and a bad one look identical from outside.

Still not covered:

* **A real browser.** The stand-in does what a browser does over HTTP, but nothing exercises an actual browser launch, and `open::with_detached` is taken on trust.
* **Keyring storage.** The live tests use the file store, as the whole suite does; the keyring path is covered only by unit tests.
* **Logging out does not revoke the grant server-side.** `gea auth logout` removes the local credential; the authorization remains listed under the account's settings until revoked there.
