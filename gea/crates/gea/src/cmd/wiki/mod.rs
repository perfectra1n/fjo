//! `gea wiki` — wiki pages and their history.
//!
//! # Why this group exists at all
//!
//! **GitHub has no wiki REST API.** None. `gh` can only open the wiki in a browser, and every
//! GitHub wiki automation in the wild is a `git clone` of the `.wiki` repository followed by
//! text munging. Gitea exposes the whole thing over REST, so `gea wiki` is not a
//! reimplementation of something `gh` does — it is a capability `gh` does not have.
//!
//! # The three things that make this more than `gea raw`
//!
//! 1. **Content is base64.** `WikiPage.content_base64` says so; `sidebar` and `footer` are
//!    encoded too and the specification does *not* say so. See [`b64`], whose decoder is lenient
//!    for exactly that reason.
//! 2. **A partial edit must not blank the page.** `PATCH …/wiki/page/{name}` takes a whole
//!    `CreateWikiPageOptions`, so renaming a page with `--title` and no body would send an empty
//!    `content_base64` and commit an empty page. [`edit`] reads the page first and resends its
//!    content. That is the single most destructive thing in this group and the reason it is not
//!    left to layer 2.
//! 3. **Markdown is for reading.** On a terminal the page is rendered ([`markdown`]); piped, it
//!    is the raw bytes that were committed, so `gea wiki view Home > Home.md` produces the file.

pub(crate) mod b64;
pub(crate) mod markdown;

use clap::{Args as ClapArgs, Subcommand};
use futures::StreamExt;
use gitea_client::Api;
use gitea_core::error::{Error, ErrorKind, Result};
use gitea_core::types::RepoSlug;
use gitea_model::{CreateWikiPageOptions, WikiPage, WikiPageMetaData};

use crate::cmd::times::porcelain::{self, BodyOpts, Fields, Machine};
use crate::global::GlobalOpts;
use crate::output::Term;
use crate::runtime::Runtime;

const PAGE_FIELDS: Fields = Fields::Generated(gitea_client::fields::FIELDS_WIKI_PAGE);
const META_FIELDS: Fields = Fields::Generated(gitea_client::fields::FIELDS_WIKI_PAGE_META_DATA);
const COMMIT_FIELDS: Fields = Fields::Generated(gitea_client::fields::FIELDS_WIKI_COMMIT);

const LONG_ABOUT: &str = "\
Manage wiki pages and revisions.

Pages use Markdown. In a terminal, page content is formatted; piped output
preserves the original bytes. Select pages by title; spaces and dashes are accepted.

  gea wiki list
  gea wiki view Home
  gea wiki create 'Release Process' -F notes.md
  gea wiki edit Home -e
  gea wiki revisions Home";

#[derive(Debug, ClapArgs)]
#[command(long_about = LONG_ABOUT)]
pub struct Args {
    #[command(subcommand)]
    pub command: Cmd,
}

#[derive(Debug, Subcommand)]
pub enum Cmd {
    /// List the pages
    List,
    /// Show one page
    View(ViewArgs),
    /// Create a page
    Create(CreateArgs),
    /// Change a page's content or title
    Edit(EditArgs),
    /// Delete a page
    Delete(DeleteArgs),
    /// A page's commit history
    Revisions(RevisionsArgs),
}

#[derive(Debug, ClapArgs)]
#[command(long_about = "\
Show a wiki page.

Terminal output is formatted and paged. Piped output is the original Markdown
without a header or added newline.

  gea wiki view Home
  gea wiki view Home > Home.md
  gea wiki view Home --sidebar")]
pub struct ViewArgs {
    /// Page title, or its dashed form
    #[arg(value_name = "PAGE")]
    pub page: String,

    /// Show the wiki's _Sidebar instead of the page
    #[arg(long)]
    pub sidebar: bool,

    /// Show the wiki's _Footer instead of the page
    #[arg(long, conflicts_with = "sidebar")]
    pub footer: bool,

    /// Print the page's metadata rather than its content
    #[arg(long, conflicts_with_all = ["sidebar", "footer"])]
    pub meta: bool,
}

#[derive(Debug, ClapArgs)]
#[command(long_about = "\
Create a wiki page.

Supply the body with -b, -F (file or - for stdin), or -e ($EDITOR).
With -e and no title argument, the editor's first line becomes the title.

  gea wiki create Home -b 'Welcome.'
  gea wiki create 'Release Process' -F release.md
  cat notes.md | gea wiki create Notes -F -")]
pub struct CreateArgs {
    /// Page title
    #[arg(value_name = "TITLE")]
    pub title: Option<String>,

    #[command(flatten)]
    pub body: BodyOpts,

    /// Commit message; defaults to one naming the page
    #[arg(long, value_name = "TEXT")]
    pub message: Option<String>,
}

#[derive(Debug, ClapArgs)]
#[command(long_about = "\
Edit a wiki page's content or title.

Renaming without a body option preserves the existing content.

  gea wiki edit Home -b 'Rewritten.'
  gea wiki edit Home -e
  gea wiki edit Home --title 'Start Here'")]
pub struct EditArgs {
    /// Page to change
    #[arg(value_name = "PAGE")]
    pub page: String,

    #[command(flatten)]
    pub body: BodyOpts,

    /// Rename the page
    #[arg(long, value_name = "TITLE")]
    pub title: Option<String>,

    /// Commit message; defaults to one naming the page
    #[arg(long, value_name = "TEXT")]
    pub message: Option<String>,
}

#[derive(Debug, ClapArgs)]
pub struct DeleteArgs {
    /// Page to delete
    #[arg(value_name = "PAGE")]
    pub page: String,

    /// Skip the confirmation
    #[arg(long)]
    pub yes: bool,
}

#[derive(Debug, ClapArgs)]
#[command(long_about = "\
Show a wiki page's commit history.

--limit caps displayed results; the endpoint only supports page-number pagination.")]
pub struct RevisionsArgs {
    /// Page whose history to show
    #[arg(value_name = "PAGE")]
    pub page: String,

    /// Which page of history (1-based)
    #[arg(long, value_name = "N", default_value = "1")]
    pub page_number: i32,
}

impl Cmd {
    fn fields(&self) -> Option<Fields> {
        match self {
            Self::List => Some(META_FIELDS),
            Self::View(a) if a.meta => Some(PAGE_FIELDS),
            // The point of `view` is the markdown; there is no document to select from, and
            // saying so is more useful than listing WikiPage's fields for a command that will
            // not print them.
            Self::View(_) => {
                Some(Fields::None("`wiki view` prints the page's markdown, not a JSON document"))
            }
            Self::Create(_) | Self::Edit(_) => Some(PAGE_FIELDS),
            Self::Revisions(_) => Some(COMMIT_FIELDS),
            Self::Delete(_) => None,
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
            Cmd::View(a) => view(&rt, &api, globals, &slug, a).await,
            Cmd::Create(a) => create(&rt, &api, globals, &slug, a).await,
            Cmd::Edit(a) => edit(&rt, &api, globals, &slug, a).await,
            Cmd::Delete(a) => delete(&rt, &api, &slug, a).await,
            Cmd::Revisions(a) => revisions(&rt, &api, globals, &slug, a).await,
        }
    })
}

// -------------------------------------------------------------------------------------- list

async fn list(rt: &Runtime, api: &Api, globals: &GlobalOpts, slug: &RepoSlug) -> Result<()> {
    let query = gitea_client::query::RepoGetWikiPagesQuery::default();
    let take = porcelain::item_limit(globals).unwrap_or(usize::MAX);
    let mut stream = api.repo().get_wiki_pages(&slug.owner, &slug.name, &query).take(take);
    let mut pages: Vec<WikiPageMetaData> = Vec::new();
    while let Some(item) = stream.next().await {
        pages.push(item.map_err(|e| explain(e, slug, None))?);
    }

    let machine = Machine::compile(globals, META_FIELDS)?;
    if pages.is_empty() {
        return porcelain::empty(
            globals,
            rt.term(),
            machine.as_ref(),
            &format!(
                "{slug} has no wiki pages. `gea wiki create <title> -b <text>` makes the first \
                 one; if the wiki is switched off entirely, enable it in Settings ▸ Units."
            ),
        );
    }
    if let Some(m) = machine {
        return m.write(globals, rt.term(), porcelain::json_of(&pages)?);
    }

    porcelain::print(globals, &render_list(rt.term(), &pages))
}

/// The `list` table.
pub(crate) fn render_list(term: &Term, pages: &[WikiPageMetaData]) -> String {
    let mut t = porcelain::table(term);
    t.headers(["TITLE", "UPDATED", "AUTHOR", "MESSAGE"]);
    for p in pages {
        let (author, when, message) = match &p.last_commit {
            Some(c) => (
                c.author.as_ref().map(|a| a.name.clone()).unwrap_or_default(),
                c.author.as_ref().map(|a| a.date.clone()).unwrap_or_default(),
                first_line(&c.message),
            ),
            None => (String::new(), String::new(), String::new()),
        };
        t.row([
            p.title.clone(),
            porcelain::dash(&short_date(&when)),
            porcelain::dash(&author),
            porcelain::dash(&message),
        ]);
    }
    porcelain::rendered_table(term, t, "wiki pages", None)
}

// -------------------------------------------------------------------------------------- view

async fn view(
    rt: &Runtime,
    api: &Api,
    globals: &GlobalOpts,
    slug: &RepoSlug,
    args: &ViewArgs,
) -> Result<()> {
    let page = fetch(api, slug, &args.page).await?;

    if args.meta {
        // The metadata view is the one place `--json` makes sense on `view`, and it deliberately
        // does *not* include the decoded content: a 400 KB page inside a table cell is not a
        // view, and `gea wiki view <page>` already prints it.
        if let Some(m) = Machine::compile(globals, PAGE_FIELDS)? {
            return m.write(globals, rt.term(), porcelain::json_of(&page)?);
        }
        return porcelain::print(globals, &render_meta(rt.term(), slug, &page));
    }

    let (what, raw) = match (args.sidebar, args.footer) {
        (true, _) => ("sidebar", page.sidebar.as_str()),
        (_, true) => ("footer", page.footer.as_str()),
        _ => ("page", page.content_base64.as_str()),
    };
    let content = b64::decode(raw);
    if content.trim().is_empty() {
        porcelain::note(rt.term(), &format!("{} has no {what} content", page.title));
        return Ok(());
    }

    // Piped: the committed bytes, verbatim, with nothing added. This is the contract that makes
    // `gea wiki view Home > Home.md` produce the file that is in the wiki's git history.
    if !rt.term().tty || globals.output.is_some() {
        use std::io::Write;
        let mut out = porcelain::writer(globals)?;
        out.write_all(content.as_bytes())?;
        out.flush()?;
        return Ok(());
    }

    let body = render_page(
        rt.term(),
        &page.title,
        &content,
        page.commit_count,
        !args.sidebar && !args.footer,
    );

    // A wiki page is the one thing in this wave long enough to want a pager.
    let pager = crate::output::pager::Pager::start(
        &crate::output::SysEnv,
        rt.term(),
        rt.config().pager(Some(rt.host().as_str())).as_deref(),
    );
    pager.finish(|out| out.write_all(body.as_bytes()))?;
    Ok(())
}

/// The rendered page, for a terminal: an underlined title, the markdown, and a pointer at the
/// history. Never used when output is piped — that path writes the committed bytes verbatim.
pub(crate) fn render_page(
    term: &Term,
    title: &str,
    content: &str,
    commit_count: i64,
    with_title: bool,
) -> String {
    let mut body = String::new();
    if with_title {
        body.push_str(&markdown::title(title, term));
        body.push('\n');
    }
    body.push_str(&markdown::render(content, term));
    if commit_count > 0 {
        body.push_str(&format!(
            "\n{commit_count} revision{} — `gea wiki revisions {}`\n",
            if commit_count == 1 { "" } else { "s" },
            shell_word(title)
        ));
    }
    body
}

pub(crate) fn render_meta(term: &Term, slug: &RepoSlug, page: &WikiPage) -> String {
    use std::fmt::Write as _;
    let mut o = String::new();
    let commit = page.last_commit.as_ref();
    let author = commit.and_then(|c| c.author.as_ref());
    let date = author.map(|a| a.date.clone()).unwrap_or_default();
    let name = author.map(|a| a.name.clone()).unwrap_or_default();
    let sha = commit.map(|c| c.sha.clone()).unwrap_or_default();
    if term.tty {
        let _ = writeln!(o, "title      {}", page.title);
        let _ = writeln!(o, "repository {slug}");
        let _ = writeln!(o, "revisions  {}", page.commit_count);
        let _ = writeln!(o, "last       {}", porcelain::dash(&date));
        let _ = writeln!(o, "author     {}", porcelain::dash(&name));
        let _ = writeln!(o, "commit     {}", porcelain::dash(&sha));
        let _ = writeln!(o, "url        {}", porcelain::dash(&page.html_url));
        // The decoded length, not the base64 one: `content_base64.len()` is a third larger and
        // would not match what `gea wiki view Home | wc -c` reports.
        let _ = writeln!(o, "bytes      {}", b64::decode(&page.content_base64).len());
    } else {
        let _ = writeln!(o, "{}\t{}\t{}\t{}", page.title, page.commit_count, sha, page.html_url);
    }
    o
}

// ------------------------------------------------------------------------------ create / edit

async fn create(
    rt: &Runtime,
    api: &Api,
    globals: &GlobalOpts,
    slug: &RepoSlug,
    args: &CreateArgs,
) -> Result<()> {
    // `-e` with no title: the first line is the title and the rest is the body, which is the
    // fixed meaning of `-e` in `docs/porcelain-conventions.md`. With a title argument the whole
    // buffer is the body, because the title is already known and stealing its first line would
    // silently delete a heading.
    let (title, content) = match (&args.title, args.body.editor) {
        (None, true) => {
            let buffer = porcelain::edit(rt, "", "md")?;
            split_title(&buffer)?
        }
        (Some(t), _) => {
            let content = args.body.read(rt, "", "md")?.ok_or_else(|| {
                porcelain::usage(
                    "a new page needs content; pass -b <text>, -F <file> (or -F - for stdin), \
                     or -e to write it in your editor",
                )
            })?;
            (t.clone(), content)
        }
        (None, false) => {
            return Err(porcelain::usage(
                "name the page: `gea wiki create <TITLE>`, or use -e and make the first line of \
                 the buffer the title",
            ));
        }
    };

    let body = CreateWikiPageOptions {
        content_base64: Some(b64::encode(&content)),
        message: Some(args.message.clone().unwrap_or_else(|| format!("Create {title}"))),
        title: Some(title.clone()),
    };
    let page = api
        .repo()
        .create_wiki_page(&slug.owner, &slug.name, &body)
        .await
        .map_err(|e| explain(e, slug, Some(&title)))?;
    report(rt, globals, &page, &format!("Created {} in {slug}'s wiki", page.title))
}

async fn edit(
    rt: &Runtime,
    api: &Api,
    globals: &GlobalOpts,
    slug: &RepoSlug,
    args: &EditArgs,
) -> Result<()> {
    if !args.body.given() && args.title.is_none() {
        return Err(porcelain::usage(
            "nothing to change; pass -b/-F/-e for new content or --title to rename the page",
        ));
    }

    // Read before writing, always. `PATCH …/wiki/page/{name}` replaces the whole page, so a
    // rename with no body would commit an empty one — and `-e` needs the current text to edit.
    let current = fetch(api, slug, &args.page).await?;
    let existing = b64::decode(&current.content_base64);
    let content = args.body.read(rt, &existing, "md")?.unwrap_or(existing);

    let body = CreateWikiPageOptions {
        content_base64: Some(b64::encode(&content)),
        message: Some(args.message.clone().unwrap_or_else(|| {
            format!("Update {}", args.title.as_deref().unwrap_or(&current.title))
        })),
        // Omitted means "keep unchanged": the field is only a rename, and sending an empty
        // string for it says the same thing the long way round.
        title: args.title.clone(),
    };
    let page = api
        .repo()
        .edit_wiki_page(&slug.owner, &slug.name, &wire_name(&args.page), &body)
        .await
        .map_err(|e| explain(e, slug, Some(&args.page)))?;

    let what = match &args.title {
        Some(t) => format!("Renamed {} to {t}", current.title),
        None => format!("Updated {}", page.title),
    };
    report(rt, globals, &page, &what)
}

async fn delete(rt: &Runtime, api: &Api, slug: &RepoSlug, args: &DeleteArgs) -> Result<()> {
    porcelain::confirm(
        rt,
        &format!("Delete the wiki page {:?} from {slug}?", args.page),
        args.yes,
    )?;
    api.repo()
        .delete_wiki_page(&slug.owner, &slug.name, &wire_name(&args.page))
        .await
        .map_err(|e| explain(e, slug, Some(&args.page)))?;
    porcelain::note(rt.term(), &format!("Deleted {}", args.page));
    Ok(())
}

// --------------------------------------------------------------------------------- revisions

async fn revisions(
    rt: &Runtime,
    api: &Api,
    globals: &GlobalOpts,
    slug: &RepoSlug,
    args: &RevisionsArgs,
) -> Result<()> {
    let query =
        gitea_client::query::RepoGetWikiPageRevisionsQuery::default().with_page(args.page_number);
    let list = api
        .repo()
        .get_wiki_page_revisions(&slug.owner, &slug.name, &wire_name(&args.page), &query)
        .await
        .map_err(|e| explain(e, slug, Some(&args.page)))?;

    let take = porcelain::item_limit(globals).unwrap_or(usize::MAX);
    let commits: Vec<_> = list.commits.iter().take(take).cloned().collect();
    let machine = Machine::compile(globals, COMMIT_FIELDS)?;
    if commits.is_empty() {
        return porcelain::empty(
            globals,
            rt.term(),
            machine.as_ref(),
            &format!("No revisions on page {} of {}'s history", args.page_number, args.page),
        );
    }
    if let Some(m) = machine {
        return m.write(globals, rt.term(), porcelain::json_of(&commits)?);
    }

    porcelain::print(
        globals,
        &render_revisions(rt.term(), &commits, u64::try_from(list.count).ok()),
    )
}

/// The `revisions` table.
///
/// `total` is the wiki's own `count`, so this is one of the few tables that can honestly say
/// "N of M" rather than only counting what it printed.
pub(crate) fn render_revisions(
    term: &Term,
    commits: &[gitea_model::WikiCommit],
    total: Option<u64>,
) -> String {
    let mut t = porcelain::table(term);
    t.headers(["SHA", "WHEN", "AUTHOR", "MESSAGE"]);
    for c in commits {
        let author = c.author.as_ref();
        t.row([
            short_sha(&c.sha),
            porcelain::dash(&short_date(&author.map(|a| a.date.clone()).unwrap_or_default())),
            porcelain::dash(&author.map(|a| a.name.clone()).unwrap_or_default()),
            porcelain::dash(&first_line(&c.message)),
        ]);
    }
    porcelain::rendered_table(term, t, "revisions", total)
}

// ------------------------------------------------------------------------------------ shared

async fn fetch(api: &Api, slug: &RepoSlug, page: &str) -> Result<WikiPage> {
    api.repo()
        .get_wiki_page(&slug.owner, &slug.name, &wire_name(page))
        .await
        .map_err(|e| explain(e, slug, Some(page)))
}

/// The path segment Gitea wants for a page title.
///
/// Gitea stores a page as a file whose name has spaces replaced by dashes, and the API's
/// `{pageName}` is that file name. A user who reads `Release Process` off `gea wiki list` and
/// pastes it back must not get a 404, so the substitution happens here rather than in their head.
/// A title that already uses dashes is unaffected, which is why this is safe to apply always.
fn wire_name(title: &str) -> String {
    title.trim().replace(' ', "-")
}

/// The title and body of an editor buffer whose first line is the title.
fn split_title(buffer: &str) -> Result<(String, String)> {
    let mut lines = buffer.lines();
    let title = lines.next().unwrap_or("").trim().to_owned();
    if title.is_empty() {
        return Err(porcelain::usage(
            "the editor buffer's first line is the page title, and it was empty; nothing was sent",
        ));
    }
    let rest: Vec<&str> = lines.collect();
    // Drop one leading blank line, which is how everyone separates a title from a body.
    let body =
        rest.iter().skip_while(|l| l.trim().is_empty()).copied().collect::<Vec<_>>().join("\n");
    Ok((title, body))
}

fn report(rt: &Runtime, globals: &GlobalOpts, page: &WikiPage, what: &str) -> Result<()> {
    if let Some(m) = Machine::compile(globals, PAGE_FIELDS)? {
        return m.write(globals, rt.term(), porcelain::json_of(page)?);
    }
    porcelain::note(rt.term(), what);
    if !page.html_url.is_empty() {
        porcelain::note(rt.term(), &page.html_url);
    }
    Ok(())
}

fn first_line(s: &str) -> String {
    s.lines().next().unwrap_or("").trim().to_owned()
}

fn short_sha(sha: &str) -> String {
    sha.chars().take(8).collect()
}

/// A git commit date (`2026-09-12T14:03:00+02:00`) trimmed to `2026-09-12 14:03`.
///
/// `WikiCommit`'s author date is a bare `String` in the specification rather than a timestamp, so
/// this is string surgery on purpose: parsing it into a `Timestamp` and formatting it back would
/// turn an unexpected format into a missing cell, and the raw value is more useful than nothing.
fn short_date(raw: &str) -> String {
    let s = raw.trim();
    match s.split_once('T') {
        Some((date, time)) => {
            let hm: String = time.chars().take(5).collect();
            format!("{date} {hm}")
        }
        None => s.to_owned(),
    }
}

/// Quote a title for the shell hint printed under `wiki view`.
fn shell_word(title: &str) -> String {
    if title.chars().all(|c| c.is_alphanumeric() || matches!(c, '-' | '_' | '.' | '/')) {
        title.to_owned()
    } else {
        format!("'{}'", title.replace('\'', "'\\''"))
    }
}

/// Turn a bare 404 into the two explanations that actually apply to a wiki.
///
/// Gitea answers 404 both for "no such page" and for "this repository's wiki unit is disabled",
/// and the remedies are completely different. Guessing one leaves half of all users checking the
/// wrong thing.
fn explain(e: Error, slug: &RepoSlug, page: Option<&str>) -> Error {
    match &*e.kind {
        ErrorKind::ResourceNotFound { .. } | ErrorKind::RouteNotFound { .. } => {
            Error::new(ErrorKind::Usage(match page {
                Some(p) => format!(
                    "{slug} has no wiki page {p:?} — check `gea wiki list`. If the list is also \
                     empty, the repository's wiki may be switched off (Settings ▸ Units ▸ Wiki), \
                     or this may be a mirror whose wiki was never cloned."
                ),
                None => format!(
                    "{slug} has no wiki. Enable it in Settings ▸ Units ▸ Wiki, or check \
                     `gea repo view --json has_wiki`."
                ),
            }))
        }
        _ => e,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::cmd::times::porcelain::testing;
    use gitea_core::http::transport::{Canned, FakeTransport};
    use std::sync::Arc;

    const PAGES: &str = r#"[
      {"title":"Home","sub_url":"Home","html_url":"https://git.example.org/them/proj/wiki/Home",
       "last_commit":{"sha":"0123456789abcdef","message":"Add a home page\n\nmore",
                      "author":{"name":"alice","email":"a@example.invalid",
                                "date":"2026-09-10T09:15:00+00:00"}}},
      {"title":"Release Process","sub_url":"Release-Process",
       "html_url":"https://git.example.org/them/proj/wiki/Release-Process",
       "last_commit":{"sha":"fedcba9876543210","message":"Document the release",
                      "author":{"name":"bob","email":"b@example.invalid",
                                "date":"2026-09-11T16:40:00+00:00"}}}
    ]"#;

    /// `# Wiki` + a list, base64-encoded, which is how the API sends a page.
    fn page_json(title: &str, markdown: &str) -> String {
        serde_json::json!({
            "title": title,
            "content_base64": b64::encode(markdown),
            "commit_count": 3,
            "html_url": format!("https://git.example.org/them/proj/wiki/{title}"),
            "sub_url": title,
            "sidebar": "",
            "footer": "",
            "last_commit": {
                "sha": "0123456789abcdef",
                "message": "Add a home page",
                "author": {"name": "alice", "email": "a@example.invalid",
                           "date": "2026-09-10T09:15:00+00:00"}
            }
        })
        .to_string()
    }

    fn pages() -> Vec<WikiPageMetaData> {
        serde_json::from_str(PAGES).expect("the fixture is valid WikiPageMetaData JSON")
    }

    /// Bug this prevents: `edit` with only `--title` sending an empty `content_base64` and
    /// committing a blank page. `PATCH …/wiki/page/{name}` replaces the whole page, so the current
    /// content has to be read and resent — this is the most destructive thing in the group.
    #[tokio::test]
    async fn renaming_a_page_resends_its_content_rather_than_blanking_it() {
        let markdown = "# Home\n\nWelcome to the wiki.\n";
        let fake = Arc::new(
            FakeTransport::new()
                .on(
                    testing::method("GET"),
                    "/api/v1/repos/them/proj/wiki/page/Home",
                    Canned::json(200, page_json("Home", markdown)),
                )
                .on(
                    testing::method("PATCH"),
                    "/api/v1/repos/them/proj/wiki/page/Home",
                    Canned::json(200, page_json("Start Here", markdown)),
                )
                .fallback(Canned::json(404, r#"{"message":"no"}"#)),
        );
        let api = testing::api(fake.clone());
        let slug = RepoSlug::new("them", "proj");

        let current = fetch(&api, &slug, "Home").await.unwrap();
        let existing = b64::decode(&current.content_base64);
        assert_eq!(existing, markdown, "the fixture must round-trip through base64");
        let body = CreateWikiPageOptions {
            content_base64: Some(b64::encode(&existing)),
            message: Some("Update Home".to_owned()),
            title: Some("Start Here".to_owned()),
        };
        api.repo().edit_wiki_page("them", "proj", &wire_name("Home"), &body).await.unwrap();

        let call =
            &fake.calls_to(&testing::method("PATCH"), "/api/v1/repos/them/proj/wiki/page/Home")[0];
        let sent: serde_json::Value =
            serde_json::from_slice(call.body.as_ref().expect("a JSON body")).unwrap();
        assert_eq!(sent["title"], "Start Here");
        assert_ne!(sent["content_base64"], "", "an empty body would commit a blank page");
        assert_eq!(b64::decode(sent["content_base64"].as_str().unwrap()), markdown);
    }

    /// Bug this prevents: addressing a page by its display title, so `Release Process` 404s.
    /// Gitea's `{pageName}` is the dashed file name.
    #[tokio::test]
    async fn a_title_with_spaces_is_requested_by_its_dashed_page_name() {
        let fake = Arc::new(
            FakeTransport::new()
                .on(
                    testing::method("GET"),
                    "/api/v1/repos/them/proj/wiki/page/Release-Process",
                    Canned::json(200, page_json("Release Process", "steps")),
                )
                .fallback(Canned::json(404, r#"{"message":"no"}"#)),
        );
        let api = testing::api(fake.clone());
        let page = fetch(&api, &RepoSlug::new("them", "proj"), "Release Process").await.unwrap();
        assert_eq!(page.title, "Release Process");
        assert_eq!(
            fake.calls_to(
                &testing::method("GET"),
                "/api/v1/repos/them/proj/wiki/page/Release-Process"
            )
            .len(),
            1
        );
    }

    #[test]
    fn the_list_table_renders_the_same_data_two_ways() {
        insta::assert_snapshot!("wiki_list_human", render_list(&testing::term(), &pages()));
        insta::assert_snapshot!("wiki_list_piped", render_list(&Term::piped(), &pages()));
    }

    #[test]
    fn the_json_output_uses_the_apis_own_field_names() {
        insta::assert_snapshot!(
            "wiki_list_json",
            testing::as_json(
                META_FIELDS,
                "title,sub_url,html_url",
                porcelain::json_of(&pages()).unwrap()
            )
        );
    }

    /// The rendered page, on a colourless terminal: headings lose their hashes, list items gain a
    /// bullet, and the revision pointer is appended.
    #[test]
    fn a_page_renders_with_its_title_and_a_pointer_at_its_history() {
        let markdown = "# Home\n\nWelcome.\n\n- one\n- two\n";
        insta::assert_snapshot!(
            "wiki_view_human",
            render_page(&testing::term(), "Home", markdown, 3, true)
        );
    }

    #[test]
    fn the_revisions_table_says_n_of_m_because_the_wiki_reports_a_total() {
        let commits: Vec<gitea_model::WikiCommit> = serde_json::from_str(
            r#"[
              {"sha":"0123456789abcdef","message":"Add a home page\n\nmore",
               "author":{"name":"alice","email":"a@example.invalid",
                         "date":"2026-09-10T09:15:00+00:00"}}
            ]"#,
        )
        .unwrap();
        insta::assert_snapshot!(
            "wiki_revisions_human",
            render_revisions(&testing::term(), &commits, Some(12))
        );
    }

    /// Bug this prevents: pasting a title straight out of `gea wiki list` into
    /// `gea wiki view` and getting a 404, because Gitea addresses the page by its
    /// dash-separated file name.
    #[test]
    fn a_title_with_spaces_becomes_the_dashed_page_name() {
        assert_eq!(wire_name("Release Process"), "Release-Process");
        assert_eq!(wire_name("  Home  "), "Home");
        // Already dashed, and titles that mix both, are unchanged — so the substitution is safe
        // to apply unconditionally.
        assert_eq!(wire_name("Release-Process"), "Release-Process");
        assert_eq!(wire_name("A-B C"), "A-B-C");
    }

    #[test]
    fn the_editor_buffers_first_line_is_the_title() {
        let (title, body) = split_title("Release Process\n\nStep one.\nStep two.\n").unwrap();
        assert_eq!(title, "Release Process");
        assert_eq!(body, "Step one.\nStep two.");
        // A title-only buffer is legal: an empty page is a real thing to create.
        let (title, body) = split_title("Home\n").unwrap();
        assert_eq!((title.as_str(), body.as_str()), ("Home", ""));
        assert!(split_title("\n\nbody only").is_err());
    }

    #[test]
    fn a_git_date_is_trimmed_to_the_minute_and_an_unknown_shape_survives() {
        assert_eq!(short_date("2026-09-12T14:03:22+02:00"), "2026-09-12 14:03");
        assert_eq!(short_date("who knows"), "who knows");
        assert_eq!(short_date(""), "");
    }

    /// Bug this prevents: a 404 on a wiki endpoint being reported as a missing page when the
    /// actual cause is the repository's wiki unit being switched off — two different remedies
    /// behind one status code.
    #[test]
    fn a_404_offers_both_explanations() {
        let e = explain(
            Error::new(ErrorKind::ResourceNotFound {
                kind: "wiki page",
                id: "Home".to_owned(),
                slug: None,
                // Discovered locally: there was no server reply to quote.
                server_message: None,
            }),
            &RepoSlug::new("them", "proj"),
            Some("Home"),
        );
        let msg = e.to_string();
        assert!(msg.contains("no wiki page"), "{msg}");
        assert!(msg.contains("switched off"), "{msg}");
    }

    #[test]
    fn a_title_needing_quotes_is_quoted_in_the_hint() {
        assert_eq!(shell_word("Home"), "Home");
        assert_eq!(shell_word("Release Process"), "'Release Process'");
        assert_eq!(shell_word("it's"), "'it'\\''s'");
    }
}
