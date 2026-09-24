# gea

A command-line interface for [Gitea](https://about.gitea.com), with commands similar to `gh`.

Use `gea pr`, `gea issue`, and `gea repo` for common tasks. For other API operations, use `gea raw` or `gea api`. The generated `raw` commands cover all 482 operations in the bundled Gitea 1.27.3 API specification, and 471 of them are driven against a real Gitea by the integration suite.

## Install

### Linux and macOS

```bash
curl -fsSL https://raw.githubusercontent.com/perfectra1n/gea/main/install.sh | sh
```

Installs to `~/.local/bin` (never needs `sudo`), verifies the published SHA-256, and installs shell completions. It probes your system to choose between the glibc and the statically linked musl build, so older distributions get a binary that actually runs. Set `GEA_INSTALL_DIR` to install elsewhere, or `GEA_VERSION` to pin a release.

### Windows

```powershell
irm https://raw.githubusercontent.com/perfectra1n/gea/main/install.ps1 | iex
```

### Container

```bash
docker run --rm -v "$PWD:/workspace" -e GEA_TOKEN ghcr.io/perfectra1n/gea pr list
```

Images are published to `ghcr.io/perfectra1n/gea` for `linux/amd64` and `linux/arm64`, tagged `latest`, `X.Y` and `X.Y.Z`. The image contains `git`, because `gea` reads the checkout to work out which server and repository a command is for. Authenticate with `GEA_TOKEN`: a container has no keyring for `gea auth login` to write to.

### Manual download

Archives for every supported target are attached to each [release](https://github.com/perfectra1n/gea/releases), each with a `.sha256` beside it and shell completions inside.

Pick `-gnu` on Ubuntu 22.04+, Debian 12+, or Fedora 36+. Pick the statically linked `-musl` on anything older, on Alpine, and on RHEL/Rocky/Alma 9 and Amazon Linux 2023 — those ship glibc 2.34, just below what the `-gnu` build needs. `install.sh` makes this choice for you.

### From source

```bash
cargo install --git https://github.com/perfectra1n/gea --locked gea
```

Requires Rust 1.95 or newer. The trailing `gea` names the package to install: the workspace root is a virtual manifest, so cargo needs to be told which one. `--locked` builds against the committed `Cargo.lock` rather than re-resolving.

Then log in to your Gitea server:

```bash
gea auth login --host git.example.org
```

Run the commands below from a repository checkout, or pass `-R owner/repo` to select a repository.

```bash
gea pr list
gea pr create --fill                     # use the branch's commits for the title and body
gea issue list --label bug --state all -L 50
gea release create v0.1.0 ./dist/*        # create a release and upload its assets
gea run watch 1234 --exit-status         # wait for an Actions run to finish
```

Use `gea --help` to list command groups, or add `--help` to any command.

## Status

`gea` is under active development. Coverage is measured rather than described:

| Plane | Surface | Covered |
| --- | --- | --- |
| Request contract (hermetic) | generated operations | 482 / 482 |
| Against a real Gitea | generated operations | 471 / 482 |
| Against a real Gitea | porcelain commands | 231 / 231 |

The contract plane checks that every generated command composes the request its metadata declares — method, path substitution, per-parameter encoding, query keys, body types. It runs in the ordinary test suite and needs no Docker.

The live plane boots a throwaway Gitea 1.27.3 and drives real lifecycles against it. It is the only thing that can show the server disagreeing with its own published specification, and it regularly does.

No operation is exempted in [`spec/live-coverage.toml`](spec/live-coverage.toml). Eleven are not yet driven live: six Actions reads and reruns that need a workflow run to have actually executed on a runner, and five cheap routes whose tests are simply not written yet. `.mise/config.toml` names each one, and the budget only goes down.

```bash
mise run test             # hermetic: contract plane included, no Docker
mise run itest            # the live plane (needs Docker)
mise run coverage-check   # both, then the ratchet that prints the table above
```

See [`crates/gea-itest/tests/`](crates/gea-itest/tests/) for the cases covered.

One known limitation: permission advice for `gea api` infers the required token scope from the URL. It can differ from the scope recorded for the equivalent generated command.

## API access

### Common commands

Commands such as `gea pr`, `gea repo`, and `gea issue` infer repository context, prompt for missing values, and format results as tables. Some combine several API requests. These hand-written commands are called *porcelain* in the contributor documentation.

### Generated commands: `gea raw`

`gea raw` provides typed flags for every operation in the bundled API specification. Each command's help shows its HTTP method and path. Path parameters can be positional arguments or flags.

```bash
gea raw --help
gea raw search pull request
gea raw repo list-git-hooks myorg myrepo
gea raw repo get-contents myorg myrepo src/main.rs
```

Use `--dry-run` to inspect a request without sending it:

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

`--body-file -` reads a JSON body from stdin. Individual field flags override values in that body. If an operation has its own `--repo` parameter, use `-R` for the global repository option.

### Direct requests: `gea api`

Use `gea api` for a specific path under `/api/v1`, including endpoints newer than the bundled specification. `{owner}`, `{repo}`, and `{branch}` use the resolved repository context.

```bash
gea api version
gea api user --jq .login
gea api 'repos/{owner}/{repo}/pulls' --paginate --jq '.[].number'
gea api -X POST -f title=hi 'repos/{owner}/{repo}/issues'
gea api -i repos/myorg/myrepo             # include the status line and headers
```

`-f` sends string values. `-F` accepts JSON types or reads a file when the value starts with `@`; `@-` reads stdin.

## Output

Tables use aligned columns in a terminal and tab-separated values when piped. Piped tables have no headers or padding and preserve empty cells. Progress and warnings go to stderr.

```bash
gea pr list
gea pr list | cut -f2
gea pr list --json number,title,head_branch
gea pr list --json number --jq '.[].number'
gea pr list --json                       # list available fields without a network request
gea pr list --json number,title,updated_at \
  --template '{{range .}}{{tablerow .number .title (timeago .updated_at)}}{{end}}'
```

JSON field names match the API, usually in `snake_case`. `--jq` runs in-process; no separate `jq` installation is needed. `--template` supports Go-style templates and table helpers.

Use `--paginate` to fetch all pages or `--limit N` to cap the total number of items. See [Output](docs/output.md) for formatting rules, pagination, and exit codes.

## Differences from `gh`

| Option | `gea` behavior |
| --- | --- |
| `--json` fields | API names such as `head_branch`, not `headRefName` |
| `--json` without fields | Lists available fields on stdout and exits successfully, without a request |
| `-R/--repo` | Selects `owner/repo`, not a Git remote name |
| `--template` | Formats output; use `repo edit --as-template` or `repo create --from-template` for template repositories |
| `--limit` | Caps result counts |

`-t` is reserved for output templates, so titles use `--title`. The local limit option on `pr list` is `-L`; `repo sync` uses `-f` to force a sync. See [Differences from gh](docs/gh-differences.md) for details.

## Accounts and authentication

You can configure multiple servers and multiple accounts per server:

```bash
gea auth login --host gitea.com               # paste a token
gea auth login --host gitea.com --web         # or log in through your browser
gea auth status
gea auth switch --host gitea.com
gea pr list --host git.example.org
```

Which server a command talks to is decided in this order: `--host` (or `$GEA_HOST`/`$GITEA_HOST`), then a host named inside `-R host/owner/name`, then the current checkout's git remote, and only then the `active` host. So `gea pr list` inside a clone of `code.example/them/proj` talks to `code.example` no matter which host `gea auth switch` selected last, and `gea repo set-default` settles a checkout with several plausible remotes. `--debug` prints the host and the repository it was chosen for, so you can always see why.

`--web` uses Gitea's own OAuth2 provider: your browser opens, you click Authorize, and no secret crosses the clipboard. The session renews itself and lapses after about 30 days, so CI should keep using a token, which does not expire. Over SSH, add `--no-browser`. See [OAuth login](docs/oauth.md).

Tokens are stored in the OS keyring when available. Without a keyring, use `GITEA_TOKEN` or explicitly choose file storage. Token files use `0600` permissions.

`gea auth setup-git` registers a Git credential helper so Git can use your saved token. The helper ignores Git's credential-removal requests, so a rejected push does not remove your `gea` login.

## Gitea features

```bash
gea pr create --agit --topic fix-typo
gea times add 42 1h25m
gea stopwatch start 42
gea wiki list
gea mirror add https://github.com/example/repo --interval 8h
gea package list myorg --type cargo
gea transfer start newowner
gea admin user list
```

AGit creates a pull request by pushing to `refs/for/<branch>/<topic>`, without a fork or new branch. Push the same topic to update the request. Use `--force-push` after rewriting the commits.

Repository Git hooks are available through `gea git-hook list`, `view`, `edit`, and `disable`. Disabling a hook clears its script; it does not remove the hook from Gitea's fixed set.

## Errors

Errors include a description, relevant details, and suggested commands. Server error messages are preserved, with secrets redacted.

For repository-related 404 responses, `gea` checks whether the repository is accessible before reporting a missing resource. If the repository itself is inaccessible, the error lists possible causes rather than assuming it was deleted.

## Limitations

- SSH `Host` aliases from `~/.ssh/config` are not resolved. Use `gea repo set-default` to select the repository instead.
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

`rust-toolchain.toml` pins the Rust version for Cargo users. The nextest configuration excludes Docker integration tests from the default run; `cargo xtask itest` builds the CLI and runs them against a temporary Gitea instance.

CI also checks code-quality counts, startup time, and binary size. When a count drops, lower its budget in the same change. See [Ratchets](docs/ratchets.md).

## Project layout

| Crate | Contents |
| --- | --- |
| `gitea-core` | HTTP, authentication, pagination, errors, configuration, and Git context |
| `gitea-model` | Generated API types and deserializers |
| `gitea-client` | Generated client methods and metadata |
| `gea-raw` | Generated-command CLI built from metadata |
| `gea` | CLI commands and output formatting |
| `xtask` | Code generation and development tasks |
| `gea-itest` | Integration tests against Gitea |

`gitea-core`, `gitea-model`, and `gitea-client` can also be used as a Rust SDK.

## Documentation

- [API layers](docs/layers.md)
- [OAuth login](docs/oauth.md)
- [Output and exit codes](docs/output.md)
- [Differences from gh](docs/gh-differences.md)
- [Command conventions](docs/porcelain-conventions.md)
- [CI budgets](docs/ratchets.md)
- [Contributing](CONTRIBUTING.md)

## Related tools

- [`tea`](https://gitea.com/gitea/tea): Gitea's official CLI.
- [`fjo`](https://github.com/perfectra1n/fjo): the Forgejo CLI `gea` was ported from, with the same architecture and command set.

## License

AGPL-3.0-only. See [LICENSE](LICENSE).
