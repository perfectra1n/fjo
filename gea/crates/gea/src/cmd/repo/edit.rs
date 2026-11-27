//! `gea repo edit` and `gea repo rename`.
//!
//! # Only what was asked for
//!
//! `PATCH /repos/{owner}/{repo}` is documented — by Gitea, and in the generated method's own doc
//! comment — as *"Only fields that are set will be changed."* In Gitea's Go source those fields
//! are pointers, and an absent key means "leave it alone".
//!
//! [`gitea_model::EditRepoOption`] says the same thing in Rust: every non-required property of a
//! request body is `Option<T>` with `skip_serializing_if = "Option::is_none"`, so `None` is an
//! absent key and `Some(v)` is a key with a value. That is a three-way distinction, and all three
//! are reachable from the command line: `--enable-wiki` is `Some(true)`, `--disable-wiki` is
//! `Some(false)`, and naming neither is `None`. Clearing a text field is the same mechanism —
//! `--description ''` sends `"description": ""` and blanks it, while omitting `--description`
//! sends nothing.
//!
//! This module used to assemble a `serde_json::Map` by hand, because the model's booleans were
//! plain `bool` and `EditRepoOption { archived: true, ..Default::default() }` serialised to a body
//! that *also* said `has_issues: false`, `has_wiki: false`, `allow_squash_merge: false`, … —
//! archiving a repository would have turned off every unit and every merge style in it. The
//! emitter now distinguishes request bodies from responses, so the typed model is correct here and
//! the hand-built body is gone. The first test below is kept as the regression net: if the emitter
//! ever reverts, it fails before anybody's repository does.

use clap::Args as ClapArgs;
use gitea_client::Api;
use gitea_core::types::RepoSlug;
use gitea_core::{Error, ErrorKind, Result};
use gitea_model::{EditRepoOption, Repository};

use crate::cmd::support;
use crate::global::GlobalOpts;
use crate::runtime::Runtime;

#[derive(Debug, ClapArgs)]
#[command(long_about = "\
Change repository settings.

Only specified settings are changed. Use paired flags such as --enable-issues
and --disable-issues to turn features on or off.

  gea repo edit --description 'A thing that does things'
  gea repo edit --default-branch main --delete-branch-on-merge
  gea repo edit them/proj --visibility private --disable-wiki")]
pub struct Args {
    /// `owner/name`, a bare name in your own account, or a URL
    #[arg(value_name = "REPOSITORY")]
    pub repo: Option<String>,

    /// One-line description
    #[arg(long, value_name = "TEXT")]
    pub description: Option<String>,

    /// Homepage URL
    #[arg(long, value_name = "URL")]
    pub website: Option<String>,

    /// Branch new pull requests target by default
    #[arg(long, value_name = "BRANCH")]
    pub default_branch: Option<String>,

    /// public or private
    #[arg(long, value_name = "WHEN", value_parser = ["public", "private"])]
    pub visibility: Option<String>,

    /// Merge style Gitea offers first
    #[arg(long, value_name = "STYLE", value_parser = MERGE_STYLES.to_vec())]
    pub default_merge_style: Option<String>,

    /// Delete the head branch after a merge, by default
    #[arg(long, conflicts_with = "keep_branch_on_merge")]
    pub delete_branch_on_merge: bool,

    /// Keep the head branch after a merge, by default
    #[arg(long)]
    pub keep_branch_on_merge: bool,

    /// Turn the issue tracker on
    #[arg(long, conflicts_with = "disable_issues")]
    pub enable_issues: bool,
    /// Turn the issue tracker off
    #[arg(long)]
    pub disable_issues: bool,

    /// Turn the wiki on
    #[arg(long, conflicts_with = "disable_wiki")]
    pub enable_wiki: bool,
    /// Turn the wiki off
    #[arg(long)]
    pub disable_wiki: bool,

    /// Turn pull requests on
    #[arg(long, conflicts_with = "disable_pull_requests")]
    pub enable_pull_requests: bool,
    /// Turn pull requests off
    #[arg(long)]
    pub disable_pull_requests: bool,

    /// Turn Gitea Actions on
    #[arg(long, conflicts_with = "disable_actions")]
    pub enable_actions: bool,
    /// Turn Gitea Actions off
    #[arg(long)]
    pub disable_actions: bool,

    /// Turn releases on
    #[arg(long, conflicts_with = "disable_releases")]
    pub enable_releases: bool,
    /// Turn releases off
    #[arg(long)]
    pub disable_releases: bool,

    /// Make it a template repository
    ///
    /// `--template` is reserved for output formatting.
    #[arg(long = "as-template", conflicts_with = "no_template")]
    pub template: bool,
    /// Stop it being a template repository
    #[arg(long)]
    pub no_template: bool,
}

/// The merge styles `default_merge_style` accepts.
///
/// Not `MergeStyle::KNOWN`: `PATCH /repos/{owner}/{repo}` documents two values that the merge
/// endpoint's enum does not carry — `rebase-update-only`, and `manually-merged` — so the two lists
/// are genuinely different and sharing them would reject a value the server accepts.
const MERGE_STYLES: &[&str] = &[
    "merge",
    "rebase",
    "rebase-merge",
    "squash",
    "fast-forward-only",
    "manually-merged",
    "rebase-update-only",
];

#[derive(Debug, ClapArgs)]
#[command(long_about = "\
Rename a repository.

Git remotes are not updated. Gitea redirects the old repository path.

  gea repo rename better-name
  gea repo rename them/proj new-name")]
pub struct RenameArgs {
    /// The new name. With two arguments, the first is the repository to rename
    #[arg(value_name = "NEW-NAME")]
    pub first: String,

    /// The new name, when a repository was named first
    #[arg(value_name = "NEW-NAME")]
    pub second: Option<String>,
}

pub fn run(globals: &GlobalOpts, args: &Args) -> Result<()> {
    let wanted = support::machine::plan(globals, gitea_client::fields::FIELDS_REPOSITORY)?;
    if matches!(wanted, support::machine::Wanted::Listed) {
        return Ok(());
    }
    let body = build(args)?;
    crate::runtime::block_on(async move {
        let rt = Runtime::new(globals)?;
        let api = support::api(&rt);
        let slug = super::target(&rt, globals, &api, args.repo.as_deref()).await?;
        let repo = patch_repo(&rt, &api, &slug, &body).await?;
        match &wanted {
            support::machine::Wanted::Machine(m) => {
                support::machine::emit(&rt, globals, m, support::to_value(&repo)?)
            }
            _ => {
                support::note(rt.term(), &format!("Updated {}", repo.full_name));
                Ok(())
            }
        }
    })
}

pub fn run_rename(globals: &GlobalOpts, args: &RenameArgs) -> Result<()> {
    let (repo, name) = match &args.second {
        Some(new_name) => (Some(args.first.clone()), new_name.clone()),
        None => (None, args.first.clone()),
    };
    if name.contains('/') {
        return Err(Error::new(ErrorKind::Usage(format!(
            "repository name {name:?} cannot contain '/'. To change owners, use `gea transfer`."
        ))));
    }
    crate::runtime::block_on(async move {
        let rt = Runtime::new(globals)?;
        let api = support::api(&rt);
        let slug = super::target(&rt, globals, &api, repo.as_deref()).await?;
        let body = EditRepoOption { name: Some(name.clone()), ..EditRepoOption::default() };
        let updated = patch_repo(&rt, &api, &slug, &body).await?;
        support::note(
            rt.term(),
            &format!(
                "Renamed {slug} to {}; Gitea redirects the old path, so existing clones keep \
                 working",
                updated.full_name
            ),
        );
        println!("{}", updated.html_url);
        Ok(())
    })
}

/// The PATCH body: exactly the settings the user asked about, and nothing else.
///
/// Every field stays `None` unless a flag named it, which is what makes the request touch only
/// what was mentioned. A paired toggle is `Some(true)` or `Some(false)` — the *off* half is a real
/// value that has to be sent, and is why the pairs exist instead of a flag taking `true|false`
/// that an empty shell variable could turn into a silent "off".
///
/// Returns a usage error when nothing was asked, because a `PATCH` with an empty body is a request
/// that succeeds and does nothing — the worst possible answer to a mistyped flag.
pub(crate) fn build(args: &Args) -> Result<EditRepoOption> {
    // A toggle is `Some` only when one of its two flags was given; `(false, false)` stays `None`
    // and so never reaches the wire.
    let toggle = |on: bool, off: bool| (on || off).then_some(on);

    let body = EditRepoOption {
        description: args.description.clone(),
        website: args.website.clone(),
        default_branch: args.default_branch.clone(),
        private: args.visibility.as_ref().map(|v| v == "private"),
        default_merge_style: args.default_merge_style.clone(),
        default_delete_branch_after_merge: toggle(
            args.delete_branch_on_merge,
            args.keep_branch_on_merge,
        ),
        has_issues: toggle(args.enable_issues, args.disable_issues),
        has_wiki: toggle(args.enable_wiki, args.disable_wiki),
        has_pull_requests: toggle(args.enable_pull_requests, args.disable_pull_requests),
        has_actions: toggle(args.enable_actions, args.disable_actions),
        has_releases: toggle(args.enable_releases, args.disable_releases),
        template: toggle(args.template, args.no_template),
        ..EditRepoOption::default()
    };

    if body == EditRepoOption::default() {
        return Err(Error::new(ErrorKind::Usage(
            "no changes specified; see `gea repo edit --help` for settings".to_owned(),
        )));
    }
    Ok(body)
}

/// `PATCH /repos/{owner}/{repo}`.
pub(crate) async fn patch_repo(
    rt: &Runtime,
    api: &Api,
    slug: &RepoSlug,
    body: &EditRepoOption,
) -> Result<Repository> {
    rt.trace(&format!(
        "PATCH {slug} {}",
        serde_json::to_string(body).unwrap_or_else(|e| format!("<unserialisable: {e}>"))
    ));
    api.repo().edit(&slug.owner, &slug.name, body).await
}

#[cfg(test)]
mod tests {
    use super::*;

    fn args(words: &[&str]) -> Args {
        #[derive(clap::Parser)]
        struct Harness {
            #[command(flatten)]
            args: Args,
        }
        <Harness as clap::Parser>::try_parse_from(words)
            .unwrap_or_else(|e| panic!("{words:?}: {e}"))
            .args
    }

    /// The body `build` produced by hand used to exist because the generated model sent every
    /// field it had. It no longer does — a not-required request-body field is `Option<T>` with
    /// `skip_serializing_if`, so the archive body below carries `archived` and nothing else.
    ///
    /// Kept as a regression test rather than deleted: if the emitter ever reverts to the
    /// response `Presence` policy for request bodies, `gea repo archive` starts turning off
    /// the issue tracker, the wiki, and every merge style the repository allows, and nothing
    /// about the call site would look wrong.
    #[test]
    fn the_generated_edit_model_sends_only_what_was_set() {
        let model = EditRepoOption { archived: Some(true), ..EditRepoOption::default() };
        let sent = serde_json::to_value(&model).expect("serialisable");
        assert_eq!(sent, serde_json::json!({"archived": true}));
        for must_be_absent in
            ["has_issues", "has_wiki", "has_releases", "allow_squash_merge", "allow_rebase"]
        {
            assert!(
                sent.get(must_be_absent).is_none(),
                "{must_be_absent} is on the wire even though nothing set it"
            );
        }
    }

    /// Bug this prevents: an edit sending settings the user did not mention, i.e. the bug above
    /// reaching a real repository. Asserted on the *serialised* body, because that is what the
    /// server sees; a `Some(false)` and a `None` are the same Rust `bool` field and differ only
    /// there.
    #[test]
    fn only_named_settings_reach_the_body() {
        let sent = |words: &[&str]| {
            serde_json::to_value(build(&args(words)).expect("a body")).expect("serialisable")
        };

        assert_eq!(sent(&["gea", "--description", "hi"]), serde_json::json!({"description": "hi"}));

        let body = sent(&["gea", "--disable-wiki", "--visibility", "private"]);
        assert_eq!(body, serde_json::json!({"has_wiki": false, "private": true}));
        assert!(body.get("has_issues").is_none(), "untouched settings must not be sent");
    }

    /// **Clearing a field and leaving it alone are different requests, and both are reachable.**
    /// This is the distinction the hand-built `serde_json::Map` used to exist to express, and the
    /// proof that removing it lost nothing: `--description ''` sends an empty string, which blanks
    /// the description, while omitting the flag sends no key at all and Gitea leaves it.
    #[test]
    fn clearing_a_field_and_leaving_it_alone_are_different_bodies() {
        let cleared = serde_json::to_value(
            build(&args(&["gea", "--description", "", "--website", ""])).expect("a body"),
        )
        .expect("serialisable");
        assert_eq!(cleared, serde_json::json!({"description": "", "website": ""}));

        // The same command without them touches neither — not even to send `""`.
        let untouched =
            serde_json::to_value(build(&args(&["gea", "--enable-wiki"])).expect("a body"))
                .expect("serialisable");
        assert!(untouched.get("description").is_none(), "{untouched}");
        assert!(untouched.get("website").is_none(), "{untouched}");

        // ...and the same three-way distinction on a boolean: on, off, and unmentioned.
        assert_eq!(build(&args(&["gea", "--enable-wiki"])).unwrap().has_wiki, Some(true));
        assert_eq!(build(&args(&["gea", "--disable-wiki"])).unwrap().has_wiki, Some(false));
        assert_eq!(build(&args(&["gea", "--enable-issues"])).unwrap().has_wiki, None);
    }

    /// Bug this prevents: `gea repo edit` with no flags reporting success while sending an empty
    /// PATCH — which is exactly what a mistyped flag name would produce if clap were lenient.
    #[test]
    fn an_edit_that_changes_nothing_is_a_usage_error() {
        let e = build(&args(&["gea"])).unwrap_err();
        assert_eq!(e.exit_code(), 2);
        assert!(e.to_string().contains("no changes specified"), "{e}");
    }

    /// `--enable-x --disable-x` is a contradiction; resolving it silently would pick one at
    /// random from the reader's point of view.
    #[test]
    fn paired_toggles_refuse_to_be_given_together() {
        #[derive(clap::Parser)]
        struct Harness {
            #[command(flatten)]
            args: Args,
        }
        for pair in [
            ["--enable-issues", "--disable-issues"],
            ["--enable-wiki", "--disable-wiki"],
            ["--delete-branch-on-merge", "--keep-branch-on-merge"],
        ] {
            assert!(
                <Harness as clap::Parser>::try_parse_from(["gea", pair[0], pair[1]]).is_err(),
                "{pair:?}"
            );
        }
    }

    /// Bug this prevents: `gea repo rename other/thing` being read as a rename *to* `other/thing`,
    /// which Gitea would reject with a validation error nobody could interpret.
    #[test]
    fn a_new_name_with_a_slash_is_refused_with_the_right_advice() {
        let args = RenameArgs { first: "other/thing".to_owned(), second: None };
        let e = run_rename(&GlobalOpts::default(), &args).unwrap_err();
        assert!(e.to_string().contains("gea transfer"), "{e}");
        assert_eq!(e.exit_code(), 2);
    }
}
