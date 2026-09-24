//! `gea topic` — a repository's topics.
//!
//! `gh` only reaches topics through `gh repo edit --add-topic`, which means removing one is a
//! different command from adding one and listing them is not a command at all. Gitea has a
//! proper collection, so this is `list` / `add` / `remove` / `set`.
//!
//! # `set` replaces, `add` and `remove` mutate
//!
//! `PUT /repos/{o}/{r}/topics` replaces the whole list, and that is what `set` is for. `add` and
//! `remove` go through the per-topic `PUT`/`DELETE` routes so that they cannot discard a topic
//! somebody else added between the read and the write — the same reason
//! `docs/porcelain-conventions.md` insists edit commands use `--add-X`/`--remove-X`.
//!
//! # Which topic was rejected
//!
//! Gitea validates topic names and answers a bad one with `422` and an
//! `{"invalidTopics": […], "message": "…"}` body. `gitea_core::error::classify` now harvests
//! `invalidTopics` alongside the `message`, so the server's own 422 does name the offending
//! values — reachable today through `gea raw repo update-topics`.
//!
//! This module still applies Gitea's own rule — lowercase, trim, then `^[a-z0-9][a-z0-9-]*$`
//! and at most [`MAX_TOPIC_LEN`] characters — *before* sending. The classifier fix retires the
//! first of the three reasons for that and leaves the other two standing:
//!
//! 1. ~~The server's 422 does not say which name was wrong.~~ It does now.
//! 2. **Nothing half-applies.** This is the load-bearing one, and no server-side fix can reach
//!    it: `add` and `remove` are a *loop* of per-topic `PUT`/`DELETE` calls (see below), so
//!    `gea topic add good bad` without this check adds `good`, then fails — leaving the
//!    repository in a state the user did not ask for and did not see coming. `set` is one
//!    request and would be atomic on its own, but having `set` validate and `add` not would be
//!    a worse tool than having both.
//! 3. **It costs no request**, so a typo is caught offline and instantly, and the rule can say
//!    *which* rule was broken — `contains ' '` — which the server's `invalidTopics` never does.
//!
//! The server stays authoritative: a name this rule accepts and Gitea refuses still surfaces
//! the server's own message.
//!
//! # The refusal is a usage error, not a validation error
//!
//! Because the check happens here, **no request is made** — so this must not borrow
//! [`gitea_core::ErrorKind::Validation`], whose renderer states "(HTTP 422)" and "the request reached the
//! server and its contents were rejected". Both sentences would be false, and an error that
//! misdescribes what just happened sends the reader to the server's logs to look for a request
//! that was never sent. It is a [`gitea_core::ErrorKind::Usage`] — the command line named something the
//! command cannot use — which is also what the sibling [`MAX_TOPICS`] check already returns.

use clap::{Args as ClapArgs, Subcommand};
use gitea_client::Api;
use gitea_client::gitea_model::RepoTopicOptions;
use gitea_core::error::Result;
use gitea_core::types::RepoSlug;

use crate::cmd::support::{self, Emit, Json};
use crate::global::GlobalOpts;
use crate::runtime::Runtime;

/// Gitea's limit on one topic name, from its own model validation.
pub const MAX_TOPIC_LEN: usize = 35;

/// Gitea's limit on how many topics one repository may carry.
pub const MAX_TOPICS: usize = 25;

const OP_LIST: &str = "repoListTopics";

#[derive(Debug, ClapArgs)]
pub struct Args {
    #[command(subcommand)]
    pub command: Cmd,
}

#[derive(Debug, Subcommand)]
pub enum Cmd {
    /// List the repository's topics
    List,
    /// Add topics, leaving the existing ones alone
    Add(Names),
    /// Remove topics, leaving the rest alone
    Remove(Names),
    /// Replace the whole topic list with exactly these names
    Set(Set),
}

#[derive(Debug, ClapArgs)]
pub struct Names {
    /// One or more topic names. Names are lowercased for you, as Gitea does.
    #[arg(value_name = "TOPIC", required = true)]
    pub topics: Vec<String>,
}

#[derive(Debug, ClapArgs)]
pub struct Set {
    /// The complete new topic list. Pass none, with --yes, to clear every topic.
    #[arg(value_name = "TOPIC")]
    pub topics: Vec<String>,

    /// Skip the confirmation for clearing the list
    #[arg(long)]
    pub yes: bool,
}

pub fn run(globals: &GlobalOpts, args: &Args) -> Result<()> {
    let fields = match &args.command {
        Cmd::List => match Json::resolve(globals, OP_LIST)? {
            Json::Listed => return Ok(()),
            Json::Fields(f) => f,
        },
        // The write routes answer 204, so there is no response to select fields from; the human
        // output is the resulting list, which `list` can show.
        _ => None,
    };

    // Validated before the runtime exists, so a bad name costs no request and no configuration.
    let names = match &args.command {
        Cmd::Add(a) | Cmd::Remove(a) => normalize(&a.topics)?,
        Cmd::Set(a) => {
            let names = normalize(&a.topics)?;
            if names.len() > MAX_TOPICS {
                return Err(support::usage(format!(
                    "Gitea allows at most {MAX_TOPICS} topics on a repository, and this is {}",
                    names.len()
                )));
            }
            names
        }
        Cmd::List => Vec::new(),
    };

    crate::runtime::block_on(async move {
        let rt = Runtime::new(globals)?;
        let api = Api::new(rt.client().clone());
        let slug = rt.repo(globals)?.slug.clone();
        let mut stdout = std::io::stdout().lock();
        let mut emit = Emit::new(globals, fields, rt.term(), &mut stdout)?;

        match &args.command {
            Cmd::List => {
                let topics = fetch(&api, &slug).await?;
                show(&mut emit, &topics)
            }
            Cmd::Add(_) => {
                for name in &names {
                    api.repo().add_topic(&slug.owner, &slug.name, name).await?;
                }
                emit.done(&plural("added", &names));
                show(&mut emit, &fetch(&api, &slug).await?)
            }
            Cmd::Remove(_) => {
                for name in &names {
                    api.repo().delete_topic(&slug.owner, &slug.name, name).await?;
                }
                emit.done(&plural("removed", &names));
                show(&mut emit, &fetch(&api, &slug).await?)
            }
            Cmd::Set(a) => {
                if names.is_empty() {
                    support::confirm_term(
                        emit.term(),
                        a.yes,
                        &format!("remove every topic from {slug}"),
                    )?;
                }
                let body = RepoTopicOptions { topics: Some(names.clone()) };
                api.repo().update_topics(&slug.owner, &slug.name, &body).await?;
                emit.done(&format!("{slug} now has {}", plural_count(names.len())));
                show(&mut emit, &fetch(&api, &slug).await?)
            }
        }
    })
}

/// The repository's topics, as the API returns them.
async fn fetch(api: &Api, slug: &RepoSlug) -> Result<Vec<String>> {
    let q = gitea_client::query::RepoListTopicsQuery::default();
    Ok(api.repo().list_topics(&slug.owner, &slug.name, &q).await?.topics)
}

/// Human output is one topic per line; `--json` keeps the API's `{"topics": […]}` envelope so
/// that `--json topics` and `gea raw repo list-topics --json topics` agree.
fn show(emit: &mut Emit<'_>, topics: &[String]) -> Result<()> {
    if emit.machine() {
        return emit.json(&serde_json::json!({ "topics": topics }));
    }
    if topics.is_empty() {
        support::note(emit.term(), "no topics");
        return Ok(());
    }
    emit.value(serde_json::json!({ "topics": topics }), |table| {
        for t in topics {
            table.row([t.clone()]);
        }
    })
}

// ------------------------------------------------------------------------------- validation

/// Normalise and validate a list of topic names the way Gitea does.
///
/// Normalisation is `trim` then `to_lowercase`, matching the server: `gea topic add Rust` and
/// `gea topic add rust` are the same request, and echoing back `Rust` when the repository now
/// carries `rust` would be a small lie the user would later trip over.
fn normalize(input: &[String]) -> Result<Vec<String>> {
    let mut bad: Vec<String> = Vec::new();
    let mut out: Vec<String> = Vec::new();
    for raw in input {
        let name = raw.trim().to_lowercase();
        match reject(&name) {
            Some(why) => bad.push(format!("{raw:?} {why}")),
            // A repeated name is not an error; it is one topic.
            None if out.contains(&name) => {}
            None => out.push(name),
        }
    }
    if !bad.is_empty() {
        // One line, lowercase, and it says what did *not* happen. A `Usage` message is the whole
        // explanation the user gets, so the "nothing was sent" half belongs in it: without that
        // sentence the natural reading of a rejection is that the server did the rejecting.
        let subject = match bad.len() {
            1 => "that is not a topic name Gitea accepts".to_owned(),
            n => format!("{n} of those are not topic names Gitea accepts"),
        };
        return Err(support::usage(format!("{subject}. No request was made: {}", bad.join("; "))));
    }
    Ok(out)
}

/// Why Gitea would refuse this name, or `None` if it would accept it.
///
/// Kept as one function returning the *reason* rather than a bool, so the error can say which
/// rule was broken instead of restating the regex at the user.
fn reject(name: &str) -> Option<String> {
    if name.is_empty() {
        return Some("a topic name cannot be empty".to_owned());
    }
    if name.chars().count() > MAX_TOPIC_LEN {
        return Some(format!(
            "{} characters; Gitea allows at most {MAX_TOPIC_LEN}",
            name.chars().count()
        ));
    }
    let first = name.chars().next()?;
    if !first.is_ascii_alphanumeric() {
        return Some(format!("starts with {first:?}; a topic must start with a letter or a digit"));
    }
    if let Some(c) = name.chars().find(|c| !c.is_ascii_alphanumeric() && *c != '-') {
        return Some(format!("contains {c:?}; a topic may only hold letters, digits and hyphens"));
    }
    None
}

fn plural(verb: &str, names: &[String]) -> String {
    format!("{verb} {}: {}", plural_count(names.len()), names.join(", "))
}

fn plural_count(n: usize) -> String {
    if n == 1 { "1 topic".to_owned() } else { format!("{n} topics") }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::cmd::support::testing;
    use gitea_core::http::FakeTransport;
    use gitea_core::http::transport::Canned;
    use std::sync::Arc;

    /// The bug this exists to prevent: reporting a bad topic without saying which of the names
    /// was wrong, or which rule it broke.
    #[test]
    fn a_rejected_topic_name_says_which_topic_and_which_rule() {
        let e = normalize(&[
            "rust".to_owned(),
            "not a topic".to_owned(),
            "-leading-hyphen".to_owned(),
            "x".repeat(36),
        ])
        .unwrap_err();
        let message = e.to_string();
        assert!(!message.contains("rust\""), "the good name must not be reported: {message}");
        insta::assert_snapshot!(message);
    }

    /// Bug this prevents — the reported one. The validation never sends a request, so borrowing
    /// `Validation` made the tool print "(HTTP 422)" and "the request reached the server and its
    /// contents were rejected" about a round trip that provably did not happen (it is absent from
    /// the server's own access log). A client-side refusal is a `Usage` error: exit 2, and a
    /// message that says nothing was sent.
    #[test]
    fn a_client_side_refusal_claims_no_http_round_trip() {
        let e = normalize(&["not a topic".to_owned()]).unwrap_err();
        assert!(
            matches!(&*e.kind, gitea_core::ErrorKind::Usage(_)),
            "a check that sends nothing must not be a server-side Validation: {:?}",
            e.kind
        );
        assert_eq!(e.exit_code(), 2, "a bad command line is exit 2");

        let rendered =
            gitea_core::error::render::render(&e, gitea_core::error::render::Color::Never);
        for lie in ["422", "reached the server"] {
            assert!(!rendered.contains(lie), "{lie:?} is not true of this error:\n{rendered}");
        }
    }

    #[test]
    fn names_are_lowercased_trimmed_and_deduplicated_like_the_server_does() {
        assert_eq!(
            normalize(&["Rust".to_owned(), " cli ".to_owned(), "RUST".to_owned()]).unwrap(),
            vec!["rust".to_owned(), "cli".to_owned()]
        );
        // Hyphens and digits are legal anywhere but the first character.
        assert_eq!(normalize(&["c99-tools".to_owned()]).unwrap(), vec!["c99-tools".to_owned()]);
    }

    /// Bug this prevents: `add` going through the replace-everything `PUT /topics`, which
    /// discards any topic added by someone else since the list was read.
    #[tokio::test]
    async fn add_uses_the_per_topic_route_not_the_replace_route() {
        let fake = Arc::new(testing::on(
            FakeTransport::new(),
            "PUT",
            "/api/v1/repos/acme/widget/topics/rust",
            testing::empty(),
        ));
        let api = testing::api(fake.clone());
        api.repo().add_topic("acme", "widget", "rust").await.unwrap();
        assert_eq!(fake.calls().len(), 1);
        assert_eq!(fake.calls()[0].path, "/api/v1/repos/acme/widget/topics/rust");
        assert!(fake.calls()[0].method == "PUT");
    }

    /// Bug this prevents: `set` sending the topics as a query string or as bare strings, which
    /// the API answers with a 422 that says nothing useful.
    #[tokio::test]
    async fn set_sends_the_whole_list_in_one_body() {
        let fake = Arc::new(testing::on(
            FakeTransport::new(),
            "PUT",
            "/api/v1/repos/acme/widget/topics",
            testing::empty(),
        ));
        let api = testing::api(fake.clone());
        let body = RepoTopicOptions { topics: Some(vec!["rust".into(), "cli".into()]) };
        api.repo().update_topics("acme", "widget", &body).await.unwrap();
        let sent: serde_json::Value = serde_json::from_str(&fake.calls()[0].body_str()).unwrap();
        assert_eq!(sent, serde_json::json!({"topics": ["rust", "cli"]}));
    }

    #[tokio::test]
    async fn list_renders_one_topic_per_line_and_keeps_the_json_envelope() {
        let fake = Arc::new(testing::on(
            FakeTransport::new(),
            "GET",
            "/api/v1/repos/acme/widget/topics",
            Canned::json(200, r#"{"topics":["cli","gitea","rust"]}"#),
        ));
        let api = testing::api(fake);
        let slug: RepoSlug = "acme/widget".parse().unwrap();
        let topics = fetch(&api, &slug).await.unwrap();

        let human =
            testing::captured(&GlobalOpts::default(), None, &crate::output::Term::piped(), |e| {
                show(e, &topics)
            });
        assert_eq!(human, "cli\ngitea\nrust\n");

        let g = GlobalOpts { json: Some("topics".into()), ..GlobalOpts::default() };
        let json = testing::captured(
            &g,
            Some(vec!["topics".into()]),
            &crate::output::Term::piped(),
            |e| show(e, &topics),
        );
        assert_eq!(json, "{\"topics\":[\"cli\",\"gitea\",\"rust\"]}\n");
    }
}
