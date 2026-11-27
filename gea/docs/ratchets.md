# Ratchets

A **ratchet** is a gate that measures one number about the codebase, fails when it grows, and nags when it shrinks. Every ratchet in this repo is a `mise` task; run them all with `mise run ratchets`, or one at a time by name. CI runs the same tasks, so there is exactly one definition of each gate.

```
mise run panic-check       # panicking constructs in shipping code
mise run exit-check        # std::process::exit outside main.rs
mise run support-check     # non-admin modules reaching into cmd/admin/support
mise run zero-check        # zero values written into omittable request-body fields
mise run complexity-check  # functions over the cognitive-complexity threshold
mise run budget-check      # startup latency and release binary size
mise run coverage-check    # commands never driven against a real Gitea (needs Docker)
```

`coverage-check` is the one ratchet that is not a text scan, and deliberately so — see [the design note](superpowers/specs/2026-09-16-command-coverage-design.md). The integration suite *skips and passes* when Docker is missing, so a scan of the test sources would report full coverage over a suite that executed nothing. It therefore counts journals the tests write **as they run**, and it is excluded from `mise run ratchets` (which must stay fast and Docker-free) and runs in CI's integration job instead.

## Why a count budget, and not a threshold

The obvious way to add a lint to a codebase that already violates it is to set the threshold at the worst offender and turn it on. That is worse than doing nothing.

Say the most complex function in the tree scores 47. Set `cognitive-complexity-threshold = 47` and the gate is green — and it now *permits* every new function up to 47. The rule has been inverted: a number that was meant to describe the debt has become a licence to write more of it, and the licence is generous precisely in proportion to how bad the worst existing code is.

A count budget inverts that back. The threshold stays where it should be (25, the aspirational number), and what is frozen is the *count* of things above it:

> It is a count budget with an aspirational per-function threshold — not a permissive threshold sized to the worst offender — so a NEW overly-complex function fails immediately even while the legacy offenders are still being paid down.

The legacy offenders are visible, countable, and unblocking. A new offender fails on the commit that introduces it, which is the only moment at which fixing it is cheap.

## The two halves, and why the second one matters more

Every ratchet task here does two things, and skipping either one breaks it:

1. **Over budget fails, and the message says how to fix the code — never how to raise the budget.** A gate whose failure message explains how to silence it is a gate that will be silenced. Read any of the `error:` blocks in `.mise/config.toml`: they name the construct to use instead (`return an ErrorKind`, `move the shared plumbing up a level`, `restore the lazy path`).

2. **Under budget prints a note telling you to ratchet the budget down to the new count.** This is the half that stops a ratchet from rotting. Without it a budget only ever describes the codebase on the day it was written: somebody removes four unwraps, nobody lowers the number, and four new unwraps can now be added for free. The gate is still green, still running, and no longer gating anything. The note is how the budget follows the code down.

So the drain loop is: fix something, run the check, read the note, lower the `BUDGET=` line in `.mise/config.toml` in the same commit. Every check takes a `--list` (or has a companion `mise run complexity`) that prints the actual offenders, which is where you start.

## No allowlist

None of these six ratchets has an exemption file, deliberately. An allowlist earns its place when an item can be *legitimately* non-compliant and a reviewer has to decide case by case — kopiur's inert-field ratchet is the example: a CRD field can be genuinely unread for a good reason, so each exemption carries a written justification.

A count budget does not have that shape. Nothing here is a permanent, defensible exception; every one of these numbers should reach zero (or, for the binary, reach the target). A list would just be a slower way of writing the same integer, with more surface to rot. If one is ever added, it must follow kopiur's rule that makes an allowlist self-draining: **an entry that becomes compliant must fail the check**, so the list cannot outlive the work.

---

## The six ratchets

### `panic-check` — panicking constructs in shipping code

Counts `unwrap()`, `expect(`, `panic!`, `todo!` and `unimplemented!` outside `generated/`, outside `tests/`, outside `#[cfg(test)]`, and outside the `gea-itest` crate — which is `publish = false` and is, by its own doc comment, "Harness for tests that run against a real Gitea". An unwrap there fails a test loudly, which is the behaviour you want. That carve-out is named explicitly in `.mise/scan.py` (`TEST_CRATES`) rather than being a silent skip, and only this check honours it.

`gitea-core`'s `FakeTransport` is *not* carved out, even though it is also a test double: it lives in a published crate and is compiled into shipping builds, so its seven `expect(`s really are in the artifact. Feature-gating it is a real drain for this ratchet.

`gea`'s entire value proposition is its error taxonomy: one renderer, a three-part shape (what failed / why / what to do), and a test asserting that **every** `ErrorKind` variant's rendering contains a "what to do" section with a literal runnable command. A panic bypasses all of it and hands the user a Rust backtrace — the one output shape the taxonomy promises never to emit. Every panic is a hole punched straight through the feature the tool is for.

This is not a hypothetical. Commit `0c2fcc7` shipped three clap panics, because clap answers a duplicate long name with a panic rather than an error: a one-word typo in layer 2's command table turns into a backtrace on *every* invocation of the binary, including `--help`.

To drain: `python3 .mise/scan.py panic --list`, then return an `ErrorKind`. If the invariant really is unreachable, make it unrepresentable rather than asserting it at runtime.

### `exit-check` — `std::process::exit` outside `main.rs`

Counts calls anywhere but `crates/gea/src/main.rs`. **Currently at zero**, so this is a "stays drained" gate.

`main.rs` owns the process exit code because the `ErrorKind` exit-code table is a public contract that scripts are invited to depend on: `0`/`1`/`2`/`4` matching `gh`, plus `5` not-found, `6` network, `7` server, `8` rate-limited, `130` SIGINT. A `process::exit` somewhere else hard-codes a number *beside* that table, so the code a calling script observes stops being the one the taxonomy documents. It also never unwinds, so it skips the pager flush and the grouped unknown-field note on the way out.

It reached zero the hard way. `crates/gea/src/cmd/pr/checks.rs` (`Verdict::Failed`) was the last such call — its own doc comment flagged it as the one thing not yet on the taxonomy — and it is now an `ErrorKind`, so the exit code for a failed check run is decided in the same table as every other exit code.

### `support-check` — non-admin modules reaching into `cmd/admin/support`

Counts files outside `crates/gea/src/cmd/admin/` that import `crate::cmd::admin::support`. **Currently at zero**, so this is a "stays drained" gate.

`support` is general-purpose output plumbing (`Emit`/`Json` and the test helpers) that happens to live inside the admin command group. Every module that reaches into it writes an edge into the dependency graph asserting that *topics depend on administration*, that *webhooks depend on administration*, that *reactions depend on administration* — none of which is true, and all of which a reader of the module tree has to un-learn.

The cause was mechanical rather than a design choice: `cmd/mod.rs` was frozen while six agents worked in parallel, so nobody could add a shared module without colliding, and the nearest already-existing home won. Eleven modules ended up reaching in — `topic`, `block`, `transfer`, `webhook`, `deploy_key`, `reaction` among them.

The plumbing now lives at `crates/gea/src/cmd/support/` where it belongs, and the count is zero. The gate stays because the cause will recur: the next time a shared file is contended, the nearest already-existing home will look attractive again, and this is what says no.

### `zero-check` — zero values written into omittable request-body fields

Counts places where a request-body field whose `Option` exists to be *omitted* is handed a value that means "the user said nothing". **Currently at zero**, so this is a "stays drained" gate.

A generated request-body model spells a not-required field `Option<T>` with `skip_serializing_if = "Option::is_none"`, so a field nobody set is left out of the JSON. That is not a style choice — it is the fix for a data-loss defect (`b62c3d1`), where the uniform `DefaultPlain` presence policy meant every PATCH was a full overwrite that blanked whatever the user had not mentioned. A call site that writes `Some(x.unwrap_or_default())` hands the model the exact zero value the `Option` existed to omit, and the overwrite comes straight back.

The live symptom was `gea repo fork` answering `500 name is empty`, because the call site turned "no `--fork-name`" into `Some("")`. It was 14 files and roughly 40 call sites.

**No test caught it, and no test could have.** `FakeTransport` accepts any body, so the only assertion that can fail is one on the *shape* of the request — `sent.get("name").is_none()` — and almost none exist. `repo fork` now has one (`an_unset_flag_is_omitted_from_the_body_rather_than_sent_as_an_empty_string`), but writing 485 of them is not a plan. This is a class the suite is structurally blind to, which is the case a ratchet is for.

#### Why a type-scoped scan, and not a grep, a clippy lint, or an allowlist

This was the design decision, so it is written down rather than implied.

**Not a grep.** `Some(...unwrap_or_default())` as a text pattern is worse than no gate. This tree contains `GlobalOpts { json: Some(String::new()) }`, `PageInfo`, `ResolveOptions`, `Some(Vec::new())` fixtures, and query clamps like `Some(i32::try_from(limit).unwrap_or(i32::MAX))` — none of them the bug. A gate that cries wolf is a gate the first person it inconveniences deletes, and then the real defect ships behind a disabled check.

**Not a clippy lint.** There is no such lint, and a custom one means `dylint`, a nightly driver and an install step, against a repo pinned to stable 1.98.1 whose scanner documents "a lint gate that needs its own install step is a lint gate people delete". The property being checked is a *serde attribute* on the field, which a lint would have to read off the HIR anyway — far more machinery than the question needs.

**Not an allowlist.** One was nearly required, and the thing that removed the need is worth recording. The bug has two shapes, and only one of them is ever ambiguous:

* a **fallback** — an absence collapsed into a zero (`unwrap_or_default()`, `unwrap_or("")`, `unwrap_or_else(String::new)`, an `if`/`match` with a zero arm). This is the bug for a field of any type, with no legitimate use.
* an **unconditional zero constant** — `Some(String::new())`, `Some(Vec::new())`, `Some(0)`. Whether this is a defect depends on the field: an explicit empty *list* on a create is ordinary API usage (a create that sends `Some(Vec::new())` for a list says so on purpose), and so is an explicit `Some(0)` where the zero is the value meant. An explicit empty *string*, though, is precisely the overwrite-with-blank that caused the data loss.

So the check reads the field's inner type out of the model and lets that decide: a fallback counts for every field, an unconditional constant counts only for a `String` or a nested model. That is a rule, not an exemption, and it takes the count to zero with nothing to justify case by case. Had it not, the allowlist would have had to follow kopiur's self-draining rule — an entry that becomes compliant must FAIL the check — which is the only version that cannot rot.

#### What the scan actually looks at

The set of fields that can be wrong is **derived, not listed**: any struct in the workspace with `skip_serializing_if = "Option::is_none"` on an `Option<T>`, because that attribute *is* the contract being protected. 127 types qualify. Three consequences:

* Query structs carry no such attribute, so the entire `unwrap_or(i32::MAX)` clamp family drops out before a single expression is examined.
* The **hand-written** patch bodies qualify too — `IssuePatch`, `LabelPatch`, `MilestonePatch`, `ReleasePatch` in `cmd/issue/shared.rs` are sparse PATCH bodies with the identical contract, and they are where an overwrite would hurt most. Scoping to `gitea-model/src/generated/` would have missed them.
* When the spec moves, the set follows codegen instead of rotting in a literal.

Only a value in **result position** counts, so `Some(xs.map(|x| x.unwrap_or_default()).collect())` is not a hit: the zero is per-element, not the field. Both construction idioms are covered — the struct literal, and `patch.title = Some(...)` on a local whose type *resolves* to a body model, which is how the sparse patches are really built. The receiver's type is resolved rather than guessed, because matching on field name alone would flag `out.title`, `ctx.path` and `self.branch`.

Left alone, correctly: `unwrap_or_else(|| current.x.clone())` read-modify-write fallbacks in `org.rs`/`team.rs`/`webhook.rs`; `release.rs`'s name falling back to the tag; the generated wiki commit message; and a bare `Default::default()` on an `Option` field, which is `None` and therefore already right.

#### The self-test is part of the gate

`zero-check` runs `python3 .mise/scan.py selftest` before it counts anything, and fails if it fails. This is the only check here whose correctness is not obvious from reading it — the other three are a regex over scrubbed text, this one classifies Rust expressions — and a classifier nobody has seen fail is not known to work. The fixtures pin 28 expressions (every spelling the real bug was found in, and every look-alike in this tree that must not count), the assignment form with its two negatives, and two scrubbing proofs: a file whose only mention of the pattern is a `///` example, a `//` explanation and a `#[cfg(test)]` fixture must count **zero**, and *the same file with a real defect planted in it* must count one. Without the second half the first is just proof of a dead scan.

That pairing is not theoretical either. Three times in one session a text search in this repo matched prose *about* a defect and reported it as the defect: a doc comment mentioning `cmd::admin::support`, a comment containing `#[ignore]`, and a server message that existed only inside a comment.

To drain — if it ever leaves zero — `python3 .mise/scan.py zero --list`, then pass the `Option` straight through (`name: args.fork_name.clone()`), so an unset flag stays unset. If the field genuinely needs a value when the user gave none, compute a *meaningful* one — read the current value back and send that, the way `org edit` does.

### `complexity-check` — functions over the cognitive-complexity threshold

Counts distinct functions clippy reports above `cognitive-complexity-threshold = 25` (pinned in `clippy.toml`).

The threshold is pinned rather than left implicit so the ratchet is stable across toolchain bumps: if clippy retunes its default, an unpinned threshold silently moves the baseline and the gate either goes red for nobody's change or quietly stops gating. The lint is allow-by-default, so the ordinary `mise run clippy` gate is unaffected either way.

`mise run complexity` prints the offenders with file and line; the repo idiom for fixing one is extracting pure, unit-testable helpers rather than reaching for `#[allow]`.

### `budget-check` — startup latency and binary size

Three numbers, all measuring the same design decision.

Layer 2 binds ~482 operations through clap's **builder** API over a `const` table, constructing each subtree lazily, precisely because clap's derive path would build all ~3,000 args on every invocation — including `gea --version`, which needs none of them. That decision is invisible in the source: nothing stops someone reintroducing an eager build, and nothing would fail except the clock.

* `gea --version` — the no-subtree case.
* `gea raw repo create-pull-request --help` — the one-subtree case. Comparing the two separates "startup got slower" from "subtree construction got slower".
* stripped release binary size — the third face of the same decision: ~61k generated lines with zero type generics and no macros-with-logic, so nothing monomorphises per call site.

The two latency budgets are the plan's stated contract (25 ms and 40 ms), not numbers fitted to a developer laptop, where they measure ~1.0 ms and ~0.6 ms. The headroom is roughly 20x, so the ratchet-down note fires on every green run: it is asking for a tightening that needs CI variance data to pick safely, because a budget fitted to a fast idle machine is how a gate goes red for the first person whose runner is busy.

Binary size was the opposite case and was real debt. The 15 MB figure had **never been met**: the stripped binary measured 18.35 MB, and the CI budgets job that asserted 15 MB was therefore red on `main` before anybody noticed. `SIZE_BUDGET` was set at 18 MiB, just above where the code actually was, with 15 MB named as the aspiration — because a gate that is red on arrival gets disabled by the first person it inconveniences.

It has since been drained, and how is worth recording, because the answer was not where the budget's own error message points. **The generated code was never the problem.** Attributing `.text` by symbol on an unstripped build:

| | `.text` |
| --- | --- |
| `gitea_client` — 54k generated lines, 482 operations | 89,856 |
| `gitea_model` — 8k generated lines, 233 structs | 153,332 |
| `gea::cmd::*` — hand-written porcelain bodies | 1,130,648 |
| clap **derive** glue for that porcelain (`augment_subcommands` et al) | 1,054,228 |

Layer 2 binds 482 operations for under 90 KB. Layer 3 binds 231 through clap's derive macro and spends more than a megabyte on the generated `augment_*` functions alone — a single one, `<gea::cmd::Porcelain as Subcommand>::augment_subcommands`, is 135,644 bytes. The "zero generics, zero macros-with-logic" rule held exactly where it was written down: the 17 `impl<...>` blocks in the generated client are lifetime-only and do not monomorphise, and there are no `macro_rules!` under either generated root. The rule simply was never applied to the hand-written half.

What actually drained it was the release profile, measured lever by lever rather than assumed. From 19,068,392 bytes:

| lever | delta |
| --- | --- |
| `lto = "fat"` (was `"thin"`) | −1,225,488 |
| `opt-level = "s"` (was the default 3) | −5,643,648 |
| `codegen-units = 1` (already set; vs the default 16) | −1,344,272 |
| `panic = "abort"` (already set; vs `"unwind"`) | −2,127,744 |

That lands at **12,199,256 bytes**, so the 15 MB aspiration is met with ~2.8 MB of headroom and `SIZE_BUDGET` follows the binary down. `opt-level = "z"` was measured too — 10,774,104 bytes, another 1.4 MB — and rejected: it costs ~29% on the heaviest CPU-bound path against ~8% for `"s"`, and the budget was already met.

Both startup budgets were re-measured after every one of those changes, because an opt-level that trades startup for size would quietly undo the lazy-subtree design this gate exists to protect. Neither regressed: `--version` went 1.09 → 1.21 ms against 25 ms, and `raw ... --help` 0.56 → 0.58 ms against 40 ms.

The lesson for the next person draining this one: the gate's error message says to check what the change "pulled into `gitea-client`", and that advice was pointed at the wrong half of the codebase. If the binary grows again, attribute `.text` by symbol before believing any story about the cause.

---

## Implementation notes

`.mise/scan.py` is the scanner behind `panic-check`, `exit-check`, `support-check` and `zero-check`. It is the `gea` analogue of kopiur's `crates/xtask/src/scan.rs` and makes the same trade deliberately: it reads `.rs` files as **text** and never parses Rust. A parser is a dependency, a build-time cost, and a new way for a gate to fail on valid source. Text scanning can only be conservative, which is the property a ratchet needs — over-removal makes a count too low, never too high, so the gate can nag but cannot cry wolf.

It does three passes, matching `scan.rs`:

* **scrub** — strip comments and string/char literals, so a name appearing only in a doc comment or an error message is not mistaken for code. (`checks.rs` mentions `std::process::exit` twice in prose for every one time it calls it.)
* **strip `#[cfg(test)]`** — by brace matching, so fixtures do not count. It also resolves `#[cfg(test)] mod tests;` to the *file* it names: without that, `gitea-core/src/context/git/tests.rs` contributes 54 perfectly good test unwraps to the panic count.
* **walk** — `.rs` under `crates/`, skipping `generated/` (machine-written; the `codegen-check` drift guard owns that tree) and `tests/`.

`zero-check` adds one bit to the first pass. `scrub(keep_quotes=True)` blanks a string literal's *interior* but leaves its delimiters standing, so `"main"` becomes `"    "` while `""` stays `""` — the check has to tell `unwrap_or("")` from `unwrap_or("main")`, and this exposes exactly that one bit and nothing else. Prose inside a literal still cannot match anything, offsets and line numbers are unchanged, and the three older checks do not ask for it. It also runs its own walk for the field derivation, because that one must include `generated/` — the models are most of what defines the contract.

Where the scan is uncertain it undercounts rather than overcounts, which is the direction a ratchet is allowed to be wrong in. `_split_top` does not track angle brackets (`<` is also a comparison operator, and guessing there would invent syntax), so an initialiser containing a generic with a comma in it splits into fragments that simply fail to match. A missed hit is a gate that has not yet fired; an invented one is a gate that gets deleted.

Its only dependency is a stock `python3`. Keep it that way: a lint gate that needs its own install step is a lint gate people delete.
