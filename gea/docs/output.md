# Output

`gea` copies `gh`'s output contract deliberately, because that contract is what makes a CLI scriptable and because a great many people already have it in their fingers. This document records the contract, and — more importantly — the three places we **deliberately differ**, so those differences do not later look like bugs. Divergences that are about *flags* rather than about output live in [gh-differences.md](gh-differences.md).

## The pipeline

```
Value ─> project (--json: keep requested top-level keys)
       ─> jq   (--jq: jaq, in-process)
       ─> render (--template | JSON | Table/TSV)
```

Projection happens **before** jq, matching `gh`. So `--json state --jq '.[].number'` yields nulls: `number` was projected away before jq ran. That is surprising exactly once, and it is `gh`'s behaviour, so we keep it.

## Terminal versus pipe

| | terminal | piped |
| --- | --- | --- |
| Table | space-padded columns, dim underlined header, `Showing N of M` banner | **TAB-separated, no header, no padding, no truncation, empty cells preserved** |
| JSON | pretty-printed | compact |
| Colour | on unless disabled | off unless forced |
| Pager | used | never |

The piped form is a stable interface. `gea pr list | cut -f2` works, and empty cells stay empty rather than collapsing, so column positions never shift.

Environment:

- `NO_COLOR` (any non-empty value) or `CLICOLOR=0` disables colour.
- `CLICOLOR_FORCE` (set and not `0`) forces it even when piped.
- `GEA_FORCE_TTY`: an integer is an absolute width; a value ending in `%` is a percentage of the real width; any other non-empty value forces terminal behaviour with no width override.
- `GEA_PAGER` → `PAGER` → `gea config get pager`. Skipped when not a terminal, when empty, or when the command is literally `cat`. `LESS=FRX` is injected if unset.
- `GEA_NO_COMPAT_NOTES=1` silences the "your server sent a value this build does not know" note.

## Divergence 1: field names are snake_case

`gh` uses camelCase (`headRefName`, `isDraft`). We use snake_case (`head_branch`, `draft`).

This looks like we broke compatibility, and it is worth being precise about why we did not. `gh`'s camelCase is not a style choice — it is fidelity to GitHub's **GraphQL** schema, which is camelCase. `gh`'s actual rule is *"field names are exactly the API's field names."* Gitea's REST API is snake_case. Applying `gh`'s rule to Gitea therefore yields snake_case; copying `gh`'s *output* instead would be cargo-culting.

Two concrete payoffs:

1. **One expression works at every layer.** These are the same filter, and all three work:
   ```bash
   gea api repos/{owner}/{repo}/pulls --jq '.[].head.ref'
   gea raw repo list-pull-requests o r --jq '.[].head.ref'
   gea pr list --json head --jq '.[].head.ref'
   ```
   Under camelCase, layer 1 passes the API through untouched (`head_repo`) while the other two would say `headRepo` — a permanent trap.
2. **Gitea's own API documentation is directly usable.** Every snippet a user copies from the Gitea docs or Swagger UI refers to snake_case names.

And a translation layer would have to be *bijective* for `--json` to round-trip. `html_url` → `htmlUrl` or `htmlURL`? `ssh_url`, `avatar_url`, `oid` all have contested camelizations. Zero translation means zero of those bugs.

There is deliberately **no `--json-case` flag**. If you need camelCase:

```bash
gea pr list --json number,head_branch \
  --jq 'map(with_entries(.key |= (split("_") | .[0] + (.[1:] | map(. | ascii_upcase[0:1] + .[1:]) | join(""))))) '
```

## Divergence 2: bare `--json` prints to stdout and exits 0

`gh pr list --json` prints the valid field names to **stderr** and exits **1**. `gea` prints them to **stdout** and exits **0**.

The reasoning: the user asked what fields exist and received a correct, complete answer. That is success, not failure. Treating it as an error makes the most natural discovery workflow impossible:

```bash
gea pr list --json | fzf --multi | paste -sd,     # pick fields interactively
gea pr list --json | grep -i url                  # what URL fields are there?
```

On a terminal the listing gains aligned type and description columns; piped, it is bare names, one per line, so the pipelines above work.

Field discovery also **short-circuits before any HTTP request**, so it needs neither authentication nor network. Asking what fields exist should not require a working token.

## Divergence 3: no `comfy-table`, and no box drawing

Output is space-padded columns with no borders, which is `gh`'s look. There is exactly one `Table` implementation, shared by the default human renderer and the `tablerender` template helper — two width algorithms would inevitably drift, and then `--template` output would not line up with default output for the same data.

Widths are computed with `unicode-width`, so CJK text and emoji align correctly rather than being counted as one column each.

## `--template`

Go `text/template` syntax, with `gh`'s helper functions: `tablerow`, `tablerender`, `timeago`, `timefmt`, `truncate`, `color`, `autocolor`, `join`, `pluck`, `hyperlink`.

```bash
gea pr list --json number,title,head_branch,updated_at --template \
  '{{range .}}{{tablerow (printf "#%v" .number | autocolor "green") .title .head_branch (timeago .updated_at)}}{{end}}'
```

`tablerender` flushes the buffered rows. **If you forget it, `gea` flushes automatically at the end of the template** — that omission is the single most common template mistake, and `gh` auto-flushes for the same reason.

A missing field evaluates to the empty string rather than erroring. Forgiving is correct here: a template is a display concern, and failing a whole command because one row lacked one field would be worse than printing a gap.

`timefmt` accepts a documented subset of Go layouts — `2006-01-02`, `2006-01-02 15:04:05`, `15:04`, and the names `RFC3339`, `RFC1123`, `Kitchen`, `DateOnly`, `TimeOnly`. Anything else is an error naming the supported set; full Go layout parsing is not attempted.

## `--jq`

`jaq` runs in-process, so **no `jq` binary is required**. Output rules match `jq`: a string result prints raw and unquoted, everything else prints as JSON, and multiple results print one per line. The filter is compiled once and reused across `--paginate` pages.

## Pagination

`--paginate` follows the collection to its last page; `--limit N` stops after N items in total, across all pages; `--slurp` (layer 1 only) wraps every page in one JSON array instead of printing each page's array in turn.

```bash
gea api 'repos/{owner}/{repo}/issues' --paginate --jq '.[].number'
gea api 'repos/{owner}/{repo}/issues' --paginate --slurp --jq 'length'
gea issue list --limit 500
```

Gitea's specification declares **no `Link` header anywhere**, which is the mechanism the live API actually paginates with, so pagination is discovered at runtime and every signal is treated as optional. Iteration stops on the first of six rules:

| | rule |
| --- | --- |
| a | a `Link` header was present but has no `rel="next"` — authoritative |
| b | the page came back empty |
| c | no `Link` header at all, and this page is shorter than the **observed first page** |
| d | `X-Total-Count` is known and we have that many |
| e | the user's `--limit` is reached |
| f | 10,000 pages — a hard ceiling, which **errors** |

Two of those are subtle enough to be worth stating, because each was a real bug:

**Rule (c) compares against the observed first page, never the requested limit.** Gitea clamps `limit` to `max_response_items` (default **50**) silently: no warning, no header, no error. Ask for 100, get 50. The intuitive rule — "a page shorter than what I asked for is the last page" — would see `50 < 100`, conclude it had everything, and stop. `--limit 100` on a repository with 300 issues would print 50 and exit 0, and nobody would notice until a script quietly stopped after the first fifty.

**Rule (f) errors rather than truncating.** Rules (a)–(e) all require the server to cooperate — to send a header, to run out of items, or to honour `?page`. An instance that ignores `?page` and re-serves page 1 forever, behind a proxy that strips both headers, satisfies none of them and the walk never ends. The ceiling exists to stop that of our own accord; it raises an error because quietly returning the first 10,000 pages would be exactly the silent data loss rules (a)–(e) exist to prevent, wearing a safety feature's clothes. If you legitimately need more than half a million items, that is what `--limit` is for, and the error says so.

## Exit codes

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

0–4 match `gh`. An empty list is success: `gea pr list --json number` on a repository with no open pull requests prints `[]` and exits 0, so `if gea pr list ...` tests reachability rather than emptiness.

`SIGPIPE` is restored to its default disposition at startup, so `gea pr list | head -1` exits 0 silently instead of panicking with "failed printing to stdout".
