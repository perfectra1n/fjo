//! `gea repo view` — the repository, and its README when a human is reading.
//!
//! Two calls that nobody wants to make by hand: the repository, then its README (found by listing
//! the root, because Gitea has no `/readme` endpoint the way GitHub does). The README is shown
//! **only on a terminal**: `gea repo view | …` is a data pipeline, and forty kilobytes of prose in
//! front of it is not a feature.

use std::io::Write;

use clap::Args as ClapArgs;
use futures::StreamExt;
use gitea_client::Api;
use gitea_core::types::RepoSlug;
use gitea_core::{Result, http::Mime};
use gitea_model::Repository;

use crate::cmd::support;
use crate::global::GlobalOpts;
use crate::output::color::{paint, style_by_name};
use crate::output::{Term, table::Table};
use crate::runtime::Runtime;

#[derive(Debug, ClapArgs)]
#[command(long_about = "\
Show repository details.

In a terminal, includes the README. Piped output omits it.
Use --json to select repository fields.

  gea repo view
  gea repo view gitea/gitea
  gea repo view -w
  gea repo view --json full_name,stars_count,default_branch")]
pub struct Args {
    /// `owner/name`, a bare name in your own account, or a URL
    #[arg(value_name = "REPOSITORY")]
    pub repo: Option<String>,

    /// Open in a browser instead of printing
    #[arg(short = 'w', long)]
    pub web: bool,

    /// Read the README at this branch, tag or commit
    #[arg(long, value_name = "REF")]
    pub r#ref: Option<String>,

    /// Do not print the README
    #[arg(long)]
    pub no_readme: bool,
}

pub fn run(globals: &GlobalOpts, args: &Args) -> Result<()> {
    let wanted = support::machine::plan(globals, gitea_client::fields::FIELDS_REPOSITORY)?;
    if matches!(wanted, support::machine::Wanted::Listed) {
        return Ok(());
    }
    crate::runtime::block_on(async move {
        let rt = Runtime::new(globals)?;
        let api = support::api(&rt);
        let slug = super::target(&rt, globals, &api, args.repo.as_deref()).await?;

        if args.web {
            // No repository fetch: the web URL is derivable, and `--web` should not fail because
            // the API is slow when all the user wanted was a browser tab.
            let url = format!("{}/{slug}", rt.client().web_base().trim_end_matches('/'));
            return support::open_web(&rt, &url);
        }

        // Decided *before* either request goes out, so `--json`/`--jq`/`--template` still costs
        // exactly one call: forty kilobytes of prose in front of a data pipeline is not a
        // feature, and a README fetch started speculatively and thrown away is worse than the
        // serial one it replaced.
        let want_readme = rt.term().tty
            && !args.no_readme
            && !matches!(wanted, support::machine::Wanted::Machine(_));

        let (repo, readme) = fetch(&api, &slug, args.r#ref.as_deref(), want_readme).await?;
        if let support::machine::Wanted::Machine(m) = &wanted {
            return support::machine::emit(&rt, globals, m, support::to_value(&repo)?);
        }

        let mut out = std::io::stdout().lock();
        out.write_all(render(&repo, readme.as_deref(), rt.term()).as_bytes())?;
        out.flush()?;
        Ok(())
    })
}

/// The repository and, when a human is going to read it, its README — concurrently.
///
/// `join!` rather than `try_join!`: [`readme`] returns an `Option` and swallows every failure by
/// design (see its doc comment), so there is no second error for a `try_join!` to race against,
/// and unwrapping `repo` afterwards leaves the one error this can report exactly where it was.
///
/// `want_readme` is decided by the caller before either future is built, so the machine path
/// never *starts* the README request — this is a gate, not a discarded result.
///
/// [`readme`]'s own two calls stay sequential: the raw-file read needs the path the
/// contents listing found.
///
/// Split out from [`run`] so a `FakeTransport` test can count the requests without a
/// [`Runtime`].
async fn fetch(
    api: &Api,
    slug: &RepoSlug,
    r#ref: Option<&str>,
    want_readme: bool,
) -> Result<(Repository, Option<String>)> {
    // Bound rather than called inline: `api.repo()` returns a borrow of `api`, and a temporary of
    // it does not outlive the `join!` that awaits both futures.
    let repos = api.repo();
    let (repo, readme) = futures::join!(repos.get(&slug.owner, &slug.name), async {
        match want_readme {
            true => readme(api, slug, r#ref).await,
            false => None,
        }
    });
    Ok((repo?, readme))
}

/// The README's text, or `None`.
///
/// Every failure here is swallowed on purpose. A repository with no README, a repository whose
/// default branch is empty, a `--ref` that does not exist, a README that is a symlink — none of
/// those is a reason for `gea repo view` to fail, because the repository *was* found and its
/// details are the answer to the question that was asked.
async fn readme(api: &Api, slug: &RepoSlug, r#ref: Option<&str>) -> Option<String> {
    let mut query = gitea_client::query::RepoGetContentsListQuery::default();
    if let Some(r) = r#ref {
        query = query.with_ref(r);
    }
    let entries = api.repo().get_contents_list(&slug.owner, &slug.name, &query).await.ok()?;
    // Case-insensitive and extension-agnostic, because `README`, `readme.md`, `README.rst` and
    // `Readme.markdown` are all in the wild and Gitea itself accepts any of them.
    let entry = entries
        .iter()
        .find(|e| e.r#type == "file" && e.name.to_ascii_lowercase().starts_with("readme"))?;

    let mut raw = gitea_client::query::RepoGetRawFileQuery::default();
    if let Some(r) = r#ref {
        raw = raw.with_ref(r);
    }
    let (mime, mut body) =
        api.repo().get_raw_file(&slug.owner, &slug.name, &entry.path, &raw).await.ok()?;
    // A README that is not text is a binary file somebody named README; printing it would wedge
    // the terminal, which `crate::output::guard_binary` exists to prevent elsewhere.
    if !is_texty(&mime) {
        return None;
    }
    let mut buf: Vec<u8> = Vec::new();
    while let Some(chunk) = body.next().await {
        buf.extend_from_slice(&chunk.ok()?);
    }
    Some(String::from_utf8_lossy(&buf).into_owned())
}

fn is_texty(mime: &Mime) -> bool {
    mime.is_text() || mime.as_str().is_empty() || mime.essence() == "application/octet-stream"
}

/// The human view. Pure, so the snapshot test below is the whole specification of the layout.
pub(crate) fn render(repo: &Repository, readme: Option<&str>, term: &Term) -> String {
    let bold = style_by_name("bold").unwrap_or_default();
    let dim = style_by_name("gray").unwrap_or_default();
    let mut out = String::new();

    out.push_str(&paint(term, bold, &repo.full_name));
    let tags = tags(repo);
    if !tags.is_empty() {
        out.push_str(&paint(term, dim, &format!("  ({})", tags.join(", "))));
    }
    out.push('\n');
    if !repo.description.is_empty() {
        out.push_str(&repo.description);
        out.push('\n');
    }

    // A small table rather than hand-aligned text, so it lines up with every other `gea` view
    // and uses the one width algorithm.
    let mut facts = Table::new(term);
    facts.row(["default branch", &repo.default_branch]);
    if !repo.language.is_empty() {
        facts.row(["language", &repo.language]);
    }
    facts.row(["stars", &repo.stars_count.to_string()]);
    facts.row(["forks", &repo.forks_count.to_string()]);
    if repo.has_issues {
        facts.row(["open issues", &repo.open_issues_count.to_string()]);
    }
    if repo.has_pull_requests {
        facts.row(["open pull requests", &repo.open_pr_counter.to_string()]);
    }
    if !repo.website.is_empty() {
        facts.row(["website", &repo.website]);
    }
    if !repo.topics.is_empty() {
        facts.row(["topics", &repo.topics.join(", ")]);
    }
    out.push('\n');
    out.push_str(&facts.render_to_string());

    match readme {
        Some(text) if !text.trim().is_empty() => {
            out.push('\n');
            out.push_str(&markdown(text, term));
        }
        // Saying so is better than an unexplained gap: "is the README missing, or did gea not
        // look?" is a question the output should not leave open.
        Some(_) | None => {
            out.push_str(&paint(term, dim, "\nThis repository has no README.\n"));
        }
    }

    out.push_str(&paint(
        term,
        dim,
        &format!("\nView this repository on the web: {}\n", repo.html_url),
    ));
    out
}

fn tags(repo: &Repository) -> Vec<String> {
    let mut tags = Vec::new();
    tags.push(if repo.private { "private" } else { "public" }.to_owned());
    if repo.archived {
        tags.push("archived".to_owned());
    }
    if repo.mirror {
        tags.push("mirror".to_owned());
    }
    if repo.template {
        tags.push("template".to_owned());
    }
    if let Some(parent) = repo.parent.as_deref() {
        tags.push(format!("fork of {}", parent.full_name));
    } else if repo.fork {
        tags.push("fork".to_owned());
    }
    tags
}

/// A deliberately small markdown-to-terminal pass.
///
/// This is **not** a markdown renderer, and it does not pretend to be: headings are emphasised,
/// emphasis markers are removed, fenced code is dimmed and indented, and everything else is left
/// exactly as the author wrote it. A real renderer means a real dependency (`gh` links glamour,
/// which is most of a terminal layout engine), and that is a decision for the project rather than
/// for one command — see the wave report.
///
/// The rules it *does* implement are chosen so that no *line* can be lost: nothing is reflowed,
/// nothing is truncated, and no line is dropped. They are still transformations, though —
/// indentation is added and paired markers are removed, and markdown is whitespace-sensitive — so
/// they run **only on a terminal**. Piped, the text is returned byte-identical, because piped
/// output is this tool's machine channel: `gea repo view > README.md` must write the README the
/// server holds, not a two-space-indented copy of it with the fences filed off.
fn markdown(text: &str, term: &Term) -> String {
    if !term.tty {
        return text.to_owned();
    }
    let bold = style_by_name("bold").unwrap_or_default();
    let dim = style_by_name("gray").unwrap_or_default();
    let mut out = String::new();
    let mut in_fence = false;

    for line in text.lines() {
        let trimmed = line.trim_start();
        if trimmed.starts_with("```") || trimmed.starts_with("~~~") {
            in_fence = !in_fence;
            continue;
        }
        if in_fence {
            out.push_str(&paint(term, dim, &format!("      {line}")));
            out.push('\n');
            continue;
        }
        if let Some(rest) = trimmed.strip_prefix('#') {
            let title = rest.trim_start_matches('#').trim();
            out.push_str(&paint(term, bold, &format!("  {}", inline(title))));
            out.push('\n');
            continue;
        }
        out.push_str(&format!("  {}\n", inline(line)));
    }
    out
}

/// Strip the inline markers that read as noise in a terminal.
///
/// Only paired markers are removed, so `2 * 3 * 4` and a lone underscore in `some_ident` survive
/// untouched — the failure mode of a naive `replace("*", "")` is mangling code and arithmetic.
fn inline(line: &str) -> String {
    let mut out = line.to_owned();
    for marker in ["**", "__", "`"] {
        if out.matches(marker).count() >= 2 && out.matches(marker).count().is_multiple_of(2) {
            out = out.replace(marker, "");
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;
    use gitea_core::http::transport::Canned;
    use std::sync::Arc;
    use support::testing;

    /// Bug this prevents: `gea repo view --json …` paying for the README it is never going to
    /// print. The README now *overlaps* the repository read, so "decide, then fetch" has to stay
    /// decided before either request leaves — a speculative fetch that is discarded is worse than
    /// the serial one it replaced, and the module header's rule is that forty kilobytes of prose
    /// in front of a data pipeline is not a feature.
    #[tokio::test]
    async fn the_machine_path_costs_one_request_and_never_starts_the_readme() {
        let fixture = || {
            let t = Arc::new(testing::on(
                testing::on(
                    testing::on(
                        testing::transport(),
                        "GET",
                        "/api/v1/repos/them/proj",
                        Canned::json(200, r#"{"full_name":"them/proj","name":"proj"}"#),
                    ),
                    "GET",
                    "/api/v1/repos/them/proj/contents",
                    Canned::json(200, r#"[{"name":"README.md","path":"README.md","type":"file"}]"#),
                ),
                "GET",
                "/api/v1/repos/them/proj/raw/README.md",
                Canned::text(200, "# proj\n"),
            ));
            (testing::api_at(testing::EXAMPLE, t.clone()), t)
        };
        let slug = RepoSlug::new("them", "proj");

        let (api, t) = fixture();
        let (repo, readme) = fetch(&api, &slug, None, false).await.expect("view");
        assert_eq!(repo.full_name, "them/proj");
        assert!(readme.is_none());
        assert_eq!(t.call_count(), 1, "{:?}", t.calls());

        // A human on a terminal still gets it, and `readme`'s own two calls stay dependent: the
        // raw read needs the path the listing found.
        let (api, t) = fixture();
        let (_, readme) = fetch(&api, &slug, None, true).await.expect("view");
        assert_eq!(readme.as_deref(), Some("# proj\n"));
        assert_eq!(t.call_count(), 3, "{:?}", t.calls());
    }

    fn repo() -> Repository {
        Repository {
            full_name: "them/proj".to_owned(),
            name: "proj".to_owned(),
            description: "A thing that does things".to_owned(),
            default_branch: "main".to_owned(),
            language: "Rust".to_owned(),
            stars_count: 12,
            forks_count: 3,
            has_issues: true,
            open_issues_count: 4,
            has_pull_requests: true,
            open_pr_counter: 1,
            topics: vec!["cli".to_owned(), "gitea".to_owned()],
            html_url: "https://git.example.org/them/proj".to_owned(),
            ..Repository::default()
        }
    }

    #[test]
    fn human_view_snapshot() {
        let readme = "# proj\n\nDoes **things**.\n\n```sh\nproj --help\n```\n";
        insta::assert_snapshot!(render(&repo(), Some(readme), &Term::tty(80)));
    }

    #[test]
    fn a_private_archived_fork_says_so() {
        let mut r = repo();
        r.private = true;
        r.archived = true;
        r.fork = true;
        r.parent = Some(Box::new(Repository {
            full_name: "orig/proj".to_owned(),
            ..Repository::default()
        }));
        assert_eq!(tags(&r), ["private", "archived", "fork of orig/proj"]);
    }

    /// Bug this prevents: an unexplained blank space where a README would be, so the reader
    /// cannot tell "there is no README" from "gea failed to fetch it".
    #[test]
    fn a_missing_readme_is_stated_rather_than_left_blank() {
        let out = render(&repo(), None, &Term::tty(80));
        assert!(out.contains("no README"), "{out}");
    }

    /// Bug this prevents: stripping every `*` and `_`, which turns `2 * 3` into `2  3` and
    /// `snake_case_name` into `snakecasename` inside a README.
    #[test]
    fn unpaired_markers_are_left_alone() {
        assert_eq!(inline("Does **things**."), "Does things.");
        assert_eq!(inline("width = 2 * 3 * 4"), "width = 2 * 3 * 4");
        assert_eq!(inline("call some_helper_fn"), "call some_helper_fn");
        assert_eq!(inline("use `cargo test`"), "use cargo test");
    }

    /// Bug this prevents: a fence marker being printed as `` ``` `` and the code inside it losing
    /// its indentation, which is the one thing a reader needs from a code block.
    #[test]
    fn fenced_code_is_indented_and_the_fence_markers_go_on_a_terminal() {
        let out = markdown("```rust\nfn main() {}\n```\nafter\n", &Term::tty(80));
        assert!(!out.contains("```"), "{out}");
        assert!(out.contains("      fn main() {}"), "{out}");
        assert!(out.contains("  after"), "{out}");
    }

    /// Bug this prevents: `gea repo view > README.md` writing a corrupted README, and
    /// `gea repo view | grep '^## Install'` matching nothing. Piped output is the machine
    /// channel everywhere else in this tool (`gea pr list | cut -f2` is real TSV), and the
    /// renderer's transformations — two leading spaces on every line, six on fenced code,
    /// dropped fence markers, stripped paired `**`/`__`/backticks — are all information loss in
    /// a whitespace-sensitive format. The property is byte-identity, not "close enough".
    #[test]
    fn piped_markdown_is_byte_identical_to_its_input() {
        let src = "# Title\n\nDoes **things** with `--json` and 2 * 3 * 4.\n\n\
                   ## Install\n\n```sh\n  cargo install gea\n```\n\n\
                   - a list\n  - nested, indentation-sensitive\n\n    four-space code\n";
        assert_eq!(markdown(src, &Term::piped()), src);
    }
}
