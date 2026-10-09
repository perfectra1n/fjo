# CI porcelain: `gea ci`, flexible run/job references, and `pr checks` hints

Status: approved (Approach 3), 2026-10-09.

## Problem

Measured against a real repository (AtvikSecurity/tyrfing on git.perfectra1n.com, ~4 workflows per
push), getting from "is CI red?" to "the failing log" is hard, for humans and for agents:

1. `run list` prints two numbers (`ID` 6442, `NUMBER` 3623). `run view 3623` is a bare 404 that
   never says "that is a run number".
2. A pasted run URL (`…/actions/runs/6442`) is rejected: "is not a number".
3. Gitea leaves `head_branch` **empty** for `pull_request` runs, so `run list --branch X` silently
   misses every PR run. Filtering by `--commit <sha>` works.
4. `pr checks` lists check names with no run or job ids, no summary, and no next command.
5. `--job` takes only a numeric job id; nobody knows those.
6. There is no "show me CI for where I am" entry point.

Users are both humans at a terminal and agents (Claude, `shepherd-ci`). So: no interactive pickers,
stable exit codes, `--json` via the existing field tables, and every human summary ends with the
exact next command to run.

## Design overview

Three layers sharing one core:

- **Core resolution** (new, shared): parse and resolve run references, job references and CI
  targets.
- **`gea run *`** (existing): accepts the flexible references. Stays the *precise addressing* layer.
  `<RUN>` stays required.
- **`gea ci`** (new): the *context* layer. Answers "what is CI doing for this PR / branch / commit /
  where I am", and goes from there to logs, watch and rerun without the user ever typing an id.
- **`gea pr checks`**: human output gains a summary and next-step hints. `--json` is unchanged.

The commit SHA is the join key throughout: a target resolves to a SHA, then runs are listed with the
server-side `head_sha` filter (the same one `run list --commit` uses), which works for
`pull_request` runs where `head_branch` is empty.

## 1. Core resolution

### 1.1 Run references (`crates/gea/src/cmd/run_ref.rs`, new)

`RunRef` replaces `RunId` as the type of every `<RUN>` positional in `gea run` (`view`, `rerun`,
`delete`, `logs`, `watch`, `jobs`, `artifacts`). `FromStr` (so clap's `value_parser!` works) accepts:

| Input | Meaning |
| --- | --- |
| `6442` | `Bare(6442)`: an id, with fallback (below) |
| `#3623` | `Number(3623)`: explicitly the per-repository run number |
| `https://host/o/r/actions/runs/6442`, optionally followed by `/jobs/2`, `/attempts/N`, or a trailing slash/query/fragment | `Url { run: 6442, job: Option<i64> }` (the job's id; see below). The run segment is treated like `Bare` (Gitea 1.27's `html_url` carries the id). The URL's owner/name are **not** used to switch repository (out of scope; `-R` does that), but if they differ from the resolved repository, print a stderr note. |

Anything else is a clap usage error. It names the accepted forms: "expected a run id (`6442`), a
run number (`#3623`), or a run URL".

Resolution: `async fn resolve_run(api: &Api, slug: &RepoSlug, r: &RunRef, term) -> Result<ActionWorkflowRun>`.

- `Bare(n)` / `Url`: `GET …/actions/runs/{n}`. On **404 only**, look for a run whose `run_number == n`
  (below). If one exists, use it and print **one stderr note**: `note: no run has id N; using run
  #N (id M)`. If not, return the original 404 error, extended with a hint naming both forms. Any
  non-404 error propagates unchanged.
- `Number(n)`: find the run whose `run_number == n`. Not found is `ResourceNotFound`, with a hint to
  use `gea run list`.
- Finding by number: there is no server filter for `run_number`. Page `GET …/actions/runs` (newest
  first) until a match is found, or the page's smallest `run_number` < n (runs are numbered
  monotonically per repository), or the pages run out. Cap the scan at 20 pages; past that, fail
  with a message suggesting the id.

The `i32` range guard (`int()` in run.rs) still applies after resolution.

### 1.2 Job references (same file)

`JobRef` replaces `JobId` for every `--job/-j` in `gea run` (and is used by `gea ci logs --job`).
`FromStr`: all digits → `Id(i64)`; anything else → `Name(String)`.

`fn resolve_job<'a>(jobs: &'a [ActionWorkflowJob], r: &JobRef) -> Result<&'a ActionWorkflowJob>`
(pure, no I/O):

- `Id(n)`: the job with that id. Not in this run is `ResourceNotFound`, listing the run's jobs as
  `name (id N)`.
- `Name(s)`: a case-insensitive **exact** name match wins. Otherwise a unique case-insensitive
  **substring** match. Zero matches is not-found (listing the jobs). Several matches is a usage
  error listing each candidate as `name (id N)` and saying "pass the id to choose one". Never guess.
- A run URL's `/jobs/<n>` segment is the job's **id** (`Url { job: Option<i64> }`), resolved like
  `--job <id>`; no such job is not-found. *Corrected after the live suite:* this section first said
  a 0-based position, but Gitea 1.27 links jobs by id (a run whose jobs are ids 3 and 4 links them
  as `jobs/3` and `jobs/4`). Commit statuses posted by Actions carry the same URL **path-only**
  (`/owner/name/actions/runs/<id>/jobs/<id>`), so the URL parser accepts that form too.

### 1.3 CI targets (`crates/gea/src/cmd/ci/target.rs`, new)

`Target`, from the optional positional of every `gea ci` subcommand:

| Input | Kind |
| --- | --- |
| omitted | `Here` |
| `42`, `#42`, a pull request URL | `Pr` (reuse `cmd::pr::common::find`'s selector handling where possible) |
| 7–40 hex chars containing at least one letter a–f | `Commit` (a full SHA is used as-is; a short one is expanded with local `git rev-parse` when inside a checkout, otherwise with `GET /repos/{o}/{r}/git/commits/{sha}`) |
| anything else | `Branch` |

`async fn resolve_target(rt, globals, api, Option<&str>) -> Result<Resolved>` where

```rust
pub struct Resolved {
    pub slug: RepoSlug,
    pub sha: String,             // full 40-hex
    pub label: String,           // human: "PR #2626 (renovate/zip-9.x)", "branch main", "commit f36d1a7"
    pub selector: Option<String> // what to put in hint commands: Some("2626"), Some("main"), or None for Here
}
```

Resolution of each kind:

- `Pr`: the PR's `head.sha`.
- `Branch`: if an **open PR** has this branch as its head, use that PR (label says so). Otherwise
  `GET /repos/{o}/{r}/branches/{b}` → `commit.id`. Both missing is `ResourceNotFound` for the branch.
- `Here`: the current branch (`rt.git().current_branch()`), then as `Branch`. On a detached HEAD,
  local `git rev-parse HEAD` as `Commit`. If the resolved server SHA differs from local `HEAD`, print
  one stderr note: `note: local HEAD abc1234 is not what the server has for <branch> (def5678);
  showing the server's. Push to run CI on your commits.` Not inside a checkout and no positional is
  a usage error naming the positional.

### 1.4 Runs for a SHA (`crates/gea/src/cmd/ci/target.rs` or a sibling `runs.rs`)

`async fn runs_for_sha(api, slug, sha) -> Result<Vec<ActionWorkflowRun>>`: every page of
`GET …/actions/runs?head_sha=<sha>`.

`fn latest_per_workflow(runs) -> Vec<&ActionWorkflowRun>`: keep the highest-`id` run per
`(path, event)`. The rest are superseded (re-pushes, cancelled duplicates). Order the result
failed → in progress/queued → succeeded → other, then by workflow file name. `--all` on `ci status`
skips this de-duplication.

### 1.5 Error taxonomy (`crates/gitea-core/src/error/`)

Add **`ErrorKind::CiFailed`** (exit **1**):

```rust
CiFailed {
    slug: Option<String>,
    /// What the user asked about, as `gea ci` would accept it back; None means "here".
    target: Option<String>,
    sha: String,
    failed: Vec<FailedRun>,
}
pub struct FailedRun { pub id: i64, pub workflow: String, pub conclusion: String, pub failed_jobs: Vec<String>, pub url: Option<String> }
```

Its headline names the SHA and the count. Its "what to do" section gives `gea ci logs [target]` and
`gea ci rerun [target]` verbatim, with the target filled in.

Add **`ErrorKind::CiPending`** (exit **8**, same as `ChecksPending`) with `{ slug, target, sha,
pending: Vec<String> }` (workflow file names). Its "what to do" is `gea ci watch [target]`.

Both variants must satisfy the existing test that every variant renders a "what to do" section, and
must be added to every exhaustive match (render, exit_code, docs/output.md exit-code table).
`FailedRun` follows the same `#[non_exhaustive]` and doc rules as `FailedCheck`; `gitea-core` is a
published crate.

### 1.6 Helpers in run.rs become `pub(crate)`

So `gea ci` reuses them instead of copying: `all_jobs`, `print_logs` (or a refactor of it that takes
already-fetched jobs plus a header prefix), `job_log`, `rerun` internals, `outcome`, `workflow_file`,
`is_finished`, `is_queued`, `diagnose_waiting`, `RUN_FIELDS`, `JOB_FIELDS`, `int`. Make them
visible; do not move them.

## 2. `gea run` changes (`crates/gea/src/cmd/run.rs`)

- Every `<RUN>` is a `RunRef` resolved through `resolve_run`, and every `--job` is a `JobRef`
  resolved through `resolve_job` against the run's jobs. Help text: `The run: an id (6442), a run
  number (#3623), or the run's URL`.
- `run artifacts [RUN]` stays optional.
- `run list --branch B` returning nothing prints a stderr note: `pull_request runs carry no branch
  on Gitea; \`gea ci B\` finds them by commit`.
- Rendered "next command" hints in existing output (e.g. `run view`'s waiting diagnosis, `run list`'s
  queued note) keep using the id. That is fine, since ids still work.
- Existing behaviour for numeric ids is unchanged, including the 404 for an id that matches no run
  and no run number.

## 3. `gea ci` (new group, `crates/gea/src/cmd/ci/mod.rs`)

Registered in `crates/gea/src/cmd/mod.rs` as `Ci(ci::Args)` with about text `CI status, logs and
reruns for a pull request, branch or commit`. Long about: the target forms, exit codes, and three
examples.

Subcommands (all take `[TARGET]`, see §1.3):

### `gea ci status [TARGET]`, also bare `gea ci [TARGET]`

Bare `gea ci [TARGET]` behaves as `status` (clap: optional subcommand plus a flattened
`StatusArgs`, with `args_conflicts_with_subcommands`). A branch literally named `logs` needs
`gea ci status logs`; document that in the long about.

Flags: `--all` (no superseded-run de-duplication), `-W/--workflow <FILE>` (only runs of this
workflow file; matched against `workflow_file(path)`).

Human output (stdout):

```
PR #2626 (renovate/zip-9.x) at f36d1a7

✗ failure  ci.yml                  run 6442  #3623  6m44s
    ✗ Fast Checks (fmt + static lints)        job 33898
    ✗ Build - workspace                       job 33900
✓ success  pr-title.yml            run 6444  #3625  12s
● queued   claude-code-review.yml  run 6443  #3624
    waiting: no runner carries label `duck-runner-huge` (from diagnose_waiting)

next: gea ci logs 2626
```

- Failed jobs are listed under each failed run (one jobs fetch per failed or pending run, bounded
  concurrency like `LOG_CONCURRENCY`). Waiting jobs get `diagnose_waiting`'s explanation.
- The `next:` line: failed → `gea ci logs [target]`; else pending → `gea ci watch [target]`; else
  nothing. It goes on stdout so agents reading the output see it, and it is suppressed under `--json`.
- No runs for the SHA: stderr note `no Actions runs for <sha7> (<label>). The workflows' \`on:\`
  filters may not match, Actions may be disabled, or the push has not been processed yet.`, then
  exit 0. Reuse `warn_if_actions_disabled`.

`--json`: the selected runs as a JSON array, validated against `RUN_FIELDS` (the generated
`ActionWorkflowRun` field table). No invented fields.

Exit: any selected run concluded `failure`/`cancelled`/`timed_out` → `CiFailed` (1). Otherwise any
not finished → `CiPending` (8). Otherwise 0. `skipped` counts as success. The exit code applies
under `--json` too, as with `pr checks`.

### `gea ci logs [TARGET]`

Prints logs for jobs in the selected (de-duplicated) runs, through the same per-job header format
`run logs` uses, prefixed with the workflow file: `==> ci.yml / Fast Checks (run 6442, job 33898) <==`.

- Default: only failed jobs, across every failed run.
- `--all`: every job of every selected run.
- `-j/--job <ID|NAME>`: `JobRef` matched across all selected runs' jobs (ambiguity rules from §1.2,
  candidates labelled with their workflow).
- `-W/--workflow <FILE>`: restrict to one workflow.
- Nothing failed: if something is pending, print a stderr note and exit with `CiPending` (8). If
  everything passed, a stderr note `no job failed for <sha7>; --all prints every job` and exit 0.
- `--json` is not supported (text pump), as with `run logs`: `Fields::None`.

### `gea ci watch [TARGET]`

Polls `runs_for_sha` + `latest_per_workflow` every `-i/--interval` seconds (default 5, range 1..=3600)
until every selected run is finished. Each poll after the first prints a one-line status change per
run to stderr (`ci.yml: in_progress → failure`). When it finishes, prints the `ci status` human view
and exits as `ci status` does (`--exit-status` is implied and documented). If no runs exist yet, it
keeps polling up to `--appear-timeout <SECONDS>` (default 60), then behaves like `ci status` with no
runs. Same interruptibility and no-spin guarantees as `run watch` (no SIGINT handler, a sleep every
non-returning iteration).

### `gea ci rerun [TARGET]`

For each selected run that failed, reruns its failed jobs (the same route `run rerun --failed` uses).
`-W/--workflow` restricts which runs. Prints one stderr line per rerun (`reran failed jobs of ci.yml
(run 6442)`) and then `next: gea ci watch [target]` on stdout. Nothing failed → stderr note, exit 0.
Not destructive, so no confirmation.

### Field table for `--json` discovery

`fields_for`: `status` → `RUN_FIELDS`; `logs`/`watch`/`rerun` → `Fields::None`.

## 4. `gea pr checks` (`crates/gea/src/cmd/pr/checks.rs`)

Human output only. `--json` and exit codes are unchanged.

- After the table, a summary line: `N passed, N failed, N pending, N skipped`.
- When any check failed or is pending, and the failing/pending checks' `target_url` points at this
  repository's `/actions/runs/{id}…`, a `next:` line: `gea ci logs <pr>` (failed) or
  `gea ci watch <pr>` (pending). External-CI checks (other URLs) get no hint. Parse the run id out of
  `target_url` with the same URL parsing `RunRef` uses.

## 5. Testing

### Unit (hermetic, per docs/porcelain-conventions.md)

- `RunRef`/`JobRef`/`Target` parsing tables: every form, plus the rejects.
- `resolve_run` with `FakeTransport`: id hit; id 404 → number fallback with the note; id 404 and no
  number → original 404 plus hint; non-404 errors are not retried as a number; `#N` paging stops
  early once `run_number` < N.
- `resolve_job`: exact beats substring; ambiguity is refused with candidates; URL job index.
- `latest_per_workflow`: superseded runs are dropped; ordering.
- `ci status` human output and `--json` snapshots (insta); exit-code mapping (`CiFailed`/`CiPending`).
- `CiFailed`/`CiPending` render snapshots; the existing every-variant-has-what-to-do test passes.
- `crates/gea/tests/porcelain_cli.rs` passes (clap tree consistency, no duplicate global flags).

### Integration, live (`crates/gea-itest`)

The existing harness has no runner, so a run never executes. Add one:

- `gea-itest`: `Instance::attach_runner(&self, labels: &[&str]) -> Result<RunnerGuard, String>` starts a
  pinned `gitea/act_runner` container (testcontainers, same LABEL/reaping conventions) that registers
  with an instance-scoped registration token, reaches Gitea at the Gitea container's own IP, and
  carries a **host-mode** label (`gea-itest:host`), so steps run inside the runner container with no
  Docker-in-Docker. Test workflows use `runs-on: gea-itest` and `defaults.run.shell: sh`. If the image
  cannot be pulled or the runner never comes online, skip through the harness's existing skip
  mechanism (honouring `allow_skip`), and never fake a pass.
- New `crates/gea-itest/tests/live_ci.rs`, with tests driving real runs end to end:
  1. A push-triggered workflow with one passing and one failing job (the failing one prints a unique
     marker then `exit 1`): `gea ci watch` exits 1; `gea ci` exits 1 and names the failing job and
     run id; `gea ci --json` decodes to runs; `gea ci logs` contains the marker and not the passing
     job's output; `gea ci rerun` queues a new attempt.
  2. An all-green workflow: `gea ci` exits 0, with no `next:` line.
  3. **The regression the design exists for:** a `pull_request` run (empty `head_branch`) is found by
     `gea ci <pr-number>` and by `gea ci <branch>`.
  4. `gea run view` by `#number`, by URL, and by bare number with the id-miss fallback note;
     `gea run logs <run> --job <name>`; an ambiguous `--job` substring is refused with candidates.
  5. `gea pr checks` on the failing PR prints the `next: gea ci logs <n>` hint.
- Coverage: use `cover!` as the existing live tests do, after any skip guard.

## Out of scope

- Mirroring into fjo (a follow-up port using the diff-translate recipe).
- Interactive pickers.
- `run cancel` (no API route on Gitea 1.27).
- Switching repository from a pasted run URL's owner/name.
