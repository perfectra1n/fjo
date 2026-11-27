//! `gea repo create` — the only place three irreversible Gitea choices can be made.
//!
//! `--mirror-from`, `--object-format` and `--trust-model` are all **creation-time only**:
//! `PATCH /repos/{owner}/{repo}` cannot turn an ordinary repository into a pull mirror, cannot
//! rewrite its object format, and cannot change its trust model. Putting them anywhere else would
//! be offering the user a flag that silently does nothing, so they live here and the module doc
//! says why.
//!
//! Everything else this command does is orchestration: one API call, then up to four `git`
//! invocations for `--source`, `--push` and `--clone`.

use std::path::PathBuf;

use clap::Args as ClapArgs;
use gitea_client::Api;
use gitea_core::context::git::{CloneSpec, GitCli, GitCtx, PushSpec};
use gitea_core::types::RepoSlug;
use gitea_core::{Error, ErrorKind, Result};
use gitea_model::{
    CreateRepoOption, CreateRepoOptionTrustModel, GenerateRepoOption, MigrateRepoOptions,
    ObjectFormatName, Repository,
};

use crate::cmd::support;
use crate::global::GlobalOpts;
use crate::runtime::Runtime;

/// The visibility values the interactive picker offers, in the order it offers them.
const VISIBILITY: &[&str] = &["Public", "Private"];

#[derive(Debug, ClapArgs)]
#[command(long_about = "\
Create a repository.

Choose --public or --private, or answer the visibility prompt.
--mirror-from, --object-format, and --trust-model can only be set at creation.

  gea repo create my-thing --private --add-readme --license MIT
  gea repo create acme/service --public --clone
  gea repo create --source . --push --private
  gea repo create backup --private --mirror-from https://github.com/o/r")]
pub struct Args {
    /// `owner/name`, or a bare name in your own account
    #[arg(value_name = "OWNER/NAME")]
    pub name: Option<String>,

    /// Anyone can read it
    #[arg(long, conflicts_with = "private")]
    pub public: bool,

    /// Only you and your collaborators can read it
    #[arg(long)]
    pub private: bool,

    /// One-line description
    #[arg(long, value_name = "TEXT")]
    pub description: Option<String>,

    /// Clone the new repository into the current directory
    #[arg(long, conflicts_with = "source")]
    pub clone: bool,

    /// Use this existing local repository as the source, adding a remote for the new one
    #[arg(short = 's', long, value_name = "PATH")]
    pub source: Option<PathBuf>,

    /// Push the source repository's current branch after creating
    #[arg(long, requires = "source")]
    pub push: bool,

    /// Name for the git remote that is added to --source
    #[arg(short = 'r', long, value_name = "NAME", default_value = "origin")]
    pub remote: String,

    /// Add a README, so the repository is not empty
    #[arg(long, conflicts_with_all = ["source", "mirror_from"])]
    pub add_readme: bool,

    /// Gitignore template(s) to seed, comma-separated (`gea raw misc list-gitignores-templates`)
    #[arg(long, value_name = "NAME", conflicts_with_all = ["source", "mirror_from"])]
    pub gitignore: Option<String>,

    /// Licence template to seed (`gea raw misc list-license-templates`)
    #[arg(long, value_name = "NAME", conflicts_with_all = ["source", "mirror_from"])]
    pub license: Option<String>,

    /// Create from a template repository. Spelled `--from-template` because `--template` is the
    /// global Go-template flag
    #[arg(long = "from-template", value_name = "OWNER/NAME", conflicts_with = "mirror_from")]
    pub from_template: Option<String>,

    /// Create as a pull mirror of this URL. Only possible at creation time — Gitea cannot
    /// convert an existing repository into a pull mirror
    #[arg(long, value_name = "URL")]
    pub mirror_from: Option<String>,

    /// Git object format. Gitea-only, and permanent
    #[arg(long, value_name = "FORMAT", value_parser = ObjectFormatName::KNOWN.to_vec())]
    pub object_format: Option<String>,

    /// Signature trust model. Gitea-only, and permanent
    #[arg(long, value_name = "MODEL", value_parser = CreateRepoOptionTrustModel::KNOWN.to_vec())]
    pub trust_model: Option<String>,

    /// Name of the initial branch
    #[arg(long, value_name = "BRANCH")]
    pub default_branch: Option<String>,
}

pub fn run(globals: &GlobalOpts, args: &Args) -> Result<()> {
    // Before the runtime, so a bad `--json` or `--jq` is a usage error rather than a
    // configuration complaint, and bare `--json` needs neither token nor network.
    let wanted = support::machine::plan(globals, gitea_client::fields::FIELDS_REPOSITORY)?;
    if matches!(wanted, support::machine::Wanted::Listed) {
        return Ok(());
    }
    crate::runtime::block_on(async move {
        let rt = Runtime::new(globals)?;
        let api = support::api(&rt);
        let repo = create(&rt, &api, args).await?;
        after(&rt, args, &repo)?;
        report(&rt, globals, &wanted, &repo)
    })
}

/// The one API call, and the interaction that has to happen before it.
async fn create(rt: &Runtime, api: &Api, args: &Args) -> Result<Repository> {
    let slug = name_for(rt, api, args).await?;
    let private = visibility(rt, args)?;

    if let Some(url) = &args.mirror_from {
        // A pull mirror is `POST /repos/migrate` with `mirror: true`, not `POST /user/repos`.
        // Everything else here would be silently ignored by that endpoint, so the flags that
        // cannot apply are `conflicts_with`ed on the argument instead of dropped quietly.
        let body = MigrateRepoOptions {
            clone_addr: url.clone(),
            repo_name: slug.name.clone(),
            repo_owner: Some(slug.owner.clone()),
            description: args.description.clone(),
            mirror: Some(true),
            private: Some(private),
            ..MigrateRepoOptions::default()
        };
        return api.repo().migrate(&body).await;
    }

    if let Some(template) = &args.from_template {
        let (owner, name) = super::split_owner(template).ok_or_else(|| {
            Error::new(ErrorKind::Usage(format!(
                "--from-template wants owner/name, not {template:?}"
            )))
        })?;
        let body = GenerateRepoOption {
            name: slug.name.clone(),
            owner: slug.owner.clone(),
            description: args.description.clone(),
            default_branch: args.default_branch.clone(),
            private: Some(private),
            // A template that copies nothing is a template nobody wanted; `git_content` is the
            // whole point of the feature and `gh repo create --template` implies it.
            git_content: Some(true),
            labels: Some(true),
            topics: Some(true),
            ..GenerateRepoOption::default()
        };
        return api.repo().generate_repo(&owner, &name, &body).await;
    }

    let body = repo_option(args, &slug.name, private);
    // The owner decides the endpoint: `POST /user/repos` for your own account, `POST /orgs/{org}/
    // repos` for anyone else's. Asking who we are costs one request and is only paid when an
    // owner was actually named — `gea repo create thing` never reaches this branch.
    let named_owner = args.name.as_deref().and_then(super::split_owner).map(|(owner, _)| owner);
    if let Some(owner) = named_owner
        && owner != support::me(api).await?
    {
        return api.org().create_org_repo(&owner, &body).await;
    }
    api.repo().create_current_user_repo(&body).await
}

/// The `CreateRepoOption` for a from-scratch repository.
///
/// Split out and `pub(crate)` for its unit test: the `auto_init` coupling below is the kind of
/// thing that is easy to get wrong and impossible to notice, because the repository is created
/// either way — just empty.
pub(crate) fn repo_option(args: &Args, name: &str, private: bool) -> CreateRepoOption {
    // Gitea only writes a README, a .gitignore or a LICENSE when `auto_init` is set, and it
    // ignores those three fields entirely without it. So asking for any of them implies it;
    // otherwise `--license MIT` would be accepted and produce a repository with no licence.
    let seeded = args.add_readme || args.gitignore.is_some() || args.license.is_some();
    CreateRepoOption {
        name: name.to_owned(),
        description: args.description.clone(),
        private: Some(private),
        auto_init: Some(seeded),
        // The name of the README *template*, not a filename. Gitea rejects `auto_init` with an
        // empty one.
        readme: seeded.then(|| "Default".to_owned()),
        gitignores: args.gitignore.clone(),
        license: args.license.clone(),
        default_branch: args.default_branch.clone(),
        object_format_name: args.object_format.as_deref().map(ObjectFormatName::from),
        trust_model: args.trust_model.as_deref().map(CreateRepoOptionTrustModel::from),
        ..CreateRepoOption::default()
    }
}

/// `--public` / `--private`, or the prompt, or an error naming both flags.
///
/// Deliberately no default. A default of public eventually publishes somebody's private work; a
/// default of private surprises the other half of users. `gh` prompts for exactly this reason and
/// refuses non-interactively, and so do we.
fn visibility(rt: &Runtime, args: &Args) -> Result<bool> {
    if args.private {
        return Ok(true);
    }
    if args.public {
        return Ok(false);
    }
    if !support::can_prompt(rt) {
        return Err(Error::new(ErrorKind::Usage(
            "visibility is not guessed, and there is no terminal to ask on; pass --public or \
             --private"
                .to_owned(),
        )));
    }
    let labels: Vec<String> = VISIBILITY.iter().map(|s| (*s).to_owned()).collect();
    Ok(support::interact::select("Visibility", &labels)? == 1)
}

/// The repository to create: the argument, the `--source` directory's name, or a prompt.
async fn name_for(rt: &Runtime, api: &Api, args: &Args) -> Result<RepoSlug> {
    if let Some(arg) = &args.name {
        return match super::split_owner(arg) {
            Some((owner, name)) => Ok(RepoSlug::new(owner, name)),
            None => Ok(RepoSlug::new(support::me(api).await?, arg.trim())),
        };
    }
    // `--source .` is the common shape, and `.` has no useful basename, so the *canonical* path
    // is what gets read. Without this the repository would be called ".".
    if let Some(source) = &args.source {
        let canonical = source.canonicalize().unwrap_or_else(|_| source.clone());
        if let Some(name) = canonical.file_name().and_then(|s| s.to_str()) {
            return Ok(RepoSlug::new(support::me(api).await?, name));
        }
    }
    if !support::can_prompt(rt) {
        return Err(support::missing("a name (or -s/--source)", "the repository name"));
    }
    let name = support::interact::ask("Repository name", None)?;
    match super::split_owner(&name) {
        Some((owner, name)) => Ok(RepoSlug::new(owner, name)),
        None => Ok(RepoSlug::new(support::me(api).await?, name.trim())),
    }
}

/// `--source`, `--push` and `--clone`, in the only order that works.
fn after(rt: &Runtime, args: &Args, repo: &Repository) -> Result<()> {
    if let Some(source) = &args.source {
        let git = GitCli::in_dir(source);
        // Refusing here rather than running `git init` ourselves: `git init` in the wrong
        // directory is a mess to undo, and a directory the user believed was a repository not
        // being one is worth knowing about.
        if git.git_dir()?.is_none() {
            return Err(Error::new(ErrorKind::Usage(format!(
                "{} is not a git repository; run `git init` and make a commit there first",
                source.display()
            ))));
        }
        if git.remote_exists(&args.remote)? {
            support::note(
                rt.term(),
                &format!("remote {} already exists; leaving it alone", args.remote),
            );
        } else {
            git.remote_add(&args.remote, &repo.clone_url)?;
            support::note(
                rt.term(),
                &format!("added remote {} -> {}", args.remote, repo.clone_url),
            );
        }
        if args.push {
            // `HEAD` rather than a branch name, so this works on a repository whose initial
            // branch is `master`, `main`, or anything else — and `-u` so the next bare
            // `git push` in that clone knows where to go.
            git.push_or_fail(
                &PushSpec::new(&args.remote)
                    .with_refspec("HEAD")
                    .setting_upstream(true)
                    .with_progress(),
            )?;
        }
    }

    if args.clone {
        rt.git().clone_repo(&CloneSpec::new(&repo.clone_url).into_dir(&repo.name))?;
    }
    Ok(())
}

fn report(
    rt: &Runtime,
    globals: &GlobalOpts,
    wanted: &support::machine::Wanted,
    repo: &Repository,
) -> Result<()> {
    match wanted {
        support::machine::Wanted::Machine(m) => {
            support::machine::emit(rt, globals, m, support::to_value(repo)?)
        }
        // The URL and nothing else, on stdout: it is the one piece of output a script wants to
        // capture, and `gh repo create` prints the same thing.
        _ => {
            println!("{}", repo.html_url);
            Ok(())
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use clap::Parser;

    #[derive(Parser)]
    struct Harness {
        #[command(flatten)]
        args: Args,
    }

    fn parse(words: &[&str]) -> Args {
        Harness::try_parse_from(words).unwrap_or_else(|e| panic!("{words:?}: {e}")).args
    }

    /// Bug this prevents: `--license MIT` being accepted and producing a repository with no
    /// LICENSE file, because Gitea ignores `license` unless `auto_init` is also set. The
    /// repository is created either way, so nothing fails — the licence is just missing.
    #[test]
    fn asking_for_a_licence_turns_on_auto_init() {
        let args = parse(&["gea", "thing", "--private", "--license", "MIT"]);
        let body = repo_option(&args, "thing", true);
        assert_eq!(
            body.auto_init,
            Some(true),
            "license without auto_init is silently dropped by Gitea"
        );
        assert_eq!(body.license.as_deref(), Some("MIT"));
        assert_eq!(
            body.readme.as_deref(),
            Some("Default"),
            "auto_init with an empty readme template is rejected"
        );

        // ...and a plain create stays empty, which is what `gh repo create` does. The field is
        // *omitted* rather than sent as `""`: an unset flag is not a value, and every
        // `Some(String::new())` in a request body is one more way to overwrite something by
        // accident — the whole point of the `skip_serializing_if` the model already carries.
        let args = parse(&["gea", "thing", "--private"]);
        let body = repo_option(&args, "thing", true);
        assert_eq!(body.auto_init, Some(false));
        assert_eq!(body.readme, None);
    }

    #[test]
    fn a_gitignore_or_readme_also_turns_on_auto_init() {
        for words in [
            &["gea", "t", "--public", "--add-readme"][..],
            &["gea", "t", "--public", "--gitignore", "Rust"][..],
        ] {
            assert_eq!(repo_option(&parse(words), "t", false).auto_init, Some(true), "{words:?}");
        }
    }

    /// The two Gitea-only permanent choices must reach the wire as the API's own strings.
    #[test]
    fn object_format_and_trust_model_round_trip() {
        let args = parse(&[
            "gea",
            "t",
            "--public",
            "--object-format",
            "sha256",
            "--trust-model",
            "committer",
        ]);
        let body = repo_option(&args, "t", false);
        let json = serde_json::to_value(&body).expect("serialisable");
        assert_eq!(json["object_format_name"], "sha256");
        assert_eq!(json["trust_model"], "committer");
    }

    /// Bug this prevents: `--mirror-from` quietly co-existing with flags `POST /repos/migrate`
    /// does not implement, so `--license MIT --mirror-from …` looks accepted and does nothing.
    #[test]
    fn mirror_from_refuses_the_flags_it_cannot_honour() {
        for words in [
            &["gea", "t", "--public", "--mirror-from", "https://x/y", "--license", "MIT"][..],
            &["gea", "t", "--public", "--mirror-from", "https://x/y", "--add-readme"][..],
            &["gea", "t", "--public", "--mirror-from", "https://x/y", "--from-template", "a/b"][..],
        ] {
            assert!(Harness::try_parse_from(words).is_err(), "{words:?} should be refused");
        }
    }

    /// `--push` without `--source` has nothing to push, and accepting it would do nothing.
    #[test]
    fn push_requires_a_source() {
        assert!(Harness::try_parse_from(["gea", "t", "--public", "--push"]).is_err());
    }

    #[test]
    fn public_and_private_are_mutually_exclusive() {
        assert!(Harness::try_parse_from(["gea", "t", "--public", "--private"]).is_err());
    }
}
