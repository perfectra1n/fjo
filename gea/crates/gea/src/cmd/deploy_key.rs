//! `gea deploy-key` — a repository's deploy keys.
//!
//! `gh` has `gh repo deploy-key`; `tea` has nothing at all, which is an open request upstream.
//! The command name is `deploy-key` (hyphen) while the module is `deploy_key`, because Rust
//! module names cannot carry a hyphen and the *command* name is the one users type.
//!
//! # Read-only is the default, and write access is spelled out
//!
//! `CreateKeyOption.read_only` defaults to `false`, i.e. the API's own default hands out **push
//! access**. That is the wrong default for a credential you paste into a CI job, and it is the
//! kind of wrong default nobody notices until the key is used to force-push. So:
//!
//! * `gea deploy-key add id_ed25519.pub` adds a read-only key.
//! * `--read-only` is accepted and is a no-op, for scripts that want to say it out loud.
//! * `--allow-write` is the only way to get a writable key, and `list` prints the access level
//!   as its own column so an audit is one command.
//!
//! # The title comes from the key
//!
//! An OpenSSH public key's third field is its comment — usually `user@host` — and that is a far
//! better default title than making everyone invent one. `-t/--title` overrides it; a key with no
//! comment is an error that names the flag rather than a key called `""`.

use std::path::PathBuf;

use clap::{Args as ClapArgs, Subcommand};
use gitea_client::Api;
use gitea_client::gitea_model::{CreateKeyOption, DeployKey};
use gitea_core::error::Result;
use gitea_core::http::Paging;
use gitea_core::types::ids::KeyId;

use crate::cmd::support::{self, Emit, Json};
use crate::global::GlobalOpts;
use crate::output::Table;
use crate::runtime::Runtime;

const OP_LIST: &str = "repoListKeys";
const OP_ONE: &str = "repoGetKey";

#[derive(Debug, ClapArgs)]
pub struct Args {
    #[command(subcommand)]
    pub command: Cmd,
}

#[derive(Debug, Subcommand)]
pub enum Cmd {
    /// List the repository's deploy keys and what each one may do
    List,
    /// Add a deploy key, read-only unless --allow-write is given
    Add(Add),
    /// Show one deploy key
    View(View),
    /// Remove a deploy key
    Delete(Delete),
}

#[derive(Debug, ClapArgs)]
pub struct Add {
    /// A file holding one OpenSSH public key; `-` reads stdin
    #[arg(value_name = "KEY-FILE")]
    pub key_file: PathBuf,

    /// Name for the key. Defaults to the key's own comment.
    ///
    /// Spelled long-only: `docs/porcelain-conventions.md` reserves `-t` for `--title`, but the
    /// global `--template` already owns `-t` and clap answers a duplicate short with a panic.
    #[arg(long, value_name = "TITLE")]
    pub title: Option<String>,

    /// Add the key with read-only access. This is already the default; say it for clarity.
    #[arg(long, conflicts_with = "allow_write")]
    pub read_only: bool,

    /// Allow pushes with this key (disabled by default)
    #[arg(long)]
    pub allow_write: bool,
}

#[derive(Debug, ClapArgs)]
pub struct View {
    /// The key's numeric id, as shown by `gea deploy-key list`
    #[arg(value_name = "ID")]
    pub id: KeyId,
}

#[derive(Debug, ClapArgs)]
pub struct Delete {
    #[arg(value_name = "ID")]
    pub id: KeyId,
    /// Skip the confirmation
    #[arg(long)]
    pub yes: bool,
}

pub fn run(globals: &GlobalOpts, args: &Args) -> Result<()> {
    let op = match &args.command {
        Cmd::List => OP_LIST,
        Cmd::Add(_) | Cmd::View(_) => OP_ONE,
        Cmd::Delete(_) => "",
    };
    let fields = if op.is_empty() {
        None
    } else {
        match Json::resolve(globals, op)? {
            Json::Listed => return Ok(()),
            Json::Fields(f) => f,
        }
    };

    // The key is read before the runtime exists: a missing file should be reported as a missing
    // file, not preceded by a complaint about configuration. Reading stdin here also keeps it out
    // of the async block, where a blocking read would stall the reactor.
    let pending = match &args.command {
        Cmd::Add(a) => Some(Pending::read(a, &mut std::io::stdin())?),
        _ => None,
    };

    crate::runtime::block_on(async move {
        let rt = Runtime::new(globals)?;
        let api = Api::new(rt.client().clone());
        let slug = rt.repo(globals)?.slug.clone();
        let mut stdout = std::io::stdout().lock();
        let mut emit = Emit::new(globals, fields, rt.term(), &mut stdout)?;

        match &args.command {
            Cmd::List => {
                let cap = support::item_cap(globals);
                let (keys, total) = if globals.paginate {
                    let q = gitea_client::query::RepoListKeysQuery::default();
                    let keys =
                        support::drain(api.repo().list_keys(&slug.owner, &slug.name, &q), cap)
                            .await?;
                    let n = keys.len() as u64;
                    (keys, Some(n))
                } else {
                    let q = gitea_client::query::RepoListKeysQuery::default();
                    let (keys, info) = api
                        .repo()
                        .list_keys_page(
                            &slug.owner,
                            &slug.name,
                            &q,
                            Paging { limit: cap, per_page: None },
                        )
                        .await?;
                    (keys, info.total_count)
                };
                emit.many(&keys, total, "deploy keys", |table, keys| {
                    table.headers(["ID", "TITLE", "ACCESS", "FINGERPRINT"]);
                    for k in keys {
                        table.row([
                            k.id.to_string(),
                            k.title.clone(),
                            access(k).to_owned(),
                            k.fingerprint.clone(),
                        ]);
                    }
                })
            }

            Cmd::Add(_) => {
                let pending = pending.expect("Add always reads its key");
                let key = api.repo().create_key(&slug.owner, &slug.name, &pending.option).await?;
                emit.done(&format!("added {} deploy key {} ({})", access(&key), key.id, key.title));
                emit.one(&key, |t| detail(t, &key))
            }

            Cmd::View(a) => {
                let key = api.repo().get_key(&slug.owner, &slug.name, a.id.get()).await?;
                emit.one(&key, |t| detail(t, &key))
            }

            Cmd::Delete(a) => {
                support::confirm_term(
                    emit.term(),
                    a.yes,
                    &format!("delete deploy key {} from {slug}", a.id),
                )?;
                api.repo().delete_key(&slug.owner, &slug.name, a.id.get()).await?;
                emit.done(&format!("deleted deploy key {}", a.id));
                Ok(())
            }
        }
    })
}

/// A validated `add`, assembled before any network or runtime work happens.
#[derive(Debug)]
struct Pending {
    option: CreateKeyOption,
}

impl Pending {
    fn read(args: &Add, stdin: &mut dyn std::io::Read) -> Result<Self> {
        let raw = support::editor::read_source(&args.key_file, stdin)?;
        let key = raw.trim();
        if key.is_empty() {
            return Err(support::usage(format!(
                "{} held no key; a deploy key is one line of OpenSSH public key",
                args.key_file.display()
            )));
        }
        if key.lines().count() > 1 {
            return Err(support::usage(format!(
                "{} holds {} lines; Gitea takes one key per deploy key, so add them one at a \
                 time",
                args.key_file.display(),
                key.lines().count()
            )));
        }
        let title = match &args.title {
            Some(t) => t.clone(),
            None => comment_of(key).ok_or_else(|| {
                support::usage("this key has no comment to use as a title; name it with --title")
            })?,
        };
        Ok(Self {
            option: CreateKeyOption {
                key: key.to_owned(),
                // The inversion is the point: the flag a user has to *add* is the dangerous one.
                read_only: Some(!args.allow_write),
                title,
            },
        })
    }
}

/// The comment field of an OpenSSH public key: `ssh-ed25519 AAAA… deploy@ci`.
fn comment_of(key: &str) -> Option<String> {
    let mut parts = key.split_whitespace();
    let _algorithm = parts.next()?;
    let _blob = parts.next()?;
    // The comment may itself contain spaces, so take the whole remainder rather than one field.
    let rest: Vec<&str> = parts.collect();
    (!rest.is_empty()).then(|| rest.join(" "))
}

fn access(k: &DeployKey) -> &'static str {
    if k.read_only { "read-only" } else { "read-write" }
}

fn detail(table: &mut Table, k: &DeployKey) {
    table.row(["id".to_owned(), k.id.to_string()]);
    table.row(["title".to_owned(), k.title.clone()]);
    table.row(["access".to_owned(), access(k).to_owned()]);
    table.row(["fingerprint".to_owned(), k.fingerprint.clone()]);
    table.row(["key".to_owned(), k.key.clone()]);
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::cmd::support::testing;
    use gitea_core::http::FakeTransport;
    use gitea_core::http::transport::Canned;
    use std::sync::Arc;

    const KEY: &str = "ssh-ed25519 AAAAC3NzaC1lZDI1NTE5AAAAI deploy@ci";

    fn add(title: Option<&str>, allow_write: bool) -> Add {
        Add {
            key_file: PathBuf::from("-"),
            title: title.map(str::to_owned),
            read_only: false,
            allow_write,
        }
    }

    /// The bug this exists to prevent: honouring `CreateKeyOption`'s own `read_only: false`
    /// default, which quietly hands a CI job push access to the repository.
    #[test]
    fn add_is_read_only_unless_write_is_asked_for() {
        let mut stdin = KEY.as_bytes();
        let p = Pending::read(&add(None, false), &mut stdin).unwrap();
        assert_eq!(p.option.read_only, Some(true), "a deploy key must default to read-only");

        let mut stdin = KEY.as_bytes();
        let p = Pending::read(&add(None, true), &mut stdin).unwrap();
        assert_eq!(
            p.option.read_only,
            Some(false),
            "--allow-write is the only way to a writable key"
        );
    }

    /// Bug this prevents: uploading a key titled `""`, which is unidentifiable in the web UI and
    /// impossible to revoke with confidence.
    #[test]
    fn the_title_defaults_to_the_keys_comment() {
        let mut stdin = KEY.as_bytes();
        assert_eq!(Pending::read(&add(None, false), &mut stdin).unwrap().option.title, "deploy@ci");

        let mut stdin = KEY.as_bytes();
        assert_eq!(
            Pending::read(&add(Some("forge runner"), false), &mut stdin).unwrap().option.title,
            "forge runner"
        );

        // A comment with spaces survives whole rather than being cut at the first one.
        let mut stdin = "ssh-rsa AAAA my laptop key".as_bytes();
        assert_eq!(
            Pending::read(&add(None, false), &mut stdin).unwrap().option.title,
            "my laptop key"
        );

        // No comment: name the flag instead of inventing a title.
        let mut stdin = "ssh-ed25519 AAAAC3Nz".as_bytes();
        let e = Pending::read(&add(None, false), &mut stdin).unwrap_err();
        assert!(e.to_string().contains("--title"), "{e}");
    }

    /// Bug this prevents: pasting an `authorized_keys` file and uploading only the first line, or
    /// uploading a many-line blob the server rejects with a message about base64.
    #[test]
    fn several_keys_in_one_file_are_refused_by_name() {
        let mut stdin = format!("{KEY}\n{KEY}\n").into_bytes();
        let mut cursor: &[u8] = &mut stdin;
        let e = Pending::read(&add(None, false), &mut cursor).unwrap_err();
        assert_eq!(e.exit_code(), 2);
        assert!(e.to_string().contains("one key"), "{e}");
    }

    #[tokio::test]
    async fn add_posts_the_key_to_the_repository_collection() {
        let fake = Arc::new(testing::on(
            FakeTransport::new(),
            "POST",
            "/api/v1/repos/acme/widget/keys",
            Canned::json(201, r#"{"id":3,"title":"deploy@ci","read_only":true}"#),
        ));
        let api = testing::api(fake.clone());
        let mut stdin = KEY.as_bytes();
        let pending = Pending::read(&add(None, false), &mut stdin).unwrap();
        let key = api.repo().create_key("acme", "widget", &pending.option).await.unwrap();
        assert_eq!(key.id.to_string(), "3");

        let body: serde_json::Value = serde_json::from_str(&fake.calls()[0].body_str()).unwrap();
        assert_eq!(body["read_only"], serde_json::json!(true));
        assert_eq!(body["title"], serde_json::json!("deploy@ci"));
        assert_eq!(body["key"], serde_json::json!(KEY));
    }

    /// The access column is the reason `list` is worth a porcelain command at all: `read_only`
    /// as a bare `true`/`false` in JSON is one glance harder to audit than a word.
    #[test]
    fn the_list_view_spells_out_the_access_level() {
        let keys: Vec<DeployKey> = serde_json::from_str(
            r#"[{"id":1,"title":"ci","read_only":true,"fingerprint":"SHA256:aa"},
                {"id":2,"title":"release","read_only":false,"fingerprint":"SHA256:bb"}]"#,
        )
        .unwrap();
        let out = testing::captured(
            &GlobalOpts::default(),
            None,
            &crate::output::Term::piped(),
            |emit| {
                emit.many(&keys, Some(2), "deploy keys", |table, keys| {
                    table.headers(["ID", "TITLE", "ACCESS", "FINGERPRINT"]);
                    for k in keys {
                        table.row([
                            k.id.to_string(),
                            k.title.clone(),
                            access(k).to_owned(),
                            k.fingerprint.clone(),
                        ]);
                    }
                })
            },
        );
        insta::assert_snapshot!(out);
    }
}
