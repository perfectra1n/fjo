//! `gea mirror` — push and pull mirrors.
//!
//! # Two different features with one name
//!
//! Gitea calls both of these "mirroring" and they behave nothing alike:
//!
//! | | pull mirror | push mirror |
//! | --- | --- | --- |
//! | direction | remote → here | here → remote |
//! | how many | **one**, and only settable **when the repository is created** | any number |
//! | the repository | read-only; you cannot push to it | a normal repository |
//! | API | `POST /repos/migrate` with `mirror: true` | `POST …/push_mirrors` |
//!
//! The asymmetry is the thing to communicate, because it is not a limitation anyone expects:
//! **a pull mirror cannot be added to an existing repository.** There is no endpoint for it. It
//! belongs to `gea repo create --mirror-from`, which is another wave's file — so this group's
//! help names that command rather than leaving the user to conclude the feature is missing.
//!
//! # What a push mirror actually does
//!
//! Three behaviours that are not obvious from the API's field names, all surfaced in `--help`:
//!
//! * **It is a force push.** The remote's history is replaced, not merged. A push mirror pointed
//!   at a repository someone else commits to will discard their commits.
//! * **It is `git push --mirror`**: every branch and every tag, and refs deleted here are deleted
//!   there. Gitea has no per-mirror branch filter, so there is no way to narrow it.
//! * **`--sync-on-commit`** pushes on every commit instead of only on the interval.
//!
//! Gitea's push-mirror API takes a username and password (or token) and nothing else: there is no
//! SSH-key authentication for push mirrors, so an address should be `https://`.

use clap::{Args as ClapArgs, Subcommand};
use futures::StreamExt;
use gitea_client::Api;
use gitea_core::error::{Error, ErrorKind, Result};
use gitea_core::types::RepoSlug;
use gitea_model::{PushMirror, Repository};

use crate::cmd::times::duration;
use crate::cmd::times::porcelain::{self, Fields, Machine};
use crate::global::GlobalOpts;
use crate::output::Term;
use crate::runtime::Runtime;

const MIRROR_FIELDS: Fields = Fields::Generated(gitea_client::fields::FIELDS_PUSH_MIRROR);

const LONG_ABOUT: &str = "\
Manage push mirrors and sync existing pull mirrors.

A push mirror copies this repository to a remote. It force-pushes changes and can
overwrite remote history: every branch, tag and deletion is mirrored, like
`git push --mirror`. --sync-on-commit syncs on each commit as well as at the
configured interval.

A pull mirror is read-only and must be set up when creating the repository:
`gea repo create <name> --mirror-from <url>`. Once created, use `mirror sync`
and `mirror status` to manage it.

  gea repo create <name> --mirror-from <url>
  gea mirror list
  gea mirror add https://github.com/me/proj.git --username me --interval 8h
  gea mirror add https://codeberg.org/me/proj.git --sync-on-commit
  gea mirror sync
  gea mirror status";

#[derive(Debug, ClapArgs)]
#[command(long_about = LONG_ABOUT)]
pub struct Args {
    #[command(subcommand)]
    pub command: Cmd,
}

#[derive(Debug, Subcommand)]
pub enum Cmd {
    /// List this repository's push mirrors
    List,
    /// Add a push mirror
    Add(AddArgs),
    /// Remove a push mirror
    Delete(DeleteArgs),
    /// Sync now, rather than waiting for the interval
    Sync(SyncArgs),
    /// Mirror configuration and the last result for each
    Status,
}

#[derive(Debug, ClapArgs)]
#[command(long_about = "\
Add a push mirror.

Syncs force-push this repository to the remote. Commits that exist only on the
remote will be lost. All branches, tags, and deletions are mirrored.

Pass --username and enter the password at the prompt. Passwords in URLs can be
saved in shell history and Gitea's stored remote.

  gea mirror add https://github.com/me/proj.git --username me
  gea mirror add https://git.example.org/me/proj.git --interval 30m --sync-on-commit")]
pub struct AddArgs {
    /// Where to push: an https:// URL
    #[arg(value_name = "ADDRESS")]
    pub address: String,

    /// How often to sync: 8h, 30m, 10m0s. `0` disables scheduled syncing
    #[arg(long, value_name = "DURATION")]
    pub interval: Option<String>,

    /// Also push whenever a commit arrives, not only on the interval
    #[arg(long)]
    pub sync_on_commit: bool,

    /// Username on the remote
    #[arg(long, value_name = "USER")]
    pub username: Option<String>,

    /// Password or token; prompted for when --username is given and this is not
    #[arg(long, value_name = "SECRET")]
    pub password: Option<String>,
}

#[derive(Debug, ClapArgs)]
pub struct DeleteArgs {
    /// The mirror's remote name, from the REMOTE column of `gea mirror list`
    #[arg(value_name = "REMOTE")]
    pub remote: String,

    /// Skip the confirmation
    #[arg(long)]
    pub yes: bool,
}

#[derive(Debug, ClapArgs)]
#[command(long_about = "\
Sync mirrors now.

Without flags, syncs both pull and push mirrors for this repository.
Gitea syncs all push mirrors together; individual mirrors cannot be selected.")]
pub struct SyncArgs {
    /// Only the push mirrors (this repository → remotes)
    #[arg(long)]
    pub push: bool,

    /// Only the pull mirror (remote → this repository)
    #[arg(long, conflicts_with = "push")]
    pub pull: bool,
}

impl Cmd {
    fn fields(&self) -> Option<Fields> {
        match self {
            Self::List | Self::Add(_) | Self::Status => Some(MIRROR_FIELDS),
            // Both are 204s.
            Self::Delete(_) | Self::Sync(_) => None,
        }
    }
}

pub fn run(globals: &GlobalOpts, args: &Args) -> Result<()> {
    if let Some(fields) = args.command.fields()
        && porcelain::discovery(globals, fields)?
    {
        return Ok(());
    }
    crate::runtime::block_on(async move {
        let rt = Runtime::new(globals)?;
        let api = Api::new(rt.client().clone());
        let slug = rt.repo(globals)?.slug.clone();
        match &args.command {
            Cmd::List => list(&rt, &api, globals, &slug).await,
            Cmd::Add(a) => add(&rt, &api, globals, &slug, a).await,
            Cmd::Delete(a) => delete(&rt, &api, &slug, a).await,
            Cmd::Sync(a) => sync(&rt, &api, &slug, a).await,
            Cmd::Status => status(&rt, &api, globals, &slug).await,
        }
    })
}

// -------------------------------------------------------------------------------------- list

async fn list(rt: &Runtime, api: &Api, globals: &GlobalOpts, slug: &RepoSlug) -> Result<()> {
    let mirrors = fetch(api, globals, slug).await?;
    let machine = Machine::compile(globals, MIRROR_FIELDS)?;
    if mirrors.is_empty() {
        return porcelain::empty(
            globals,
            rt.term(),
            machine.as_ref(),
            &format!(
                "{slug} has no push mirrors. Add one with `gea mirror add <address>`. For a new pull mirror, use `gea repo create --mirror-from`."
            ),
        );
    }
    if let Some(m) = machine {
        return m.write(globals, rt.term(), porcelain::json_of(&mirrors)?);
    }

    porcelain::print(globals, &render_list(rt.term(), &mirrors))?;
    if mirrors.iter().any(|m| !m.last_error.trim().is_empty()) {
        porcelain::note(
            rt.term(),
            "A mirror with an ERROR is not retrying on its own schedule until the cause is fixed; \
             `gea mirror sync --push` retries now.",
        );
    }
    Ok(())
}

/// The `list` table.
pub(crate) fn render_list(term: &Term, mirrors: &[PushMirror]) -> String {
    let mut t = porcelain::table(term);
    t.headers(["REMOTE", "ADDRESS", "INTERVAL", "ON COMMIT", "LAST SYNC", "ERROR"]);
    for m in mirrors {
        t.row([
            porcelain::dash(&m.remote_name),
            porcelain::dash(&m.remote_address),
            porcelain::dash(&m.interval),
            if m.sync_on_commit { "yes".to_owned() } else { "no".to_owned() },
            porcelain::when(m.last_update),
            porcelain::dash(&first_line(&m.last_error)),
        ]);
    }
    porcelain::rendered_table(term, t, "push mirrors", None)
}

// --------------------------------------------------------------------------------------- add

async fn add(
    rt: &Runtime,
    api: &Api,
    globals: &GlobalOpts,
    slug: &RepoSlug,
    args: &AddArgs,
) -> Result<()> {
    if let Some(warning) = credentials_in_address(&args.address) {
        porcelain::note(rt.term(), &warning);
    }

    let interval = match &args.interval {
        Some(text) => Some(go_interval(text)?),
        None => None,
    };
    let password = match (&args.username, &args.password) {
        (Some(user), None) => Some(porcelain::ask_secret(
            rt,
            &format!("Password or token for {user} on the remote"),
            "--password (or omit --username for an unauthenticated remote)",
        )?),
        (_, given) => given.clone(),
    };

    let body = gitea_model::CreatePushMirrorOption {
        interval,
        remote_address: Some(args.address.clone()),
        remote_password: password,
        remote_username: args.username.clone(),
        sync_on_commit: Some(args.sync_on_commit),
    };
    let mirror =
        api.repo().add_push_mirror(&slug.owner, &slug.name, &body).await.map_err(explain)?;

    porcelain::note(
        rt.term(),
        &format!(
            "Mirroring {slug} to {} as {}: all branches and tags (force push, like git push \
             --mirror), every {}",
            porcelain::dash(&mirror.remote_address),
            porcelain::dash(&mirror.remote_name),
            porcelain::dash(&mirror.interval)
        ),
    );
    match Machine::compile(globals, MIRROR_FIELDS)? {
        Some(m) => m.write(globals, rt.term(), porcelain::json_of(&mirror)?),
        None => Ok(()),
    }
}

/// A warning when the address embeds credentials.
///
/// Gitea stores the remote address verbatim, so a password in the URL is persisted server-side
/// *and* is now in the user's shell history. `--username`/`--password` keep it out of both.
fn credentials_in_address(address: &str) -> Option<String> {
    let after_scheme = address.split_once("://").map(|(_, rest)| rest).unwrap_or(address);
    let authority = after_scheme.split('/').next().unwrap_or("");
    // `git@host:owner/name` is SSH's own syntax and carries no secret; a colon before the `@` is
    // what makes it a password.
    let (userinfo, _) = authority.split_once('@')?;
    if !userinfo.contains(':') {
        return None;
    }
    Some(
        "note: this URL contains a password that may be saved in shell history and Gitea. Use --username and enter the password at the prompt instead."
            .to_owned(),
    )
}

/// Validate an interval and render it the way Go's `time.ParseDuration` reads back.
///
/// Gitea parses the field with `time.ParseDuration` and rejects anything below its configured
/// `MIN_INTERVAL` (ten minutes by default). Reusing the [`duration`] grammar means `--interval 8h`
/// and `--interval 30m` both work, and re-emitting the canonical `8h0m0s` form avoids depending on
/// which spellings that parser accepts.
fn go_interval(text: &str) -> Result<String> {
    let nanos = duration::parse_nanos(text)?;
    let seconds = (nanos / 1_000_000_000) as i64;
    if seconds == 0 {
        // Gitea reads an empty interval as "never on a schedule"; `0` is how a user asks for
        // that, and `0s` is what Go's parser accepts for it.
        return Ok("0s".to_owned());
    }
    if seconds < 0 {
        return Err(porcelain::usage("a mirror interval cannot be negative"));
    }
    Ok(format!("{}h{}m{}s", seconds / 3_600, seconds % 3_600 / 60, seconds % 60))
}

// ------------------------------------------------------------------------------------ delete

async fn delete(rt: &Runtime, api: &Api, slug: &RepoSlug, args: &DeleteArgs) -> Result<()> {
    porcelain::confirm(rt, &format!("Stop mirroring {slug} to {}?", args.remote), args.yes)?;
    api.repo()
        .delete_push_mirror(&slug.owner, &slug.name, &args.remote)
        .await
        .map_err(|e| explain_named(e, &args.remote))?;
    porcelain::note(rt.term(), &format!("Removed push mirror {}", args.remote));
    Ok(())
}

// -------------------------------------------------------------------------------------- sync

async fn sync(rt: &Runtime, api: &Api, slug: &RepoSlug, args: &SyncArgs) -> Result<()> {
    // Which endpoints apply is a property of the repository, so read it first rather than making
    // the user know. `mirror-sync` on a repository that is not a pull mirror is an error, and
    // `push_mirrors-sync` on one with no push mirrors does nothing quietly.
    let (repo, has_push) = sync_state(api, slug).await?;

    let do_pull = args.pull || (!args.push && repo.mirror);
    let do_push = args.push || (!args.pull && has_push);

    if !do_pull && !do_push {
        return Err(porcelain::usage(format!(
            "{slug} has no mirrors to sync. Add a push mirror with `gea mirror add <address>`. Pull mirrors must be created with `gea repo create --mirror-from`."
        )));
    }

    if do_pull {
        if !repo.mirror {
            return Err(porcelain::usage(format!(
                "{slug} is not a pull mirror. Pull mirroring must be enabled when creating the repository."
            )));
        }
        api.repo().mirror_sync(&slug.owner, &slug.name).await.map_err(explain)?;
        porcelain::note(
            rt.term(),
            &format!(
                "Queued a pull-mirror sync of {slug} from {}",
                porcelain::dash(&repo.original_url)
            ),
        );
    }
    if do_push {
        api.repo().push_mirror_sync(&slug.owner, &slug.name).await.map_err(explain)?;
        porcelain::note(
            rt.term(),
            &format!(
                "Queued a push to all mirrors of {slug}. Check results with `gea mirror list`."
            ),
        );
    }
    Ok(())
}

// ------------------------------------------------------------------------------------ status

async fn status(rt: &Runtime, api: &Api, globals: &GlobalOpts, slug: &RepoSlug) -> Result<()> {
    let (repo, mirrors) = status_state(api, globals, slug).await?;

    if let Some(m) = Machine::compile(globals, MIRROR_FIELDS)? {
        return m.write(globals, rt.term(), porcelain::json_of(&mirrors)?);
    }

    porcelain::print(globals, &render_status(rt.term(), slug, &repo, &mirrors))
}

/// The `status` detail view: the pull mirror (or the fact that there is none, and why one cannot
/// be added), then every push mirror with the four behaviours that surprise people.
pub(crate) fn render_status(
    term: &Term,
    slug: &RepoSlug,
    repo: &Repository,
    mirrors: &[PushMirror],
) -> String {
    // `let _ =` throughout: `fmt::Write` on a `String` cannot fail.
    use std::fmt::Write as _;
    let mut o = String::new();

    if !term.tty {
        // One line per mirror, prefixed by direction, so both kinds share one stable shape:
        // `direction<TAB>name<TAB>address<TAB>interval<TAB>error`.
        if repo.mirror {
            let _ = writeln!(o, "pull\t-\t{}\t{}\t", repo.original_url, repo.mirror_interval);
        }
        for m in mirrors {
            let _ = writeln!(
                o,
                "push\t{}\t{}\t{}\t{}",
                m.remote_name,
                m.remote_address,
                m.interval,
                first_line(&m.last_error)
            );
        }
        return o;
    }

    let _ = writeln!(o, "{slug}");
    let _ = writeln!(o);
    if repo.mirror {
        let _ = writeln!(o, "pull mirror  (this repository is read-only)");
        let _ = writeln!(o, "  from      {}", porcelain::dash(&repo.original_url));
        let _ = writeln!(o, "  interval  {}", porcelain::dash(&repo.mirror_interval));
        let _ = writeln!(o, "  last      {}", porcelain::when(repo.mirror_updated));
    } else {
        let _ = writeln!(o, "pull mirror  none");
        let _ = writeln!(
            o,
            "  Gitea can only make a repository a pull mirror when it is created:\n  \
             `gea repo create <name> --mirror-from <url>`."
        );
    }
    let _ = writeln!(o);
    if mirrors.is_empty() {
        let _ = writeln!(o, "push mirrors none  (`gea mirror add <address>`)");
        return o;
    }
    let _ = writeln!(o, "push mirrors  {} (force push of every branch and tag)", mirrors.len());
    for m in mirrors {
        let _ = writeln!(o);
        let _ = writeln!(o, "  {}", porcelain::dash(&m.remote_name));
        let _ = writeln!(o, "    to        {}", porcelain::dash(&m.remote_address));
        let _ = writeln!(o, "    interval  {}", porcelain::dash(&m.interval));
        let _ = writeln!(o, "    on commit {}", if m.sync_on_commit { "yes" } else { "no" });
        let _ = writeln!(o, "    last sync {}", porcelain::when(m.last_update));
        if !m.last_error.trim().is_empty() {
            let _ = writeln!(o, "    error     {}", first_line(&m.last_error));
        }
    }
    o
}

// ------------------------------------------------------------------------------------ shared

/// The repository and whether it has any push mirror, concurrently.
///
/// `join!` rather than `try_join!`, and this one is not a preference: the mirror listing is
/// *allowed* to fail — a non-`Ok` first item is read below as "no push mirrors", which is how an
/// instance with `ALLOW_PUSH_MIRRORS = false` still gets a working `mirror sync --pull`.
/// `try_join!` would turn that tolerated failure into a hard abort. `repo` is unwrapped first,
/// through [`explain`], so the error precedence is the serial version's.
///
/// Split out from [`sync`] so a `FakeTransport` test can assert both requests without a
/// [`Runtime`].
async fn sync_state(api: &Api, slug: &RepoSlug) -> Result<(Repository, bool)> {
    let query = gitea_client::query::RepoListPushMirrorsQuery::default();
    // Bound rather than called inline: `api.repo()` returns a borrow of `api`, and a temporary of
    // it does not outlive the `join!` that awaits both futures.
    let repos = api.repo();
    let (repo, mirrors) = futures::join!(
        repos.get(&slug.owner, &slug.name),
        repos.list_push_mirrors(&slug.owner, &slug.name, &query).take(1).collect::<Vec<_>>(),
    );
    let repo: Repository = repo.map_err(explain)?;
    Ok((repo, mirrors.first().is_some_and(std::result::Result::is_ok)))
}

/// The repository and its push mirrors, concurrently.
///
/// `join!` rather than `try_join!` for the same reason as everywhere else in this build: the
/// error a user is shown must not depend on which request the network answered first. `repo` is
/// unwrapped before `mirrors`, which is the order the serial version reported them in.
///
/// Split out from [`status`] so a `FakeTransport` test can assert both requests without a
/// [`Runtime`].
async fn status_state(
    api: &Api,
    globals: &GlobalOpts,
    slug: &RepoSlug,
) -> Result<(Repository, Vec<PushMirror>)> {
    // Bound rather than called inline: `api.repo()` returns a borrow of `api`, and a temporary of
    // it does not outlive the `join!` that awaits both futures.
    let repos = api.repo();
    let (repo, mirrors) =
        futures::join!(repos.get(&slug.owner, &slug.name), fetch(api, globals, slug));
    let repo: Repository = repo.map_err(explain)?;
    Ok((repo, mirrors?))
}

async fn fetch(api: &Api, globals: &GlobalOpts, slug: &RepoSlug) -> Result<Vec<PushMirror>> {
    let query = gitea_client::query::RepoListPushMirrorsQuery::default();
    let take = porcelain::item_limit(globals).unwrap_or(usize::MAX);
    let mut stream = api.repo().list_push_mirrors(&slug.owner, &slug.name, &query).take(take);
    let mut out = Vec::new();
    while let Some(item) = stream.next().await {
        out.push(item.map_err(explain)?);
    }
    Ok(out)
}

fn first_line(s: &str) -> String {
    s.lines().next().unwrap_or("").trim().to_owned()
}

/// Push-mirror endpoints 404 when the *instance* has mirroring switched off, which is
/// indistinguishable from a missing repository by status alone.
fn explain(e: Error) -> Error {
    match &*e.kind {
        ErrorKind::RouteNotFound { .. } => Error::new(ErrorKind::Usage(
            "this instance did not answer the mirror endpoint. Mirroring can be switched off \
             instance-wide with [mirror] ENABLED = false (and push mirrors separately with \
             ALLOW_PUSH_MIRRORS = false) — ask an administrator, or check `gea nodeinfo`."
                .to_owned(),
        )),
        _ => e,
    }
}

fn explain_named(e: Error, remote: &str) -> Error {
    match &*e.kind {
        ErrorKind::ResourceNotFound { .. } => Error::new(ErrorKind::Usage(format!(
            "no push mirror named {remote:?}. Use the generated name from the REMOTE column of `gea mirror list`."
        ))),
        _ => explain(e),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::cmd::times::porcelain::testing;
    use gitea_core::http::transport::{Canned, FakeTransport};
    use std::sync::Arc;

    const MIRRORS: &str = r#"[
      {"remote_name":"remote_mirror_abc123",
       "remote_address":"https://github.com/me/proj.git",
       "interval":"8h0m0s","sync_on_commit":true,
       "last_error":"","last_update":null,"repo_name":"proj"},
      {"remote_name":"remote_mirror_def456",
       "remote_address":"https://codeberg.org/me/proj.git",
       "interval":"10m0s","sync_on_commit":false,
       "last_error":"authentication required\nsee the log","last_update":null,
       "repo_name":"proj"}
    ]"#;

    fn mirrors() -> Vec<PushMirror> {
        serde_json::from_str(MIRRORS).expect("the fixture is valid PushMirror JSON")
    }

    /// How many times the repository and the push-mirror listing were asked for.
    fn requests(t: &FakeTransport) -> (usize, usize) {
        let get = testing::method("GET");
        (
            t.calls_to(&get, "/api/v1/repos/them/proj").len(),
            t.calls_to(&get, "/api/v1/repos/them/proj/push_mirrors").len(),
        )
    }

    /// Bug this prevents: `sync` deciding which endpoints apply from two *serial* reads, and then
    /// — having overlapped them — a failing push-mirror listing aborting the whole command.
    /// A non-`Ok` first item is "no push mirrors", which is how an instance with push mirroring
    /// switched off still gets a working `mirror sync --pull`; `try_join!` would have turned that
    /// tolerated failure into a hard error.
    #[tokio::test]
    async fn sync_reads_the_repository_and_the_mirror_list_and_tolerates_the_list_failing() {
        let both = Arc::new(
            FakeTransport::new()
                .on(
                    testing::method("GET"),
                    "/api/v1/repos/them/proj",
                    Canned::json(200, r#"{"full_name":"them/proj","name":"proj","mirror":true}"#),
                )
                .on(
                    testing::method("GET"),
                    "/api/v1/repos/them/proj/push_mirrors",
                    testing::one_page(MIRRORS),
                ),
        );
        let slug = RepoSlug::new("them", "proj");
        let (repo, has_push) = sync_state(&testing::api(both.clone()), &slug).await.expect("sync");
        assert!(repo.mirror);
        assert!(has_push);
        // Counted per path rather than with `call_count`: the paginator's one-off capability
        // probe (`/settings/api`, `/version`) is cached on the client and is not this command's
        // cost, so counting every recorded call would measure the wrong thing.
        assert_eq!(requests(&both), (1, 1), "{:?}", both.calls());

        // The listing 404s — this instance has push mirroring switched off — and the pull mirror
        // is still syncable.
        let pull_only = Arc::new(
            FakeTransport::new()
                .on(
                    testing::method("GET"),
                    "/api/v1/repos/them/proj",
                    Canned::json(200, r#"{"full_name":"them/proj","name":"proj","mirror":true}"#),
                )
                .fallback(Canned::json(404, r#"{"message":"Not Found"}"#)),
        );
        let (repo, has_push) =
            sync_state(&testing::api(pull_only.clone()), &slug).await.expect("sync");
        assert!(repo.mirror);
        assert!(!has_push, "a failing listing is 'no push mirrors', not an error");
        assert_eq!(requests(&pull_only), (1, 1), "{:?}", pull_only.calls());
    }

    /// Bug this prevents: `status`'s two independent reads staying serial, and the error the user
    /// sees depending on which one the network answered first. The repository is unwrapped before
    /// the mirrors, which is the order the serial version reported them in.
    #[tokio::test]
    async fn status_reads_the_repository_and_its_mirrors_and_keeps_the_repositorys_error_first() {
        let fake = Arc::new(
            FakeTransport::new()
                .on(
                    testing::method("GET"),
                    "/api/v1/repos/them/proj",
                    Canned::json(200, r#"{"full_name":"them/proj","name":"proj"}"#),
                )
                .on(
                    testing::method("GET"),
                    "/api/v1/repos/them/proj/push_mirrors",
                    testing::one_page(MIRRORS),
                ),
        );
        let globals = GlobalOpts::default();
        let slug = RepoSlug::new("them", "proj");
        let (repo, mirrors) =
            status_state(&testing::api(fake.clone()), &globals, &slug).await.expect("status");
        assert_eq!(repo.full_name, "them/proj");
        assert_eq!(mirrors.len(), 2);
        assert_eq!(requests(&fake), (1, 1), "{:?}", fake.calls());

        // Both fail; the repository's error is the one reported, whichever arrived first.
        let broken = Arc::new(
            FakeTransport::new()
                .on(
                    testing::method("GET"),
                    "/api/v1/repos/them/proj",
                    Canned::json(500, r#"{"message":"boom"}"#),
                )
                .fallback(Canned::json(403, r#"{"message":"token does not have scope"}"#)),
        );
        let e = status_state(&testing::api(broken), &globals, &slug).await.unwrap_err();
        assert!(matches!(e.kind(), ErrorKind::ServerError { .. }), "{e:?}");
    }

    /// Bug this prevents: `add` putting the interval, the credentials or `sync_on_commit` in the
    /// wrong field — a mirror that syncs on every commit when the user asked for a schedule
    /// force-pushes every ref far more often than they expected.
    #[tokio::test]
    async fn add_posts_every_option_under_the_apis_own_field_name() {
        let fake = Arc::new(FakeTransport::new().on(
            testing::method("POST"),
            "/api/v1/repos/them/proj/push_mirrors",
            Canned::json(
                201,
                r#"{"remote_name":"remote_mirror_abc123",
              "remote_address":"https://github.com/me/proj.git",
              "interval":"0h30m0s","sync_on_commit":true}"#,
            ),
        ));
        let api = testing::api(fake.clone());
        let body = gitea_model::CreatePushMirrorOption {
            interval: Some(go_interval("30m").unwrap()),
            remote_address: Some("https://github.com/me/proj.git".to_owned()),
            remote_password: Some("hunter2".to_owned()),
            remote_username: Some("me".to_owned()),
            sync_on_commit: Some(true),
        };
        api.repo().add_push_mirror("them", "proj", &body).await.unwrap();

        let call =
            &fake.calls_to(&testing::method("POST"), "/api/v1/repos/them/proj/push_mirrors")[0];
        let sent: serde_json::Value =
            serde_json::from_slice(call.body.as_ref().expect("a JSON body")).unwrap();
        assert_eq!(sent["interval"], "0h30m0s");
        assert_eq!(sent["sync_on_commit"], true);
        assert_eq!(sent["remote_password"], "hunter2");
        assert_eq!(sent["remote_username"], "me");
    }

    /// Bug this prevents: `sync` calling the wrong endpoint. `mirror-sync` pulls *in* and
    /// `push_mirrors-sync` pushes *out*; the names are one hyphen apart and the directions are
    /// opposite, so getting it wrong on a pull mirror overwrites the upstream.
    #[tokio::test]
    async fn the_two_sync_endpoints_are_not_interchangeable() {
        let fake = Arc::new(
            FakeTransport::new()
                .on(
                    testing::method("POST"),
                    "/api/v1/repos/them/proj/mirror-sync",
                    Canned::new(200),
                )
                .on(
                    testing::method("POST"),
                    "/api/v1/repos/them/proj/push_mirrors-sync",
                    Canned::new(200),
                )
                .fallback(Canned::json(404, r#"{"message":"no"}"#)),
        );
        let api = testing::api(fake.clone());
        api.repo().mirror_sync("them", "proj").await.unwrap();
        api.repo().push_mirror_sync("them", "proj").await.unwrap();
        assert_eq!(
            fake.calls_to(&testing::method("POST"), "/api/v1/repos/them/proj/mirror-sync").len(),
            1
        );
        assert_eq!(
            fake.calls_to(&testing::method("POST"), "/api/v1/repos/them/proj/push_mirrors-sync")
                .len(),
            1
        );
    }

    #[test]
    fn the_list_table_renders_the_same_data_two_ways() {
        insta::assert_snapshot!("mirror_list_human", render_list(&testing::term(), &mirrors()));
        insta::assert_snapshot!("mirror_list_piped", render_list(&Term::piped(), &mirrors()));
    }

    #[test]
    fn the_json_output_uses_the_apis_own_field_names() {
        insta::assert_snapshot!(
            "mirror_list_json",
            testing::as_json(
                MIRROR_FIELDS,
                "remote_name,remote_address,interval,sync_on_commit",
                porcelain::json_of(&mirrors()).unwrap()
            )
        );
    }

    /// The `status` view for a repository that is *not* a pull mirror, which is the common case
    /// and the one where the help has to explain that one cannot be added later.
    #[test]
    fn the_status_view_explains_that_a_pull_mirror_cannot_be_added_later() {
        let repo = Repository::default();
        let out =
            render_status(&testing::term(), &RepoSlug::new("them", "proj"), &repo, &mirrors());
        assert!(out.contains("--mirror-from"), "{out}");
        insta::assert_snapshot!("mirror_status_human", out);
    }

    #[test]
    fn an_interval_is_validated_and_re_emitted_in_gos_own_form() {
        assert_eq!(go_interval("8h").unwrap(), "8h0m0s");
        assert_eq!(go_interval("30m").unwrap(), "0h30m0s");
        assert_eq!(go_interval("10m0s").unwrap(), "0h10m0s");
        assert_eq!(go_interval("1h30m").unwrap(), "1h30m0s");
        // `0` disables scheduled syncing, which is a real thing to ask for — so unlike a tracked
        // time it must not be refused.
        assert_eq!(go_interval("0").unwrap(), "0s");
        assert!(go_interval("soon").is_err());
        assert!(go_interval("8").is_err(), "a bare number is as ambiguous here as anywhere");
    }

    /// Bug this prevents: a password pasted into the address being stored server-side and left in
    /// the user's shell history with nothing said about it. `git@host:owner/name` must not trip
    /// the warning, because that is ordinary SSH syntax with no secret in it.
    #[test]
    fn a_password_in_the_address_is_warned_about_and_plain_ssh_is_not() {
        assert!(credentials_in_address("https://me:hunter2@github.com/me/proj.git").is_some());
        assert!(credentials_in_address("git@codeberg.org:me/proj.git").is_none());
        assert!(credentials_in_address("https://me@github.com/me/proj.git").is_none());
        assert!(credentials_in_address("https://github.com/me/proj.git").is_none());
        // A colon in the *path* is not a password.
        assert!(credentials_in_address("https://github.com/me/proj:x.git").is_none());
    }

    #[test]
    fn a_missing_remote_name_explains_where_the_name_comes_from() {
        let e = explain_named(
            Error::new(ErrorKind::ResourceNotFound {
                kind: "push mirror",
                id: "origin".to_owned(),
                slug: None,
                // Discovered locally: there was no server reply to quote.
                server_message: None,
            }),
            "origin",
        );
        assert!(e.to_string().contains("REMOTE column"), "{e}");
    }

    /// A 404 from an instance with mirroring disabled must name the setting, not the repository.
    #[test]
    fn a_missing_route_blames_the_instance_setting() {
        let e = explain(Error::new(ErrorKind::RouteNotFound {
            method: "GET".to_owned(),
            path: "/repos/o/r/push_mirrors".to_owned(),
            instance: None,
        }));
        let msg = e.to_string();
        assert!(msg.contains("[mirror] ENABLED"), "{msg}");
        assert!(msg.contains("ALLOW_PUSH_MIRRORS"), "{msg}");
    }
}
