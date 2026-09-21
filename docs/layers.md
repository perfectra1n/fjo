# The four layers

`fjo` exposes the Forgejo REST API three times over, plus a fourth layer underneath all of them for the handful of things Forgejo does not expose as an API at all. The three API layers are not redundancy — they are the mechanism that lets the tool claim complete coverage of that API while still having a small, opinionated set of everyday commands. Layer 0 exists because "complete coverage of the API" and "complete coverage of Forgejo" are different claims, and Forgejo's project boards are the proof: they have no REST API to cover.

```
fjo <noun> <verb>        layer 3   hand-written porcelain   35 groups, 238 commands
fjo raw <group> <op>     layer 2   generated                506 operations, all of them
fjo api <path>           layer 1   generated from nothing   any path under /api/v1
fjo web <path>           layer 0   web-root, cookie-authenticated     any route under /
```

Layers 1 and 2 are complete by construction, over the surface the vendored spec describes. Layer 3 is deliberately partial and always will be. Layer 0 is neither: there is no spec for it to be complete or partial against — see below.

## Why coverage is a property of the build

This is the story for layers 1 through 3 — everything downstream of the generator. Layer 0 has no generator to be downstream of; its own coverage story is next.

The generator (`cargo xtask codegen`) reads `spec/forgejo-v16.0.4.json` — a de-templated, key-sorted copy of Forgejo's Swagger 2.0 document, vendored at git tag `v16.0.4` — and emits four things:

| Emitter | Output | Feeds |
| --- | --- | --- |
| `models` | 246 types | the typed client and every renderer |
| `client` | 506 methods, one per operation | the SDK, and layer 3 |
| `meta` | `static OPS: &[OpMeta]` | layer 2's command tree |
| `fields` | per-model `FieldSpec` tables | `--json` projection and discovery |

`ops_is_a_bijection_with_the_specs_operation_ids`, in `crates/forgejo-client/src/generated/meta/invariants.rs`, loads the same spec at test time and asserts a bijection between its `operationId`s and `OPS` — nothing in the spec missing from the table, nothing in the table absent from the spec. Coverage is therefore a test failure, not a judgement call, and it fails loudly the moment `cargo xtask update-spec` pulls in new endpoints.

Nobody writes 506 commands. Nobody has to.

## Layer 0 — `fjo web <path>`

Forgejo has features with no REST API at all. Projects — the per-repository and per-organisation kanban boards — are the one that forced this layer into existence: the OpenAPI spec Forgejo serves has zero `project` paths on 16.0.5, and the web routes that actually back the board UI reject an API token outright, answering `303 → /user/login` instead of `401`. Layers 1 through 3 all descend from the vendored spec — a generated client, a generated command tree, and porcelain hand-written on top of both — so none of them can reach a route the spec never mentions.

`fjo web <path>` requests the instance's web root instead of `/api/v1`: the same flags as `fjo api` (`-X`, `-f`, `-F`, `--input`, `-H`, `-i`), the same `{owner}`/`{repo}` substitution, the same `--json`/`--jq`/`--template` pipeline, but authenticated with a session cookie rather than a token, because the web routes only accept the former.

This is a difference in kind, not degree, and it should be said plainly rather than softened: layers 1 through 3 are pinned to the vendored spec — the JSON file this repository vendors and diffs on every bump, named above. Layer 0 is pinned to Forgejo's *source* at the same tag — undocumented, unversioned web routes that no spec describes and that Forgejo is free to change in any release, including a patch release, without telling anyone. There is nothing to diff.

That pin is nevertheless enforced, which is what makes it acceptable rather than reckless: the live integration suite boots `codeberg.org/forgejo/forgejo:16.0.5`, the same version this layer is written against, so a Forgejo release that changes a route or the board template turns the suite red before it reaches a user. This is the same mechanism in spirit as `ops_is_a_bijection_with_the_specs_operation_ids` enforcing the spec for layer 2 — coverage and correctness are test failures here too, not judgement calls — and the `coverage-check` ratchet refuses a new porcelain leaf, `fjo project` included, that was never actually driven against a real server.

```bash
fjo web POST AtvikSecurity/VulnCorp/projects/3/12/move --input cards.json
fjo web GET  AtvikSecurity/VulnCorp/projects/3 > board.html
```

`fjo web` never warns about server versions. It is the escape hatch below the escape hatch.

**Reach for it when** the vendored spec has nothing for the feature you need — check it first — because that is the only case this layer exists for.

When the upstream project API lands (`forgejo` PR #9384) and `cargo xtask update-spec` generates its operations, `fjo project` re-targets to layer 2 and its HTML parser is deleted. `fjo web` and the session stay: they exist for every web-only surface Forgejo has, not for this one.

## Layer 1 — `fjo api`

The escape hatch, modelled on `gh api`.

```bash
fjo api version
fjo api user --jq .login
fjo api 'repos/{owner}/{repo}/pulls' --paginate --jq '.[].number'
fjo api -X POST -f title=hi -F draft=true 'repos/{owner}/{repo}/issues'
fjo api -i repos/myorg/myrepo
```

- The endpoint is a path relative to `/api/v1`. A leading `/` is optional, and an `/api/v1` prefix is accepted and not doubled.
- `{owner}`, `{repo}` and `{branch}` are substituted from the resolved repository.
- `-f/--raw-field` sends strings; `-F/--field` sends JSON types and reads a file when the value starts with `@` (`@-` is stdin). They look inverted; they are `gh`'s, exactly.
- `--method` is inferred as `GET`, or `POST` when any field is supplied.
- `--input <file>` supplies the whole body, after which field flags become query parameters.
- `--paginate`, `--slurp`, `-i/--include`, `-H/--header`, `--silent`, `--verbose`.

**Reach for it when** the endpoint is newer than the vendored spec, you want the response untouched, or you are transcribing a `curl` out of Forgejo's documentation.

## Layer 2 — `fjo raw <group> <op>`

Every operation the specification describes, as a command, with typed flags derived from the same `Param` values the client's function signatures are derived from.

```bash
fjo raw --help                               # 17 groups
fjo raw repo --help                          # the operations in one group
fjo raw search pull request                  # rank the metadata table by name, summary and path
```

The groups are `activitypub`, `admin`, `artifact`, `git`, `issue`, `misc`, `notify`, `org`, `package`, `repo`, `run`, `settings`, `task`, `team`, `topic`, `user`, `workflow` — plus `fjo raw search`.

Each operation's `--help` states the real HTTP method and path, the token scope, and the request body type:

```console
$ fjo raw repo create-pull-request --help
Create a pull request

HTTP: POST /repos/{owner}/{repo}/pulls
token scope: write:repository
request body: CreatePullRequestOption (application/json, optional)

Usage: fjo raw repo create-pull-request [OPTIONS] [OWNER] [REPO]
```

### Path parameters, positionally or as flags

Both, always. clap cannot make one `Arg` do both, so the generator emits two and merges them with precedence *flag > positional > repository context > error*. Giving both with different values is a specific error, not clap's baffling "unexpected argument".

```bash
fjo raw repo list-branches myorg myrepo
fjo raw repo list-branches --owner myorg --repo myrepo
```

When an operation's own path parameter is named `repo`, it takes the long `--repo` and the global repository flag is available as `-R` only. The `--help` for that command says so on both arguments.

### Bodies

Request body fields flatten to depth 1 as typed flags. Anything deeper, or an array of objects, is excluded from the flags and `--help` says so; supply it with `--body-file` and let flags override individual fields.

```bash
fjo raw repo create-pull-request myorg myrepo --title t --head fix --base main
fjo raw repo create-pull-request myorg myrepo --body-file ./pr.json --title "override"
echo '{"title":"t","head":"fix","base":"main"}' \
  | fjo raw repo create-pull-request myorg myrepo --body-file -
```

### `--dry-run`

Assembles the request and prints it without sending. It needs no token and no network, which makes the whole generated layer inspectable offline:

```console
$ fjo raw repo create-pull-request myorg myrepo \
    --title "Fix typo" --head fix --base main --dry-run
POST /repos/myorg/myrepo/pulls
content-type: application/json
{
  "base": "main",
  "head": "fix",
  "title": "Fix typo"
}
```

### Path encoding is per-parameter

Most path parameters are percent-encoded with the path-segment set. Parameters on a `PathLike` allowlist — `filepath`, `treePath`, `ref`, `path`, `filename` — preserve `/`, so this works rather than 404ing:

```bash
fjo raw repo get-contents myorg myrepo src/main.rs
```

**Reach for it when** the porcelain has no command for what you need. That is most of the API, by design.

## Layer 3 — the porcelain

35 groups, 238 commands, hand-written and shaped like `gh`.

A command belongs here only if it is *nicer* than layer 2 — which means at least one of:

- **context inference** — resolves the repository, the current branch, or the current user;
- **multi-call orchestration** — `pr create --fill` reads the branch's commits first; `repo fork --clone` forks, clones, and renames remotes;
- **interactive fallback** — prompts when a required value is missing and both streams are terminals;
- **human rendering** — a table or detail view materially better than pretty-printed JSON.

A porcelain command that is merely a renamed `fjo raw` call is not worth its maintenance. The binding rules are in [porcelain-conventions.md](porcelain-conventions.md).

**Reach for it** first, for anything it covers.

## A missing porcelain command is never a missing capability

This is the property the layering exists to buy, and it is worth being concrete about, because the two most obvious gaps in the command list are gaps for completely different reasons.

### `fjo admin badge` — the API has no badge route

`spec/forgejo-v16.0.4.json` contains the string `badge` zero times:

```console
$ grep -c badge spec/forgejo-v16.0.4.json
0
```

There is no endpoint, so there is nothing for any layer to call. `crates/fjo/src/cmd/admin/mod.rs` says so in prose. If a future Forgejo adds badge routes, `cargo xtask update-spec` makes them `fjo raw` commands on the day the spec is bumped, and `fjo raw search badge` will find them before any porcelain does.

### `fjo git-hook` — a gap that cost nothing, and how it closed

Four operations exist in the spec — `repoListGitHooks`, `repoGetGitHook`, `repoEditGitHook`, `repoDeleteGitHook` — and for several waves no porcelain was written for them. All four were reachable the entire time:

```bash
fjo raw repo list-git-hooks myorg myrepo
fjo raw repo get-git-hook myorg myrepo pre-receive
fjo raw repo edit-git-hook myorg myrepo pre-receive --content '#!/bin/sh
exit 0'
fjo raw repo delete-git-hook myorg myrepo pre-receive
```

That is the designed failure mode: a missing convenience, with the capability already in your hands. Contrast it with a hand-written CLI, where "no command for it" and "no way to do it" are the same sentence.

Layer 3 has since caught up, and the shape of what it added is the argument for the layering:

```bash
fjo git-hook list                            # which hooks are active, in the current repo
fjo git-hook view pre-receive > hook.sh      # the script, verbatim — a table would flatten it
fjo git-hook edit pre-receive -F hook.sh     # or -F - for stdin, or -e for $EDITOR
fjo git-hook disable pre-receive
```

Every one of those is something layer 2 cannot do well: infer the repository, print a multi-line script without mangling it, read a script from a file or a pipe. And one of them is a *correction* — there is no `fjo git-hook delete`, because the API's `DELETE` does not remove a hook. Forgejo's git hooks are a fixed set (`pre-receive`, `update`, `post-receive`) that every repository always has; the route empties the script and leaves the hook listed as inactive. Layer 2 reports the route's own name, faithfully; layer 3 is where it gets a name that is true. `fjo git-hook delete` is accepted, hidden, purely so it can say that.

## One `--jq` expression, three layers

Field names are the API's own snake_case at every layer, with no translation anywhere. That is what makes this true:

```bash
fjo api 'repos/{owner}/{repo}/pulls' --jq '.[].head.ref'
fjo raw repo list-pull-requests myorg myrepo --jq '.[].head.ref'
fjo pr list --json head --jq '.[].head.ref'
```

Under `gh`'s camelCase, layer 1 would pass `head_repo` through untouched while layers 2 and 3 said `headRepo` — a permanent trap. See [gh-differences.md](gh-differences.md).
