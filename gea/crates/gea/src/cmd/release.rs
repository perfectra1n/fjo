//! `gea release` — releases and their assets.
//!
//! # Uploads stream; they are never buffered
//!
//! `gitea_core::http::multipart` builds the body from a `Part::file`, which is read as a
//! stream, so a 2 GB asset never lands in RAM. Nothing here reads a file into a `Vec` — that is
//! the single most important property of this module, and it is why `upload` hands the client a
//! `Part` and a `Progress` rather than bytes.
//!
//! # HTTP 413 is Gitea's quota, not a size limit
//!
//! Gitea counts LFS objects, packages and release assets against one quota and answers 413
//! when it is exceeded. `ErrorKind::QuotaExceeded` already renders that with a remedy, so every
//! upload error is returned untouched: wrapping it in a local "upload failed" would throw away
//! both the reason and the remedy. See [`upload_one`].
//!
//! # Assets are downloaded from the web root, not from the API
//!
//! There is no API route that returns an asset's bytes:
//! `GET /repos/{o}/{r}/releases/{id}/assets/{attachment_id}` answers with JSON metadata.
//! Downloads therefore go to `Attachment.browser_download_url`, which is under the instance's
//! *web* root. See [`web_get`] for how that is reached, and for what should replace it.

use std::io::Write as _;
use std::path::{Path, PathBuf};

use clap::{Args as ClapArgs, Subcommand};
use futures::StreamExt;
use gitea_core::error::{Error, ErrorKind, Result};
use gitea_core::http::{Accept, Part, Progress, Request};
use gitea_core::types::ids::ReleaseId;
use gitea_model::{Attachment, Release};

use crate::cmd::issue::shared::{self, BodyFlags, Cx, Out, ReleasePatch};
use crate::cmd::support;
use crate::global::GlobalOpts;
use crate::output::{self, Dest, Table, Term};
use crate::runtime::Runtime;

#[derive(Debug, ClapArgs)]
#[command(long_about = LONG_ABOUT)]
pub struct Args {
    #[command(subcommand)]
    pub cmd: Cmd,
}

const LONG_ABOUT: &str = "\
Manage releases and their assets.

Pass asset paths to `create` or `upload`; shell wildcards are supported.
Uploads are streamed. Downloads use the server's web URLs.

  gea release create v1.0.0 ./dist/* --generate-notes
  gea release upload v1.0.0 ./dist/extra.tar.gz
  gea release download v1.0.0 -p '*.tar.gz' -D ./dl
  gea release download v1.0.0 -A tar.gz -O - | tar tzf -";

#[derive(Debug, Subcommand)]
pub enum Cmd {
    /// List releases
    List(ListArgs),
    /// Create a release, optionally with assets
    Create(CreateArgs),
    /// Show one release
    View(ViewArgs),
    /// Change a release's title, notes or flags
    Edit(EditArgs),
    /// Delete a release
    Delete(DeleteArgs),
    /// Upload assets to an existing release
    Upload(UploadArgs),
    /// Download a release's assets, or its source archive
    Download(DownloadArgs),
    /// Delete one asset from a release
    DeleteAsset(DeleteAssetArgs),
}

/// A release, by tag.
#[derive(Debug, ClapArgs)]
pub struct TagArgs {
    /// Tag name, e.g. v1.0.0
    #[arg(value_name = "TAG")]
    pub tag: String,
}

#[derive(Debug, ClapArgs)]
pub struct ListArgs {
    /// Maximum number of releases (also settable as --limit)
    #[arg(short = 'L', value_name = "N")]
    pub limit: Option<usize>,

    /// Only drafts, or only published releases
    #[arg(long, value_name = "BOOL")]
    pub draft: Option<bool>,

    /// Only prereleases, or only stable releases
    #[arg(long, value_name = "BOOL")]
    pub prerelease: Option<bool>,
}

#[derive(Debug, ClapArgs)]
pub struct CreateArgs {
    /// Tag to release. Created from --target if it does not exist
    #[arg(value_name = "TAG")]
    pub tag: String,

    /// Files to attach. Shell globs work: ./dist/*
    #[arg(value_name = "FILE")]
    pub assets: Vec<PathBuf>,

    /// Release title. Defaults to the tag
    #[arg(long, value_name = "TITLE")]
    pub title: Option<String>,

    /// Release notes
    #[arg(short = 'n', long, value_name = "TEXT")]
    pub notes: Option<String>,

    /// Read the notes from a file; '-' reads stdin
    #[arg(short = 'F', long = "notes-file", value_name = "FILE")]
    pub notes_file: Option<String>,

    /// Write the notes from the commits since the previous release
    #[arg(long)]
    pub generate_notes: bool,

    /// Save as a draft rather than publishing
    #[arg(short = 'd', long)]
    pub draft: bool,

    /// Mark as a prerelease
    #[arg(short = 'p', long)]
    pub prerelease: bool,

    /// Branch or commit the tag should point at, when the tag does not exist yet
    #[arg(long, value_name = "REF")]
    pub target: Option<String>,

    /// Refuse to create the release unless the tag already exists
    #[arg(long)]
    pub verify_tag: bool,
}

#[derive(Debug, ClapArgs)]
pub struct ViewArgs {
    #[command(flatten)]
    pub target: TagArgs,

    /// Open the release in a browser
    #[arg(short = 'w', long)]
    pub web: bool,
}

#[derive(Debug, ClapArgs)]
pub struct EditArgs {
    #[command(flatten)]
    pub target: TagArgs,

    /// New title
    #[arg(long, value_name = "TITLE")]
    pub title: Option<String>,

    /// New notes
    #[arg(short = 'n', long, value_name = "TEXT")]
    pub notes: Option<String>,

    /// Read the new notes from a file; '-' reads stdin
    #[arg(short = 'F', long = "notes-file", value_name = "FILE")]
    pub notes_file: Option<String>,

    /// Make it a draft, or publish it
    #[arg(long, value_name = "BOOL")]
    pub draft: Option<bool>,

    /// Mark or unmark as a prerelease
    #[arg(long, value_name = "BOOL")]
    pub prerelease: Option<bool>,

    /// Rename the tag the release points at
    ///
    /// The field is `new_tag` so that its clap id does not collide with the positional `tag`
    /// flattened in above; clap answers two arguments with one id with a panic.
    #[arg(long = "tag", value_name = "TAG")]
    pub new_tag: Option<String>,
}

#[derive(Debug, ClapArgs)]
pub struct DeleteArgs {
    #[command(flatten)]
    pub target: TagArgs,

    /// Also delete the git tag
    #[arg(long)]
    pub cleanup_tag: bool,

    /// Skip the confirmation
    #[arg(long)]
    pub yes: bool,
}

#[derive(Debug, ClapArgs)]
pub struct UploadArgs {
    #[command(flatten)]
    pub target: TagArgs,

    /// Files to attach
    #[arg(value_name = "FILE", required = true)]
    pub assets: Vec<PathBuf>,

    /// Replace an asset of the same name instead of failing
    #[arg(long)]
    pub clobber: bool,

    /// Name the asset something other than the file's basename. Only with one file
    #[arg(long, value_name = "NAME")]
    pub name: Option<String>,
}

#[derive(Debug, ClapArgs)]
pub struct DownloadArgs {
    #[command(flatten)]
    pub target: TagArgs,

    /// Only assets whose name matches this glob. Repeatable
    #[arg(short = 'p', long, value_name = "GLOB")]
    pub pattern: Vec<String>,

    /// Download the source archive instead of the assets
    #[arg(short = 'A', long, value_name = "FORMAT", value_enum)]
    pub archive: Option<Archive>,

    /// Directory to write into. Created if missing
    #[arg(short = 'D', long, value_name = "DIR")]
    pub dir: Option<PathBuf>,

    /// Write to this file; '-' is stdout. Only with a single asset
    #[arg(short = 'O', value_name = "FILE")]
    pub output: Option<String>,

    /// Overwrite files that already exist
    #[arg(long, conflicts_with = "skip_existing")]
    pub clobber: bool,

    /// Leave files that already exist alone
    #[arg(long)]
    pub skip_existing: bool,
}

/// The two archive formats Gitea serves for a tag.
#[derive(Debug, Clone, Copy, PartialEq, Eq, clap::ValueEnum)]
pub enum Archive {
    Zip,
    #[value(name = "tar.gz")]
    TarGz,
}

impl Archive {
    /// The suffix `GET /archive/{archive}` wants: the route is `{tag}.zip`, not `{tag}?fmt=zip`.
    fn suffix(self) -> &'static str {
        match self {
            Self::Zip => "zip",
            Self::TarGz => "tar.gz",
        }
    }
}

#[derive(Debug, ClapArgs)]
pub struct DeleteAssetArgs {
    #[command(flatten)]
    pub target: TagArgs,

    /// The asset's name, as `gea release view` prints it
    #[arg(value_name = "NAME")]
    pub name: String,

    /// Skip the confirmation
    #[arg(long)]
    pub yes: bool,
}

pub fn run(globals: &GlobalOpts, args: &Args) -> Result<()> {
    let table = match &args.cmd {
        Cmd::Upload(_) | Cmd::DeleteAsset(_) => gitea_client::fields::FIELDS_ATTACHMENT,
        _ => gitea_client::fields::FIELDS_RELEASE,
    };
    let Some(out) = Out::prepare(globals, table)? else { return Ok(()) };

    crate::runtime::block_on(async move {
        let rt = Runtime::new(globals)?;
        let cx = Cx::in_repo(&rt, globals, out)?;
        match &args.cmd {
            Cmd::List(a) => list(&cx, globals, a).await,
            Cmd::Create(a) => create(&cx, a).await,
            Cmd::View(a) => view(&cx, a).await,
            Cmd::Edit(a) => edit(&cx, a).await,
            Cmd::Delete(a) => delete(&cx, a).await,
            Cmd::Upload(a) => upload(&cx, a).await,
            Cmd::Download(a) => download(&cx, globals, a).await,
            Cmd::DeleteAsset(a) => delete_asset(&cx, a).await,
        }
    })
}

// ---------------------------------------------------------------------------------- list

async fn list(cx: &Cx, globals: &GlobalOpts, a: &ListArgs) -> Result<()> {
    let mut query = gitea_client::query::RepoListReleasesQuery::default();
    if let Some(v) = a.draft {
        query = query.with_draft(v);
    }
    if let Some(v) = a.prerelease {
        query = query.with_pre_release(v);
    }
    let cap = support::limit(a.limit, globals);
    let mut stream = cx.api.repo().list_releases(cx.owner()?, cx.name()?, &query).take(cap);

    let mut releases: Vec<Release> = Vec::new();
    while let Some(item) = stream.next().await {
        releases.push(item?);
    }

    if cx.out.is_machine() {
        return cx.out.machine(support::to_value(&releases)?, &cx.term);
    }
    if releases.is_empty() {
        support::note(&cx.term, &format!("no releases in {}", cx.repo()?));
        return cx.out.table(&Table::new(&cx.term));
    }

    // What the capped stream could not say — see `support::total_if_truncated`.
    let total = {
        let (owner, name) = (cx.owner()?, cx.name()?);
        let ops = cx.api.repo();
        support::total_if_truncated(releases.len(), cap, |p| {
            ops.list_releases_page(owner, name, &query, p)
        })
        .await
    };

    let mut table = Table::new(&cx.term);
    table.headers(["TAG", "TITLE", "KIND", "ASSETS", "PUBLISHED"]);
    for r in &releases {
        table.row([
            r.tag_name.clone(),
            r.name.clone(),
            kind(r).to_owned(),
            r.assets.len().to_string(),
            shared::timeago(r.published_at),
        ]);
    }
    let n = total.unwrap_or(releases.len() as u64);
    table.banner(support::banner(
        releases.len(),
        total,
        &format!("release{} in {}", support::plural_s(n), cx.repo()?),
    ));
    cx.out.table(&table)
}

/// The one word that matters most about a release, and the one the table has room for.
fn kind(r: &Release) -> &'static str {
    match (r.draft, r.prerelease) {
        (true, _) => "draft",
        (false, true) => "prerelease",
        (false, false) => "release",
    }
}

// -------------------------------------------------------------------------------- create

async fn create(cx: &Cx, a: &CreateArgs) -> Result<()> {
    // Every asset is checked before the release exists. Creating a release and *then* failing on
    // a mistyped path leaves a published, empty release behind, which somebody has to clean up
    // by hand — and which a CI job has already announced.
    for path in &a.assets {
        check_readable(path)?;
    }

    if a.verify_tag {
        // gh's `--verify-tag`: refuse rather than create the tag. Gitea creates a tag from
        // `target_commitish` silently, which in a release pipeline is how you get `v1.0.0`
        // pointing at whatever the default branch happened to be.
        cx.api.repo().get_tag(cx.owner()?, cx.name()?, &a.tag).await.map_err(|e| {
            match &*e.kind {
                ErrorKind::ResourceNotFound { .. } | ErrorKind::RouteNotFound { .. } => {
                    support::usage(format!(
                        "--verify-tag: {} has no tag {:?}; push the tag first, or drop \
                         --verify-tag to have Gitea create it from --target",
                        cx.repo().map(|s| s.to_string()).unwrap_or_default(),
                        a.tag
                    ))
                }
                _ => e,
            }
        })?;
    }

    let mut notes =
        BodyFlags { body: a.notes.as_deref(), body_file: a.notes_file.as_deref(), editor: false }
            .read(&mut std::io::stdin())?
            .unwrap_or_default();

    if a.generate_notes {
        let generated = generate_notes(cx, &a.tag).await?;
        // Appended rather than replacing: `-n 'headline' --generate-notes` is how a changelog
        // usually wants to read, and silently dropping the text the user typed would be worse.
        if notes.trim().is_empty() {
            notes = generated;
        } else {
            notes = format!("{}\n\n{generated}", notes.trim_end());
        }
    }

    let body = gitea_model::CreateReleaseOption {
        tag_name: a.tag.clone(),
        target_commitish: a.target.clone(),
        name: Some(a.title.clone().unwrap_or_else(|| a.tag.clone())),
        body: Some(notes),
        draft: Some(a.draft),
        prerelease: Some(a.prerelease),
        ..gitea_model::CreateReleaseOption::default()
    };
    let release = cx.api.repo().create_release(cx.owner()?, cx.name()?, &body).await?;

    let mut uploaded = Vec::new();
    for path in &a.assets {
        uploaded.push(upload_one(cx, release.id, path, None, false, &[]).await?);
    }

    if cx.out.is_machine() {
        // The release as it is *after* the uploads, so `--json assets` is not empty.
        let full = cx.api.repo().get_release(cx.owner()?, cx.name()?, release.id.get()).await?;
        return cx.out.machine(support::to_value(&full)?, &cx.term);
    }
    let mut text =
        format!("Created {} {}\n{}\n", kind(&release), release.tag_name, release.html_url);
    if !uploaded.is_empty() {
        text.push_str(&format!(
            "Uploaded {}\n",
            uploaded.iter().map(|a| a.name.clone()).collect::<Vec<_>>().join(", ")
        ));
    }
    cx.out.text(&text)
}

/// Notes from the commits between the previous release's tag and this one.
///
/// Gitea has no "generate release notes" endpoint — this is `GET /compare/{prev}...{tag}` plus
/// formatting, which is exactly the kind of multi-call orchestration a porcelain command is for.
/// With no previous release there is nothing to compare against, so the notes say so rather than
/// comparing against the root commit and printing the whole history.
async fn generate_notes(cx: &Cx, tag: &str) -> Result<String> {
    let query = gitea_client::query::RepoListReleasesQuery::default();
    let mut stream = cx.api.repo().list_releases(cx.owner()?, cx.name()?, &query).take(10);
    let mut previous: Option<String> = None;
    while let Some(item) = stream.next().await {
        let r = item?;
        if r.tag_name != tag && !r.draft {
            previous = Some(r.tag_name);
            break;
        }
    }

    let Some(prev) = previous else {
        cx.trace("--generate-notes: no previous release to compare against");
        return Ok(format!("First release, {tag}.\n"));
    };

    let compare = cx
        .api
        .repo()
        .compare_diff(cx.owner()?, cx.name()?, &format!("{prev}...{tag}"), &Default::default())
        .await?;
    let mut notes = format!("## Changes since {prev}\n\n");
    for commit in &compare.commits {
        let subject = commit
            .commit
            .as_ref()
            .map(|c| c.message.lines().next().unwrap_or_default().to_owned())
            .unwrap_or_default();
        let short: String = commit.sha.chars().take(7).collect();
        notes.push_str(&format!("- {subject} ({short})\n"));
    }
    if compare.commits.is_empty() {
        notes.push_str(&format!("- no commits between {prev} and {tag}\n"));
    }
    Ok(notes)
}

// ---------------------------------------------------------------------------------- view

async fn view(cx: &Cx, a: &ViewArgs) -> Result<()> {
    let release = shared::release_by_tag(cx, &a.target.tag).await?;
    if a.web {
        return cx.browse(&release.html_url);
    }
    if cx.out.is_machine() {
        return cx.out.machine(support::to_value(&release)?, &cx.term);
    }

    let mut text = String::new();
    text.push_str(&format!("{} — {}\n", release.tag_name, release.name));
    let mut facts = Vec::from([kind(&release).to_owned()]);
    if let Some(author) = &release.author {
        facts.push(format!("published by {}", author.login));
    }
    if release.published_at.is_some_and(|t| !t.is_unset()) {
        facts.push(shared::timeago(release.published_at));
    }
    text.push_str(&format!("{}\n", facts.join(" • ")));
    text.push('\n');
    if release.body.trim().is_empty() {
        text.push_str("No release notes.\n");
    } else {
        text.push_str(&shared::markdown(&cx.term, &release.body));
    }

    if !release.assets.is_empty() {
        let mut table = Table::new(&cx.term);
        table.headers(["ASSET", "SIZE", "DOWNLOADS"]);
        for asset in &release.assets {
            table.row([
                asset.name.clone(),
                human_size(asset.size),
                asset.download_count.to_string(),
            ]);
        }
        text.push('\n');
        text.push_str(&table.render_to_string());
    }
    text.push_str(&format!("\n{}\n", release.html_url));
    cx.out.text(&text)
}

// ---------------------------------------------------------------------------------- edit

async fn edit(cx: &Cx, a: &EditArgs) -> Result<()> {
    let notes =
        BodyFlags { body: a.notes.as_deref(), body_file: a.notes_file.as_deref(), editor: false }
            .read(&mut std::io::stdin())?;

    let patch = ReleasePatch {
        tag_name: a.new_tag.clone(),
        name: a.title.clone(),
        body: notes,
        draft: a.draft,
        prerelease: a.prerelease,
        ..ReleasePatch::default()
    };
    if patch.is_empty() {
        return Err(support::usage(
            "nothing to change; pass --title, -n/--notes, -F/--notes-file, --draft, \
             --prerelease or --tag",
        ));
    }

    let existing = shared::release_by_tag(cx, &a.target.tag).await?;
    // A sparse patch matters most here: `EditReleaseOption` sends `draft: false` whether or not
    // the user asked, so a `-n notes` edit of a draft release would publish it.
    let release = shared::patch_release(cx, existing.id, &patch).await?;
    if cx.out.is_machine() {
        return cx.out.machine(support::to_value(&release)?, &cx.term);
    }
    cx.out.text(&format!("Updated {} {}\n{}\n", kind(&release), release.tag_name, release.html_url))
}

// -------------------------------------------------------------------------------- delete

async fn delete(cx: &Cx, a: &DeleteArgs) -> Result<()> {
    let release = shared::release_by_tag(cx, &a.target.tag).await?;
    cx.confirm(
        &format!(
            "Delete {} {} and its {} asset(s) from {}",
            kind(&release),
            release.tag_name,
            release.assets.len(),
            cx.repo()?
        ),
        a.yes,
    )?;
    cx.api.repo().delete_release(cx.owner()?, cx.name()?, release.id.get()).await?;
    if a.cleanup_tag {
        cx.api.repo().delete_tag(cx.owner()?, cx.name()?, &release.tag_name).await?;
    }
    if cx.out.is_machine() {
        return cx.out.machine(support::to_value(&release)?, &cx.term);
    }
    let tag = if a.cleanup_tag { " and its git tag" } else { "" };
    cx.out.text(&format!("Deleted release {}{tag}\n", release.tag_name))
}

async fn delete_asset(cx: &Cx, a: &DeleteAssetArgs) -> Result<()> {
    let release = shared::release_by_tag(cx, &a.target.tag).await?;
    let asset = shared::asset_by_name(&release, &a.name)
        .ok_or_else(|| unknown_asset(&release, &a.name))?
        .clone();
    cx.confirm(&format!("Delete asset {:?} from {}", asset.name, release.tag_name), a.yes)?;
    cx.api
        .repo()
        .delete_release_attachment(cx.owner()?, cx.name()?, release.id.get(), asset.id.get())
        .await?;
    if cx.out.is_machine() {
        return cx.out.machine(support::to_value(&asset)?, &cx.term);
    }
    cx.out.text(&format!("Deleted asset {:?} from {}\n", asset.name, release.tag_name))
}

fn unknown_asset(release: &Release, name: &str) -> Error {
    let mut names: Vec<&str> = release.assets.iter().map(|a| a.name.as_str()).collect();
    names.sort_unstable();
    support::usage(format!(
        "{} has no asset called {name:?}; it has: {}",
        release.tag_name,
        if names.is_empty() { "none".to_owned() } else { names.join(", ") }
    ))
}

// -------------------------------------------------------------------------------- upload

async fn upload(cx: &Cx, a: &UploadArgs) -> Result<()> {
    if a.name.is_some() && a.assets.len() > 1 {
        return Err(support::usage(
            "--name renames one asset, so it cannot be used with several files",
        ));
    }
    for path in &a.assets {
        check_readable(path)?;
    }
    let release = shared::release_by_tag(cx, &a.target.tag).await?;

    let mut uploaded = Vec::new();
    for path in &a.assets {
        uploaded.push(
            upload_one(cx, release.id, path, a.name.as_deref(), a.clobber, &release.assets).await?,
        );
    }

    if cx.out.is_machine() {
        return cx.out.machine(support::to_value(&uploaded)?, &cx.term);
    }
    let mut text = String::new();
    for asset in &uploaded {
        text.push_str(&format!("Uploaded {} ({})\n", asset.name, human_size(asset.size)));
    }
    cx.out.text(&text)
}

/// Upload one file, streaming it, with a progress bar on a terminal.
///
/// **Errors are returned untouched.** A 413 here is `ErrorKind::QuotaExceeded`, which the
/// renderer turns into a message naming the quota and what to delete; replacing it with
/// "could not upload asset.tar.gz" would discard the only useful part.
async fn upload_one(
    cx: &Cx,
    release: ReleaseId,
    path: &Path,
    rename: Option<&str>,
    clobber: bool,
    existing: &[Attachment],
) -> Result<Attachment> {
    let name = match rename {
        Some(n) => n.to_owned(),
        None => path.file_name().map(|n| n.to_string_lossy().into_owned()).ok_or_else(|| {
            support::usage(format!("{} has no file name to use as the asset name", path.display()))
        })?,
    };

    if let Some(old) = existing.iter().find(|a| a.name == name) {
        if !clobber {
            // `Usage`, not `ErrorKind::Conflict`: nothing has been sent, so a headline saying
            // the *server* refused a conflict would point at the wrong place.
            return Err(support::usage(format!(
                "an asset called {name:?} is already attached to this release (uploading {}); \
                 pass --clobber to replace it",
                path.display()
            )));
        }
        // Deleted first: Gitea happily accepts two assets with the same name, and a release
        // with two `gea-x86_64.tar.gz` files is worse than a failed upload.
        cx.api
            .repo()
            .delete_release_attachment(cx.owner()?, cx.name()?, release.get(), old.id.get())
            .await?;
    }

    let bar =
        ProgressBar::for_upload(&cx.term, &name, std::fs::metadata(path).ok().map(|m| m.len()));
    let progress = bar.hook();
    let part = Part::file("attachment", path.to_path_buf()).with_filename(name.clone());
    let query = gitea_client::query::RepoCreateReleaseAttachmentQuery::default().with_name(&name);

    let result = cx
        .api
        .repo()
        .create_release_attachment(
            cx.owner()?,
            cx.name()?,
            release.get(),
            Some(part),
            &query,
            progress,
        )
        .await;
    bar.finish();
    result
}

/// An `indicatif` bar on a terminal, and nothing at all otherwise.
///
/// A progress bar drawn into a pipe becomes thousands of carriage-return-separated lines in a CI
/// log, so this is a no-op unless stdout is a terminal. It also draws to **stderr**, so
/// `gea release upload … --json` stays parseable while the bar is running.
struct ProgressBar(Option<indicatif::ProgressBar>);

impl ProgressBar {
    fn for_upload(term: &Term, name: &str, total: Option<u64>) -> Self {
        if !term.tty {
            return Self(None);
        }
        let bar = match total {
            Some(n) => indicatif::ProgressBar::new(n),
            // An unknown length (a stream) gets a spinner rather than a bar that never fills.
            None => indicatif::ProgressBar::new_spinner(),
        };
        bar.set_style(
            indicatif::ProgressStyle::with_template(
                "{msg} {bar:30} {bytes}/{total_bytes} {bytes_per_sec}",
            )
            .unwrap_or_else(|_| indicatif::ProgressStyle::default_bar()),
        );
        bar.set_message(name.to_owned());
        Self(Some(bar))
    }

    /// A [`Progress`] that drives the bar. Safe to hand to the client either way: with no bar
    /// the callback is absent and the client's counter is the only cost.
    fn hook(&self) -> Progress {
        match &self.0 {
            None => Progress::new(),
            Some(bar) => {
                let bar = bar.clone();
                Progress::new().with_callback(move |sent, total| {
                    if let Some(t) = total {
                        bar.set_length(t);
                    }
                    bar.set_position(sent);
                })
            }
        }
    }

    fn set(&self, sent: u64, total: Option<u64>) {
        if let Some(bar) = &self.0 {
            if let Some(t) = total {
                bar.set_length(t);
            }
            bar.set_position(sent);
        }
    }

    fn finish(&self) {
        if let Some(bar) = &self.0 {
            bar.finish_and_clear();
        }
    }
}

// ------------------------------------------------------------------------------ download

async fn download(cx: &Cx, globals: &GlobalOpts, a: &DownloadArgs) -> Result<()> {
    if let Some(format) = a.archive {
        return download_archive(cx, globals, a, format).await;
    }

    let release = shared::release_by_tag(cx, &a.target.tag).await?;
    let wanted: Vec<&Attachment> = if a.pattern.is_empty() {
        release.assets.iter().collect()
    } else {
        release
            .assets
            .iter()
            .filter(|asset| a.pattern.iter().any(|p| glob_match(p, &asset.name)))
            .collect()
    };

    if wanted.is_empty() {
        // Not an error when the release simply has no assets; a pattern that matched nothing is
        // a mistake worth reporting, because the exit code is what a script checks.
        if a.pattern.is_empty() {
            support::note(&cx.term, &format!("{} has no assets", release.tag_name));
            return Ok(());
        }
        let mut names: Vec<&str> = release.assets.iter().map(|x| x.name.as_str()).collect();
        names.sort_unstable();
        return Err(support::usage(format!(
            "no asset of {} matches {}; it has: {}",
            release.tag_name,
            a.pattern.join(", "),
            if names.is_empty() { "none".to_owned() } else { names.join(", ") }
        )));
    }
    if a.output.is_some() && wanted.len() > 1 {
        return Err(support::usage(format!(
            "-O writes one file, and {} assets matched; use -D/--dir, or narrow -p/--pattern",
            wanted.len()
        )));
    }

    for asset in wanted {
        let dest = destination(a, &asset.name)?;
        if let Some(path) = existing_path(&dest) {
            if a.skip_existing {
                support::note(&cx.term, &format!("{} exists; skipping", path.display()));
                continue;
            }
            if !a.clobber {
                return Err(support::usage(format!(
                    "{} already exists; pass --clobber to overwrite it or --skip-existing to \
                     leave it alone",
                    path.display()
                )));
            }
        }
        stream_to(
            cx,
            &asset.browser_download_url,
            &dest,
            &asset.name,
            Some(asset.size as u64),
            globals,
        )
        .await?;
    }
    Ok(())
}

async fn download_archive(
    cx: &Cx,
    globals: &GlobalOpts,
    a: &DownloadArgs,
    format: Archive,
) -> Result<()> {
    // `{tag}.zip` / `{tag}.tar.gz` — the format is part of the path, and this is one of the few
    // API routes that answers with bytes rather than JSON.
    let name = format!("{}.{}", a.target.tag, format.suffix());
    let dest = destination(a, &name)?;
    if let Some(path) = existing_path(&dest) {
        if a.skip_existing {
            support::note(&cx.term, &format!("{} exists; skipping", path.display()));
            return Ok(());
        }
        if !a.clobber {
            return Err(support::usage(format!(
                "{} already exists; pass --clobber to overwrite it",
                path.display()
            )));
        }
    }

    let (mime, mut body) =
        cx.api.repo().get_archive(cx.owner()?, cx.name()?, &name, &Default::default()).await?;
    let bar = ProgressBar::for_upload(&cx.term, &name, None);
    let written = write_stream(mime.as_str(), &mut body, &dest, cx, globals, &bar).await?;
    bar.finish();
    report_written(cx, &dest, &name, written)
}

/// Where one asset goes: `-O` wins, then `-D`, then the working directory.
fn destination(a: &DownloadArgs, name: &str) -> Result<Dest> {
    if let Some(out) = &a.output {
        return Ok(if out == "-" { Dest::Stdout } else { Dest::File(PathBuf::from(out)) });
    }
    let dir = a.dir.clone().unwrap_or_else(|| PathBuf::from("."));
    // Created up front so that a whole multi-asset download does not fail on the second file.
    std::fs::create_dir_all(&dir)
        .map_err(|e| support::usage(format!("-D/--dir {}: {e}", dir.display())))?;
    // The server chose the name, so it must not be able to choose the *directory*: an asset
    // called `../../.ssh/authorized_keys` would otherwise escape `-D`.
    let base = Path::new(name).file_name().ok_or_else(|| {
        support::usage(format!("the asset name {name:?} is not a usable file name"))
    })?;
    Ok(Dest::File(dir.join(base)))
}

fn existing_path(dest: &Dest) -> Option<PathBuf> {
    match dest {
        Dest::File(p) if p.exists() => Some(p.clone()),
        _ => None,
    }
}

async fn stream_to(
    cx: &Cx,
    url: &str,
    dest: &Dest,
    name: &str,
    size: Option<u64>,
    globals: &GlobalOpts,
) -> Result<()> {
    let (mime, mut body) = web_get(cx, url).await?;
    let bar = ProgressBar::for_upload(&cx.term, name, size);
    let written = write_stream(&mime, &mut body, dest, cx, globals, &bar).await?;
    bar.finish();
    report_written(cx, dest, name, written)
}

/// Copy a byte stream to its destination, refusing to put binary on a terminal.
///
/// The guard runs **before the first byte is written**: a few kilobytes of a tarball interpreted
/// as terminal input can leave a session needing `reset`. `--force` overrides it, and a pipe or a
/// file never triggers it.
async fn write_stream(
    mime: &str,
    body: &mut gitea_core::http::ByteStream,
    dest: &Dest,
    cx: &Cx,
    globals: &GlobalOpts,
    bar: &ProgressBar,
) -> Result<u64> {
    output::guard_binary(mime, &cx.term, dest, globals.force)?;
    let mut sink = output::open_dest(dest)?;
    let mut total = 0u64;
    while let Some(chunk) = body.next().await {
        let chunk = chunk?;
        total += chunk.len() as u64;
        sink.write_all(&chunk)?;
        bar.set(total, None);
    }
    sink.flush()?;
    Ok(total)
}

fn report_written(cx: &Cx, dest: &Dest, name: &str, bytes: u64) -> Result<()> {
    match dest {
        // Nothing on stdout: the bytes *are* the output, and a "wrote N bytes" line would be
        // appended to the file the user is piping into.
        Dest::Stdout => {
            support::note(
                &cx.term,
                &format!("wrote {} of {name} to stdout", human_size(bytes as i64)),
            );
            Ok(())
        }
        Dest::File(path) => {
            if cx.out.is_machine() {
                return Ok(());
            }
            cx.out.text(&format!("Downloaded {} ({})\n", path.display(), human_size(bytes as i64)))
        }
    }
}

/// GET an absolute URL under the instance's **web** root rather than its API root.
///
/// Release assets have no API route that returns bytes — `GET /releases/{id}/assets/{id}` answers
/// with JSON — so the only way to fetch one is `Attachment.browser_download_url`. `Client` builds
/// every URL as `api_base + path`, and `api_base` is exactly `web_base + "/api/v1"`, so the path
/// is prefixed with `/../..` to climb back out: URL parsing removes dot segments (RFC 3986
/// §5.2.4), and `https://host/api/v1/../../owner/repo/releases/download/v1/x.tgz` is sent as
/// `https://host/owner/repo/releases/download/v1/x.tgz` — with the same credentials, retry policy
/// and error classification as every other request, which is the reason not to reach for a bare
/// HTTP client here.
///
/// **This is a workaround.** `gitea-core` should grow a `Client::web_bytes(&str)`; it is
/// reported alongside this change.
///
/// A URL that is not under the web root is refused rather than followed. Gitea supports
/// *external* attachments whose URL points anywhere, and following one would make the server able
/// to aim an authenticated client at a host of its choosing.
async fn web_get(cx: &Cx, url: &str) -> Result<(String, gitea_core::http::ByteStream)> {
    let base = cx.api.client().web_base();
    let rest = url.strip_prefix(base).ok_or_else(|| {
        support::usage(format!(
            "{url} is outside {base}. Download this external attachment separately; gea will not send your token to another host."
        ))
    })?;
    let rest = if rest.starts_with('/') { rest.to_owned() } else { format!("/{rest}") };
    let req = Request::get(format!("/../..{rest}")).accept(Accept::Any);
    cx.trace(&format!("GET {base}{rest}"));
    let (mime, body) = cx.api.client().bytes(req).await?;
    Ok((mime.as_str().to_owned(), body))
}

// ------------------------------------------------------------------------------- helpers

fn check_readable(path: &Path) -> Result<()> {
    let meta = std::fs::metadata(path)
        .map_err(|e| support::usage(format!("cannot attach {}: {e}", path.display())))?;
    if meta.is_dir() {
        return Err(support::usage(format!(
            "{} is a directory; name the files instead (a shell glob such as ./dist/* expands to \
             them)",
            path.display()
        )));
    }
    Ok(())
}

/// `*` and `?` globbing on a single asset name.
///
/// Hand-written because `-p '*.tar.gz'` is the whole use case and a glob crate would be a new
/// dependency for eleven lines. There is no `/` to be special about: an asset name is one
/// segment, so `*` may match anything including dots.
fn glob_match(pattern: &str, name: &str) -> bool {
    let (p, n): (Vec<char>, Vec<char>) = (pattern.chars().collect(), name.chars().collect());
    // Classic two-pointer wildcard match: `star`/`mark` remember where to resume after a `*`,
    // which keeps it linear instead of exponential on patterns like `*a*a*a*`.
    let (mut pi, mut ni) = (0usize, 0usize);
    let (mut star, mut mark) = (None, 0usize);
    while ni < n.len() {
        if pi < p.len() && (p[pi] == '?' || p[pi] == n[ni]) {
            pi += 1;
            ni += 1;
        } else if pi < p.len() && p[pi] == '*' {
            star = Some(pi);
            mark = ni;
            pi += 1;
        } else if let Some(s) = star {
            pi = s + 1;
            mark += 1;
            ni = mark;
        } else {
            return false;
        }
    }
    p[pi..].iter().all(|c| *c == '*')
}

/// `1.4 MiB`. Binary units, because that is what every other tool prints for a file size.
fn human_size(bytes: i64) -> String {
    const UNITS: [&str; 5] = ["B", "KiB", "MiB", "GiB", "TiB"];
    let mut value = bytes.max(0) as f64;
    let mut unit = 0;
    while value >= 1024.0 && unit + 1 < UNITS.len() {
        value /= 1024.0;
        unit += 1;
    }
    if unit == 0 { format!("{} B", bytes.max(0)) } else { format!("{value:.1} {}", UNITS[unit]) }
}

#[cfg(test)]
mod tests {
    use super::*;
    use gitea_core::http::transport::Canned;
    use gitea_core::http::{Auth, Client, FakeTransport, RetryPolicy};
    use gitea_core::types::RepoSlug;
    use std::sync::{Arc, Mutex};

    macro_rules! method {
        ($name:literal) => {
            $name.parse().expect("a valid HTTP method")
        };
    }

    fn cx(
        fake: Arc<FakeTransport>,
        buf: &Arc<Mutex<Vec<u8>>>,
        globals: &GlobalOpts,
        term: Term,
    ) -> Cx {
        let client = Client::builder("https://git.example.org", Auth::token("t"))
            .transport(fake)
            .retry(RetryPolicy { max: 1, ..RetryPolicy::default() })
            .build()
            .expect("a well-formed base URL");
        let out = Out::to_buffer(globals, gitea_client::fields::FIELDS_RELEASE, buf);
        Cx::for_test(
            gitea_client::Api::new(client),
            Some(RepoSlug::new("perf3ct", "gea")),
            term,
            out,
        )
    }

    fn text(buf: &Arc<Mutex<Vec<u8>>>) -> String {
        String::from_utf8(buf.lock().unwrap().clone()).expect("utf-8 output")
    }

    const RELEASE: &str = r#"{
        "id": 3, "tag_name": "v1.0.0", "name": "First", "draft": false, "prerelease": false,
        "body": "notes here", "html_url": "https://git.example.org/perf3ct/gea/releases/tag/v1.0.0",
        "author": {"login": "alice"},
        "assets": [
            {"id": 11, "name": "gea-x86_64.tar.gz", "size": 2048, "download_count": 7,
             "browser_download_url": "https://git.example.org/perf3ct/gea/releases/download/v1.0.0/gea-x86_64.tar.gz"},
            {"id": 12, "name": "checksums.txt", "size": 64, "download_count": 1,
             "browser_download_url": "https://git.example.org/perf3ct/gea/releases/download/v1.0.0/checksums.txt"}
        ]
    }"#;

    fn by_tag() -> FakeTransport {
        FakeTransport::new().on(
            method!("GET"),
            "/api/v1/repos/perf3ct/gea/releases/tags/v1.0.0",
            Canned::json(200, RELEASE),
        )
    }

    #[tokio::test]
    async fn view_renders_notes_and_an_asset_table() {
        let fake = Arc::new(by_tag());
        let buf = Arc::new(Mutex::new(Vec::new()));
        let globals = GlobalOpts::default();
        let cx = cx(fake, &buf, &globals, Term::tty(80));
        view(&cx, &ViewArgs { target: TagArgs { tag: "v1.0.0".to_owned() }, web: false })
            .await
            .unwrap();
        insta::assert_snapshot!("view_human", text(&buf));
    }

    #[tokio::test]
    async fn view_json_projects_fields() {
        let fake = Arc::new(by_tag());
        let buf = Arc::new(Mutex::new(Vec::new()));
        let globals = GlobalOpts {
            json: Some("tag_name,draft,prerelease".to_owned()),
            ..GlobalOpts::default()
        };
        let cx = cx(fake, &buf, &globals, Term::piped());
        view(&cx, &ViewArgs { target: TagArgs { tag: "v1.0.0".to_owned() }, web: false })
            .await
            .unwrap();
        insta::assert_snapshot!("view_json", text(&buf));
    }

    /// Bug this prevents: `release edit -n notes` on a **draft** release publishing it, which is
    /// what `EditReleaseOption` does — it serialises `draft: false` whether or not the user said
    /// anything about drafts.
    #[tokio::test]
    async fn edit_sends_only_what_was_asked_for() {
        let fake = Arc::new(by_tag().on(
            method!("PATCH"),
            "/api/v1/repos/perf3ct/gea/releases/3",
            Canned::json(200, RELEASE),
        ));
        let buf = Arc::new(Mutex::new(Vec::new()));
        let globals = GlobalOpts::default();
        let cx = cx(fake.clone(), &buf, &globals, Term::piped());
        let args = EditArgs {
            target: TagArgs { tag: "v1.0.0".to_owned() },
            title: None,
            notes: Some("new notes".to_owned()),
            notes_file: None,
            draft: None,
            prerelease: None,
            new_tag: None,
        };
        edit(&cx, &args).await.unwrap();
        let patch = fake.calls_to(&method!("PATCH"), "/api/v1/repos/perf3ct/gea/releases/3");
        assert_eq!(patch[0].body_str(), r#"{"body":"new notes"}"#);
    }

    /// Bug this prevents: uploading a second asset with a name that is already taken, leaving a
    /// release with two files called the same thing and a download that picks one at random.
    #[tokio::test]
    async fn uploading_over_an_existing_asset_needs_clobber() {
        let fake = Arc::new(by_tag());
        let buf = Arc::new(Mutex::new(Vec::new()));
        let globals = GlobalOpts::default();
        let cx = cx(fake.clone(), &buf, &globals, Term::piped());
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("checksums.txt");
        std::fs::write(&path, b"deadbeef").unwrap();

        let args = UploadArgs {
            target: TagArgs { tag: "v1.0.0".to_owned() },
            assets: Vec::from([path.clone()]),
            clobber: false,
            name: None,
        };
        let e = upload(&cx, &args).await.unwrap_err();
        assert_eq!(e.exit_code(), 2);
        assert!(e.to_string().contains("--clobber"), "{e}");
        assert!(e.to_string().contains("checksums.txt"), "{e}");
        assert!(
            fake.calls().iter().all(|c| c.method.as_str() != "POST"),
            "nothing is uploaded when the name is taken"
        );
    }

    /// The asset download goes to the **web** root, because no API route serves the bytes.
    #[tokio::test]
    async fn an_asset_is_fetched_from_the_web_root_not_the_api() {
        // Registered at the *unresolved* path, because `FakeTransport` matches the URL string
        // it is handed and does no URL parsing. A real transport parses first, and dot-segment
        // removal turns this into `/perf3ct/gea/releases/download/v1.0.0/checksums.txt` — which
        // the itest against a real Gitea is what actually proves. See `web_get`.
        let fake = Arc::new(by_tag().on_fn(
            method!("GET"),
            "/api/v1/../../perf3ct/gea/releases/download/v1.0.0/checksums.txt",
            |_| Canned::new(200).with_header("content-type", "text/plain").with_body("deadbeef"),
        ));
        let buf = Arc::new(Mutex::new(Vec::new()));
        let globals = GlobalOpts::default();
        let cx = cx(fake.clone(), &buf, &globals, Term::piped());
        let dir = tempfile::tempdir().unwrap();
        let args = DownloadArgs {
            target: TagArgs { tag: "v1.0.0".to_owned() },
            pattern: Vec::from(["checksums*".to_owned()]),
            archive: None,
            dir: Some(dir.path().to_path_buf()),
            output: None,
            clobber: false,
            skip_existing: false,
        };
        download(&cx, &globals, &args).await.unwrap();

        let get = fake
            .calls()
            .into_iter()
            .find(|c| c.path.ends_with("checksums.txt"))
            .expect("the asset request");
        // It climbs out of `/api/v1` rather than asking the API for bytes it does not serve.
        assert_eq!(
            get.path, "/api/v1/../../perf3ct/gea/releases/download/v1.0.0/checksums.txt",
            "{}",
            get.url
        );
        assert_eq!(std::fs::read(dir.path().join("checksums.txt")).unwrap(), b"deadbeef");
    }

    /// Bug this prevents: following an *external* attachment URL with the user's token attached.
    /// Gitea lets a release point an asset at any host, so the URL is server-controlled data.
    #[tokio::test]
    async fn an_external_attachment_url_is_refused() {
        let fake = Arc::new(FakeTransport::new());
        let buf = Arc::new(Mutex::new(Vec::new()));
        let globals = GlobalOpts::default();
        let cx = cx(fake.clone(), &buf, &globals, Term::piped());
        // `unwrap_err` would need `Debug` on a `ByteStream`, which is a trait object.
        let e = match web_get(&cx, "https://evil.example.net/x.tar.gz").await {
            Err(e) => e,
            Ok(_) => panic!("an off-instance URL must be refused"),
        };
        assert_eq!(e.exit_code(), 2);
        assert!(e.to_string().contains("external attachment"), "{e}");
        assert_eq!(fake.call_count(), 0, "not one byte is sent anywhere");
    }

    /// Bug this prevents: wrapping an upload failure in a local message. Gitea answers 413
    /// when a repository's quota is exhausted — counting LFS, packages **and** release assets
    /// together — and `ErrorKind::QuotaExceeded` renders that with the remedy. A local
    /// "could not upload x.tar.gz" would throw away both the reason and the remedy, and would
    /// also lose the 413's exit code.
    #[tokio::test]
    async fn a_413_reaches_the_user_as_quota_exceeded() {
        let fake = Arc::new(by_tag().on(
            method!("POST"),
            "/api/v1/repos/perf3ct/gea/releases/3/assets",
            Canned::json(413, r#"{"message":"quota exceeded"}"#),
        ));
        let buf = Arc::new(Mutex::new(Vec::new()));
        let globals = GlobalOpts::default();
        let cx = cx(fake, &buf, &globals, Term::piped());
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("new.tar.gz");
        std::fs::write(&path, b"payload").unwrap();
        let args = UploadArgs {
            target: TagArgs { tag: "v1.0.0".to_owned() },
            assets: Vec::from([path]),
            clobber: false,
            name: None,
        };
        let e = upload(&cx, &args).await.unwrap_err();
        assert!(
            matches!(&*e.kind, ErrorKind::QuotaExceeded { .. }),
            "a 413 must stay a quota error, not become a local message: {:?}",
            e.kind
        );
    }

    #[tokio::test]
    async fn a_pattern_that_matches_nothing_is_an_error_naming_the_assets() {
        let fake = Arc::new(by_tag());
        let buf = Arc::new(Mutex::new(Vec::new()));
        let globals = GlobalOpts::default();
        let cx = cx(fake, &buf, &globals, Term::piped());
        let args = DownloadArgs {
            target: TagArgs { tag: "v1.0.0".to_owned() },
            pattern: Vec::from(["*.deb".to_owned()]),
            archive: None,
            dir: None,
            output: None,
            clobber: false,
            skip_existing: false,
        };
        let e = download(&cx, &globals, &args).await.unwrap_err();
        assert!(e.to_string().contains("checksums.txt"), "{e}");
    }

    /// Bug this prevents: a server-chosen asset name escaping `-D/--dir`. `browser_download_url`
    /// and the name beside it are both server data.
    #[test]
    fn an_asset_name_cannot_escape_the_download_directory() {
        let dir = tempfile::tempdir().unwrap();
        let args = DownloadArgs {
            target: TagArgs { tag: "v1".to_owned() },
            pattern: Vec::new(),
            archive: None,
            dir: Some(dir.path().to_path_buf()),
            output: None,
            clobber: false,
            skip_existing: false,
        };
        let dest = destination(&args, "../../etc/passwd").unwrap();
        assert_eq!(dest, Dest::File(dir.path().join("passwd")));
    }

    #[test]
    fn dash_means_stdout() {
        let args = DownloadArgs {
            target: TagArgs { tag: "v1".to_owned() },
            pattern: Vec::new(),
            archive: None,
            dir: None,
            output: Some("-".to_owned()),
            clobber: false,
            skip_existing: false,
        };
        assert_eq!(destination(&args, "x.tgz").unwrap(), Dest::Stdout);
    }

    #[test]
    fn globs_match_the_way_a_shell_would() {
        assert!(glob_match("*.tar.gz", "gea-x86_64.tar.gz"));
        assert!(glob_match("gea-*", "gea-x86_64.tar.gz"));
        assert!(glob_match("*", "anything"));
        assert!(glob_match("checksums.???", "checksums.txt"));
        assert!(!glob_match("*.deb", "gea.tar.gz"));
        // The pathological pattern that makes a naive recursive matcher hang.
        assert!(!glob_match("*a*a*a*a*a*a*b", &"a".repeat(40)));
    }

    #[test]
    fn sizes_are_binary_units() {
        assert_eq!(human_size(0), "0 B");
        assert_eq!(human_size(512), "512 B");
        assert_eq!(human_size(2048), "2.0 KiB");
        assert_eq!(human_size(5 * 1024 * 1024), "5.0 MiB");
    }

    #[tokio::test]
    async fn edit_with_no_flags_is_a_usage_error_before_any_request() {
        let fake = Arc::new(FakeTransport::new());
        let buf = Arc::new(Mutex::new(Vec::new()));
        let globals = GlobalOpts::default();
        let cx = cx(fake.clone(), &buf, &globals, Term::piped());
        let args = EditArgs {
            target: TagArgs { tag: "v1.0.0".to_owned() },
            title: None,
            notes: None,
            notes_file: None,
            draft: None,
            prerelease: None,
            new_tag: None,
        };
        assert_eq!(edit(&cx, &args).await.unwrap_err().exit_code(), 2);
        assert_eq!(fake.call_count(), 0);
    }
}
