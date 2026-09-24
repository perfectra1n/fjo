# Deliberate divergences from `gh`

`gea` is shaped like `gh` on purpose: transferable muscle memory is most of the value of copying a tool's UX. So every place the two differ is a decision, and every decision has a reason. They are collected here so that none of them later looks like a bug.

If you are coming from `tea` rather than `gh`, jump to [`-R` means something else in `tea`](#-r-means-something-else-in-tea).

## Summary

| Topic | `gh` | `gea` | Short reason |
| --- | --- | --- | --- |
| `--json` field case | `headRefName` | `head_branch` | `gh`'s rule is *the API's own names*; Gitea's REST API is snake_case |
| bare `--json` | stderr, exit 1 | stdout, exit 0, no request sent | It is a successful answer to a question |
| `-R/--repo` | `owner/name` | `owner/name` | Same as `gh`. `tea` uses `-R` for `--remote`; we do not |
| template repo flag | `--template` | `--as-template` / `--from-template` | `--template` is the global output formatter |
| `--title` short | `-t` | none | `-t` is `--template` |
| list cap | `-L/--limit` | `-L` short, `--limit` global | Same two spellings, different ownership |
| tables | padded columns | padded columns | Same, and deliberately no box drawing |
| exit codes | 0–4 | 0–4 identical, plus 5–8 | More failure modes deserve distinct codes |

Details below. Divergences 1–3 are also covered in [output.md](output.md), which owns the full output contract; this document adds the flag-level ones.

## 1. `--json` field names are snake_case

`gh` prints `headRefName`, `isDraft`, `createdAt`. `gea` prints `head_branch`, `draft`, `created_at`.

This looks like broken compatibility, and it is worth being precise about why it is not. `gh`'s camelCase is not a style choice — it is fidelity to GitHub's **GraphQL** schema, which is camelCase. `gh`'s actual rule is *"field names are exactly the API's field names."* Gitea's REST API is snake_case. Applying `gh`'s rule to Gitea yields snake_case; copying `gh`'s *output* instead would be cargo-culting the surface while abandoning the principle.

Two concrete payoffs:

**One expression works at every layer.** These are the same filter, and all three work:

```bash
gea api 'repos/{owner}/{repo}/pulls' --jq '.[].head.ref'
gea raw repo list-pull-requests myorg myrepo --jq '.[].head.ref'
gea pr list --json head --jq '.[].head.ref'
```

Under camelCase, layer 1 would pass the API through untouched (`head_repo`) while layers 2 and 3 said `headRepo`. That trap would be permanent and would fire in exactly the situation where someone is debugging.

**Gitea's own documentation is directly usable.** Every snippet copied from Gitea's docs or its Swagger UI refers to snake_case names.

And a translation layer would have to be **bijective** for `--json` to round-trip. Is `html_url` `htmlUrl` or `htmlURL`? `ssh_url`? `oid`? Every contested camelization is a bug waiting to be filed. Zero translation means zero of those bugs.

There is deliberately no `--json-case` flag. If you need camelCase, `--jq 'with_entries(...)'` is the documented route — see [output.md](output.md).

## 2. Bare `--json` prints to stdout and exits 0

`gh pr list --json` prints the valid field names to **stderr** and exits **1**. `gea` prints them to **stdout** and exits **0**.

The user asked what fields exist and received a correct, complete answer. That is success. And treating it as failure makes the most natural discovery workflow impossible:

```bash
gea pr list --json | fzf --multi | paste -sd,   # pick fields interactively
gea pr list --json | grep -i url                # which URL fields are there?
```

It also **short-circuits before the HTTP request**, so discovery needs neither authentication nor a network:

```console
$ gea pr list --json | head -4
additions
allow_maintainer_edit
assignee
assignees
```

On a terminal the listing gains aligned type and description columns; piped, it is bare names one per line, so the pipelines above work. An unknown field gets a Levenshtein-1 "did you mean" plus the full list, and exits 2.

## 3. `-R` means something else in `tea`

`gea` follows `gh`: `-R/--repo` takes `owner/name` (and also `host/owner/name` or a full URL).

```bash
gea pr list -R myorg/myrepo
gea pr list -R git.example.org/myorg/myrepo
gea pr list -R https://git.example.org/myorg/myrepo
```

`tea` spells `-R` as `--remote` and gives it a git remote *name*. Those two meanings are close enough to be dangerous: `gea pr list -R origin` is a repository named `origin` with no owner, not the `origin` remote.

So `--remote` is answered, not ignored. Typing it anywhere gets an explanation naming `-R owner/name` and `gea repo set-default`, instead of clap's generic "a similar argument exists: `--repo`". It is hidden, and it is deliberately **not** a global flag: every global is propagated into every subcommand, clap answers a duplicate long name with a *panic*, and `--remote` already exists with its real meaning on `pr create`, `repo create` (also `-r`) and `repo fork`. A global copy would crash all three. Instead there is a non-global copy on the root, which catches `gea --remote origin pr list`, plus a check that recognises clap's own unknown-argument refusal, which catches `gea pr list --remote origin` — where a `tea` user actually puts it. Neither path can reach a command that has a real `--remote`, because clap accepts it there and never errors. See `crates/gea/src/main.rs`.

`--hostname` **is** accepted as a hidden alias of `--host`, so `gh`'s spelling works.

The hidden `tea` command aliases exist too: `pull` → `pr`, `labels` → `label`, `ms` → `milestone`, `login` → `auth login`, `whoami` → `auth status`. They are hidden in the strict sense — they work, and they appear in no `--help`, no completion, and no `gea alias list`; `gh`'s names remain the only names the tool advertises. They live in one table, `gea::cmd::alias::BUILTIN`, and are applied by the same argv rewrite that expands your own aliases, which is why two of them can expand to two words. Your own alias of the same name wins:

```bash
gea alias set whoami 'api user --jq .login'   # replaces the built-in
```

## 4. Three flags renamed because a global owns the name

clap answers a duplicate long name with a **panic**, not an error. A command declaring a flag a global already owns would therefore crash on an ordinary command line instead of printing a usage message — which is how these were found, by a test that walks the whole command tree through clap's own consistency checks.

| Command | Spelling | Instead of | Because |
| --- | --- | --- | --- |
| `repo edit` | `--as-template` | `--template` | `-t/--template` is the global Go-template output formatter |
| `admin repo create` | `--as-template` | `--template` | same |
| `repo create` | `--from-template <owner/name>` | `--template` | same |

```bash
gea repo edit myorg/myrepo --as-template
gea repo create myorg/newrepo --from-template myorg/template-repo
```

The fix is deliberately a per-command rename rather than suppressing the global for that subtree: clap's `global` suppression is tree-wide, so one command declaring `--limit` would delete the global `--limit` everywhere else in the tool.

Three short flags are reserved by globals for the same reason:

- **no `-t` for `--title`** anywhere. `-t` is `--template`. `gea pr create --title x`, `gea issue create --title x`.
- **`-L` has no long form** on list commands. `gea pr list -L 50`. The long spelling is the global `--limit`, which means the same thing, so both work: `gea pr list --limit 50`.
- **`-f` has no long form** on `repo sync`. `gea repo sync -f` resets a diverged local branch; the global `--force` means "allow binary output to a terminal", which is a different thing.

## 5. `gea reaction add -1` is a reaction, not a flag

`-1` is one of the two most common values `gea reaction add` takes — Gitea's thumbs-down — and clap read it as a flag. It is parsed as a value.

```bash
gea reaction add --issue 42 +1
gea reaction add --issue 42 -1
```

## 6. No box-drawing, one table implementation

Output is space-padded columns with no borders, which is `gh`'s look. There is exactly one `Table` implementation, shared by the default human renderer and the `tablerender` template helper — two width algorithms would inevitably drift, and `--template` output would stop lining up with default output for the same data. Widths use `unicode-width`, so CJK and emoji align.

## 7. Exit codes 0–4 match `gh`; 5–8 are additions

| code | meaning |
| --- | --- |
| 0 | success — **including an empty result set** |
| 1 | generic failure |
| 2 | usage error |
| 4 | authentication or authorization required |
| 5 | not found |
| 6 | network or transport failure |
| 7 | server error (5xx) |
| 8 | rate limited |
| 130 | interrupted |

An empty list is success, exactly as in `gh`: `gea pr list --json number` on a repository with no open pull requests prints `[]` and exits 0, so `if gea pr list ...` tests reachability rather than emptiness.

## 8. Two things `gh` has no equivalent of

- **`gea raw`** — a complete, generated command for every one of the 482 API operations. `gh` has `gh api` and nothing between it and the porcelain. See [layers.md](layers.md).
- **`gea admin`** — instance administration. `gh` has no admin surface at all, because GitHub Enterprise administration is not in the API `gh` targets.

And several groups have no GitHub counterpart to copy from, so their shape is `gea`'s own following the same verbs: `times`, `stopwatch`, `wiki`, `mirror`, `package`, `transfer`, `nodeinfo`, `block`.
