//! `gea ci` — CI status, logs and reruns for a pull request, branch or commit.
//!
//! The *context* layer over `gea run`: where `gea run` addresses one run precisely, `gea ci`
//! answers "what is CI doing for this pull request / branch / commit / where I am" and goes from
//! there to logs, watch and rerun without the user ever typing an id. The commit SHA is the join
//! key throughout — a target resolves to a SHA, and runs are listed with the server-side
//! `head_sha` filter, which (unlike `--branch`) finds `pull_request` runs, whose `head_branch`
//! Gitea leaves empty.
//!
//! The command group itself is registered separately; [`target`] is the resolution it is built
//! on. See `docs/superpowers/specs/2026-10-09-ci-porcelain-design.md`.

pub(crate) mod target;
