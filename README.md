# fjo

A command-line interface for [Forgejo](https://forgejo.org), with commands similar to `gh`.

Use `fjo pr`, `fjo issue`, and `fjo repo` for common tasks. For other API operations, use `fjo raw` or `fjo api`. The generated `raw` commands cover all 506 operations in the bundled Forgejo 16.0.4 API specification, and 494 of them are driven against a real Forgejo by the integration suite.

## Install

```bash
cargo install --git https://github.com/perfectra1n/fjo --locked fjo
```

Requires Rust 1.95 or newer. The trailing `fjo` names the package to install: the workspace root is a virtual manifest, so cargo needs to be told which one. `--locked` builds against the committed `Cargo.lock` rather than re-resolving.

Prebuilt binaries for Linux, macOS, and Windows are attached to each [release](https://github.com/perfectra1n/fjo/releases), with shell completions included.

Then log in to your Forgejo server:

```bash
fjo auth login --host git.example.org
```

Run the commands below from a repository checkout, or pass `-R owner/repo` to select a repository.

```bash
fjo pr list
fjo pr create --fill                     # use the branch's commits for the title and body
fjo issue list --label bug --state all -L 50
fjo release create v0.1.0 ./dist/*        # create a release and upload its assets
fjo run watch 1234 --exit-status         # wait for an Actions run to finish
```

Use `fjo --help` to list command groups, or add `--help` to any command.

## Status

`fjo` is under active development. Coverage is measured rather than described:

| Plane | Surface | Covered |
| --- | --- | --- |
| Request contract (hermetic) | generated operations | 506 / 506 |
| Against a real Forgejo | generated operations | 494 / 495 |
| Against a real Forgejo | porcelain commands | 243 / 244 |

The contract plane checks that every generated command composes the request its metadata declares — method, path substitution, per-parameter encoding, query keys, body types. It runs in the ordinary test suite and needs no Docker.

The live plane boots a throwaway Forgejo 16.0.4 and drives real lifecycles against it. It is the only thing that can show the server disagreeing with its own published specification, and it regularly does.

Eleven operations are held unreachable in [`spec/live-coverage.toml`](spec/live-coverage.toml), each with a reason and what would unblock it: ten ActivityPub routes that need a second federating instance and HTTP-signed requests, and one Actions log route that needs a runner. The single remaining gap on each plane is one command with an open defect, not missing tests.

```bash
mise run test             # hermetic: contract plane included, no Docker
mise run itest            # the live plane (needs Docker)
mise run coverage-check   # both, then the ratchet that prints the table above
```

See [`crates/fjo-itest/tests/`](crates/fjo-itest/tests/) for the cases covered.

One known limitation: permission advice for `fjo api` infers the required token scope from the URL. It can differ from the scope recorded for the equivalent generated command.

## API access

### Common commands

Commands such as `fjo pr`, `fjo repo`, and `fjo issue` infer repository context, prompt for missing values, and format results as tables. Some combine several API requests. These hand-written commands are called *porcelain* in the contributor documentation.

### Generated commands: `fjo raw`

`fjo raw` provides typed flags for every operation in the bundled API specification. Each command's help shows its HTTP method and path. Path parameters can be positional arguments or flags.

```bash
fjo raw --help
fjo raw search pull request
fjo raw repo list-git-hooks myorg myrepo
fjo raw repo get-contents myorg myrepo src/main.rs
```

Use `--dry-run` to inspect a request without sending it:

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

`--body-file -` reads a JSON body from stdin. Individual field flags override values in that body. If an operation has its own `--repo` parameter, use `-R` for the global repository option.

### Direct requests: `fjo api`

Use `fjo api` for a specific path under `/api/v1`, including endpoints newer than the bundled specification. `{owner}`, `{repo}`, and `{branch}` use the resolved repository context.

```bash
fjo api version
fjo api user --jq .login
fjo api 'repos/{owner}/{repo}/pulls' --paginate --jq '.[].number'
fjo api -X POST -f title=hi 'repos/{owner}/{repo}/issues'
fjo api -i repos/myorg/myrepo             # include the status line and headers
```

`-f` sends string values. `-F` accepts JSON types or reads a file when the value starts with `@`; `@-` reads stdin.

## Output

Tables use aligned columns in a terminal and tab-separated values when piped. Piped tables have no headers or padding and preserve empty cells. Progress and warnings go to stderr.

```bash
fjo pr list
fjo pr list | cut -f2
fjo pr list --json number,title,head_branch
fjo pr list --json number --jq '.[].number'
fjo pr list --json                       # list available fields without a network request
fjo pr list --json number,title,updated_at \
  --template '{{range .}}{{tablerow .number .title (timeago .updated_at)}}{{end}}'
```

JSON field names match the API, usually in `snake_case`. `--jq` runs in-process; no separate `jq` installation is needed. `--template` supports Go-style templates and table helpers.

Use `--paginate` to fetch all pages or `--limit N` to cap the total number of items. See [Output](docs/output.md) for formatting rules, pagination, and exit codes.

## Differences from `gh`

| Option | `fjo` behavior |
| --- | --- |
| `--json` fields | API names such as `head_branch`, not `headRefName` |
| `--json` without fields | Lists available fields on stdout and exits successfully, without a request |
| `-R/--repo` | Selects `owner/repo`, not a Git remote name |
| `--template` | Formats output; use `repo edit --as-template` or `repo create --from-template` for template repositories |
| `--limit` | Caps result counts; use `quota rules create --bytes` for storage limits |

`-t` is reserved for output templates, so titles use `--title`. The local limit option on `pr list` is `-L`; `repo sync` uses `-f` to force a sync. See [Differences from gh](docs/gh-differences.md) for details.

## Accounts and authentication

You can configure multiple servers and multiple accounts per server:

```bash
fjo auth login --host codeberg.org             # paste a token
fjo auth login --host codeberg.org --web       # or log in through your browser
fjo auth status
fjo auth switch --host codeberg.org
fjo pr list --host git.example.org
```

Which server a command talks to is decided in this order: `--host` (or `$FJO_HOST`/`$FORGEJO_HOST`), then a host named inside `-R host/owner/name`, then the current checkout's git remote, and only then the `active` host. So `fjo pr list` inside a clone of `code.example/them/proj` talks to `code.example` no matter which host `fjo auth switch` selected last, and `fjo repo set-default` settles a checkout with several plausible remotes. `--debug` prints the host and the repository it was chosen for, so you can always see why.

`--web` uses Forgejo's own OAuth2 provider: your browser opens, you click Authorize, and no secret crosses the clipboard. The session renews itself and lapses after about 30 days, so CI should keep using a token, which does not expire. Over SSH, add `--no-browser`. See [OAuth login](docs/oauth.md).

Tokens are stored in the OS keyring when available. Without a keyring, use `FORGEJO_TOKEN` or explicitly choose file storage. Token files use `0600` permissions.

`fjo auth setup-git` registers a Git credential helper so Git can use your saved token. The helper ignores Git's credential-removal requests, so a rejected push does not remove your `fjo` login.

## Forgejo features

```bash
fjo pr create --agit --topic fix-typo
fjo times add 42 1h25m
fjo stopwatch start 42
fjo wiki list
fjo quota status
fjo quota rules create small --bytes 1GiB --subject size:all
fjo mirror add https://github.com/example/repo --interval 8h
fjo package list myorg --type cargo
fjo transfer start newowner
fjo admin user list
```

AGit creates a pull request by pushing to `refs/for/<branch>/<topic>`, without a fork or new branch. Push the same topic to update the request. Use `--force-push` after rewriting the commits.

Repository Git hooks are available through `fjo git-hook list`, `view`, `edit`, and `disable`. Disabling a hook clears its script; it does not remove the hook from Forgejo's fixed set.

## Errors

Errors include a description, relevant details, and suggested commands. Server error messages are preserved, with secrets redacted.

For repository-related 404 responses, `fjo` checks whether the repository is accessible before reporting a missing resource. If the repository itself is inaccessible, the error lists possible causes rather than assuming it was deleted.

## Limitations

- SSH `Host` aliases from `~/.ssh/config` are not resolved. Use `fjo repo set-default` to select the repository instead.
- No third-party extension commands or TUI.
- No translated messages.
- No persistent cache of GET responses. Instance capabilities are cached for the current process.
- `--sudo` is a global option, not a separate per-command option.

## Development

[mise](https://mise.jdx.dev) installs the pinned toolchain and tools from `.mise/config.toml`. Local checks and CI use the same tasks:

```bash
mise run build-release
mise run test             # unit and snapshot tests; no Docker required
mise run test-doc
mise run codegen-check    # check generated code against the bundled specification
mise run itest            # integration tests; requires Docker
mise run ci               # all CI checks
```

`mise tasks` lists available tasks. To run Cargo directly:

```bash
cargo build --release
cargo nextest run --workspace --locked
cargo test --workspace --doc --locked
cargo xtask codegen --check
cargo xtask itest
```

`rust-toolchain.toml` pins the Rust version for Cargo users. The nextest configuration excludes Docker integration tests from the default run; `cargo xtask itest` builds the CLI and runs them against a temporary Forgejo instance.

CI also checks code-quality counts, startup time, and binary size. When a count drops, lower its budget in the same change. See [Ratchets](docs/ratchets.md).

## Project layout

| Crate | Contents |
| --- | --- |
| `forgejo-core` | HTTP, authentication, pagination, errors, configuration, and Git context |
| `forgejo-model` | Generated API types and deserializers |
| `forgejo-client` | Generated client methods and metadata |
| `fjo-raw` | Generated-command CLI built from metadata |
| `fjo` | CLI commands and output formatting |
| `xtask` | Code generation and development tasks |
| `fjo-itest` | Integration tests against Forgejo |

`forgejo-core`, `forgejo-model`, and `forgejo-client` can also be used as a Rust SDK.

## Documentation

- [API layers](docs/layers.md)
- [OAuth login](docs/oauth.md)
- [Output and exit codes](docs/output.md)
- [Differences from gh](docs/gh-differences.md)
- [Command conventions](docs/porcelain-conventions.md)
- [CI budgets](docs/ratchets.md)
- [Contributing](CONTRIBUTING.md)

## Related tools

- [`forgejo-cli`](https://codeberg.org/forgejo-contrib/forgejo-cli) (`fj`): another Forgejo CLI.
- [`tea`](https://gitea.com/gitea/tea): Gitea's CLI, also usable with Forgejo.

## License

AGPL-3.0-only. See [LICENSE](LICENSE).
