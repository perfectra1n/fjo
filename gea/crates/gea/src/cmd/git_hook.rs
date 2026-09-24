//! `gea git-hook` — the server-side git hooks Gitea runs on push.
//!
//! Neither `gh` nor `tea` has this: GitHub has no server-side hook API at all, and `tea` never
//! wrapped Gitea's. Layer 2 already reaches all four operations
//! (`gea raw repo list-git-hooks` and friends), so this group exists only for the three things
//! layer 2 cannot do well — see `docs/porcelain-conventions.md`.
//!
//! # There is no `delete`, because the API's DELETE does not delete
//!
//! `DELETE /repos/{o}/{r}/hooks/git/{id}` reads as "remove the hook", and it does not. Gitea's
//! git hooks are a **fixed set** built into the server ([`KNOWN_HOOKS`]); a repository always has
//! all of them and the API exposes no way to add or remove one. The DELETE route loads the named
//! hook, sets its content to the empty string and writes it back, which removes the hook *file*
//! from disk and leaves the hook itself listed with `is_active: false`. `list` shows it
//! afterwards, unchanged but inactive.
//!
//! So the verb here is [`Cmd::Disable`], not `delete`. Somebody who types
//! `gea git-hook delete pre-receive` expecting the hook to stop existing has been misled by the
//! route name, and the fix is not to repeat the lie: `delete` is accepted as a **hidden**
//! subcommand whose whole job is to explain what actually happens and name `disable`. That is
//! strictly better than clap's "unrecognized subcommand", which leaves the reader believing the
//! capability is missing rather than misnamed.
//!
//! # Why a table cannot render a hook
//!
//! A hook is a shell script. [`crate::output::Table`] replaces every newline and tab in a cell
//! with a space — it has to, or a piped row would parse as several records — so rendering a
//! hook's content through it produces one long unrunnable line. `view` therefore writes the
//! script to stdout **verbatim**, so that
//!
//! ```text
//! gea git-hook view pre-receive > pre-receive && chmod +x pre-receive
//! ```
//!
//! yields the file the server is running. `--json`/`--jq`/`--template` still go through the
//! normal pipeline, because those callers asked for a document rather than a script.
//!
//! # Hook names are suggested, never enforced
//!
//! [`KNOWN_HOOKS`] is what Gitea ships today, and it is used for help text and for the hint in
//! an error — but any name is sent to the server as typed. This is the same rule the generated
//! open enums follow: the server is authoritative about its own vocabulary, and a client-side
//! allowlist would make a newer Gitea's fourth hook unreachable through the porcelain while
//! `gea raw` kept working. Guessing wrong costs one 404 that names the hook.

use clap::{Args as ClapArgs, Subcommand};
use gitea_client::Api;
use gitea_client::gitea_model::{EditGitHookOption, GitHook};
use gitea_core::config::SystemEnv;
use gitea_core::error::Result;

use crate::cmd::support::{self, Emit, Json};
use crate::global::GlobalOpts;
use crate::output::Table;
use crate::runtime::Runtime;

/// The git hooks Gitea ships. Advisory — see the module comment.
pub const KNOWN_HOOKS: &[&str] = &["pre-receive", "update", "post-receive"];

const OP_LIST: &str = "repoListGitHooks";
const OP_ONE: &str = "repoGetGitHook";
const OP_EDIT: &str = "repoEditGitHook";

#[derive(Debug, ClapArgs)]
#[command(after_long_help = LONG_HELP)]
pub struct Args {
    #[command(subcommand)]
    pub command: Cmd,
}

const LONG_HELP: &str = "\
Manage server-side Git hooks: pre-receive, update, and post-receive.

These scripts run on the Gitea server during pushes. Editing them usually
requires an admin token. `disable` clears a hook's script; the hook remains listed.

  gea git-hook list
  gea git-hook view pre-receive > pre-receive.sh
  gea git-hook edit pre-receive -F pre-receive.sh
  cat hook.sh | gea git-hook edit update -F -
  gea git-hook edit post-receive -e            # opens $EDITOR on what is there now
  gea git-hook disable pre-receive --yes";

#[derive(Debug, Subcommand)]
pub enum Cmd {
    /// List the repository's git hooks and which ones are active
    List,
    /// Print one hook's script, verbatim
    View(View),
    /// Replace one hook's script
    Edit(Edit),
    /// Empty one hook's script, so it stops running
    Disable(Disable),
    /// Not a real operation; explains why and points at `disable`
    ///
    /// Hidden on purpose: it exists so that the obvious-but-wrong command answers with the truth
    /// instead of "unrecognized subcommand".
    #[command(hide = true)]
    Delete(Disable),
}

#[derive(Debug, ClapArgs)]
pub struct View {
    /// Hook name: pre-receive, update or post-receive
    #[arg(value_name = "HOOK")]
    pub hook: String,
}

#[derive(Debug, ClapArgs)]
pub struct Edit {
    /// Hook name: pre-receive, update or post-receive
    #[arg(value_name = "HOOK")]
    pub hook: String,

    /// The script, inline
    #[arg(short = 'b', long, value_name = "TEXT", conflicts_with_all = ["body_file", "editor"])]
    pub body: Option<String>,

    /// A file holding the script; `-` reads stdin
    #[arg(short = 'F', long, value_name = "FILE", conflicts_with = "editor")]
    pub body_file: Option<std::path::PathBuf>,

    /// Open $EDITOR on the hook's current script
    #[arg(short = 'e', long)]
    pub editor: bool,
}

#[derive(Debug, ClapArgs)]
pub struct Disable {
    /// Hook name: pre-receive, update or post-receive
    #[arg(value_name = "HOOK")]
    pub hook: String,

    /// Skip the confirmation
    #[arg(long)]
    pub yes: bool,
}

pub fn run(globals: &GlobalOpts, args: &Args) -> Result<()> {
    // Answered before anything else: no runtime, no configuration, no request. Somebody reaching
    // for `delete` deserves the explanation immediately, not after a failed auth lookup.
    if let Cmd::Delete(a) = &args.command {
        return Err(delete_is_not_what_it_sounds_like(&a.hook));
    }

    let op = match &args.command {
        Cmd::List => OP_LIST,
        Cmd::View(_) => OP_ONE,
        Cmd::Edit(_) => OP_EDIT,
        // A 204 with no body: there are no fields to select from.
        Cmd::Disable(_) | Cmd::Delete(_) => "",
    };
    let fields = if op.is_empty() {
        None
    } else {
        match Json::resolve(globals, op)? {
            Json::Listed => return Ok(()),
            Json::Fields(f) => f,
        }
    };

    // Where `edit`'s script comes from is settled before the runtime exists, for the same reason
    // `deploy-key add` reads its key here: an unreadable file should be reported as an unreadable
    // file rather than behind a complaint about configuration, a blocking stdin read does not
    // belong inside the async block, and — the part that matters — `edit` with no script at all
    // must fail **before** it spends a request finding out.
    let given = match &args.command {
        Cmd::Edit(a) => Some(Source::read(a, &mut std::io::stdin())?),
        _ => None,
    };

    crate::runtime::block_on(async move {
        let rt = Runtime::new(globals)?;
        let api = Api::new(rt.client().clone());
        let slug = rt.repo(globals)?.slug.clone();
        let mut stdout = std::io::stdout().lock();

        match &args.command {
            Cmd::List => {
                let hooks = api.repo().list_git_hooks(&slug.owner, &slug.name).await?;
                let mut emit = Emit::new(globals, fields, rt.term(), &mut stdout)?;
                // Not paginated: the route answers with the whole fixed set in one response, so
                // there is no total to claim beyond what we were handed.
                emit.many(&hooks, None, "git hooks", |table, hooks| {
                    table.headers(["NAME", "ACTIVE", "SCRIPT"]);
                    for h in hooks {
                        table.row([h.name.clone(), active(h).to_owned(), summary(h)]);
                    }
                })
            }

            Cmd::View(a) => {
                let hook = api.repo().get_git_hook(&slug.owner, &slug.name, &a.hook).await?;
                if globals.wants_machine_output() {
                    let mut emit = Emit::new(globals, fields, rt.term(), &mut stdout)?;
                    return emit.one(&hook, |t| detail(t, &hook));
                }
                // The human rendering of a script is the script. See the module comment.
                if hook.content.is_empty() {
                    support::note(
                        rt.term(),
                        &format!("{} is not active: its script is empty", hook.name),
                    );
                    return Ok(());
                }
                write_verbatim(&mut stdout, &hook.content)
            }

            Cmd::Edit(a) => {
                let content = match given.as_ref().expect("Edit always resolves a source") {
                    Source::Text(t) => t.clone(),
                    // Both the explicit `-e` and the interactive fallback need what is there
                    // now, which is the second call that makes this more than a renamed PATCH.
                    Source::Editor => {
                        let current =
                            api.repo().get_git_hook(&slug.owner, &slug.name, &a.hook).await?;
                        let editor =
                            rt.config().resolved_editor(Some(rt.host().as_str()), &SystemEnv);
                        open_editor(&editor, &seed(&current))?
                    }
                };
                let body = EditGitHookOption { content: Some(content) };
                let hook =
                    api.repo().edit_git_hook(&slug.owner, &slug.name, &a.hook, &body).await?;
                let mut emit = Emit::new(globals, fields, rt.term(), &mut stdout)?;
                emit.done(&format!("{} is now {} ({})", hook.name, active(&hook), summary(&hook)));
                emit.one(&hook, |t| detail(t, &hook))
            }

            Cmd::Disable(a) => {
                let emit = Emit::new(globals, fields, rt.term(), &mut stdout)?;
                // Worded as what it is. "delete the pre-receive hook" would be a confirmation
                // for something that is not about to happen.
                support::confirm_term(
                    emit.term(),
                    a.yes,
                    &format!("empty the {} git hook on {slug}, so it stops running", a.hook),
                )?;
                api.repo().delete_git_hook(&slug.owner, &slug.name, &a.hook).await?;
                emit.done(&format!(
                    "{} is now inactive; it is still listed, with an empty script",
                    a.hook
                ));
                Ok(())
            }

            // Refused above, before any of this ran.
            Cmd::Delete(a) => Err(delete_is_not_what_it_sounds_like(&a.hook)),
        }
    })
}

/// Where `edit`'s new script comes from, settled from the flags alone.
#[derive(Debug, PartialEq)]
enum Source {
    /// `-b` or `-F`: the script is already in hand.
    Text(String),
    /// `-e`, or the interactive fallback: fetch the current script and open an editor on it.
    Editor,
}

impl Source {
    /// Resolve `edit`'s script, or explain what is missing — **without** a runtime, a token, or a
    /// request.
    ///
    /// Off a terminal a missing script is an error **naming the flags**, never a prompt: an editor
    /// launched from a CI job either fails oddly or waits for a keystroke nobody will type. Doing
    /// this here rather than after the `GET` is the difference between a usage error and a usage
    /// error charged to the user's rate limit.
    fn read(args: &Edit, stdin: &mut dyn std::io::Read) -> Result<Self> {
        if let Some(text) = &args.body {
            return Ok(Self::Text(text.clone()));
        }
        if let Some(path) = &args.body_file {
            return Ok(Self::Text(support::editor::read_source(path, stdin)?));
        }
        if args.editor {
            if terminals_attached() {
                return Ok(Self::Editor);
            }
            // Kept separate from the message below so that `-e` in a pipe says why `-e` in
            // particular could not work, rather than listing itself as the remedy.
            return Err(support::usage(
                "-e/--editor needs a terminal; pass -b/--body or -F/--body-file instead (`-F -` \
                 reads stdin)",
            ));
        }
        if can_prompt() {
            return Ok(Self::Editor);
        }
        Err(support::usage(format!(
            "{} needs a script; pass -b/--body, -F/--body-file (- reads stdin), or -e/--editor",
            args.hook
        )))
    }
}

/// What the editor opens on: the script that is running now, or a shebang for an empty hook.
///
/// Seeding an inactive hook with `#!/bin/sh` rather than an empty buffer is the difference
/// between a hook that runs and one the server ignores, and it is not obvious that it matters.
fn seed(current: &GitHook) -> String {
    if current.content.is_empty() { "#!/bin/sh\n".to_owned() } else { current.content.clone() }
}

/// `$EDITOR` on a temporary file holding `initial`, returning what came back.
///
/// The suffix is `.sh` — not the `.md` the issue and pull-request editors use — so that an editor
/// applies shell highlighting to what is, in fact, a shell script.
fn open_editor(editor: &str, initial: &str) -> Result<String> {
    let path = std::env::temp_dir().join(format!(
        "gea-hook-{}-{}.sh",
        std::process::id(),
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|d| d.as_nanos())
            .unwrap_or_default()
    ));
    std::fs::write(&path, initial)?;

    let mut parts = crate::output::pager::split_command(editor);
    if parts.is_empty() {
        return Err(support::usage(
            "no editor is configured; set $EDITOR or `gea config set editor <cmd>`",
        ));
    }
    let program = parts.remove(0);
    let status = std::process::Command::new(&program)
        .args(&parts)
        .arg(&path)
        .status()
        .map_err(|e| support::usage(format!("could not run the editor {program:?}: {e}")))?;
    if !status.success() {
        let _ = std::fs::remove_file(&path);
        return Err(support::usage(format!(
            "the editor {program:?} exited with {status}; nothing was sent"
        )));
    }
    let text = std::fs::read_to_string(&path)?;
    let _ = std::fs::remove_file(&path);
    Ok(text)
}

/// Whether an interactive fallback is allowed: both streams are terminals and prompting has not
/// been switched off.
fn can_prompt() -> bool {
    terminals_attached() && std::env::var_os("GEA_PROMPT_DISABLED").is_none()
}

/// Both stdin and stdout are terminals. Stdout matters as much as stdin: an editor that paints
/// over a pipe leaves the user staring at a command that looks hung.
fn terminals_attached() -> bool {
    use std::io::IsTerminal;
    std::io::stdin().is_terminal() && std::io::stdout().is_terminal()
}

/// Write a hook's script to `out` exactly as the server holds it, with a trailing newline added
/// only if the script lacks one.
///
/// Adding the newline unconditionally would change the bytes of a script that already ends in
/// one; omitting it entirely leaves a shell prompt glued to the last line of output.
fn write_verbatim(out: &mut impl std::io::Write, content: &str) -> Result<()> {
    out.write_all(content.as_bytes())?;
    if !content.ends_with('\n') {
        out.write_all(b"\n")?;
    }
    out.flush()?;
    Ok(())
}

fn active(h: &GitHook) -> &'static str {
    if h.is_active { "active" } else { "inactive" }
}

/// A one-cell description of a script, for the list table: `12 lines` or `empty`.
///
/// The script itself cannot go in a cell — newlines are flattened to spaces — so the column says
/// how much there is and `view` prints it.
fn summary(h: &GitHook) -> String {
    let lines = h.content.lines().count();
    match lines {
        0 => "empty".to_owned(),
        1 => "1 line".to_owned(),
        n => format!("{n} lines"),
    }
}

fn detail(table: &mut Table, h: &GitHook) {
    table.row(["name".to_owned(), h.name.clone()]);
    table.row(["active".to_owned(), h.is_active.to_string()]);
    table.row(["script".to_owned(), summary(h)]);
}

/// The whole point of the hidden `delete` subcommand.
fn delete_is_not_what_it_sounds_like(hook: &str) -> gitea_core::Error {
    support::usage(format!(
        "Git hooks cannot be deleted (fixed hooks: {}). Disable {hook:?} with `gea git-hook disable {hook}`. This clears the script; the hook remains listed with active=false.",
        KNOWN_HOOKS.join(", ")
    ))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::cmd::support::testing;
    use gitea_core::http::FakeTransport;
    use gitea_core::http::transport::Canned;
    use std::sync::Arc;

    const SCRIPT: &str = "#!/bin/sh\nexec >&2\necho no force pushes\nexit 1\n";

    fn listing() -> String {
        serde_json::json!([
            {"name": "pre-receive", "is_active": true, "content": SCRIPT},
            {"name": "update", "is_active": false, "content": ""},
            {"name": "post-receive", "is_active": false, "content": ""},
        ])
        .to_string()
    }

    /// The bug this exists to prevent, and the reason the group is named the way it is: a user
    /// typing the obvious command and being told the subcommand does not exist, when what is
    /// actually true is that the operation does not mean what its name says. The refusal has to
    /// name `disable` and say the hook survives.
    #[test]
    fn delete_is_refused_with_the_truth_about_what_the_api_does() {
        let e = delete_is_not_what_it_sounds_like("pre-receive");
        assert_eq!(e.exit_code(), 2);
        let text = e.to_string();
        assert!(text.contains("gea git-hook disable pre-receive"), "{text}");
        assert!(text.contains("fixed hooks"), "{text}");
        assert!(text.contains("clears the script"), "{text}");
        for name in KNOWN_HOOKS {
            assert!(text.contains(name), "the known hooks should be named: {text}");
        }
    }

    /// Bug this prevents: rendering a hook through the shared `Table`, which replaces every
    /// newline with a space. `gea git-hook view pre-receive > hook.sh` would then write one
    /// unrunnable line, and nothing about the output would say it had been mangled.
    #[test]
    fn view_writes_the_script_verbatim_rather_than_through_a_table() {
        let mut buf: Vec<u8> = Vec::new();
        write_verbatim(&mut buf, SCRIPT).unwrap();
        assert_eq!(String::from_utf8(buf).unwrap(), SCRIPT);

        // ...and the table it is deliberately not going through would have flattened it.
        let mut table = Table::new(&crate::output::Term::piped());
        table.row([SCRIPT.to_owned()]);
        let flattened = table.render_to_string();
        assert!(!flattened.contains('\n') || flattened.lines().count() == 1);
        assert!(flattened.contains("exec >&2 echo no force pushes"), "{flattened:?}");
    }

    /// A script with no trailing newline still ends the line it was printed on, without gaining
    /// a byte the server does not hold.
    #[test]
    fn a_script_without_a_trailing_newline_gains_exactly_one() {
        let mut buf: Vec<u8> = Vec::new();
        write_verbatim(&mut buf, "echo hi").unwrap();
        assert_eq!(String::from_utf8(buf).unwrap(), "echo hi\n");
    }

    #[tokio::test]
    async fn list_says_which_hooks_are_active_and_how_big_each_script_is() {
        let fake = Arc::new(testing::on(
            FakeTransport::new(),
            "GET",
            "/api/v1/repos/acme/widget/hooks/git",
            Canned::json(200, listing()),
        ));
        let api = testing::api(fake.clone());
        let hooks = api.repo().list_git_hooks("acme", "widget").await.unwrap();
        assert_eq!(fake.calls().len(), 1);

        let human =
            testing::captured(&GlobalOpts::default(), None, &crate::output::Term::piped(), |e| {
                e.many(&hooks, None, "git hooks", |table, hooks| {
                    table.headers(["NAME", "ACTIVE", "SCRIPT"]);
                    for h in hooks {
                        table.row([h.name.clone(), active(h).to_owned(), summary(h)]);
                    }
                })
            });
        assert_eq!(
            human,
            "pre-receive\tactive\t4 lines\nupdate\tinactive\tempty\npost-receive\tinactive\tempty\n"
        );
    }

    /// An empty result set is exit 0 with nothing on stdout, per `docs/porcelain-conventions.md`.
    #[test]
    fn a_repository_with_no_hooks_is_success_not_an_error() {
        let human =
            testing::captured(&GlobalOpts::default(), None, &crate::output::Term::piped(), |e| {
                e.many::<GitHook>(&[], None, "git hooks", |_, _| unreachable!())
            });
        assert_eq!(human, "");
    }

    /// Bug this prevents: `edit` sending the script somewhere other than the body's `content`
    /// field — as a query parameter, say — which Gitea answers by quietly storing nothing.
    #[tokio::test]
    async fn edit_sends_the_script_as_the_body_content_field() {
        let fake = Arc::new(testing::on(
            FakeTransport::new(),
            "PATCH",
            "/api/v1/repos/acme/widget/hooks/git/pre-receive",
            Canned::json(
                200,
                serde_json::json!({"name":"pre-receive","is_active":true,"content":SCRIPT})
                    .to_string(),
            ),
        ));
        let api = testing::api(fake.clone());
        let body = EditGitHookOption { content: Some(SCRIPT.to_owned()) };
        let hook = api.repo().edit_git_hook("acme", "widget", "pre-receive", &body).await.unwrap();

        let sent: serde_json::Value = serde_json::from_str(&fake.calls()[0].body_str()).unwrap();
        assert_eq!(sent, serde_json::json!({ "content": SCRIPT }));
        assert!(hook.is_active);
    }

    /// Bug this prevents: `disable` going to some invented route, or being sent as a PATCH with
    /// an empty string — which works, but is not the operation the API documents.
    #[tokio::test]
    async fn disable_uses_the_delete_route_which_only_empties_the_script() {
        let fake = Arc::new(testing::on(
            FakeTransport::new(),
            "DELETE",
            "/api/v1/repos/acme/widget/hooks/git/pre-receive",
            testing::empty(),
        ));
        let api = testing::api(fake.clone());
        api.repo().delete_git_hook("acme", "widget", "pre-receive").await.unwrap();
        assert_eq!(fake.calls().len(), 1);
        assert_eq!(fake.calls()[0].method, "DELETE");
        assert_eq!(fake.calls()[0].path, "/api/v1/repos/acme/widget/hooks/git/pre-receive");
    }

    #[test]
    fn a_hook_name_is_sent_as_typed_so_a_newer_gitea_stays_reachable() {
        // No allowlist: `proc-receive` is not in KNOWN_HOOKS and must still be expressible.
        assert!(!KNOWN_HOOKS.contains(&"proc-receive"));
        let e = delete_is_not_what_it_sounds_like("proc-receive");
        assert!(e.to_string().contains("gea git-hook disable proc-receive"), "{e}");
    }

    #[test]
    fn body_and_body_file_are_read_before_anything_else_and_dash_means_stdin() {
        let args = Edit {
            hook: "update".into(),
            body: Some("echo hi\n".into()),
            body_file: None,
            editor: false,
        };
        assert_eq!(
            Source::read(&args, &mut std::io::empty()).unwrap(),
            Source::Text("echo hi\n".to_owned())
        );

        let args =
            Edit { hook: "update".into(), body: None, body_file: Some("-".into()), editor: false };
        let mut stdin = SCRIPT.as_bytes();
        assert_eq!(
            Source::read(&args, &mut stdin).unwrap(),
            Source::Text(SCRIPT.to_owned()),
            "`-F -` must read stdin, as it does everywhere else in the tool"
        );
    }

    /// Bug this prevents: `gea git-hook edit update` with no script spending a `GET` before
    /// discovering it has nothing to send. The refusal has to happen from the flags alone, and it
    /// has to name all three ways to supply a script.
    #[test]
    fn edit_with_no_script_fails_from_the_flags_alone_and_names_them() {
        // Under `cargo test` neither stream is a terminal, so the interactive fallback is off —
        // which is exactly the CI case this rule exists for.
        if terminals_attached() {
            return;
        }
        let args = Edit { hook: "update".into(), body: None, body_file: None, editor: false };
        let e = Source::read(&args, &mut std::io::empty()).unwrap_err();
        assert_eq!(e.exit_code(), 2);
        for flag in ["-b/--body", "-F/--body-file", "-e/--editor"] {
            assert!(e.to_string().contains(flag), "{e}");
        }

        // `-e` in a pipe says why `-e` could not work, rather than offering itself as the fix.
        let args = Edit { hook: "update".into(), body: None, body_file: None, editor: true };
        let e = Source::read(&args, &mut std::io::empty()).unwrap_err();
        assert!(e.to_string().contains("needs a terminal"), "{e}");
        assert!(!e.to_string().contains("or -e/--editor"), "{e}");
    }

    /// Bug this prevents: an editor seeded with an empty buffer for an inactive hook, so the
    /// script the user writes has no shebang and the server never runs it.
    #[test]
    fn the_editor_is_seeded_with_the_current_script_or_a_shebang() {
        let live =
            GitHook { name: "pre-receive".into(), is_active: true, content: SCRIPT.to_owned() };
        assert_eq!(seed(&live), SCRIPT);

        let empty = GitHook { name: "update".into(), is_active: false, content: String::new() };
        assert_eq!(seed(&empty), "#!/bin/sh\n");
    }

    #[test]
    fn a_script_summary_counts_lines_and_names_an_empty_one() {
        let hook = |content: &str| GitHook {
            name: "update".into(),
            is_active: !content.is_empty(),
            content: content.to_owned(),
        };
        assert_eq!(summary(&hook("")), "empty");
        assert_eq!(summary(&hook("echo hi")), "1 line");
        assert_eq!(summary(&hook(SCRIPT)), "4 lines");
        assert_eq!(active(&hook("")), "inactive");
        assert_eq!(active(&hook(SCRIPT)), "active");
    }

    /// Every subcommand has to be reachable and clap-consistent. `crates/gea/tests/
    /// porcelain_cli.rs` walks the whole tree, but that is another wave's file; this keeps the
    /// group honest on its own.
    #[test]
    fn the_group_parses_and_survives_claps_consistency_checks() {
        use clap::{CommandFactory, Parser};

        #[derive(Parser)]
        #[command(name = "git-hook")]
        struct Root {
            #[command(flatten)]
            _args: Args,
        }

        Root::command().debug_assert();

        for argv in [
            &["git-hook", "list"][..],
            &["git-hook", "view", "pre-receive"][..],
            &["git-hook", "edit", "update", "-b", "echo hi"][..],
            &["git-hook", "edit", "update", "-F", "-"][..],
            &["git-hook", "edit", "update", "-e"][..],
            &["git-hook", "disable", "post-receive", "--yes"][..],
            // Hidden, but it must parse — answering it is the whole point.
            &["git-hook", "delete", "pre-receive"][..],
        ] {
            assert!(
                Root::try_parse_from(argv).is_ok(),
                "{argv:?} must parse: {:?}",
                Root::try_parse_from(argv).err().map(|e| e.to_string())
            );
        }

        // Two sources for one script is a mistake, not a precedence puzzle.
        assert!(
            Root::try_parse_from(["git-hook", "edit", "update", "-b", "x", "-F", "f"]).is_err()
        );
        assert!(Root::try_parse_from(["git-hook", "edit", "update", "-e", "-F", "f"]).is_err());
    }
}
