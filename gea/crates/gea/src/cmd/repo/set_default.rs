//! `gea repo set-default` — write down which remote is *the* repository.
//!
//! Repository resolution scores remote names (`upstream` 3, `gitea`/`codeberg` 2, `origin` 1) and
//! a tie at the top is [`ErrorKind::AmbiguousRemote`], which names the candidates and stops. This
//! command is the answer to that error, and to its opposite: a checkout where the *highest*-scoring
//! remote is not the one you mean.
//!
//! What it writes is git config `remote.<name>.gea-resolved`, namespaced per remote exactly as
//! `gh` namespaces `gh-resolved`, so both tools live in one clone without fighting. Three values:
//!
//! | value | meaning |
//! | --- | --- |
//! | `base` | this remote's URL *is* the repository |
//! | `owner/name` or `host/owner/name` | the repository, named directly — no URL parsing at all |
//! | `NONE` | stop asking: this checkout has no Gitea repository |
//!
//! The middle form is the escape hatch that matters. A remote written through an SSH alias
//! (`git@work-forge:whatever/thing.git`) cannot be parsed into a host and a slug by anything, and
//! `set-default` is how such a checkout is made to work at all — which is why the resolver reads
//! that value *verbatim* rather than looking at the URL again.

use clap::Args as ClapArgs;
use gitea_core::context::{
    RESOLVED_BASE, RESOLVED_NONE, RESOLVED_SUFFIX, remote_score, resolved_key,
};
use gitea_core::types::RepoSlug;
use gitea_core::{Error, ErrorKind, Result};

use crate::cmd::support;
use crate::global::GlobalOpts;
use crate::runtime::Runtime;

#[derive(Debug, ClapArgs)]
#[command(long_about = "\
Set the default repository for this checkout.

Overrides automatic remote selection. Without an argument, prompts for a
repository. The picker also lets you disable automatic selection for this checkout.

  gea repo set-default --view
  gea repo set-default gitea/gitea
  gea repo set-default --unset")]
pub struct Args {
    /// The repository to record. It should be one this checkout has a remote for
    #[arg(value_name = "REPOSITORY")]
    pub repo: Option<String>,

    /// Print what gea currently resolves, and why
    #[arg(long, conflicts_with_all = ["unset", "repo"])]
    pub view: bool,

    /// Forget every recorded choice in this checkout
    #[arg(long, conflicts_with = "repo")]
    pub unset: bool,
}

/// One remote, and what repository (if any) its URL names.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct Candidate {
    pub remote: String,
    pub slug: Option<RepoSlug>,
    pub url: String,
    pub score: u8,
}

impl Candidate {
    fn label(&self) -> String {
        match &self.slug {
            Some(slug) => format!("{slug}  ({})", self.remote),
            None => format!("{}  ({} — not a repository URL on this host)", self.url, self.remote),
        }
    }
}

pub fn run(globals: &GlobalOpts, args: &Args) -> Result<()> {
    crate::runtime::block_on(async move {
        let rt = Runtime::new(globals)?;
        // Every path here writes or reads git config in *this* checkout, so unlike the rest of the
        // group there is no `-R` shortcut: without a work tree there is nothing to configure.
        gitea_core::context::require_git_repo(rt.git())?;

        if args.view {
            return view(&rt, globals);
        }
        if args.unset {
            return unset(&rt);
        }

        let candidates = candidates(&rt)?;
        match &args.repo {
            Some(arg) => {
                let api = support::api(&rt);
                let slug = super::slug_from_arg(&rt, &api, arg).await?;
                record(&rt, &candidates, &slug)
            }
            None => pick(&rt, &candidates),
        }
    })
}

/// `--view`: what is resolved, and which rule produced it.
///
/// The `source` line is the point. "gea is talking to them/proj" invites the question "why?", and
/// `RepoSource` already knows the answer — `-R`, an environment variable, this very config key, or
/// remote-name scoring with the score it won by.
fn view(rt: &Runtime, globals: &GlobalOpts) -> Result<()> {
    let ctx = rt.repo(globals)?;
    println!("{}", ctx.slug);
    support::note(rt.term(), &format!("resolved from {}", ctx.source));
    Ok(())
}

fn unset(rt: &Runtime) -> Result<()> {
    let existing = rt.git().config_get_regexp(&format!(r"^remote\..*\.{RESOLVED_SUFFIX}$"))?;
    if existing.is_empty() {
        support::note(rt.term(), "no default repository saved for this checkout");
        return Ok(());
    }
    for (key, _) in &existing {
        rt.git().config_unset_local(key)?;
        support::note(rt.term(), &format!("unset {key}"));
    }
    Ok(())
}

/// Every remote, with the repository its URL names, best-scoring first.
fn candidates(rt: &Runtime) -> Result<Vec<Candidate>> {
    use gitea_core::context::remote_url;
    let keys = [rt.host().clone()];
    let mut out: Vec<Candidate> = Vec::new();
    for remote in rt.git().remotes()? {
        let url = remote.fetch.clone().or_else(|| remote.push.clone()).unwrap_or_default();
        let slug = remote.urls().find_map(|u| match remote_url::resolve(u, &keys) {
            remote_url::Resolution::Matched { slug, .. } => Some(slug),
            _ => None,
        });
        out.push(Candidate { score: remote_score(&remote.name), remote: remote.name, slug, url });
    }
    // Same ordering as the resolver's, so the picker's first entry is the one that would have been
    // chosen automatically. A picker that offered them in a different order would quietly teach the
    // user the wrong mental model.
    out.sort_by(|a, b| b.score.cmp(&a.score).then_with(|| a.remote.cmp(&b.remote)));
    Ok(out)
}

/// Write the choice for `slug`, using the narrowest form that works.
///
/// `base` when a remote's URL already names the repository — self-maintaining, because it keeps
/// working after `git remote set-url`. The literal `owner/name` only when no remote's URL parses,
/// which is the SSH-alias case; then it goes on the best-scoring remote, because the resolver walks
/// remotes in score order and would otherwise never reach it.
pub(crate) fn record(rt: &Runtime, candidates: &[Candidate], slug: &RepoSlug) -> Result<()> {
    let git = rt.git();
    if let Some(c) = candidates.iter().find(|c| c.slug.as_ref() == Some(slug)) {
        git.config_set_local(&resolved_key(&c.remote), RESOLVED_BASE)?;
        support::note(
            rt.term(),
            &format!("{slug} recorded: remote {} is the base repository", c.remote),
        );
        return Ok(());
    }

    let Some(fallback) = candidates.first() else {
        return Err(Error::new(ErrorKind::Usage(format!(
            "this checkout has no Git remotes. Add one or pass -R {slug} with each command."
        ))));
    };
    git.config_set_local(&resolved_key(&fallback.remote), &slug.to_string())?;
    support::note(
        rt.term(),
        &format!(
            "{slug} saved for remote {}.\n\
             The repository name is stored explicitly because the remote URL could not be resolved.",
            fallback.remote
        ),
    );
    Ok(())
}

/// The picker. The last entry is `NONE`, which is not padding.
///
/// Without it, a user in a checkout with no Gitea remote is asked the same unanswerable question
/// on every command. `NONE` is a real answer, the resolver honours it by stopping rather than
/// falling through to scoring, and its error then says "you opted out for this remote" instead of
/// re-listing the same candidates.
fn pick(rt: &Runtime, candidates: &[Candidate]) -> Result<()> {
    if !support::can_prompt(rt) {
        return Err(Error::new(ErrorKind::Usage(
            "there is no terminal to show a picker on; name the repository \
             (`gea repo set-default owner/name`), or use --view / --unset"
                .to_owned(),
        )));
    }
    if candidates.is_empty() {
        return Err(Error::new(ErrorKind::Usage("this checkout has no Git remotes".to_owned())));
    }

    let mut labels: Vec<String> = candidates.iter().map(Candidate::label).collect();
    labels.push("none (disable automatic repository selection)".to_owned());

    let chosen =
        support::interact::select("Which repository should gea commands act on?", &labels)?;
    if chosen == candidates.len() {
        // On the best-scoring remote, because that is the first one the resolver looks at, and an
        // opt-out the resolver reaches only after scoring would not stop anything.
        rt.git().config_set_local(&resolved_key(&candidates[0].remote), RESOLVED_NONE)?;
        support::note(
            rt.term(),
            &format!(
                "recorded {RESOLVED_NONE} on remote {}; gea will stop guessing here",
                candidates[0].remote
            ),
        );
        return Ok(());
    }
    match &candidates[chosen].slug {
        Some(slug) => record(rt, candidates, slug),
        None => Err(Error::new(ErrorKind::Usage(format!(
            "remote {} ({}) does not identify a repository on {}. Run `gea repo set-default owner/name`.",
            candidates[chosen].remote,
            candidates[chosen].url,
            rt.host()
        )))),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn candidate(remote: &str, slug: Option<&str>) -> Candidate {
        Candidate {
            remote: remote.to_owned(),
            slug: slug.map(|s| s.parse().expect("a slug")),
            url: format!("https://git.example.org/{}", slug.unwrap_or("weird/thing")),
            score: remote_score(remote),
        }
    }

    /// Bug this prevents: a picker whose first entry is not the choice the resolver would have made
    /// on its own, which teaches the user a wrong model of how resolution works.
    #[test]
    fn candidates_are_ordered_the_way_the_resolver_walks_remotes() {
        let mut cs = [
            candidate("origin", Some("me/fork")),
            candidate("zzz", Some("other/thing")),
            candidate("upstream", Some("them/proj")),
        ];
        cs.sort_by(|a, b| b.score.cmp(&a.score).then_with(|| a.remote.cmp(&b.remote)));
        assert_eq!(
            cs.iter().map(|c| c.remote.as_str()).collect::<Vec<_>>(),
            ["upstream", "origin", "zzz"]
        );
    }

    /// The label has to say *why* an entry cannot be chosen, or the picker looks broken.
    #[test]
    fn an_unparseable_remote_says_so_in_the_picker() {
        let label = candidate("weird", None).label();
        assert!(label.contains("not a repository URL"), "{label}");
        assert!(label.contains("weird"), "{label}");
    }

    #[test]
    fn a_parseable_remote_is_labelled_by_its_repository() {
        assert_eq!(candidate("upstream", Some("them/proj")).label(), "them/proj  (upstream)");
    }

    /// `--view` and `--unset` and a repository argument are three different requests; accepting two
    /// at once would make one of them silently win.
    #[test]
    fn the_three_modes_are_mutually_exclusive() {
        #[derive(clap::Parser)]
        struct Harness {
            #[command(flatten)]
            args: Args,
        }
        for words in [
            &["gea", "--view", "--unset"][..],
            &["gea", "--view", "o/r"][..],
            &["gea", "--unset", "o/r"][..],
        ] {
            assert!(<Harness as clap::Parser>::try_parse_from(words).is_err(), "{words:?}");
        }
    }
}
