# Porcelain conventions

Layer 3 is the hand-written, `gh`-shaped surface. Layer 2 (`fjo raw`) already reaches every endpoint, so layer 3 exists **only** to be nicer than layer 2 for things people do daily. A porcelain command that is merely a renamed `fjo raw` call is not worth its maintenance.

These rules exist so that commands written by different people at different times feel like one tool. Where a rule seems arbitrary, it is usually copying `gh` deliberately, because transferable muscle memory is the whole reason this project is shaped like `gh`.

## What earns a porcelain command

At least one of:

- **Context inference** — resolves the repository, the current branch, or the current user, so the user types less than the API requires.
- **Multi-call orchestration** — one command that is several API calls (`pr create --fill` reads commits; `repo fork --clone` forks then clones then renames remotes).
- **Interactive fallback** — prompts when a required value is missing and stdin is a terminal.
- **Human rendering** — a table or detail view materially better than pretty-printed JSON.

If a command does none of these, leave it to `fjo raw` and say so in the group's `--help`.

## Naming

- **Verbs match `gh`**: `list`, `view`, `create`, `edit`, `close`, `reopen`, `delete`, `comment`, `checkout`, `merge`, `diff`, `status`. Not `get`/`update`/`remove`.
- **Groups are singular**: `fjo pr`, `fjo issue`, `fjo label` — never `prs`, `issues`.
- `list` is plural in output but singular in the command name. `gh` does this; consistency beats grammar.
- Forgejo-only groups still follow the same verbs: `fjo times list`, `fjo wiki view`.
- **A verb must describe what the API does, not what its route is called.** Where the two differ, the route is the one that is wrong for a user. `fjo git-hook disable` exists rather than `delete` because `DELETE /repos/{o}/{r}/hooks/git/{id}` only empties the hook's script — the hook is one of a fixed set and survives. Copying the route's name there would be a silent lie about what just happened to a production repository.
- The built-in `tea` aliases are `pull` → `pr`, `labels` → `label`, `ms` → `milestone`, `login` → `auth login`, `whoami` → `auth status`. They are **hidden**: they work, and they appear in no `--help`, no completion script, and no `fjo alias list`. `gh`'s names are still the only names the tool advertises. They live in **one table**, `cmd::alias::BUILTIN`, applied by the same argv rewrite that expands user aliases — not scattered across the groups as clap aliases, which could not express the two that expand to two words. A user's own alias of the same name wins. Do not add more without a reason as concrete as `tea` muscle memory.

## Addressing by title, not id

`fjo project` follows a convention `fjo milestone` already set: **the user names things by title, never by numeric id.** Forgejo's project routes are all `/projects/{id}/{columnID}`, and nobody knows that a board called `Roadmap` is id 3 — the same reasoning that put `milestone_by_title` in `crates/fjo/src/cmd/issue/shared.rs` and is written out in full in `crates/fjo/src/cmd/milestone.rs`'s module doc.

The corollary `fjo milestone` established holds here too: an **ambiguous** title is refused, not guessed. Forgejo does not require titles to be unique — real testing found two milestones called `1.0` on the same repository — and `fjo milestone` answers that by listing the colliding ids and telling the user to rename or delete one; it will not guess which was meant. `fjo project` lists the matches the same way, and adds the one thing `fjo milestone` does not have: `--id` is accepted as a single escape hatch, so an ambiguous project or column can be pinned to one of the ids the error just listed instead of requiring a rename.

## Flags every command shares

Inherited from `GlobalOpts`; **do not redeclare them**:

`-R/--repo`, `--host`, `--login`, `--json`, `--jq`, `--template`, `--color`, `--sudo`, `--otp`, `--debug`, `--paginate`, `--limit`, `--output`, `--force`, `--no-retry`, `--max-retries`, `--insecure-skip-tls-verify`.

This is not a style rule. **clap answers a duplicate long name with a panic, not an error**, so a command redeclaring one of these crashes on an ordinary command line instead of printing usage. Pick a different name: `repo edit --as-template`, `repo create --from-template` and `quota rules create --bytes` all exist for exactly this reason, and [gh-differences.md](gh-differences.md) records why. Do not reach for clap's `global` suppression instead — it is tree-wide, so one command suppressing `--limit` deletes the global `--limit` everywhere else in the tool. `crates/fjo/tests/porcelain_cli.rs` walks the whole tree through clap's own consistency checks and is what catches this.

## Flags with fixed meanings

Copy these exactly. A flag that means something different here than in `gh` is worse than a missing flag, because it fails silently in a script someone adapted.

| Flag | Meaning |
| --- | --- |
| `-b/--body <text>` | Body text inline |
| `-F/--body-file <path>` | Body from a file; **`-` means stdin** |
| `-e/--editor` | Open `$EDITOR`; **first line is the title, the rest is the body** |
| `-w/--web` | Open in a browser instead of acting |
| `--title` | Title. **There is no `-t`** — `-t` is the global `--template` |
| `-l/--label` | Repeatable |
| `-a/--assignee` | Repeatable; `@me` means the authenticated user |
| `-m/--milestone` | By title, not id — resolve it for the user |
| `-s/--state` | `open` \| `closed` \| `all`, default `open` for list commands |
| `--owner <ORG>` | Organisation or user board instead of the repository board |
| `-L <n>` | Maximum items, default 30. **Short only** — the long form is the global `--limit`, which means the same thing |
| `--yes` | Skip a destructive confirmation |
| `--add-X` / `--remove-X` | Edit commands mutate; they never replace a whole set |

`--owner` exists because Forgejo exposes the same project routes twice: once under `/{owner}/{repo}/projects/...` for a repository board, and once under `/{owner}/-/projects/...` for an organisation or user board. Same verbs, one flag picks which. It does not collide with any global flag — `--owner` is not among `GlobalOpts`'s names.

`--add-label`/`--remove-label` rather than `--label` on `edit` is `gh`'s convention and it matters: replace-semantics on an edit silently discards labels someone else added.

## Interaction rules

- **Prompt only when stdin *and* stdout are both terminals** and prompting is not disabled (`FJO_PROMPT_DISABLED`, or `prompt = "disabled"` in config).
- When a required value is missing and prompting is unavailable, **error naming the flag** — never hang, never guess.
- Destructive actions confirm on a terminal and require `--yes` otherwise.
- `@me` is resolved for any user-valued flag.

## Output

Every command routes through `fjo::output`. Never `println!` a result directly.

- Provide a human renderer using the shared `Table` (`output::table`), and pass the field table from `forgejo_client::fields` so `--json` works and bare `--json` can list names.
- **Human output goes to stdout; progress and warnings go to stderr.** A `Showing N of M` banner is terminal-only.
- An empty list is **exit 0** with an empty table (or `[]` under `--json`), plus a one-line "no results" note on stderr for a terminal. Emptiness is not an error.
- Never invent field names. `--json` names are the API's own, snake_case — see [output.md](output.md) for why.

## Errors

- Report through `forgejo_core::Error`. Do not build strings and do not print your own diagnostics: `error::render` owns presentation, and it guarantees every error carries a "what to do" section.
- If a call fails in a way the taxonomy cannot express, **add a variant** rather than degrading to `Usage` or a bare message. The taxonomy is the product here.
- Never swallow a server message. `tea`'s worst UX bug is printing `failed to merge PR, is it still open?` for every refusal while discarding the actual reason.

## Repository context

Take a resolved `RepoContext` from `Runtime`. Never call `resolve_repo` yourself, and never parse a remote URL in a command — that logic lives in `forgejo_core::context` and has a seven-form test table behind it.

Commands that genuinely work without a repository (`fjo auth`, `fjo search`, `fjo api`, `fjo org list`) must not trigger resolution at all, so they work outside a checkout.

## Newtyped ids

Use `IssueIndex` for what users type as `#42`, and `IssueId`/`CommentId` for database ids. The API is inconsistent about which a path wants, and mixing them silently operates on a *different, real* issue. The newtypes make that a compile error; do not `.get()` your way around them.

## Tests

Per command, at minimum:

- A `FakeTransport` unit test for the request it builds — method, path, query, body.
- An `insta` snapshot of human output, and one of `--json` output.
- An integration test in `crates/fjo-itest` for anything with more than one API call.
- A test naming any bug the command's logic exists to prevent.

Golden rule for test names: `merges_with_squash_when_asked`, not `test_merge_2`.
