//! HTTP status + body → [`ErrorKind`].
//!
//! # Never swallow a server message
//!
//! Gitea's error bodies come in at least four shapes, and which one you get depends on the
//! handler rather than on the status code:
//!
//! ```text
//! {"message": "…", "url": "…"}                   the APIError shape, most common
//! {"errors": ["…", "…"]}                          bulk validation
//! {"title": ["can't be blank"], "body": [ … ]}    a field map
//! <html>…</html>                                  the route does not exist at all
//! ```
//!
//! [`parse_body`] tries all of them, and when none matches it keeps the raw bytes so the
//! renderer can print them verbatim under `server says:`. That fallback is not a nicety. `tea`'s
//! single worst UX bug is printing `failed to merge PR, is it still open?` for *every* merge
//! refusal while discarding the server's actual reason — which might have been "the base branch
//! has protected-branch checks pending" or "you are not a reviewer". A message we do not
//! understand is still the only person in the conversation who knows what went wrong.
//!
//! The shapes **mix**, which is the part that is easy to get wrong. `PUT /repos/{o}/{r}/topics`
//! answers a bad topic name with
//!
//! ```text
//! {"invalidTopics": ["Bad Topic"], "message": "Topic names are invalid"}
//! ```
//!
//! — an envelope *and* a field map in one object. Recognising `message` and stopping there
//! leaves the user with "the server rejected the values in this request" and no idea which
//! value, which is the same bug as `tea`'s wearing a better hat. So the field-map pass runs in
//! two modes: on its own when the body has no envelope keys at all, and **alongside** an
//! envelope, harvesting only the keys the envelope does not claim. `invalidTopics` survives
//! either way.
//!
//! # 405 is a refusal at least as often as it is a missing route
//!
//! `405 Method Not Allowed` reads like "this instance has a different API shape", and sometimes
//! it is. But Gitea also answers a *refused operation* with it: `PUT /pulls/{n}/merge` on a
//! closed, draft, behind-its-base, or check-blocked pull request is a 405 whose body says
//! exactly which. Mapping every 405 to [`ErrorKind::RouteNotFound`] throws that body away by
//! construction — the variant has nowhere to put it. So a 405 that carries a readable message
//! becomes [`ErrorKind::StateConflict`], which prints it; only an HTML or empty-bodied 405 is
//! read as a missing route, the same test `classify_404` already applies.
//!
//! # The 404 problem
//!
//! A `404` from this API is genuinely ambiguous, and the three explanations have nothing in
//! common: the inner resource is missing, the repository is missing (or private, or on another
//! host), or the *endpoint* does not exist on this instance. Guessing wrong sends the user to
//! debug the wrong thing. So classification takes a [`RepoProbe`] result: the client fires one
//! cheap `GET /repos/{owner}/{repo}` before classifying, which collapses the first two cases
//! into a definite answer, and an HTML body or an unrecognised route root identifies the third.
//!
//! There is a **fourth** case, and it looks like the first until you notice the path. A 404 on a
//! *collection* — `POST /repos/{o}/{r}/pulls` — names no object, so no object in it can be the
//! one that is missing: what 404'd is something the request *referred to*, which for a
//! `pr create` is the head branch. `resource_of` returns an empty identifier there, and reading
//! that as an object is what produced "that pull request does not exist … the identifier is what
//! is wrong" for a user who was creating one, alongside an invitation to go and list the pull
//! requests. The identifier stays empty and the renderer reports the collection.
//!
//! Which is also where the rule at the top of this module bit hardest for longest. Gitea
//! answers exactly that request with
//!
//! ```text
//! ["could not find 'no-such-branch' to be a commit, branch or tag…"]
//! ```
//!
//! — a bare array, the third body shape, and the only thing in the whole response that knows what
//! went wrong. It was discarded **by construction**: neither [`ErrorKind::ResourceNotFound`] nor
//! [`ErrorKind::RepoNotFound`] had a field to put it in. Both do now, filled by
//! `not_found_message`, which drops a body only when it restates the status code — so
//! `server says: Not Found` never takes up a line and `could not find 'no-such-branch'…` always
//! does.

use http::HeaderMap;

use super::{ErrorKind, FieldError};
use crate::types::scope::{Access, Scope};

/// How much of an unparseable body to keep. Enough to recognise a Go panic or an nginx error
/// page; not enough to fill a terminal with a minified HTML document.
const EXCERPT: usize = 400;

/// The outcome of the one cheap `GET /repos/{owner}/{repo}` probe.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum RepoProbe {
    /// Not asked — probing was disabled, or the probe itself failed. Classification then has to
    /// fall back to the honest, ambiguous message, and — this is the part that is easy to lose —
    /// it must say the repository was *not checked* rather than that it was not found. That is
    /// what [`ErrorKind::RepoNotFound::probed`] carries; collapsing this into
    /// [`RepoProbe::Missing`] made the two produce identical wording, so a never-checked 404
    /// claimed knowledge nobody had.
    #[default]
    NotAttempted,
    /// The repository is there, so only the inner resource is missing.
    Exists,
    /// The repository 404s too.
    Missing,
}

/// Everything classification needs that is not in the response itself.
#[derive(Debug, Clone, Default)]
pub struct ClassifyCtx {
    pub host: String,
    pub method: String,
    /// The request path. `/api/v1` is stripped if present, so callers may pass either form.
    pub path: String,
    /// Whether we actually presented a credential. Decides `401` → `TokenRejected` versus
    /// `NotAuthenticated`, which have completely different remedies.
    pub had_token: bool,
    pub login: Option<String>,
    /// The scope the operation is documented to need — `OpMeta::scope` from the generated
    /// client, which codegen derives from the spec's `tags[0]` plus the HTTP method. Set it with
    /// [`ClassifyCtx::with_op_scope`]; when it is empty, [`infer_scope`] re-derives the same rule
    /// from the method and path, which agrees with codegen for every ordinary route and guesses
    /// for the odd ones.
    ///
    /// This names the scope in an `InsufficientScope` message. It deliberately does **not**
    /// decide whether a 403 *is* a scope problem — see the 403 arm of [`classify`].
    pub needed_scope: Vec<String>,
    /// The token's actual scopes, if they are somehow known. Gitea does not report them, so
    /// this is almost always `None` and the renderer says so instead of guessing.
    pub have_scopes: Option<Vec<String>>,
    /// Where to create a token. `{web_base}/user/settings/applications`.
    pub settings_url: String,
    /// From `/version`, for a `RouteNotFound` message. Display only.
    pub instance: Option<String>,
    pub repo_probe: RepoProbe,
    /// What was being uploaded, for a `413`.
    pub uploading: Option<String>,
}

impl ClassifyCtx {
    /// Record the scope codegen says this operation needs.
    ///
    /// Takes `Option<&str>` because that is exactly the shape of `OpMeta::scope` in the generated
    /// client (`Option<&'static str>`), so the call site is `cctx.with_op_scope(meta.scope)` with
    /// no massaging and no `unwrap_or_default`. `None` and `Some("")` both leave the field empty,
    /// which falls back to [`infer_scope`].
    #[must_use]
    pub fn with_op_scope(mut self, scope: Option<&str>) -> Self {
        self.needed_scope = scope
            .map(str::trim)
            .filter(|s| !s.is_empty())
            .map(|s| vec![s.to_owned()])
            .unwrap_or_default();
        self
    }

    /// The scope to *name* in a message: what was recorded, or the inference when nothing was.
    ///
    /// Never empty for a route this client knows how to call, so the `needs:` line in an
    /// `InsufficientScope` rendering is never blank.
    pub fn scope_to_name(&self) -> Vec<String> {
        if !self.needed_scope.is_empty() {
            return self.needed_scope.clone();
        }
        infer_scope(&self.method, &self.path).into_iter().map(|s| s.to_string()).collect()
    }
}

/// A Gitea error body, in whatever shape it arrived.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct ServerBody {
    pub message: Option<String>,
    pub errors: Vec<String>,
    pub fields: Vec<FieldError>,
    /// The body as text, clipped. Always populated, because this is the fallback that keeps the
    /// promise in the module comment.
    pub raw: String,
    pub is_html: bool,
}

impl ServerBody {
    /// The best single sentence available, preferring the most structured source.
    ///
    /// Returns the raw excerpt rather than an empty string when nothing parsed. The renderer
    /// prints this under `server says:` and an empty line there means we threw information away.
    pub fn best_message(&self) -> String {
        if let Some(m) = self.message.as_deref().map(str::trim).filter(|m| !m.is_empty()) {
            return m.to_owned();
        }
        if !self.errors.is_empty() {
            return self.errors.join("; ");
        }
        if !self.fields.is_empty() {
            return self
                .fields
                .iter()
                .map(|f| match &f.field {
                    Some(name) => format!("{name}: {}", f.message),
                    None => f.message.clone(),
                })
                .collect::<Vec<_>>()
                .join("; ");
        }
        // Last resort: whatever bytes arrived. An HTML page is not worth quoting at a user.
        if self.is_html { String::new() } else { self.raw.clone() }
    }

    pub fn is_empty(&self) -> bool {
        self.message.is_none() && self.errors.is_empty() && self.fields.is_empty()
    }
}

/// Parse an error body, trying every shape Gitea uses.
pub fn parse_body(headers: &HeaderMap, body: &[u8]) -> ServerBody {
    let text = String::from_utf8_lossy(body);
    let trimmed = text.trim();
    let content_type = headers
        .get("content-type")
        .and_then(|v| v.to_str().ok())
        .unwrap_or("")
        .to_ascii_lowercase();
    let is_html = content_type.contains("text/html")
        || trimmed.starts_with("<!DOCTYPE")
        || trimmed.starts_with("<!doctype")
        || trimmed.starts_with("<html");

    let mut out = ServerBody { raw: clip(trimmed), is_html, ..ServerBody::default() };

    let Ok(v) = serde_json::from_str::<serde_json::Value>(trimmed) else {
        return out;
    };
    let serde_json::Value::Object(map) = v else {
        // A bare JSON string or array. `["a","b"]` does occur.
        if let serde_json::Value::Array(items) = v {
            out.errors = items.iter().filter_map(as_message).collect();
        } else if let serde_json::Value::String(s) = v {
            out.message = Some(s);
        }
        return out;
    };

    // Shape 1: {"message": "…"}.
    if let Some(m) = map.get("message").and_then(as_message) {
        out.message = Some(m);
    }
    // Gitea occasionally uses `error` or `error_description` (the OAuth handlers do).
    if out.message.is_none() {
        out.message =
            map.get("error_description").or_else(|| map.get("error")).and_then(as_message);
    }

    // Shape 2: {"errors": […]}. Items may be strings, or objects with `message`/`field`.
    if let Some(serde_json::Value::Array(items)) = map.get("errors") {
        for item in items {
            match item {
                serde_json::Value::Object(o) => {
                    let message =
                        o.get("message").and_then(as_message).unwrap_or_else(|| item.to_string());
                    let field = o.get("field").or_else(|| o.get("resource")).and_then(as_message);
                    match field {
                        Some(f) => out.fields.push(FieldError { field: Some(f), message }),
                        None => out.errors.push(message),
                    }
                }
                other => {
                    if let Some(m) = as_message(other) {
                        out.errors.push(m);
                    }
                }
            }
        }
    }

    // Shape 3: a field map. Two modes, because the shapes mix.
    //
    // *Strict*, when nothing envelope-shaped was found: every key must be a field, so a single
    // unusable value disqualifies the whole reading. Being conservative here is what stops
    // `{"message": "..."}` being reported as a complaint about a field called `message`.
    //
    // *Supplementary*, when an envelope was found: the envelope keys are skipped rather than
    // treated as disqualifying, and whatever else is there is harvested. This is the case that
    // `PUT /repos/{o}/{r}/topics` needs — `{"invalidTopics": [...], "message": "..."}` is both
    // shapes at once, and stopping at `message` loses the only part that names the bad value.
    let strict = out.is_empty();
    let mut fields = Vec::new();
    let mut plausible = !map.is_empty();
    for (k, v) in &map {
        if ENVELOPE.contains(&k.as_str()) {
            if strict {
                plausible = false;
                break;
            }
            continue;
        }
        match v {
            serde_json::Value::String(s) if !s.trim().is_empty() => {
                fields.push(FieldError { field: Some(k.clone()), message: s.trim().to_owned() })
            }
            serde_json::Value::Array(items) => {
                for item in items {
                    if let Some(m) = as_message(item) {
                        fields.push(FieldError { field: Some(k.clone()), message: m });
                    }
                }
            }
            // A nested object, a bool, a number, a null. In strict mode that means this is not a
            // field map at all; alongside an envelope it just means this one key is not a
            // complaint, and the rest still are.
            _ => {
                if strict {
                    plausible = false;
                    break;
                }
            }
        }
    }
    if plausible && !fields.is_empty() {
        out.fields = fields;
    }

    out
}

/// Keys that belong to the error *envelope* rather than to the request, and so are never read as
/// field complaints. `url` and `documentation_url` are pointers back at the API docs; the rest
/// carry the message itself.
const ENVELOPE: &[&str] =
    &["message", "errors", "url", "error", "error_description", "documentation_url"];

fn as_message(v: &serde_json::Value) -> Option<String> {
    match v {
        serde_json::Value::String(s) if !s.trim().is_empty() => Some(s.trim().to_owned()),
        serde_json::Value::Number(n) => Some(n.to_string()),
        _ => None,
    }
}

fn clip(s: &str) -> String {
    if s.chars().count() <= EXCERPT {
        return s.to_owned();
    }
    let head: String = s.chars().take(EXCERPT).collect();
    format!("{head}…")
}

/// Classify a non-2xx response.
pub fn classify(status: u16, headers: &HeaderMap, body: &[u8], ctx: &ClassifyCtx) -> ErrorKind {
    let sb = parse_body(headers, body);
    let msg = sb.best_message();
    let path = api_path(&ctx.path);

    match status {
        // 400: the server could not make sense of the request. Reported as `Validation`
        // because that is the shape of the remedy — fix a value and try again — even though the
        // complaint is about the request rather than a named field.
        400 => ErrorKind::Validation {
            fields: sb.fields.clone(),
            server_message: validation_message(&sb, &msg),
        },

        401 => {
            // A 2FA-enabled account using basic auth gets a 401 that says so. Treating it as a
            // rejected token would send the user to regenerate a perfectly good credential.
            if mentions_two_factor(&msg) {
                return ErrorKind::TwoFactorRequired { host: ctx.host.clone() };
            }
            if ctx.had_token {
                ErrorKind::TokenRejected {
                    host: ctx.host.clone(),
                    login: ctx.login.clone(),
                    settings_url: ctx.settings_url.clone(),
                }
            } else {
                ErrorKind::NotAuthenticated { host: ctx.host.clone() }
            }
        }

        403 => {
            // Prefer scopes the server named — Gitea's own message often spells them out as
            // `required scope(s): [write:issue]`, which beats any inference we could make.
            let named = scopes_in_message(&msg);

            // **The message decides whether this is a scope problem; `needed_scope` only decides
            // what to call it.** Every operation needs some scope, so once `needed_scope` is
            // populated — which is the whole point of wiring `OpMeta::scope` through — treating a
            // known scope as evidence would report "user is not a collaborator on this
            // repository" as a missing scope and send the user to mint a token that changes
            // nothing.
            if named.is_empty() && !looks_like_scope_problem(&msg) {
                // A 403 that is not about scopes: not a collaborator, org membership required,
                // the instance forbids the action outright. The server message is the whole
                // value here, which is why it is carried verbatim.
                return ErrorKind::Forbidden { server_message: msg };
            }

            // Naming, best source first: what the server said, then the scope codegen recorded
            // for this operation, then inference from the method and path. The last is a guess
            // and the first two are not, which is why it is last.
            let needed = if named.is_empty() { ctx.scope_to_name() } else { named };

            ErrorKind::InsufficientScope {
                host: ctx.host.clone(),
                needed,
                have: ctx.have_scopes.clone(),
                settings_url: ctx.settings_url.clone(),
            }
        }

        404 => classify_404(&sb, &path, ctx),

        // 405 reads like "this path exists but not with this method", and sometimes that is what
        // it is. But Gitea answers a refused *operation* with it too — merging a closed or
        // un-mergeable pull request is a 405 whose body says which — and `RouteNotFound` has
        // nowhere to put a message, so reading every 405 that way discards the only useful part
        // of the response. The body decides: words mean a refusal, an HTML page or silence means
        // the route really is absent.
        405 => {
            if sb.is_html || msg.trim().is_empty() {
                ErrorKind::RouteNotFound {
                    method: ctx.method.clone(),
                    path: path.clone(),
                    instance: ctx.instance.clone(),
                }
            } else {
                let (kind, id) = named_resource(&path);
                ErrorKind::StateConflict {
                    resource: (!id.is_empty()).then(|| format!("{kind} {id}")),
                    // A 405 never states the state; the message does, in prose.
                    state: None,
                    server_message: msg,
                }
            }
        }

        409 => ErrorKind::Conflict { server_message: msg },

        // 413 is Gitea's quota response. Quotas count LFS objects, packages, and release
        // assets together, so "the repository is small" is not a contradiction.
        413 => ErrorKind::QuotaExceeded { server_message: msg, uploading: ctx.uploading.clone() },

        422 => ErrorKind::Validation {
            fields: sb.fields.clone(),
            server_message: validation_message(&sb, &msg),
        },

        423 => ErrorKind::Archived {
            slug: repo_slug_of(&path).unwrap_or_default(),
            host: ctx.host.clone(),
        },

        429 => ErrorKind::RateLimited {
            host: ctx.host.clone(),
            retry_after: headers
                .get("retry-after")
                .and_then(|v| v.to_str().ok())
                .and_then(crate::http::retry::retry_after),
        },

        500..=599 => ErrorKind::ServerError { status, server_message: msg },

        _ => ErrorKind::UnexpectedStatus {
            status,
            body_excerpt: if msg.is_empty() { sb.raw.clone() } else { msg },
        },
    }
}

fn non_empty(s: &str) -> Option<String> {
    (!s.trim().is_empty()).then(|| s.to_owned())
}

/// The `server says:` line for a validation failure, or `None` when it would only repeat the
/// per-field facts printed directly above it.
///
/// This is the one place it is safe to drop part of a server message, and only because every
/// word of it is already on screen: [`ServerBody::best_message`] falls back to joining the field
/// map, so a pure shape-3 body would otherwise print `title: can't be blank` as a fact and then
/// `server says: title: can't be blank` underneath. Anything the fields do *not* already say —
/// a `message`, an `errors` array, an unparseable body — still goes through verbatim.
fn validation_message(sb: &ServerBody, msg: &str) -> Option<String> {
    if sb.message.is_none() && sb.errors.is_empty() && !sb.fields.is_empty() {
        return None;
    }
    non_empty(msg)
}

/// `("pull request", "4212")` for `/repos/o/r/pulls/4212/merge`.
///
/// The same walk [`classify_404`] uses to name a missing object, reused to name the object an
/// operation was refused on. Returns an empty id when the path names no identifiable thing.
fn named_resource(path: &str) -> (&'static str, String) {
    let segs: Vec<&str> = path.trim_matches('/').split('/').filter(|s| !s.is_empty()).collect();
    let tail =
        if segs.first() == Some(&"repos") && segs.len() >= 3 { &segs[3..] } else { &segs[..] };
    resource_of(tail)
}

/// The `server says:` line for a 404, or `None` when the body only restated the status.
///
/// The counterpart of [`validation_message`], and it exists for the same narrow reason: a
/// message is dropped only when printing it would add a line that says "404" a second time.
/// Gitea's generic miss is `{"message": "The target couldn't be found."}`, which is the status
/// code in a sentence; its *specific* misses are the whole diagnosis —
/// `["could not find 'no-such-branch' to be a commit, branch or tag …"]` names the branch that a
/// `pr create` was missing, and nothing else in the response does.
///
/// The test is an exact match on the whole flattened message, never a substring: "could not
/// find …" contains "not found"'s words and must survive.
fn not_found_message(sb: &ServerBody) -> Option<String> {
    let msg = sb.best_message();
    let msg = msg.trim();
    if msg.is_empty() {
        return None;
    }
    let flat: String = msg
        .to_ascii_lowercase()
        .chars()
        .filter(|c| c.is_ascii_alphanumeric() || c.is_ascii_whitespace())
        .collect();
    let flat = flat.split_whitespace().collect::<Vec<_>>().join(" ");
    // Nothing but punctuation survived, so the "message" was an empty envelope — `{}` or `[]`,
    // which `best_message` hands back verbatim as the last-resort raw excerpt.
    if flat.is_empty() {
        return None;
    }
    const RESTATES_THE_STATUS: &[&str] = &[
        "null",
        "not found",
        "notfound",
        "404",
        "404 not found",
        "page not found",
        "the target couldnt be found",
        "the target could not be found",
        "object does not exist",
        "resource not found",
    ];
    if RESTATES_THE_STATUS.contains(&flat.as_str()) {
        return None;
    }
    Some(msg.to_owned())
}

fn classify_404(sb: &ServerBody, path: &str, ctx: &ClassifyCtx) -> ErrorKind {
    // An HTML body means we hit the web router, not the API router: this instance has no such
    // endpoint. That is an *older Gitea*, not a missing object, and the remedy is completely
    // different — which is why it must not be reported as a missing resource.
    if sb.is_html {
        return ErrorKind::RouteNotFound {
            method: ctx.method.clone(),
            path: path.to_owned(),
            instance: ctx.instance.clone(),
        };
    }

    let segs: Vec<&str> = path.trim_matches('/').split('/').filter(|s| !s.is_empty()).collect();
    let Some(root) = segs.first() else {
        return ErrorKind::RouteNotFound {
            method: ctx.method.clone(),
            path: path.to_owned(),
            instance: ctx.instance.clone(),
        };
    };

    // A path root we do not recognise cannot be a missing object, because we would not have
    // generated a call to it.
    if !KNOWN_ROOTS.contains(root) {
        return ErrorKind::RouteNotFound {
            method: ctx.method.clone(),
            path: path.to_owned(),
            instance: ctx.instance.clone(),
        };
    }

    let server_message = not_found_message(sb);

    if *root == "repos" && segs.len() >= 3 {
        let slug = format!("{}/{}", segs[1], segs[2]);
        // `/repos/{owner}/{repo}` itself: nothing inner to blame. No probe was fired for this
        // path — probing would only repeat the request that just failed — but the request *is*
        // the probe, and it 404'd, so the repository's absence is established rather than
        // assumed.
        if segs.len() == 3 {
            return ErrorKind::RepoNotFound {
                slug,
                host: ctx.host.clone(),
                login: ctx.login.clone(),
                probed: true,
                server_message,
            };
        }
        return match ctx.repo_probe {
            // The probe settled it: the repository is there, so only the inner thing is gone.
            //
            // *Unless the path never named one.* `resource_of(["pulls"])` returns an empty id,
            // because `POST /repos/{o}/{r}/pulls` ends on the collection — there is no
            // identifier in it to be wrong about. Carrying that empty id through as though it
            // were an object is what told a user creating a pull request that "that pull request
            // does not exist … the identifier is what is wrong", and sent them to
            // `gea pr list --state all` to look for the thing they were trying to make. The id
            // is left empty on purpose and the renderer reports the collection; `RouteNotFound`
            // would be wrong in the other direction, since both the repository and the endpoint
            // demonstrably exist.
            RepoProbe::Exists => {
                let (kind, id) = resource_of(&segs[3..]);
                ErrorKind::ResourceNotFound { kind, id, slug: Some(slug), server_message }
            }
            // The probe 404'd too: the repository really is unreachable, ambiguously between
            // missing, private, and wrong host, and the message names all three.
            RepoProbe::Missing => ErrorKind::RepoNotFound {
                slug,
                host: ctx.host.clone(),
                login: ctx.login.clone(),
                probed: true,
                server_message,
            },
            // Nobody asked. These used to be the same variant *and* the same wording, so a 404
            // whose repository was never checked still announced "could not find the repository
            // perf3ct/gea" — an assertion on no evidence, which `server_message` then made
            // visibly wrong by printing the server's contradicting sentence underneath it. The
            // variant is the same; `probed: false` is what stops the renderer claiming the
            // check happened.
            RepoProbe::NotAttempted => ErrorKind::RepoNotFound {
                slug,
                host: ctx.host.clone(),
                login: ctx.login.clone(),
                probed: false,
                server_message,
            },
        };
    }

    let (kind, id) = resource_of(&segs[1..]);
    if id.is_empty() {
        // A known root with no identifier in it — `/notifications`, say. Nothing was named, so
        // nothing can be missing; the endpoint must be. (The repository branch above cannot use
        // this reading: there the probe has just proved the repository is served, so the route
        // under it exists too.)
        return ErrorKind::RouteNotFound {
            method: ctx.method.clone(),
            path: path.to_owned(),
            instance: ctx.instance.clone(),
        };
    }
    ErrorKind::ResourceNotFound { kind, id, slug: None, server_message }
}

/// Top-level API path segments that exist in the Gitea spec.
///
/// Used only to tell "an object under a real endpoint is missing" from "this endpoint does not
/// exist here". Being slightly out of date is harmless in one direction (a new root would be
/// reported as `RouteNotFound`, which is *approximately* true for a client generated against an
/// older spec) and this list is cheap to extend.
const KNOWN_ROOTS: &[&str] = &[
    "activitypub",
    "admin",
    "gitignore",
    "issues",
    "label_templates",
    "labels",
    "licenses",
    "markdown",
    "markup",
    "nodeinfo",
    "notifications",
    "org",
    "orgs",
    "packages",
    "repos",
    "repositories",
    "settings",
    "signing-key.gpg",
    "teams",
    "topics",
    "user",
    "users",
    "version",
];

/// How many path segments the identifier after a collection occupies.
///
/// The distinction is not ours to invent: [`crate::http::encode`] already draws exactly this
/// line on the way *out*, because a `/` inside a path parameter is either structural or it
/// changes which route matches. The parameters it lets through unencoded — `filepath`,
/// `treePath`, `ref`, `path`, `filename` — are the ones whose values are whole paths, and the
/// collections below marked [`IdShape::Path`] are the ones the spec gives such a parameter *as
/// the final segment of the route*. Reading one segment there and dropping the rest is what
/// reported `file: src` for `GET /repos/o/r/contents/src/main.rs`.
///
/// The "final segment" qualifier is the whole safety of it. `/commits/{ref}/status` also has a
/// path-like `ref`, but something follows it, so `commits` stays [`IdShape::Segment`] rather
/// than swallowing `status` into the identifier.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum IdShape {
    /// One segment: `{index}`, `{sha}`, `{username}`.
    Segment,
    /// Everything that is left: `{filepath}`, `{ref}`.
    Path,
}

/// Human names for collection segments, so `ResourceNotFound` can say "pull request #4212"
/// instead of "pulls 4212".
const KINDS: &[(&str, &str, IdShape)] = &[
    ("assets", "release asset", IdShape::Segment),
    ("attachments", "attachment", IdShape::Segment),
    ("branch_protections", "branch protection rule", IdShape::Segment),
    ("branches", "branch", IdShape::Segment),
    ("collaborators", "collaborator", IdShape::Segment),
    ("comments", "comment", IdShape::Segment),
    ("commits", "commit", IdShape::Segment),
    // `/repos/{owner}/{repo}/contents/{filepath}` and its three siblings, all terminal.
    ("contents", "file", IdShape::Path),
    ("editorconfig", "file", IdShape::Path),
    ("gpg_keys", "GPG key", IdShape::Segment),
    ("hooks", "webhook", IdShape::Segment),
    ("issues", "issue", IdShape::Segment),
    ("jobs", "workflow job", IdShape::Segment),
    ("keys", "key", IdShape::Segment),
    ("labels", "label", IdShape::Segment),
    ("media", "file", IdShape::Path),
    ("members", "member", IdShape::Segment),
    ("milestones", "milestone", IdShape::Segment),
    ("orgs", "organization", IdShape::Segment),
    ("packages", "package", IdShape::Segment),
    ("pulls", "pull request", IdShape::Segment),
    ("raw", "file", IdShape::Path),
    // `/repos/{owner}/{repo}/git/refs/{ref}`: `refs/heads/main` is three segments, one ref.
    ("refs", "ref", IdShape::Path),
    ("releases", "release", IdShape::Segment),
    ("reviews", "review", IdShape::Segment),
    ("runners", "runner", IdShape::Segment),
    ("runs", "workflow run", IdShape::Segment),
    ("secrets", "secret", IdShape::Segment),
    ("tags", "tag", IdShape::Segment),
    ("teams", "team", IdShape::Segment),
    ("times", "tracked time", IdShape::Segment),
    ("topics", "topic", IdShape::Segment),
    ("users", "user", IdShape::Segment),
    ("variables", "variable", IdShape::Segment),
    ("wiki", "wiki page", IdShape::Segment),
    ("workflows", "workflow", IdShape::Segment),
];

fn kind_of(seg: &str) -> Option<(&'static str, IdShape)> {
    KINDS.iter().find(|(k, _, _)| *k == seg).map(|(_, name, shape)| (*name, *shape))
}

/// Name the most specific `(collection, identifier)` pair in a path tail.
///
/// The *last* pair wins, because paths nest from general to specific:
/// `/issues/5/comments/91` is a missing comment, not a missing issue. When the tail ends on a
/// collection with nothing after it, the identifier is empty and the caller decides what that
/// means.
///
/// An [`IdShape::Path`] collection takes the **whole** remaining tail as its identifier, because
/// its spec parameter is path-like and terminal — see [`IdShape`]. Taking one segment there
/// reported `file: src` for `/repos/o/r/contents/src/main.rs`, naming a directory that was never
/// what 404'd, with the rest of the identifier sitting unused two elements away.
fn resource_of(tail: &[&str]) -> (&'static str, String) {
    let mut best: (&'static str, String) = ("resource", String::new());
    let mut i = 0;
    while i < tail.len() {
        if let Some((kind, shape)) = kind_of(tail[i]) {
            if shape == IdShape::Path {
                // Terminal by construction, so nothing after this can be a nested collection and
                // there is nothing to keep walking for.
                return (kind, tail[i + 1..].join("/"));
            }
            let id = tail.get(i + 1).copied().unwrap_or("");
            // The next segment is another collection (`/issues/5/comments`), so this level has
            // no identifier of its own here.
            if !id.is_empty() && kind_of(id).is_none() {
                best = (kind, id.to_owned());
                i += 2;
                continue;
            }
            best = (kind, id.to_owned());
        }
        i += 1;
    }
    if best.1.is_empty() && best.0 == "resource" {
        // No recognised collection at all: name the last segment, which is usually the thing.
        if let Some(last) = tail.last() {
            best = ("resource", (*last).to_owned());
        }
    }
    best
}

/// `owner/name` from a repository path, or `None` when the path is not repository-scoped.
///
/// Used both for classification and to populate [`super::RequestCtx::repo`], so that every error
/// message can name the repository the request was about without the caller having to plumb it
/// through.
pub fn repo_slug_of(path: &str) -> Option<String> {
    let p = api_path(path);
    let segs: Vec<&str> = p.trim_matches('/').split('/').collect();
    match segs.as_slice() {
        ["repos", owner, repo, ..] if !owner.is_empty() && !repo.is_empty() => {
            Some(format!("{owner}/{repo}"))
        }
        _ => None,
    }
}

/// Whether a 404 on this path warrants the one cheap `GET /repos/{owner}/{repo}` probe.
///
/// Only for paths *under* a repository — probing `/repos/{o}/{r}` itself would just repeat the
/// request that already failed.
pub fn probe_target(status: u16, path: &str) -> Option<(String, String)> {
    if status != 404 {
        return None;
    }
    let p = api_path(path);
    let segs: Vec<&str> = p.trim_matches('/').split('/').filter(|s| !s.is_empty()).collect();
    match segs.as_slice() {
        ["repos", owner, repo, _rest, ..] => Some(((*owner).to_owned(), (*repo).to_owned())),
        _ => None,
    }
}

/// Strip a `/api/v1` prefix so callers may pass either the wire path or the API-relative one.
fn api_path(path: &str) -> String {
    let p = path.split(['?', '#']).next().unwrap_or(path);
    let p = p.strip_prefix("/api/v1").unwrap_or(p);
    if p.is_empty() { "/".to_owned() } else { p.to_owned() }
}

/// Derive the scope an operation probably needs, from its method and path.
///
/// Mirrors the codegen rule — `read:` for GET/HEAD and `write:` otherwise, over the route group
/// from the spec's `tags[0]` — so an inferred scope and a generated one agree. It is a *guess*
/// and the renderer presents it as one; the server's own message is always preferred when it
/// names scopes.
pub fn infer_scope(method: &str, path: &str) -> Option<Scope> {
    let p = api_path(path);
    let segs: Vec<&str> = p.trim_matches('/').split('/').filter(|s| !s.is_empty()).collect();
    let root = segs.first()?;
    let area = match *root {
        // Issue-shaped routes live under /repos/... but are tagged `issue` in the spec, and the
        // scope follows the tag, not the URL prefix. A user told to create `write:repository`
        // when they need `write:issue` gets a second 403 and stops trusting the tool.
        "repos" | "repositories"
            if segs.iter().any(|s| {
                matches!(
                    *s,
                    "issues" | "pulls" | "milestones" | "labels" | "times" | "issue_templates"
                )
            }) =>
        {
            "issue"
        }
        "repos" | "repositories" => "repository",
        "issues" => "issue",
        "user" | "users" => "user",
        "org" | "orgs" | "teams" => "organization",
        "admin" => "admin",
        "notifications" => "notification",
        "packages" => "package",
        "activitypub" => "activitypub",
        _ => "misc",
    };
    Some(Scope::new(Access::for_method(method), area))
}

/// Pull scope names out of a server message like
/// `token does not have at least one of required scope(s): [write:issue]`.
///
/// Worth the twenty lines: when the server tells us the answer, inference is strictly worse.
fn scopes_in_message(msg: &str) -> Vec<String> {
    let lower = msg.to_ascii_lowercase();
    if !lower.contains("scope") {
        return Vec::new();
    }
    let Some(open) = msg.find('[') else { return Vec::new() };
    let Some(close) = msg[open..].find(']').map(|i| open + i) else { return Vec::new() };
    msg[open + 1..close]
        .split([',', ' ', '\t'])
        .map(|s| s.trim().trim_matches('"').trim_matches('\''))
        .filter(|s| s.contains(':'))
        .map(str::to_owned)
        .collect()
}

fn looks_like_scope_problem(msg: &str) -> bool {
    let m = msg.to_ascii_lowercase();
    m.contains("scope")
        || m.contains("insufficient")
        || (m.contains("token") && (m.contains("permission") || m.contains("not allowed")))
}

fn mentions_two_factor(msg: &str) -> bool {
    let m = msg.to_ascii_lowercase();
    m.contains("otp") || m.contains("two-factor") || m.contains("two factor") || m.contains("2fa")
}

#[cfg(test)]
mod tests {
    use super::*;

    fn headers(pairs: &[(&str, &str)]) -> HeaderMap {
        let mut h = HeaderMap::new();
        for (k, v) in pairs {
            h.append(
                http::HeaderName::from_bytes(k.as_bytes()).unwrap(),
                http::HeaderValue::from_str(v).unwrap(),
            );
        }
        h
    }

    fn json() -> HeaderMap {
        headers(&[("content-type", "application/json")])
    }

    fn ctx() -> ClassifyCtx {
        ClassifyCtx {
            host: "git.example.org".into(),
            method: "GET".into(),
            path: "/api/v1/repos/perf3ct/gea/pulls/4212".into(),
            had_token: true,
            settings_url: "https://git.example.org/user/settings/applications".into(),
            ..ClassifyCtx::default()
        }
    }

    /// The full user-visible diagnostic for a response, so a snapshot shows what actually
    /// reaches the terminal rather than which variant was chosen. The point of this module is
    /// what the user reads; asserting on the enum alone would let the message be lost one layer
    /// later and never notice.
    fn rendered(status: u16, headers: &HeaderMap, body: &[u8], c: &ClassifyCtx) -> String {
        let kind = classify(status, headers, body, c);
        let err = crate::error::Error {
            kind: Box::new(kind),
            ctx: Box::new(crate::error::RequestCtx {
                host: Some(c.host.clone()),
                method: Some(c.method.clone()),
                path: Some(c.path.clone()),
                repo: repo_slug_of(&c.path),
                status: Some(status),
                ..crate::error::RequestCtx::default()
            }),
        };
        crate::error::render::render(&err, crate::error::render::Color::Never)
    }

    // ------------------------------------------------------------------- body shapes

    /// Shape 1: the APIError envelope. The most common, and the one whose message must never be
    /// discarded.
    #[test]
    fn parses_the_message_envelope() {
        let b = parse_body(&json(), br#"{"message":"branch already exists","url":"https://x"}"#);
        assert_eq!(b.message.as_deref(), Some("branch already exists"));
        assert_eq!(b.best_message(), "branch already exists");
    }

    #[test]
    fn parses_the_errors_array() {
        let b = parse_body(&json(), br#"{"errors":["title is empty","body too long"]}"#);
        assert_eq!(b.errors, vec!["title is empty", "body too long"]);
        assert_eq!(b.best_message(), "title is empty; body too long");
    }

    #[test]
    fn parses_a_field_map() {
        let b = parse_body(&json(), br#"{"title":["can't be blank"],"body":"too long"}"#);
        // `serde_json` is built with `preserve_order`, so field order matches the body — which
        // is what we want in a message: complaints appear in the order the server listed them.
        assert_eq!(
            b.fields,
            vec![
                FieldError { field: Some("title".into()), message: "can't be blank".into() },
                FieldError { field: Some("body".into()), message: "too long".into() },
            ]
        );
    }

    /// A field map must not swallow the envelope shape: reading `{"message": …}` as a field
    /// called `message` would produce "message: branch already exists".
    #[test]
    fn the_envelope_is_never_mistaken_for_a_field_map() {
        let b = parse_body(&json(), br#"{"message":"nope","url":"https://x"}"#);
        assert!(b.fields.is_empty());
        assert_eq!(b.best_message(), "nope");
    }

    /// THE promise of this module: an unrecognised body is still reported verbatim. `tea` prints
    /// a canned string here and throws the server's reason away.
    #[test]
    fn an_unparseable_body_is_kept_verbatim() {
        let b = parse_body(&headers(&[]), b"pq: duplicate key value violates unique constraint");
        assert!(b.is_empty(), "nothing structured parsed");
        assert_eq!(b.best_message(), "pq: duplicate key value violates unique constraint");
    }

    #[test]
    fn an_html_body_is_recognised_and_not_quoted_at_the_user() {
        let b = parse_body(&headers(&[("content-type", "text/html")]), b"<html>404 page</html>");
        assert!(b.is_html);
        assert_eq!(b.best_message(), "", "an HTML page is not a message");
    }

    #[test]
    fn a_huge_body_is_clipped() {
        let big = "x".repeat(5000);
        let b = parse_body(&headers(&[]), big.as_bytes());
        assert!(b.raw.chars().count() <= EXCERPT + 1, "{}", b.raw.chars().count());
    }

    // ---------------------------------------------------------------------- statuses

    /// The distinction that decides whether the user is told to log in or told their existing
    /// credential was refused.
    #[test]
    fn a_401_distinguishes_no_credential_from_a_rejected_one() {
        let mut c = ctx();
        c.had_token = true;
        assert!(matches!(
            classify(401, &json(), br#"{"message":"unauthorized"}"#, &c),
            ErrorKind::TokenRejected { .. }
        ));
        c.had_token = false;
        assert!(matches!(
            classify(401, &json(), br#"{"message":"unauthorized"}"#, &c),
            ErrorKind::NotAuthenticated { .. }
        ));
    }

    /// A 2FA prompt reported as a rejected token sends the user to regenerate a credential that
    /// was never the problem.
    #[test]
    fn a_401_mentioning_otp_is_a_two_factor_prompt() {
        let body = br#"{"message":"missing otp code, two-factor is enabled"}"#;
        assert!(matches!(
            classify(401, &json(), body, &ctx()),
            ErrorKind::TwoFactorRequired { .. }
        ));
    }

    /// When the server names the scope, use its answer rather than our inference.
    #[test]
    fn a_403_prefers_the_scopes_the_server_named() {
        let body =
            br#"{"message":"token does not have at least one of required scope(s): [write:issue]"}"#;
        let ErrorKind::InsufficientScope { needed, have, .. } =
            classify(403, &json(), body, &ctx())
        else {
            panic!("expected InsufficientScope");
        };
        assert_eq!(needed, vec!["write:issue"]);
        assert_eq!(have, None, "Gitea does not report a token's scopes");
    }

    #[test]
    fn a_403_with_no_scope_signal_is_plain_forbidden_and_keeps_the_message() {
        let body = br#"{"message":"user is not a collaborator on this repository"}"#;
        let ErrorKind::Forbidden { server_message } = classify(403, &json(), body, &ctx()) else {
            panic!("expected Forbidden");
        };
        assert_eq!(server_message, "user is not a collaborator on this repository");
    }

    #[test]
    fn scope_inference_follows_the_tag_not_the_url_prefix() {
        // Tagged `issue` in the spec despite living under /repos/.
        assert_eq!(
            infer_scope("POST", "/api/v1/repos/o/r/issues").map(|s| s.to_string()),
            Some("write:issue".to_owned())
        );
        assert_eq!(
            infer_scope("GET", "/repos/o/r/branches").map(|s| s.to_string()),
            Some("read:repository".to_owned())
        );
        assert_eq!(
            infer_scope("DELETE", "/admin/users/x").map(|s| s.to_string()),
            Some("write:admin".to_owned())
        );
    }

    // ------------------------------------------------------------------------ 404s

    /// The probe said the repository is there, so only the pull request is missing — and the
    /// message can say so with confidence.
    #[test]
    fn a_404_under_an_existing_repo_names_the_inner_resource() {
        let mut c = ctx();
        c.repo_probe = RepoProbe::Exists;
        let ErrorKind::ResourceNotFound { kind, id, slug, server_message } =
            classify(404, &json(), br#"{"message":"not found"}"#, &c)
        else {
            panic!("expected ResourceNotFound");
        };
        assert_eq!((kind, id.as_str()), ("pull request", "4212"));
        assert_eq!(slug.as_deref(), Some("perf3ct/gea"));
        assert_eq!(server_message, None, "a bare `not found` only restates the status code");
    }

    // ------------------------------------------------- a 404 on a collection, and its body
    //
    // Both defects that the live Forgejo 16.0.4 (measured for fjo, which gea was ported from) run turned up, as tests. `gea pr create --head
    // no-such-branch` is a `POST /repos/{o}/{r}/pulls`: the repository exists, the endpoint
    // exists, and the only thing that does not is a branch named in the request body.

    fn create_pull_ctx() -> ClassifyCtx {
        let mut c = ctx();
        c.method = "POST".into();
        c.path = "/api/v1/repos/perf3ct/gea/pulls".into();
        c.repo_probe = RepoProbe::Exists;
        c
    }

    /// **Defect 1.** The path ends on the collection, so `resource_of` hands back an empty id.
    /// Reporting that as an object left the user hunting for a pull request they were trying to
    /// create; the id stays empty and the renderer reports the collection.
    #[test]
    fn a_404_on_a_collection_does_not_invent_an_identifier() {
        let body = br#"["could not find 'no-such-branch' to be a commit, branch or tag"]"#;
        let ErrorKind::ResourceNotFound { kind, id, slug, .. } =
            classify(404, &json(), body, &create_pull_ctx())
        else {
            panic!("expected ResourceNotFound");
        };
        assert_eq!(kind, "pull request");
        assert!(id.is_empty(), "the path named no pull request, so nothing in it can be wrong");
        assert_eq!(slug.as_deref(), Some("perf3ct/gea"));
    }

    /// **Defect 2.** The body is the entire diagnosis, and the variant had nowhere to put it.
    /// All three body shapes carry it, because Gitea picks between them per handler.
    #[test]
    fn a_404_keeps_whatever_the_server_said() {
        let want = "could not find 'no-such-branch' to be a commit, branch or tag";
        for body in [
            br#"["could not find 'no-such-branch' to be a commit, branch or tag"]"#.as_slice(),
            br#"{"errors":["could not find 'no-such-branch' to be a commit, branch or tag"]}"#,
            br#"{"message":"could not find 'no-such-branch' to be a commit, branch or tag"}"#,
        ] {
            let ErrorKind::ResourceNotFound { server_message, .. } =
                classify(404, &json(), body, &create_pull_ctx())
            else {
                panic!("expected ResourceNotFound for {}", String::from_utf8_lossy(body));
            };
            assert_eq!(server_message.as_deref(), Some(want));
        }
    }

    /// A body that only restates the status is dropped, so `server says: Not Found` never takes
    /// up a line. This is the only thing `not_found_message` is allowed to discard.
    #[test]
    fn a_404_that_only_restates_the_status_says_nothing_extra() {
        for body in [
            br#"{"message":"Not Found"}"#.as_slice(),
            br#"{"message":"The target couldn't be found."}"#,
            br#"{"errors":["404 Not Found"]}"#,
            b"{}",
        ] {
            let ErrorKind::ResourceNotFound { server_message, .. } =
                classify(404, &json(), body, &create_pull_ctx())
            else {
                panic!("expected ResourceNotFound for {}", String::from_utf8_lossy(body));
            };
            assert_eq!(server_message, None, "{}", String::from_utf8_lossy(body));
        }
    }

    /// The probe is what decides *which* 404 variant this is, and the fix must not have moved
    /// that line. With the probe off, the same request is ambiguous again — and the server's
    /// sentence has to survive the trip through the other variant too.
    #[test]
    fn the_probe_still_decides_which_404_this_is() {
        let body = br#"["could not find 'no-such-branch' to be a commit, branch or tag"]"#;
        let mut c = create_pull_ctx();
        c.repo_probe = RepoProbe::NotAttempted;
        let ErrorKind::RepoNotFound { slug, server_message, .. } = classify(404, &json(), body, &c)
        else {
            panic!("expected RepoNotFound when the probe never ran");
        };
        assert_eq!(slug, "perf3ct/gea");
        assert_eq!(
            server_message.as_deref(),
            Some("could not find 'no-such-branch' to be a commit, branch or tag")
        );

        c.repo_probe = RepoProbe::Missing;
        assert!(matches!(classify(404, &json(), body, &c), ErrorKind::RepoNotFound { .. }));

        // ...and the probe is still asked for, which is the half of the mechanism that lives in
        // the transport.
        assert_eq!(
            probe_target(404, &c.path),
            Some(("perf3ct".to_owned(), "gea".to_owned())),
            "the disambiguation probe must still fire for a collection path"
        );
    }

    /// The end-to-end rendering of the real response from Forgejo 16.0.4 (measured for fjo, which gea was ported from). Before this change it
    /// read "that pull request does not exist … the identifier is what is wrong" and pointed at
    /// `gea pr list --state all`, for a create.
    #[test]
    fn snapshot_404_post_to_a_collection_names_the_real_reason() {
        insta::assert_snapshot!(rendered(
            404,
            &json(),
            br#"["could not find 'no-such-branch' to be a commit, branch or tag, or the branch does not exist"]"#,
            &create_pull_ctx()
        ));
    }

    /// The same failure when the probe did not run: a different variant, the same sentence.
    #[test]
    fn snapshot_404_post_to_a_collection_without_the_probe() {
        let mut c = create_pull_ctx();
        c.repo_probe = RepoProbe::NotAttempted;
        insta::assert_snapshot!(rendered(
            404,
            &json(),
            br#"["could not find 'no-such-branch' to be a commit, branch or tag, or the branch does not exist"]"#,
            &c
        ));
    }

    /// A 404 that *does* name an object keeps both: the identifier it named and the server's
    /// reason for rejecting it.
    #[test]
    fn snapshot_404_on_an_object_keeps_the_message_as_well() {
        let mut c = ctx();
        c.repo_probe = RepoProbe::Exists;
        insta::assert_snapshot!(rendered(
            404,
            &json(),
            br#"{"message":"pull request 4212 has been deleted"}"#,
            &c
        ));
    }

    /// The probe 404'd too. This really is ambiguous — missing, private, or wrong host — and the
    /// renderer must be allowed to say all three.
    #[test]
    fn a_404_with_a_missing_repo_is_reported_as_ambiguous() {
        let mut c = ctx();
        c.repo_probe = RepoProbe::Missing;
        assert!(matches!(
            classify(404, &json(), br#"{"message":"not found"}"#, &c),
            ErrorKind::RepoNotFound { .. }
        ));
    }

    /// An HTML body means we reached the web router: the API endpoint does not exist on this
    /// instance. Reporting a missing object here sends the user hunting for a pull request that
    /// was never the problem.
    #[test]
    fn a_404_with_an_html_body_is_a_missing_endpoint() {
        let mut c = ctx();
        c.repo_probe = RepoProbe::Exists;
        c.instance = Some("gitea 7.0.0".into());
        let ErrorKind::RouteNotFound { instance, .. } = classify(
            404,
            &headers(&[("content-type", "text/html")]),
            b"<!DOCTYPE html><html>Not Found</html>",
            &c,
        ) else {
            panic!("expected RouteNotFound");
        };
        assert_eq!(instance.as_deref(), Some("gitea 7.0.0"));
    }

    #[test]
    fn a_404_on_an_unknown_root_is_a_missing_endpoint() {
        let mut c = ctx();
        c.path = "/api/v1/quota/attachments".into();
        assert!(matches!(
            classify(404, &json(), br#"{"message":"Not Found"}"#, &c),
            ErrorKind::RouteNotFound { .. }
        ));
    }

    #[test]
    fn a_404_on_the_repository_itself_needs_no_probe() {
        let mut c = ctx();
        c.path = "/api/v1/repos/perf3ct/nope".into();
        let ErrorKind::RepoNotFound { slug, .. } = classify(404, &json(), b"{}", &c) else {
            panic!("expected RepoNotFound");
        };
        assert_eq!(slug, "perf3ct/nope");
        assert_eq!(probe_target(404, &c.path), None, "probing would repeat the failed request");
    }

    #[test]
    fn probe_target_names_the_repo_for_inner_paths_only() {
        assert_eq!(
            probe_target(404, "/api/v1/repos/o/r/pulls/1"),
            Some(("o".to_owned(), "r".to_owned()))
        );
        assert_eq!(probe_target(404, "/api/v1/repos/o/r"), None);
        assert_eq!(probe_target(200, "/api/v1/repos/o/r/pulls/1"), None);
        assert_eq!(probe_target(404, "/api/v1/user"), None);
    }

    /// Paths nest general → specific, so the *last* pair is the thing that is missing:
    /// `/issues/5/comments/91` is a missing comment.
    #[test]
    fn the_most_specific_resource_in_the_path_is_named() {
        assert_eq!(resource_of(&["issues", "5", "comments", "91"]), ("comment", "91".to_owned()));
        assert_eq!(resource_of(&["pulls", "4212"]), ("pull request", "4212".to_owned()));
        assert_eq!(resource_of(&["releases", "tags", "v1.0"]), ("tag", "v1.0".to_owned()));
        assert_eq!(resource_of(&["contents", "src/main.rs"]), ("file", "src/main.rs".to_owned()));
        assert_eq!(resource_of(&["issues", "5", "comments"]), ("comment", String::new()));
    }

    /// Bug this prevents: `GET /repos/o/r/contents/src/main.rs` reported as `file: src`. A URL
    /// arrives here already split on `/`, so a path-like parameter is several elements of the
    /// tail and taking one of them names a directory that was never what 404'd.
    #[test]
    fn a_path_like_identifier_keeps_every_segment_of_itself() {
        assert_eq!(
            resource_of(&["contents", "src", "main.rs"]),
            ("file", "src/main.rs".to_owned())
        );
        assert_eq!(
            resource_of(&["raw", "docs", "guide", "index.md"]),
            ("file", "docs/guide/index.md".to_owned())
        );
        assert_eq!(resource_of(&["media", "img", "logo.png"]), ("file", "img/logo.png".to_owned()));
        assert_eq!(
            resource_of(&["editorconfig", "src", "lib.rs"]),
            ("file", "src/lib.rs".to_owned())
        );
        // `git/refs/{ref}`: `refs/heads/feature/x` is four segments and one ref.
        assert_eq!(
            resource_of(&["git", "refs", "refs", "heads", "feature", "x"]),
            ("ref", "refs/heads/feature/x".to_owned())
        );
        // Ending on the collection still yields no identifier, which the caller reads as "the
        // path named nothing".
        assert_eq!(resource_of(&["contents"]), ("file", String::new()));

        // And the qualifier that keeps this safe: `commits` is *not* path-like, because its
        // path-like `{ref}` is followed by more route. Swallowing the tail there would report a
        // missing commit called `abc123/status`.
        assert_eq!(resource_of(&["commits", "abc123", "status"]), ("commit", "abc123".to_owned()));
    }

    /// The same thing through the real entry point, since that is where the segments get split.
    #[test]
    fn a_404_on_a_nested_file_names_the_whole_path() {
        let mut c = ctx();
        c.repo_probe = RepoProbe::Exists;
        c.path = "/api/v1/repos/perf3ct/gea/contents/src/main.rs".into();
        let ErrorKind::ResourceNotFound { kind, id, .. } =
            classify(404, &json(), br#"{"message":"Not Found"}"#, &c)
        else {
            panic!("expected ResourceNotFound");
        };
        assert_eq!((kind, id.as_str()), ("file", "src/main.rs"));
    }

    // -------------------------------------------------------------- the rest of the table

    #[test]
    fn the_remaining_statuses_map_as_specified() {
        let c = ctx();
        assert!(matches!(
            classify(409, &json(), br#"{"message":"already exists"}"#, &c),
            ErrorKind::Conflict { .. }
        ));
        assert!(matches!(
            classify(413, &json(), br#"{"message":"quota exceeded"}"#, &c),
            ErrorKind::QuotaExceeded { .. }
        ));
        assert!(matches!(
            classify(422, &json(), br#"{"errors":["title is empty"]}"#, &c),
            ErrorKind::Validation { .. }
        ));
        assert!(matches!(classify(423, &json(), b"{}", &c), ErrorKind::Archived { .. }));
        assert!(matches!(
            classify(500, &json(), b"{}", &c),
            ErrorKind::ServerError { status: 500, .. }
        ));
        assert!(matches!(
            classify(418, &json(), b"teapot", &c),
            ErrorKind::UnexpectedStatus { .. }
        ));
    }

    #[test]
    fn a_423_names_the_archived_repository() {
        let ErrorKind::Archived { slug, .. } = classify(423, &json(), b"{}", &ctx()) else {
            panic!("expected Archived");
        };
        assert_eq!(slug, "perf3ct/gea");
    }

    #[test]
    fn a_429_honours_retry_after() {
        let h = headers(&[("content-type", "application/json"), ("retry-after", "42")]);
        let ErrorKind::RateLimited { retry_after, .. } = classify(429, &h, b"{}", &ctx()) else {
            panic!("expected RateLimited");
        };
        assert_eq!(retry_after, Some(std::time::Duration::from_secs(42)));
    }

    // ------------------------------------------------ the three 422 shapes, end to end
    //
    // One snapshot per shape, of the *rendered* diagnostic. Together they are the executable
    // form of the rule in the module comment: whichever shape Gitea picks, its words reach the
    // user.

    fn topics_ctx() -> ClassifyCtx {
        let mut c = ctx();
        c.method = "PUT".into();
        c.path = "/api/v1/repos/perf3ct/gea/topics".into();
        c
    }

    #[test]
    fn snapshot_422_message_envelope() {
        insta::assert_snapshot!(rendered(
            422,
            &json(),
            br#"{"message":"user does not exist [name: nope]","url":"https://git.example.org/api/swagger"}"#,
            &ctx()
        ));
    }

    #[test]
    fn snapshot_422_errors_array() {
        insta::assert_snapshot!(rendered(
            422,
            &json(),
            br#"{"errors":["title is empty","body is longer than 65535 characters"]}"#,
            &ctx()
        ));
    }

    #[test]
    fn snapshot_422_field_map() {
        insta::assert_snapshot!(rendered(
            422,
            &json(),
            br#"{"title":["can't be blank"],"assignees":["user does not exist [name: nope]"]}"#,
            &ctx()
        ));
    }

    /// **The topics regression.** `PUT /repos/{o}/{r}/topics` answers a bad name with an envelope
    /// *and* a field map in one object. Recognising `message` and stopping there is what left the
    /// user with "the server rejected the values in this request" and no idea which value.
    #[test]
    fn snapshot_422_invalid_topics_names_the_bad_value() {
        insta::assert_snapshot!(rendered(
            422,
            &json(),
            br#"{"invalidTopics":["Bad Topic","another bad one"],"message":"Topic names are invalid"}"#,
            &topics_ctx()
        ));
    }

    #[test]
    fn invalid_topics_survives_alongside_the_message() {
        let b = parse_body(
            &json(),
            br#"{"invalidTopics":["Bad Topic"],"message":"Topic names are invalid"}"#,
        );
        assert_eq!(b.message.as_deref(), Some("Topic names are invalid"));
        assert_eq!(
            b.fields,
            vec![FieldError { field: Some("invalidTopics".into()), message: "Bad Topic".into() }],
            "the rejected value must survive the envelope"
        );

        let ErrorKind::Validation { fields, server_message } = classify(
            422,
            &json(),
            br#"{"invalidTopics":["Bad Topic"],"message":"Topic names are invalid"}"#,
            &topics_ctx(),
        ) else {
            panic!("expected Validation");
        };
        assert_eq!(fields.len(), 1);
        assert_eq!(server_message.as_deref(), Some("Topic names are invalid"));
    }

    /// The supplementary pass must skip envelope keys rather than adopt them, and must not be
    /// derailed by a key whose value is not a complaint.
    #[test]
    fn the_supplementary_field_pass_ignores_envelope_and_non_string_keys() {
        let b = parse_body(
            &json(),
            br#"{"message":"nope","url":"https://x","documentation_url":"https://y","ok":false,"invalidTopics":["Bad"]}"#,
        );
        assert_eq!(
            b.fields,
            vec![FieldError { field: Some("invalidTopics".into()), message: "Bad".into() }]
        );
    }

    // ------------------------------------------------------------------------ 405

    /// The defect this module comment now documents: a 405 is Gitea's answer to an un-mergeable
    /// pull request, and `RouteNotFound` has nowhere to put the reason.
    #[test]
    fn snapshot_405_merge_refusal_keeps_the_reason() {
        let mut c = ctx();
        c.method = "POST".into();
        c.path = "/api/v1/repos/perf3ct/gea/pulls/4212/merge".into();
        insta::assert_snapshot!(rendered(
            405,
            &json(),
            br#"{"message":"Please try again later: the head branch is out of date with the base branch"}"#,
            &c
        ));
    }

    #[test]
    fn a_405_with_a_message_is_a_state_conflict_that_names_the_resource() {
        let mut c = ctx();
        c.method = "POST".into();
        c.path = "/api/v1/repos/perf3ct/gea/pulls/4212/merge".into();
        let ErrorKind::StateConflict { resource, server_message, .. } =
            classify(405, &json(), br#"{"message":"the pull request is closed"}"#, &c)
        else {
            panic!("expected StateConflict");
        };
        assert_eq!(resource.as_deref(), Some("pull request 4212"));
        assert_eq!(server_message, "the pull request is closed");
    }

    /// A 405 with nothing to say, or an HTML page, really is a missing route — the same test
    /// `classify_404` applies, and the reason the 405 arm is not simply always a refusal.
    #[test]
    fn a_405_with_no_message_is_still_a_missing_route() {
        let mut c = ctx();
        c.method = "DELETE".into();
        c.path = "/api/v1/repos/perf3ct/gea/actions/runs/12".into();
        assert!(matches!(classify(405, &json(), b"", &c), ErrorKind::RouteNotFound { .. }));
        assert!(matches!(
            classify(
                405,
                &headers(&[("content-type", "text/html")]),
                b"<!DOCTYPE html><html>Method Not Allowed</html>",
                &c
            ),
            ErrorKind::RouteNotFound { .. }
        ));
    }

    // -------------------------------------------------------------- the scope wiring

    /// `OpMeta::scope` reaches the `needs:` line. `with_op_scope` takes exactly the shape the
    /// generated table holds, so the call site is one expression.
    #[test]
    fn the_operations_own_scope_names_the_needs_line() {
        let c = ctx().with_op_scope(Some("write:issue"));
        assert_eq!(c.needed_scope, vec!["write:issue".to_owned()]);

        let ErrorKind::InsufficientScope { needed, .. } =
            classify(403, &json(), br#"{"message":"token does not have required scope"}"#, &c)
        else {
            panic!("expected InsufficientScope");
        };
        assert_eq!(needed, vec!["write:issue"]);

        let text =
            rendered(403, &json(), br#"{"message":"token does not have required scope"}"#, &c);
        let needs =
            text.lines().find(|l| l.trim_start().starts_with("needs:")).expect("a needs: line");
        assert_eq!(needs.split_whitespace().collect::<Vec<_>>(), ["needs:", "write:issue"]);
    }

    /// **The regression this restructuring exists to prevent.** Every operation needs *some*
    /// scope, so once `needed_scope` is populated it cannot be allowed to decide that a 403 is a
    /// scope problem — or "user is not a collaborator" turns into advice to mint a token that
    /// will not help.
    #[test]
    fn a_known_scope_does_not_turn_every_403_into_a_scope_problem() {
        let c = ctx().with_op_scope(Some("write:issue"));
        let ErrorKind::Forbidden { server_message } = classify(
            403,
            &json(),
            br#"{"message":"user is not a collaborator on this repository"}"#,
            &c,
        ) else {
            panic!("expected Forbidden even though the operation's scope is known");
        };
        assert_eq!(server_message, "user is not a collaborator on this repository");
    }

    /// Priority: what the server named beats what codegen recorded.
    #[test]
    fn the_servers_own_scope_list_outranks_the_recorded_one() {
        let c = ctx().with_op_scope(Some("write:repository"));
        let ErrorKind::InsufficientScope { needed, .. } = classify(
            403,
            &json(),
            br#"{"message":"token does not have at least one of required scope(s): [write:issue]"}"#,
            &c,
        ) else {
            panic!("expected InsufficientScope");
        };
        assert_eq!(needed, vec!["write:issue"]);
    }

    #[test]
    fn with_op_scope_treats_none_and_empty_alike() {
        assert!(ctx().with_op_scope(None).needed_scope.is_empty());
        assert!(ctx().with_op_scope(Some("  ")).needed_scope.is_empty());
        // And the fallback still names something, so `needs:` is never blank.
        assert_eq!(ctx().with_op_scope(None).scope_to_name(), vec!["read:issue".to_owned()]);
    }

    /// A 5xx from a Go panic or a reverse proxy is not JSON, and its text is the only clue.
    #[test]
    fn a_500_keeps_a_non_json_body() {
        let ErrorKind::ServerError { server_message, .. } = classify(
            502,
            &headers(&[("content-type", "text/plain")]),
            b"upstream connect error",
            &ctx(),
        ) else {
            panic!("expected ServerError");
        };
        assert_eq!(server_message, "upstream connect error");
    }
}
