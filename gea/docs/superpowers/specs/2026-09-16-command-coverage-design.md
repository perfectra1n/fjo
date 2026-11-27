> **Inherited from fjo.** This is a design note from [fjo](https://github.com/perfectra1n/fjo), the Forgejo CLI that gea was ported from, kept verbatim for its reasoning. Every measurement in it was taken against Forgejo, not Gitea, and command and crate names are fjo's.

# Exhaustive command coverage for `fjo raw` and the porcelain

**Status:** accepted, 2026-09-16.

## The problem

`fjo` exposes 750 commands: 506 generated `fjo raw` operations and 244 porcelain leaves. `CONTRIBUTING.md` records the consequence in a sentence nobody could act on:

> **Verification is uneven.** Most porcelain groups have only been driven through `FakeTransport` and `insta` snapshots, never against a real server. A green `cargo test` is not evidence that the wire format is right.

That was true, unmeasured, and getting worse with every spec bump. A mock test of a generated command confirms our own reading of the specification; it cannot confirm the server's.

## What this adds

Two planes of coverage and one ratchet over both.

### Plane 1 — contract (hermetic, in `mise run test`)

`crates/fjo/tests/raw_contract.rs` loops over `forgejo_client::meta::OPS` and, for each of the 506 operations, synthesizes an argv from the operation's own `ParamMeta`/`BodyMeta`, binds it, and asserts the resulting `PlannedRequest` against that metadata: method, path substitution, per-parameter encoding, query placement under the *wire* name, and required body fields.

This is cheap because of a decision the codebase already made for unrelated reasons. `fjo_raw::bind` is a pure function — `crates/fjo-raw/src/bind.rs` turns `clap::ArgMatches` into a described request and stops, doing no I/O. The architecture built to keep 506 operations off the startup path is the same architecture that makes exhaustively testing them a sub-second in-process loop.

`crates/fjo/tests/porcelain_inventory.rs` does the layer-3 equivalent: it walks `fjo::cmd::Porcelain`, asserts every leaf renders help without panicking and keeps its global flags, and emits the leaf inventory the ratchet reads.

Coverage here is complete **by construction** — the test *is* the loop — so the ratchet treats any gap as a hard failure rather than budgeting it.

### Plane 2 — live (Docker, `mise run itest`)

Real lifecycles against a real Forgejo, one file per group under `crates/fjo-itest/tests/`. This is the only plane that can prove the server agrees with the vendored specification.

### The ratchet

`cargo xtask coverage-check`, driven by `mise run coverage-check`:

Measured on landing:

```
==> command coverage

  plane     surface       covered   total    gap   budget
  contract  raw ops           506     506      0        0
  live      raw ops           494     495      1        1
  live      porcelain         243     244      1        1

  11 operation(s) held unreachable by spec/live-coverage.toml
```

Both budgets are 1, and the same defect is behind both: `fjo user token create` always sends `"repositories": []` and Forgejo answers 400. It is deliberately **not** exempted — the route works, our command does not, and an exemption would hide a real bug behind a clean number. That budget drains to zero by fixing the bug, which is the only kind of debt a ratchet should carry.

Both halves of `docs/ratchets.md`: over budget fails with a message about writing the test and never about raising the number; under budget prints the note that stops a budget rotting.

## Why coverage is journalled at runtime rather than scanned from sources

The repository's other ratchets are text scans (`.mise/scan.py`). This one cannot be, for a reason specific to `fjo-itest`: `instance_or_skip!` makes a test **`return` early and pass** when no Forgejo is reachable, printing `SKIPPED`. A source scan cannot distinguish that from a test that ran, so on a machine without Docker it would report total coverage over a suite that executed nothing — reintroducing exactly the failure `cargo xtask itest` exists to prevent.

So `cover!` writes to `$FJO_COVERAGE_DIR/live-<pid>.jsonl` **at the moment it executes**, placed after `instance_or_skip!`. A skipped test, an `#[ignore]`d test, or a test whose body was commented out all record nothing. One file per process because `cargo test` compiles each file in `tests/` into its own binary and the supported runner runs six concurrently.

## The `cover!` contract

```rust
#[test]
fn adding_a_topic_makes_it_visible_to_a_fresh_list() {
    let inst = instance_or_skip!();                    // skip => nothing recorded
    let repo = TestRepo::create(inst, "topic");
    cover!(porcelain: ["topic add", "topic list"],
           hits: ["repoAddTopic", "repoListTopics"]);
    ...
}
```

- `raw:` names `operationId`s — the keys of `spec/name-lock.toml`.
- `porcelain:` names leaf paths as the user types them, e.g. `"admin user create"`.
- `hits:` lets a porcelain test pay for the endpoints underneath it.

Every id is validated against `spec/name-lock.toml` and the porcelain inventory. An id that names nothing is a **hard error**, not a silent zero — which is what makes `hits:` trustworthy without a second mechanism. An operation renamed by a spec bump fails the gate with a file and line rather than quietly crediting nothing.

Lists are bracketed because `macro_rules!` does not backtrack: `$($id:expr),+ , hits: [...]` would try to parse `hits` as an expression and fail pointing at the wrong token.

## Operations no container can drive

Per the decision on 2026-09-16, the answer is **harness capability, not exemption**. A second instance is in scope where federation needs one. `spec/live-coverage.toml` exists for anything that survives that, and each entry carries a `reason` and an `unblock`, modelled on `spec/name-lock.toml` — the point is the reviewable diff, not the exemption. Exempt operations leave the denominator rather than counting as covered, so the budget stays pure debt and its honest target is zero.

Groups investigated before writing this, and what they actually need:

| Group | Finding |
| --- | --- |
| Runners (24 ops) | Drivable: registration tokens are plain `GET`s, `register-*-runner` a `POST` |
| Packages (6) | Drivable: Forgejo's generic registry is a plain `PUT`, so a fixture is one upload |
| Artifacts (4) | `list`/`view` drivable; `download`/`delete` need a real Actions run |
| ActivityPub `GET` (6) | Drivable directly |
| ActivityPub inboxes (5) | The real blocker: a second instance and HTTP-signed payloads |
| `adminCronRun` | Drivable by choosing a side-effect-free task from `cron-list` |

## Scope note

`fjo api` and `fjo raw search` are layer-1 and layer-2 *meta* commands rather than porcelain leaves, so they are outside the inventory. They keep their existing coverage in `crates/fjo/tests/cli.rs` and `crates/fjo-itest/tests/errors.rs`.
