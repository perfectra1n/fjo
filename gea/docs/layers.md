# The four layers

`gea` exposes the Gitea REST API three times over, plus a fourth layer underneath all of them for the handful of things Gitea does not expose as an API at all. The three API layers are not redundancy — they are the mechanism that lets the tool claim complete coverage of that API while still having a small, opinionated set of everyday commands. Layer 0 exists because "complete coverage of the API" and "complete coverage of Gitea" are different claims, and Gitea's project boards are the proof: they have no REST API to cover.

```
gea <noun> <verb>        layer 3   hand-written porcelain   35 groups, 238 commands
gea raw <group> <op>     layer 2   generated                482 operations, all of them
gea api <path>           layer 1   generated from nothing   any path under /api/v1
gea web <path>           layer 0   web-root, cookie-authenticated     any route under /
```

Layers 1 and 2 are complete by construction, over the surface the vendored spec describes. Layer 3 is deliberately partial and always will be. Layer 0 is neither: there is no spec for it to be complete or partial against — see below.

## Why coverage is a property of the build

This is the story for layers 1 through 3 — everything downstream of the generator. Layer 0 has no generator to be downstream of; its own coverage story is next.

The generator (`cargo xtask codegen`) reads `spec/gitea-v1.27.3.json` — a de-templated, key-sorted copy of Gitea's Swagger 2.0 document, vendored at git tag `v1.27.3` — and emits four things:

| Emitter | Output | Feeds |
| --- | --- | --- |
| `models` | 239 types | the typed client and every renderer |
| `client` | 482 methods, one per operation | the SDK, and layer 3 |
| `meta` | `static OPS: &[OpMeta]` | layer 2's command tree |
| `fields` | per-model `FieldSpec` tables | `--json` projection and discovery |

`ops_is_a_bijection_with_the_specs_operation_ids`, in `crates/gitea-client/src/generated/meta/invariants.rs`, loads the same spec at test time and asserts a bijection between its `operationId`s and `OPS` — nothing in the spec missing from the table, nothing in the table absent from the spec. Coverage is therefore a test failure, not a judgement call, and it fails loudly the moment `cargo xtask update-spec` pulls in new endpoints.

Nobody writes 482 commands. Nobody has to.

## Layer 0 — `gea web <path>`

Gitea has features with no REST API at all. Projects — the per-repository and per-organisation kanban boards — are the one that forced this layer into existence: the OpenAPI spec Gitea serves has zero `project` paths on 1.27.3, and the web routes that actually back the board UI reject an API token outright, answering `303 → /user/login` instead of `401`. Layers 1 through 3 all descend from the vendored spec — a generated client, a generated command tree, and porcelain hand-written on top of both — so none of them can reach a route the spec never mentions.

`gea web <path>` requests the instance's web root instead of `/api/v1`: the same flags as `gea api` (`-X`, `-f`, `-F`, `--input`, `-H`, `-i`), the same `{owner}`/`{repo}` substitution, the same `--json`/`--jq`/`--template` pipeline, but authenticated with a session cookie rather than a token, because the web routes only accept the former.

This is a difference in kind, not degree, and it should be said plainly rather than softened: layers 1 through 3 are pinned to the vendored spec — the JSON file this repository vendors and diffs on every bump, named above. Layer 0 is pinned to Gitea's *source* at the same tag — undocumented, unversioned web routes that no spec describes and that Gitea is free to change in any release, including a patch release, without telling anyone. There is nothing to diff.

That pin is nevertheless enforced, which is what makes it acceptable rather than reckless: the live integration suite boots `docker.io/gitea/gitea:1.27.3`, the same version this layer is written against, so a Gitea release that changes a route or the board template turns the suite red before it reaches a user. This is the same mechanism in spirit as `ops_is_a_bijection_with_the_specs_operation_ids` enforcing the spec for layer 2 — coverage and correctness are test failures here too, not judgement calls — and the `coverage-check` ratchet refuses a new porcelain leaf, `gea project` included, that was never actually driven against a real server.

```bash
gea web POST AtvikSecurity/VulnCorp/projects/3/12/move --input cards.json
gea web GET  AtvikSecurity/VulnCorp/projects/3 > board.html
```

`gea web` never warns about server versions. It is the escape hatch below the escape hatch.

**Reach for it when** the vendored spec has nothing for the feature you need — check it first — because that is the only case this layer exists for.

When an upstream project API lands and `cargo xtask update-spec` generates its operations, `gea project` re-targets to layer 2 and its HTML parser is deleted. `gea web` and the session stay: they exist for every web-only surface Gitea has, not for this one.

## Layer 1 — `gea api`

The escape hatch, modelled on `gh api`.

```bash
gea api version
gea api user --jq .login
gea api 'repos/{owner}/{repo}/pulls' --paginate --jq '.[].number'
gea api -X POST -f title=hi -F draft=true 'repos/{owner}/{repo}/issues'
gea api -i repos/myorg/myrepo
```

- The endpoint is a path relative to `/api/v1`. A leading `/` is optional, and an `/api/v1` prefix is accepted and not doubled.
- `{owner}`, `{repo}` and `{branch}` are substituted from the resolved repository.
- `-f/--raw-field` sends strings; `-F/--field` sends JSON types and reads a file when the value starts with `@` (`@-` is stdin). They look inverted; they are `gh`'s, exactly.
- `--method` is inferred as `GET`, or `POST` when any field is supplied.
- `--input <file>` supplies the whole body, after which field flags become query parameters.
- `--paginate`, `--slurp`, `-i/--include`, `-H/--header`, `--silent`, `--verbose`.

**Reach for it when** the endpoint is newer than the vendored spec, you want the response untouched, or you are transcribing a `curl` out of Gitea's documentation.

## Layer 2 — `gea raw <group> <op>`

Every operation the specification describes, as a command, with typed flags derived from the same `Param` values the client's function signatures are derived from.

```bash
gea raw --help                               # 17 groups
gea raw repo --help                          # the operations in one group
gea raw search pull request                  # rank the metadata table by name, summary and path
```

The groups are `admin`, `artifact`, `git`, `issue`, `job`, `misc`, `notify`, `org`, `package`, `repo`, `run`, `settings`, `task`, `team`, `topic`, `user`, `workflow` — plus `gea raw search`.

Each operation's `--help` states the real HTTP method and path, the token scope, and the request body type:

```console
$ gea raw repo create-pull-request --help
Create a pull request

HTTP: POST /repos/{owner}/{repo}/pulls
token scope: write:repository
request body: CreatePullRequestOption (application/json, optional)

Usage: gea raw repo create-pull-request [OPTIONS] [OWNER] [REPO]
```

### Path parameters, positionally or as flags

Both, always. clap cannot make one `Arg` do both, so the generator emits two and merges them with precedence *flag > positional > repository context > error*. Giving both with different values is a specific error, not clap's baffling "unexpected argument".

```bash
gea raw repo list-branches myorg myrepo
gea raw repo list-branches --owner myorg --repo myrepo
```

When an operation's own path parameter is named `repo`, it takes the long `--repo` and the global repository flag is available as `-R` only. The `--help` for that command says so on both arguments.

### Bodies

Request body fields flatten to depth 1 as typed flags. Anything deeper, or an array of objects, is excluded from the flags and `--help` says so; supply it with `--body-file` and let flags override individual fields.

```bash
gea raw repo create-pull-request myorg myrepo --title t --head fix --base main
gea raw repo create-pull-request myorg myrepo --body-file ./pr.json --title "override"
echo '{"title":"t","head":"fix","base":"main"}' \
  | gea raw repo create-pull-request myorg myrepo --body-file -
```

### `--dry-run`

Assembles the request and prints it without sending. It needs no token and no network, which makes the whole generated layer inspectable offline:

```console
$ gea raw repo create-pull-request myorg myrepo \
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
gea raw repo get-contents myorg myrepo src/main.rs
```

**Reach for it when** the porcelain has no command for what you need. That is most of the API, by design.

## Layer 3 — the porcelain

35 groups, 238 commands, hand-written and shaped like `gh`.

A command belongs here only if it is *nicer* than layer 2 — which means at least one of:

- **context inference** — resolves the repository, the current branch, or the current user;
- **multi-call orchestration** — `pr create --fill` reads the branch's commits first; `repo fork --clone` forks, clones, and renames remotes;
- **interactive fallback** — prompts when a required value is missing and both streams are terminals;
- **human rendering** — a table or detail view materially better than pretty-printed JSON.

A porcelain command that is merely a renamed `gea raw` call is not worth its maintenance. The binding rules are in [porcelain-conventions.md](porcelain-conventions.md).

**Reach for it** first, for anything it covers.

## A missing porcelain command is never a missing capability

This is the property the layering exists to buy, and it is worth being concrete about, because the two most obvious gaps in the command list show the same property from two sides.

### `gea admin badge` — no porcelain, full capability

Gitea's API has three badge routes, and no porcelain has been written for them:

```bash
gea raw admin list-user-badges alice
gea raw admin add-user-badges alice --body-file badges.json
gea raw admin delete-user-badges alice --body-file badges.json
```

`gea raw search badge` finds all three. That is the designed failure mode: a missing convenience, with the capability already in your hands. (Forgejo, which `gea`'s sibling `fjo` targets, has no badge route at all — there the gap is in the API, and no layer can close it.)

### `gea git-hook` — a gap that cost nothing, and how it closed

Four operations exist in the spec — `repoListGitHooks`, `repoGetGitHook`, `repoEditGitHook`, `repoDeleteGitHook` — and for several waves no porcelain was written for them. All four were reachable the entire time:

```bash
gea raw repo list-git-hooks myorg myrepo
gea raw repo get-git-hook myorg myrepo pre-receive
gea raw repo edit-git-hook myorg myrepo pre-receive --content '#!/bin/sh
exit 0'
gea raw repo delete-git-hook myorg myrepo pre-receive
```

That is the designed failure mode: a missing convenience, with the capability already in your hands. Contrast it with a hand-written CLI, where "no command for it" and "no way to do it" are the same sentence.

Layer 3 has since caught up, and the shape of what it added is the argument for the layering:

```bash
gea git-hook list                            # which hooks are active, in the current repo
gea git-hook view pre-receive > hook.sh      # the script, verbatim — a table would flatten it
gea git-hook edit pre-receive -F hook.sh     # or -F - for stdin, or -e for $EDITOR
gea git-hook disable pre-receive
```

Every one of those is something layer 2 cannot do well: infer the repository, print a multi-line script without mangling it, read a script from a file or a pipe. And one of them is a *correction* — there is no `gea git-hook delete`, because the API's `DELETE` does not remove a hook. Gitea's git hooks are a fixed set (`pre-receive`, `update`, `post-receive`) that every repository always has; the route empties the script and leaves the hook listed as inactive. Layer 2 reports the route's own name, faithfully; layer 3 is where it gets a name that is true. `gea git-hook delete` is accepted, hidden, purely so it can say that.

## One `--jq` expression, three layers

Field names are the API's own snake_case at every layer, with no translation anywhere. That is what makes this true:

```bash
gea api 'repos/{owner}/{repo}/pulls' --jq '.[].head.ref'
gea raw repo list-pull-requests myorg myrepo --jq '.[].head.ref'
gea pr list --json head --jq '.[].head.ref'
```

Under `gh`'s camelCase, layer 1 would pass `head_repo` through untouched while layers 2 and 3 said `headRepo` — a permanent trap. See [gh-differences.md](gh-differences.md).
