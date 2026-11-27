//! `xtask` — the gea generator and repository tooling.
//!
//! Never a dependency of the binary. Everything here runs on a developer's machine or in CI,
//! which is why it is allowed to shell out to `curl` instead of pulling an HTTP stack into the
//! workspace.
//!
//! The pipeline this crate implements, in order:
//!
//! ```text
//! upstream v1_json.tmpl ──update-spec──> spec/gitea-vX.json ──lower──> Ir ──emit──> code
//!                                             + spec/lock.toml              + spec/name-lock.toml
//! ```
//!
//! Two invariants make the rest of the project safe to build on:
//!
//! 1. **`spec-stats --verify` is a self-test of the loader**, not a report. Its expected
//!    numbers were established by independent inspection of the spec. If the loader
//!    disagrees, the loader is wrong — and finding that out here is far cheaper than finding
//!    it out after 42k lines of generated code have been shaped by a miscount.
//! 2. **`spec/name-lock.toml` makes every command name a versioned contract.** A spec bump
//!    that would rename a command is a hard error until a human passes `--accept-renames`,
//!    because the alternative is silently breaking every script our users have written.
#![forbid(unsafe_code)]
// The IR is a contract that the four emitters (M4–M6) consume, so its fields necessarily
// exist before their readers do. The alternative — per-field `allow(dead_code)` removed one
// at a time as emitters land — produces churn without catching anything: what actually
// guards against unused work here are the uniqueness assertions and `spec-stats --verify`.
#![allow(dead_code)]

/// `return Err(format!(...))`, for the many places where the useful thing to do with a
/// broken spec is stop and explain.
macro_rules! bail {
    ($($arg:tt)*) => {
        return Err(::std::convert::Into::into(format!($($arg)*)))
    };
}

mod coverage;
mod emit;
mod ir;
mod itest;
mod name_lock;
mod overrides;
mod spec;
mod spec_diff;
mod stats;
mod swagger;
mod update_spec;

use std::path::{Path, PathBuf};
use std::process::ExitCode;

use clap::{ArgAction, Args, Parser, Subcommand};

/// Errors here only ever reach a developer's terminal, so a boxed message beats a taxonomy.
/// `gitea-core`'s [`Error`](../gitea_core/error/struct.Error.html) exists because *users*
/// need remedies; `xtask` failures are read by whoever just ran the command.
pub type Result<T, E = Box<dyn std::error::Error + Send + Sync>> = std::result::Result<T, E>;

/// The workspace root, derived from this crate's manifest directory at compile time.
///
/// Deliberately **not** `current_dir()`: `cargo xtask` is run from wherever the developer
/// happens to be standing, and a generator whose output depends on the invocation directory
/// produces diffs that nobody can reproduce.
pub fn workspace_root() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR"))
        .ancestors()
        .nth(2)
        .expect("crates/xtask is two levels below the workspace root")
        .to_path_buf()
}

#[derive(Parser)]
#[command(name = "xtask", about = "gea generator and repository tooling", version)]
struct Cli {
    #[command(subcommand)]
    cmd: Cmd,
}

#[derive(Subcommand)]
enum Cmd {
    /// Vendor the Gitea Swagger spec at a git tag into `spec/`.
    UpdateSpec {
        /// Gitea tag, e.g. `v1.27.2`. A bare `1.27.2` is accepted too.
        #[arg(long)]
        version: String,
        /// Fetch, check and canonicalize, but write nothing. Prints what would change.
        #[arg(long, action = ArgAction::SetTrue)]
        dry_run: bool,
        /// Write the files even though `spec-stats` will not verify against the new version.
        /// What the spec-drift workflow passes; a human still has to update stats.rs.
        #[arg(long, action = ArgAction::SetTrue)]
        no_verify: bool,
    },

    /// Compare two specs and print a Markdown report of what moved: operations, parameters,
    /// response shapes, shared responses, definitions.
    ///
    /// The right-hand side is the required `--tag`/`--branch`/`--file`. The left-hand side is
    /// the vendored spec unless a `--baseline-*` moves it: `--baseline-tag <latest> --branch
    /// gitea` reports what upstream has on its development branch and in no release yet.
    ///
    /// Exit status is 0 when the two agree, 3 when they differ, 1 on error — so
    /// `.github/workflows/spec-drift.yaml` can branch on drift without parsing the report.
    SpecDiff {
        #[command(flatten)]
        source: SpecDiffSource,
        #[command(flatten)]
        baseline: SpecDiffBaseline,
        /// Write the report here instead of stdout.
        #[arg(long)]
        out: Option<PathBuf>,
    },

    /// Print counts from the vendored spec, and by default assert they are the known-good ones.
    SpecStats {
        /// Redundant — verification is on by default. Accepted so scripts can be explicit.
        #[arg(long, action = ArgAction::SetTrue)]
        verify: bool,
        /// Print the table without asserting the numbers. Only useful while investigating a
        /// spec bump; CI must never pass this.
        #[arg(long, action = ArgAction::SetTrue, conflicts_with = "verify")]
        no_verify: bool,
    },

    /// Lower the spec to the IR and regenerate the committed generated trees.
    Codegen {
        /// Regenerate into a temp dir and report differences without touching the working
        /// tree. What CI runs: it makes a hand-edit under `src/generated/` fail the build,
        /// which is the whole reason 42k lines of generated code can be trusted.
        #[arg(long, action = ArgAction::SetTrue)]
        check: bool,
        /// Assert the naming invariants: 506 unique `(module, fn)` and `(group, command)`.
        #[arg(long, action = ArgAction::SetTrue)]
        check_names: bool,
        /// Pretty-print the whole IR to stdout.
        #[arg(long, action = ArgAction::SetTrue)]
        dump_ir: bool,
        /// Rewrite `spec/name-lock.toml` for renamed commands, printing a CHANGELOG-ready diff.
        #[arg(long, action = ArgAction::SetTrue)]
        accept_renames: bool,
        /// Allow operations that disappeared upstream to be dropped from the name lock.
        #[arg(long, action = ArgAction::SetTrue)]
        accept_removals: bool,
    },

    /// Ratchet: how much of the command surface the test suites actually drove.
    ///
    /// Reads the journals the suites write (see crates/gea-itest/src/coverage.rs) and compares
    /// them against spec/name-lock.toml and the porcelain inventory. The contract plane is a
    /// hard failure at one gap; the live planes are count budgets that drain.
    CoverageCheck {
        /// Journal directory. Defaults to target/gea-coverage under the workspace root.
        #[arg(long)]
        dir: Option<PathBuf>,
        /// Ceiling on live raw operations still untested.
        #[arg(long, default_value_t = 0)]
        raw_budget: usize,
        /// Ceiling on live porcelain commands still untested.
        #[arg(long, default_value_t = 0)]
        porcelain_budget: usize,
        /// Print the uncovered ids. How you find what to write next.
        #[arg(long, action = ArgAction::SetTrue)]
        list: bool,
    },

    /// Run the integration suite against a real Gitea, booting one in Docker if needed.
    Itest {
        /// Leave the container running afterwards for inspection.
        #[arg(long, action = ArgAction::SetTrue)]
        keep: bool,
        /// Use a different Gitea image than the vendored spec targets.
        #[arg(long)]
        image: Option<String>,
        /// Permit a skip when no Gitea is reachable. CI must never pass this.
        #[arg(long, action = ArgAction::SetTrue)]
        allow_skip: bool,
        /// Only run tests whose name contains this.
        filter: Option<String>,
        /// Wall-clock ceiling for the whole suite, in seconds. `cargo test` has no timeout of
        /// its own, so without this a hung test hangs the run forever.
        #[arg(long)]
        timeout_secs: Option<u64>,
    },
}

/// Exactly one of these names the upstream document for `spec-diff`.
#[derive(Args)]
#[group(required = true, multiple = false)]
struct SpecDiffSource {
    /// A Gitea release tag, e.g. `v1.27.3`.
    #[arg(long)]
    tag: Option<String>,
    /// A Gitea branch, e.g. `gitea` (upstream's development branch).
    #[arg(long)]
    branch: Option<String>,
    /// A `v1_json.tmpl` or canonical JSON file already on disk.
    #[arg(long)]
    file: Option<PathBuf>,
}

/// Moves the left-hand side of `spec-diff` off the vendored spec. Optional; at most one.
///
/// `--baseline-tag <latest> --branch main` is the comparison that separates "upstream has
/// released this, take the bump" from "upstream is only thinking about this, wait".
#[derive(Args)]
#[group(required = false, multiple = false)]
struct SpecDiffBaseline {
    /// Compare against this release tag instead of the vendored spec.
    #[arg(long)]
    baseline_tag: Option<String>,
    /// Compare against this branch instead of the vendored spec.
    #[arg(long)]
    baseline_branch: Option<String>,
    /// Compare against this local file instead of the vendored spec.
    #[arg(long)]
    baseline_file: Option<PathBuf>,
}

impl SpecDiffBaseline {
    fn into_source(self) -> Option<spec_diff::Source> {
        match (self.baseline_tag, self.baseline_branch, self.baseline_file) {
            (Some(tag), _, _) => Some(spec_diff::Source::Tag(tag)),
            (_, Some(branch), _) => Some(spec_diff::Source::Branch(branch)),
            (_, _, Some(path)) => Some(spec_diff::Source::File(path)),
            // clap's group rules make more than one unreachable; none means the vendored spec.
            (None, None, None) => None,
        }
    }
}

impl SpecDiffSource {
    fn into_source(self) -> Result<spec_diff::Source> {
        match (self.tag, self.branch, self.file) {
            (Some(tag), None, None) => Ok(spec_diff::Source::Tag(tag)),
            (None, Some(branch), None) => Ok(spec_diff::Source::Branch(branch)),
            (None, None, Some(file)) => Ok(spec_diff::Source::File(file)),
            // clap's group rules make this unreachable; the error keeps it a message, not a panic.
            _ => Err("spec-diff needs exactly one of --tag, --branch, --file".into()),
        }
    }
}

fn main() -> ExitCode {
    let cli = Cli::parse();
    match run(cli) {
        Ok(code) => code,
        Err(e) => {
            eprintln!("xtask: {e}");
            ExitCode::FAILURE
        }
    }
}

fn run(cli: Cli) -> Result<ExitCode> {
    let root = workspace_root();
    match cli.cmd {
        Cmd::SpecDiff { source, baseline, out } => {
            let baseline = baseline.into_source();
            let report =
                spec_diff::run(&root, baseline.as_ref(), &source.into_source()?, out.as_deref())?;
            Ok(if report.has_drift() {
                ExitCode::from(spec_diff::DRIFT_EXIT_CODE)
            } else {
                ExitCode::SUCCESS
            })
        }
        other => run_unit(&root, other).map(|()| ExitCode::SUCCESS),
    }
}

/// Every subcommand whose only outcomes are "done" and "failed".
fn run_unit(root: &Path, cmd: Cmd) -> Result<()> {
    match cmd {
        Cmd::SpecDiff { .. } => Err("spec-diff is dispatched by `run`".into()),
        Cmd::UpdateSpec { version, dry_run, no_verify } => {
            update_spec::run(root, &version, dry_run, !no_verify)
        }

        Cmd::SpecStats { no_verify, .. } => {
            let loaded = spec::load(root)?;
            let stats = stats::Stats::compute(&loaded.spec);
            print!("{}", stats.render());
            if no_verify {
                eprintln!("note: --no-verify given; the loader self-test did not run");
                Ok(())
            } else {
                stats.verify(&loaded.lock.version)
            }
        }

        Cmd::CoverageCheck { dir, raw_budget, porcelain_budget, list } => {
            coverage::run(root, coverage::Options { dir, raw_budget, porcelain_budget, list })
        }

        Cmd::Itest { keep, image, allow_skip, filter, timeout_secs } => itest::run(
            root,
            itest::Options {
                keep,
                image,
                allow_skip,
                filter,
                timeout: timeout_secs.map(std::time::Duration::from_secs),
            },
        ),

        Cmd::Codegen { check, check_names, dump_ir, accept_renames, accept_removals } => {
            let loaded = spec::load(root)?;
            let overrides = overrides::Overrides::load()?;
            let ir = ir::lower::lower(&loaded, &overrides)?;

            let mode = name_lock::Mode { accept_renames, accept_removals };
            name_lock::reconcile(root, &ir, mode)?;

            if check_names {
                println!(
                    "ok: {} operations, {} unique (module, fn), {} unique (group, command)",
                    ir.operations.len(),
                    ir.operations.len(),
                    ir.operations.len(),
                );
            }
            if dump_ir {
                print!("{}", ir.dump());
                return Ok(());
            }
            if check_names {
                return Ok(());
            }

            let files = emit::emit_all(&ir)?;
            let write_mode = if check { emit::Mode::Check } else { emit::Mode::Write };
            let report = emit::write_all(root, &files, write_mode)?;
            print!("{}", report.render());

            if check && !report.is_clean() {
                for p in &report.changed {
                    eprintln!("  would change: {}", p.display());
                }
                for p in &report.removed {
                    eprintln!("  would remove: {}", p.display());
                }
                bail!(
                    "the committed generated tree does not match the spec.\n\
                     Run `cargo xtask codegen` and commit the result.\n\
                     Edit the generator, not files under src/generated/. See CONTRIBUTING.md."
                );
            }
            Ok(())
        }
    }
}
