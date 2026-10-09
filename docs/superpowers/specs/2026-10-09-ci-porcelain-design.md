# CI porcelain: `fjo ci`, flexible run/job references, and `pr checks` hints

Status: ported 2026-10-09 from gea's approved design (perfectra1n/gea#5, branch `feat/ci-porcelain`, Approach 3) with the diff-translate recipe, and adapted wherever Forgejo diverges from Gitea. Every divergence was **measured** against Forgejo 16.0.5 with forgejo-runner 13.2.0 rather than assumed from the Gitea port; [Forgejo divergences](#forgejo-divergences-measured) lists them, and the sections below already describe fjo's behaviour.

## Problem

Getting from "is CI red?" to "the failing log" is hard, for humans and for agents:

1. `run list` prints two numbers (`ID` 6442, `INDEX` 3623). `run view 3623` is a bare 404 that never says "that is a run number".
2. A pasted run URL (`…/actions/runs/3623`) is rejected: "is not a number". On Forgejo that URL carries the run *number*, so even reading the digits out of it and passing them as an id addresses the wrong run.
3. Forgejo files a `pull_request` run under the pull request, not its branch: its `prettyref` reads `#12`, and `run list --ref refs/heads/<branch>` silently misses it. (`--ref <branch>` without `refs/heads/` matches nothing at all.) Filtering by `--commit <sha>` works.
4. `pr checks` lists check names with no summary and no next command.
5. `--job` takes only a numeric job id; nobody knows those.
6. There is no "show me CI for where I am" entry point.

Users are both humans at a terminal and agents (Claude, `shepherd-ci`). So: no interactive pickers, stable exit codes, `--json` via the existing field tables, and every human summary ends with the exact next command to run.

## Design overview

Three layers sharing one core:

- **Core resolution** (new, shared): parse and resolve run references, job references and CI targets.
- **`fjo run *`** (existing): accepts the flexible references. Stays the *precise addressing* layer. `<RUN>` stays required.
- **`fjo ci`** (new): the *context* layer. Answers "what is CI doing for this PR / branch / commit / where I am", and goes from there to logs and watch without the user ever typing an id.
- **`fjo pr checks`**: human output gains a summary and next-step hints. `--json` is unchanged.

The commit SHA is the join key throughout: a target resolves to a SHA, then runs are listed with the server-side `head_sha` filter (the same one `run list --commit` uses), which finds `pull_request` runs that a ref filter misses.

## 1. Core resolution

### 1.1 Run references (`crates/fjo/src/cmd/run_ref.rs`, new)

`RunRef` replaces `RunId` as the type of every `<RUN>` positional in `fjo run` (`view`, `cancel`, `delete`, `logs`, `watch`, `jobs`, `artifacts`). `FromStr` (so clap's `value_parser!` works) accepts:

| Input | Meaning |
| --- | --- |
| `6442` | `Bare(6442)`: an id, with fallback (below) |
| `#3623` | `Number(3623)`: explicitly the per-repository run number (`index_in_repo`) |
| `https://host/o/r/actions/runs/3623`, optionally followed by `/jobs/1`, `/attempts/N`, or a trailing slash/query/fragment; also the same path with no scheme and host | `Url { run: 3623, job: Option<usize> }`. The run segment is the run **number** and is resolved like `Number`; the job segment is a 0-based **position**. The URL's owner/name are **not** used to switch repository (`-R` does that), but if they differ from the resolved repository, a stderr warning says so. |

Anything else is a clap usage error naming the accepted forms: "expected a run id (`6442`), a run number (`#3623`), or a run URL".

Resolution: `async fn resolve_run(api, slug, &RunRef) -> Result<ActionRun>`.

- `Bare(n)`: `GET …/actions/runs/{n}`. On **404 only**, look for the run numbered `n`. If one exists, use it and print one stderr warning: `note: no run has id N; using run #N (id M)`. If not, return the original 404, after a warning naming both forms. Any non-404 error propagates unchanged.
- `Number(n)` and `Url`: the run numbered `n`. Not found is `ResourceNotFound`.
- Finding by number is **one request**: `GET …/actions/runs?run_number=n`. The answer is checked against `index_in_repo`, so a server that ignored the filter (and answered with its newest run) reads as "not found" rather than as the wrong run.

### 1.2 Job references (same file)

`JobRef` replaces `JobId` for every `--job/-j` in `fjo run` (and is used by `fjo ci logs --job`). `FromStr`: all digits → `Id(i64)`; anything else → `Name(String)`.

`fn resolve_job<'a>(jobs: &'a [ActionRunJob], r: &JobRef) -> Result<&'a ActionRunJob>` (pure, no I/O), over a generic `pick_job` that `fjo ci logs` reuses across several runs:

- `Id(n)`: the job with that id. Not in this run is `ResourceNotFound`, listing the run's jobs as `name (id N)`.
- `Name(s)`: a case-insensitive **exact** name match wins. Otherwise a unique case-insensitive **substring** match. Zero matches is not-found (listing the jobs). Several matches is a usage error listing each candidate as `name (id N)` and saying "pass the id to choose one". Never guess.
- A URL's `/jobs/<n>` is the job at **position** `n` in the run's job listing (`job_at`); past the end is not-found. `--job` wins over a job URL, being the more deliberate of the two.

### 1.3 CI targets (`crates/fjo/src/cmd/ci/target.rs`, new)

`Target`, from the optional positional of every `fjo ci` subcommand:

| Input | Kind |
| --- | --- |
| omitted | `Here` |
| `42`, `#42`, a pull request URL | `Pr` (through `cmd::pr::common::find`) |
| 7–40 hex chars containing at least one letter a–f | `Commit` (a full SHA is used as-is; a short one is expanded with local `git rev-parse --verify <sha>^{commit}` inside a checkout, otherwise with `GET /repos/{o}/{r}/git/commits/{sha}`; a short SHA no commit has falls back to a branch of that name) |
| anything else | `Branch` |

`resolve_target` answers `Resolved { slug, sha, label, selector }`: the full 40-hex SHA, a human label (`PR #2626 (renovate/zip-9.x)`, `branch main`, `commit f36d1a7`), and the selector hint commands should repeat (`None` for "here").

- `Pr`: the PR's `head.sha`.
- `Branch`: if an **open PR** has this branch as its head, use that PR (the label says so). Otherwise `GET /repos/{o}/{r}/branches/{b}` → `commit.id`. Both missing is `ResourceNotFound` for the branch.
- `Here`: the current branch, then as `Branch`. On a detached HEAD, local `HEAD` as `Commit`. If the server's SHA differs from local `HEAD`, one stderr warning: `note: local HEAD abc1234 is not what the server has for <branch> (def5678); showing the server's. Push to run CI on your commits.` Outside a checkout with no positional is a usage error naming the positional.

### 1.4 Runs for a SHA

`runs_for_sha`: every page of `GET …/actions/runs?head_sha=<sha>`, filtered again on `commit_sha` (an instance ignoring the parameter must not report another commit's red run).

`latest_per_workflow`: keep the highest-`id` run per `(workflow_id, event)` (Forgejo's `workflow_id` is the file name). The rest are superseded. Order failed → pending → passed → other, then by workflow file. `--all` on `ci status` skips the de-duplication.

States, from `status` alone (Forgejo has no separate conclusion): `failure`/`cancelled` → failed; `waiting`/`running`/`blocked`/`unknown` → pending; `success`/`skipped` → passed; anything else → other.

### 1.5 Error taxonomy (`crates/forgejo-core/src/error/`)

**`ErrorKind::CiFailed`** (exit **1**) `{ slug, target, sha, failed: Vec<FailedRun> }`, with `FailedRun { id, workflow, conclusion, failed_jobs, url }` (`#[non_exhaustive]`, built with `FailedRun::new`; `forgejo-core` is published). The headline names the SHA and the count. "What to do": `fjo ci logs [target]`, then `fjo run view <id of the first failed run> [-R slug] --web` — Forgejo can rerun only from the run's page.

**`ErrorKind::CiPending`** (exit **8**, as `ChecksPending`) `{ slug, target, sha, pending: Vec<String> }`. "What to do": `fjo ci watch [target]`.

Both satisfy the every-variant-has-a-runnable-command test and are in every exhaustive match and the `docs/output.md` exit-code table. `ChecksFailed`'s advice now points at `fjo ci logs <pr>`.

### 1.6 Helpers in run.rs become `pub(crate)`

So `fjo ci` reuses rather than copies: `LOG_CONCURRENCY`, `RUN_FIELDS`, `JOB_FIELDS`, `print_logs`, `concat_logs`, `fetch_view`, `fetch_logs`, `diagnose_waiting`, `warn_if_actions_disabled`, `is_finished`, `is_queued`, `labels`, `duration`. Visible, not moved.

## 2. `fjo run` changes

- Every `<RUN>` is a `RunRef` resolved through `resolve_run`; every `--job` a `JobRef` resolved through `resolve_job`. Help text: `The run: an id (6442), a run number (#3623), or the run's URL`.
- `fetch_view` overlaps the run and its jobs only for a bare id (the likely case); `#N` and URLs resolve first, and an id that fell back to a number re-reads the jobs of the run it resolved to.
- `run delete`'s confirmation names the resolved run by id, number and workflow. `run cancel` resolves too.
- `run list --ref R` returning nothing prints a stderr warning: pull_request runs carry the pull request, not the branch, as their ref; `fjo ci <branch>` finds them by commit — and, for a short `R`, "did you mean `--ref refs/heads/R`?".
- `duration` reads Forgejo's nanoseconds (it read them as seconds: a one-second run printed `16666666m40s`).
- Existing behaviour for numeric ids is unchanged, including the 404 for an id that matches no run and no run number.

## 3. `fjo ci` (new group, `crates/fjo/src/cmd/ci/`)

Registered as `Ci(ci::Args)`, about `CI status and logs for a pull request, branch or commit`, with a long about listing the target forms, the exit codes and examples. All subcommands take `[TARGET]` (§1.3).

### `fjo ci status [TARGET]`, also bare `fjo ci [TARGET]`

Bare `fjo ci` is `status` (optional subcommand plus a flattened `StatusArgs`, `args_conflicts_with_subcommands`). A branch named like a subcommand needs `fjo ci status logs`. Flags: `--all`, `-W/--workflow <FILE>`.

```
PR #2626 (renovate/zip-9.x) at f36d1a7

✗ failure    ci.yml                    run 6442  #3623  6m44s
    ✗ Fast Checks (fmt + static lints)  job 33898
● waiting    claude-code-review.yml    run 6443  #3624
    waiting: job review requires runs-on duck-runner-huge, but no runner matches all labels. …
✓ success    pr-title.yml              run 6444  #3625  12s

next: fjo ci logs 2626
```

- Failed jobs are listed under each failed run; waiting jobs get `diagnose_waiting`'s explanation (only in the human view, which is the only one that prints it).
- `next:` is on stdout: failed → `fjo ci logs [target]`; else pending → `fjo ci watch [target]`; else nothing. Suppressed under `--json`. `-R` is carried into hints when the user gave it.
- No runs: a stderr warning, exit 0, plus `warn_if_actions_disabled`.
- `--json`: the selected runs, validated against `ActionRun`'s field table.
- Exit: any failed → `CiFailed` (1); else any pending → `CiPending` (8); else 0. Also under `--json`.

### `fjo ci logs [TARGET]`

Logs of the selected runs' jobs, each under `==> ci.yml / build (run 6442, job 11) <==`. Default: only failed jobs; `--all`: every job; `-j/--job <ID|NAME>`: matched across all selected runs, ambiguous candidates labelled with their workflow; `-W/--workflow`. Nothing failed: a stderr note and the exit code `status` would give (8 while pending, else 0). Text pump, no `--json`.

### `fjo ci watch [TARGET]`

Polls every `-i/--interval` seconds (default 5, 1..=3600) until every selected run has finished, printing each status change (`ci.yml: running → failure`) to stderr, then prints `ci status` and exits as it does. No runs yet: keeps polling up to `--appear-timeout` (default 60). No SIGINT handler; a sleep every non-returning iteration. A run that has *failed* is finished even while its other jobs run (see divergences): the verdict cannot change, so it is reported at once.

### `pr checks`

Human output only; `--json` and exit codes unchanged. A summary line (`N passed, N failed, N pending, N skipped`) and, when a failed/pending check's `target_url` is an Actions run of this repository (parsed with the same parser as `RunRef`, including the path-only form), `next: fjo ci logs <pr>` / `next: fjo ci watch <pr>`. On a terminal these go to stdout; piped, to stderr so stdout stays TSV.

### Messages that change how output should be read

The number fallback, no runs, local HEAD differing from the server, nothing failed, and the empty `--ref` listing use `support::warn` (always stderr), not the TTY-only `support::note`.

## 4. Testing

### Unit (hermetic)

- `RunRef`/`JobRef`/`Target` parsing tables, every form plus the rejects; `#N`'s one filtered request; a server ignoring `run_number` reads as not found; a URL never hits `GET runs/{n}`.
- `resolve_job`: exact beats substring, ambiguity refused with candidates, a URL job is a position.
- `latest_per_workflow`, the state of every Forgejo status, `ci status` human and `--json` snapshots, exit-code mapping, `watch`'s no-spin and appear-timeout behaviour, the absence of `ci rerun`.
- `CiFailed`/`CiPending` render snapshots, including that no advice names `ci rerun`.
- The nanosecond duration (red against the old seconds reading), and the empty-`--ref` note.

### Live (`crates/fjo-itest`)

- `fjo_itest::runner`: `Instance::attach_runner` registers a runner with `POST /admin/actions/runners`, starts pinned `code.forgejo.org/forgejo/runner:13.2.0` with that `uuid`/`token` in `server.connections` and a host-mode label (`fjo-itest:host`), and waits until the runner's own record is not `offline`. One shared runner per test binary (`shared_runner`, `runner_or_skip!`), which under `FJO_ITEST_REQUIRE` turns "no runner" into a failure. Workflows use `runs-on: fjo-itest` and `shell: sh`, under `.forgejo/workflows`.
- `tests/live_ci.rs` (7 tests): a failed push run through `ci watch`/`ci`/`ci status`/`--json`/`ci logs` and the run-page advice; an all-green run; the `pull_request` run found by `ci <pr>`, `ci #<pr>` and `ci <branch>` with the premise and the `run list --ref` contrast asserted; `run view` by `#N`, URL and bare-number fallback, `run logs --job` by name, by an Actions status `target_url` (path-only, position), and an ambiguous name; `pr checks`' hint; bare `fjo ci` in a checkout with the local-HEAD warning; `ci watch` on a slow job.
- `tests/live_runner.rs`: `run view`/`jobs`/`logs --failed`/`watch --exit-status` on a real finished run, the run-level log zip, and a real duration.
- Coverage: `repoGetActionRunLogs` was exempt in `spec/live-coverage.toml` until a runner existed; it is now driven, and the exemption is gone.

## Forgejo divergences (measured)

Each of these was measured on Forgejo 16.0.5 + forgejo-runner 13.2.0 with a throwaway probe against the harness, and each is asserted by a live test so a server change that invalidates it fails the suite.

| # | Gitea 1.27 (gea) | Forgejo 16.0.5 (fjo) | Consequence here |
| --- | --- | --- | --- |
| 1 | `pull_request` runs have **no `head_branch` key** | `pull_request` runs have `prettyref: "#<pr>"`; `?ref=refs/heads/<branch>` finds only the push run, and `?ref=<branch>` finds nothing | Same remedy (`head_sha`). The `run list` note is about `--ref`, and also catches the short-ref mistake. |
| 2 | `html_url` carries the run **id** | `html_url` carries **`index_in_repo`**; `GET actions/runs/<index>` is a 404 once id ≠ index | A URL resolves as `#N`, never as an id. Hints and advice that name a run for `fjo run` use the id. |
| 3 | `/jobs/<n>` in a run URL is the job **id** | `/jobs/<n>` is the job's **0-based position** (job ids 4 and 5 link as `jobs/0`, `jobs/1`) | `RunRef::Url.job` is a `usize` position, resolved with `job_at`. |
| 4 | Actions commit statuses have path-only `target_url`s | Same: `/<owner>/<repo>/actions/runs/<index>/jobs/<pos>`; each job posts `pending` then its outcome, and both stay listed | The URL parser accepts the path-only form (unchanged from gea). |
| 5 | No run-cancel API | `POST actions/runs/{id}/cancel` exists (204, even for a finished run) | `run cancel` keeps existing and takes a `RunRef`. |
| 6 | Rerun routes exist (`rerun`, `rerun-failed-jobs`, per job) | **No rerun route at all** (`POST actions/runs/{id}/rerun` is a router 404) | **No `fjo ci rerun`.** `CiFailed`'s second step is `fjo run view <id> --web`. `fjo ci rerun` parses as a branch named `rerun` and exits 5. |
| 7 | No server filter for `run_number`; gea pages newest-first, capped at 20 pages | `ListActionRuns` takes `run_number` | `#N` is one request; the 20-page scan and its cap are gone. |
| 8 | Jobs endpoint is paged, `{total_count, jobs}` | `GET actions/runs/{id}/jobs` is an unpaged bare array | No `fetch_jobs` pager; `api.run().jobs` directly. |
| 9 | `status` (`queued`/`in_progress`/`completed`) plus a separate `conclusion` | `status` **is** the outcome: `waiting`/`running`/`blocked`/`unknown`, then `success`/`failure`/`cancelled`/`skipped` | One state function over `status`; `FailedRun.conclusion` carries the status. |
| 10 | A run is `completed` when every job is | A run reads **`failure` as soon as one job fails**, while its other jobs may still run | `ci watch` reports a decided run at once (fail fast); live tests wait on jobs, not only runs (`wait_for_jobs_finished`). |
| 11 | `started_at`/`completed_at` timestamps | `duration` is a Go `time.Duration` in **nanoseconds**, `started`→`stopped` at 1 s granularity; `stopped` is stamped when the status is decided, so a fast-failing run reports none | `duration()` reads nanoseconds (a bug fix for `run view` too). |
| 12 | Workflow file from `path` (`ci.yml@refs/…`) | `workflow_id` is the file name | Dedup key `(workflow_id, event)`; no `workflow_file` helper. |
| 13 | act_runner registers from env (`GITEA_RUNNER_REGISTRATION_TOKEN`), labels in registration | `POST /admin/actions/runners {name}` → `{id, uuid, token}`; runner v13 takes them in `server.connections` with per-connection `labels` and runs `forgejo-runner daemon --config`; `register` is deprecated | No registration step, id known before the container starts, online check is `GET admin/actions/runners/{id}`. Image `code.forgejo.org/forgejo/runner`, status reads `active` while up. |

## Out of scope

- Interactive pickers.
- Rerunning from the CLI (no API route on Forgejo 16.0.5; see divergence 6).
- Switching repository from a pasted run URL's owner/name.
