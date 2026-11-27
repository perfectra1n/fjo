//! `cargo xtask coverage-check` — the ratchet over which commands the test suites actually drove.
//!
//! # What it measures
//!
//! Two planes, because they answer different questions and deserve different gates:
//!
//! * **contract** — every one of the 506 generated operations had its composed request checked
//!   against its own metadata, hermetically (`crates/gea/tests/raw_contract.rs`). That test is a
//!   loop over `OPS`, so its coverage is complete *by construction* and any gap is a bug in the
//!   test rather than missing work. This plane is therefore a **hard failure at one**, not a
//!   budget.
//! * **live** — the operation was driven against a real Gitea (`crates/gea-itest`). This one
//!   is genuine, drainable debt, so it gets a count budget in the style of
//!   [`docs/ratchets.md`](../../docs/ratchets.md): over budget fails, under budget nags.
//!
//! # Why it reads journals instead of scanning sources
//!
//! `crates/gea-itest/src/coverage.rs` explains the reasoning in full. The short version: the
//! live suite *skips* when Docker is unreachable, and a skip prints `ok`. A source scan cannot
//! tell a skipped test from an executed one, so it would report full coverage over a suite that
//! ran nothing — which is the exact failure `cargo xtask itest` already exists to prevent.
//!
//! # Why the budgets are arguments rather than constants
//!
//! They live in `.mise/config.toml` beside every other `BUDGET=` in this repository, so there is
//! one place a reader looks for "what are the current gates and what are their numbers". A
//! budget compiled into this binary would be the only one that is invisible there.

use std::collections::{BTreeMap, BTreeSet};
use std::path::{Path, PathBuf};

use serde::Deserialize;

use crate::Result;
use crate::name_lock::Entry;
use crate::spec;

/// The variable that tells both suites where to record.
///
/// Duplicated from `gea_itest::coverage::DIR_ENV` rather than shared, because `xtask` is
/// deliberately not in the same dependency graph as the crates it tools (see this crate's
/// module docs). The two spellings must match; that is what the name in this comment is for.
pub const DIR_ENV: &str = "GEA_COVERAGE_DIR";

/// Default location for journals, matching what `cargo xtask itest` sets [`DIR_ENV`] to.
pub const DEFAULT_DIR: &str = "target/gea-coverage";

/// Where `crates/gea/tests/porcelain_inventory.rs` writes the layer-3 leaf list.
const INVENTORY: &str = "porcelain-inventory.json";

/// The reviewed list of operations no throwaway container can drive.
const UNREACHABLE: &str = "spec/live-coverage.toml";

pub struct Options {
    /// Override the journal directory. Defaults to [`DEFAULT_DIR`] under the workspace root.
    pub dir: Option<PathBuf>,
    /// Ceiling on live raw operations still untested.
    pub raw_budget: usize,
    /// Ceiling on live porcelain commands still untested.
    pub porcelain_budget: usize,
    /// Print the uncovered ids rather than only the counts. How you drain the budget.
    pub list: bool,
}

/// One line of a journal. `site` is carried for diagnostics only — it is what turns
/// "`repoAddTopic` is claimed but unknown" into a file and line to go and fix.
#[derive(Debug, Deserialize)]
struct Record {
    kind: String,
    id: String,
    site: String,
}

/// An operation that cannot be driven from the test topology, with the reason committed.
///
/// Modelled on `spec/name-lock.toml`: the point is not the exemption but the **reviewable
/// diff**. Adding an entry is a decision someone signs off on, and `unblock` keeps it from
/// becoming permanent by recording what would remove it.
#[derive(Debug, Deserialize)]
struct Unreachable {
    reason: String,
    unblock: String,
}

#[derive(Debug, Deserialize)]
struct UnreachableFile {
    #[serde(default)]
    unreachable: BTreeMap<String, Unreachable>,
}

pub fn run(root: &Path, opts: Options) -> Result<()> {
    let Options { dir, raw_budget, porcelain_budget, list } = opts;
    let dir = dir.unwrap_or_else(|| root.join(DEFAULT_DIR));
    if !dir.is_dir() {
        bail!(
            "no coverage journals at {}.\n\
             Coverage is recorded while the suites run, so they have to run first:\n\
             \n    mise run coverage-check\n\
             \nwhich drives the hermetic inventory and then `cargo xtask itest` with \
             GEA_COVERAGE_DIR set.",
            dir.display()
        );
    }

    let ops = load_ops(root)?;
    let porcelain = load_inventory(&dir)?;
    let unreachable = load_unreachable(root)?;

    let journals = read_journals(&dir)?;
    if journals.is_empty() {
        bail!(
            "{} exists but holds no journal lines.\n\
             That means the suites ran without recording, which happens when GEA_COVERAGE_DIR \
             was not set, or when every test skipped for want of a Gitea. Run \
             `cargo xtask itest` (it sets GEA_ITEST_REQUIRE, so a skip becomes an error).",
            dir.display()
        );
    }

    validate_ids(&journals, &ops, &porcelain)?;
    check_unreachable_are_real(&unreachable, &ops)?;

    let contract_raw = covered(&journals, Plane::Contract, "raw");
    let live_raw = covered(&journals, Plane::Live, "raw");
    let live_porcelain = covered(&journals, Plane::Live, "porcelain");

    let all_ops: BTreeSet<&str> = ops.keys().map(String::as_str).collect();
    let exempt: BTreeSet<&str> = unreachable.keys().map(String::as_str).collect();

    // Exempt operations are excluded from the denominator, not counted as covered. Counting
    // them as covered would make the live figure a claim the suite cannot support; excluding
    // them keeps the budget pure debt, so its honest target is zero.
    let live_raw_universe: BTreeSet<&str> = all_ops.difference(&exempt).copied().collect();
    let live_raw_gap = gap(&live_raw_universe, &live_raw);
    let contract_gap = gap(&all_ops, &contract_raw);

    let porcelain_universe: BTreeSet<&str> = porcelain.iter().map(String::as_str).collect();
    let live_porcelain_gap = gap(&porcelain_universe, &live_porcelain);

    print_table(Report {
        contract_raw: contract_raw.len(),
        contract_total: all_ops.len(),
        contract_gap: contract_gap.len(),
        live_raw: live_raw.len(),
        live_raw_total: live_raw_universe.len(),
        live_raw_gap: live_raw_gap.len(),
        live_porcelain: live_porcelain.len(),
        live_porcelain_total: porcelain_universe.len(),
        live_porcelain_gap: live_porcelain_gap.len(),
        unreachable: exempt.len(),
        raw_budget,
        porcelain_budget,
    });

    if list {
        list_section("contract raw", &contract_gap);
        list_section("live raw", &live_raw_gap);
        list_section("live porcelain", &live_porcelain_gap);
        for (id, u) in &unreachable {
            println!("  unreachable {id}: {} (unblock: {})", u.reason, u.unblock);
        }
    }

    let budgets = Budgets { raw: raw_budget, porcelain: porcelain_budget };
    enforce(&budgets, contract_gap.len(), live_raw_gap.len(), live_porcelain_gap.len())
}

struct Budgets {
    raw: usize,
    porcelain: usize,
}

/// Both halves of a ratchet, per `docs/ratchets.md`: over budget fails with a message about
/// fixing the code and never about raising the number; under budget prints the note that stops
/// the budget rotting into a figure nobody has looked at since the day it was written.
fn enforce(b: &Budgets, contract_gap: usize, raw_gap: usize, porcelain_gap: usize) -> Result<()> {
    let mut failed = false;

    if contract_gap > 0 {
        eprintln!(
            "error: {contract_gap} operation(s) have no request-contract check.\n\
             That plane is a loop over OPS in crates/gea/tests/raw_contract.rs, so a gap means \
             an operation was added to a skip list there. Re-run with --list to name them, then \
             make the synthesis handle them rather than excluding them."
        );
        failed = true;
    }

    if raw_gap > b.raw {
        eprintln!(
            "error: untested raw operations grew ({raw_gap} > {}).\n\
             Add a live test under crates/gea-itest/tests/ that drives the operation and \
             declares it with cover!(raw: [..]). Run with --list to name them.\n\
             Do not raise the budget.",
            b.raw
        );
        failed = true;
    } else if raw_gap < b.raw {
        println!(
            "note: the live raw gap shrank to {raw_gap}; lower the raw budget to {raw_gap} in \
             .mise/config.toml"
        );
    }

    if porcelain_gap > b.porcelain {
        eprintln!(
            "error: untested porcelain commands grew ({porcelain_gap} > {}).\n\
             Add a live test that drives the command and declares it with \
             cover!(porcelain: [..], hits: [..]). Run with --list to name them.\n\
             Do not raise the budget.",
            b.porcelain
        );
        failed = true;
    } else if porcelain_gap < b.porcelain {
        println!(
            "note: the live porcelain gap shrank to {porcelain_gap}; lower the porcelain budget \
             to {porcelain_gap} in .mise/config.toml"
        );
    }

    if failed { Err("coverage ratchet failed".into()) } else { Ok(()) }
}

struct Report {
    contract_raw: usize,
    contract_total: usize,
    contract_gap: usize,
    live_raw: usize,
    live_raw_total: usize,
    live_raw_gap: usize,
    live_porcelain: usize,
    live_porcelain_total: usize,
    live_porcelain_gap: usize,
    unreachable: usize,
    raw_budget: usize,
    porcelain_budget: usize,
}

fn print_table(r: Report) {
    println!("==> command coverage\n");
    println!(
        "  {:<9} {:<12} {:>8} {:>7} {:>6} {:>8}",
        "plane", "surface", "covered", "total", "gap", "budget"
    );
    println!(
        "  {:<9} {:<12} {:>8} {:>7} {:>6} {:>8}",
        "contract", "raw ops", r.contract_raw, r.contract_total, r.contract_gap, 0
    );
    println!(
        "  {:<9} {:<12} {:>8} {:>7} {:>6} {:>8}",
        "live", "raw ops", r.live_raw, r.live_raw_total, r.live_raw_gap, r.raw_budget
    );
    println!(
        "  {:<9} {:<12} {:>8} {:>7} {:>6} {:>8}",
        "live",
        "porcelain",
        r.live_porcelain,
        r.live_porcelain_total,
        r.live_porcelain_gap,
        r.porcelain_budget
    );
    if r.unreachable > 0 {
        println!("\n  {} operation(s) held unreachable by {UNREACHABLE}", r.unreachable);
    }
    println!();
}

fn list_section(label: &str, gap: &BTreeSet<String>) {
    if gap.is_empty() {
        return;
    }
    println!("\nuncovered ({label}):");
    for id in gap {
        println!("  {id}");
    }
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum Plane {
    Contract,
    Live,
}

/// Which plane a journal belongs to is carried by its filename rather than by a field, so the
/// hermetic writer in `crates/gea` and the live writer in `crates/gea-itest` can share one line
/// format without either having to know the other exists.
fn plane_of(name: &str) -> Option<Plane> {
    if name.starts_with("live-") {
        Some(Plane::Live)
    } else if name.starts_with("contract-") {
        Some(Plane::Contract)
    } else {
        None
    }
}

fn read_journals(dir: &Path) -> Result<Vec<(Plane, Record)>> {
    let mut out = Vec::new();
    for entry in std::fs::read_dir(dir)? {
        let entry = entry?;
        let name = entry.file_name().to_string_lossy().into_owned();
        if !name.ends_with(".jsonl") {
            continue;
        }
        let Some(plane) = plane_of(&name) else { continue };
        let text = std::fs::read_to_string(entry.path())?;
        for (n, line) in text.lines().enumerate() {
            if line.trim().is_empty() {
                continue;
            }
            let rec: Record =
                serde_json::from_str(line).map_err(|e| format!("{name} line {}: {e}", n + 1))?;
            out.push((plane, rec));
        }
    }
    Ok(out)
}

fn covered(journals: &[(Plane, Record)], plane: Plane, kind: &str) -> BTreeSet<String> {
    journals
        .iter()
        .filter(|(p, r)| *p == plane && r.kind == kind)
        .map(|(_, r)| r.id.clone())
        .collect()
}

fn gap(universe: &BTreeSet<&str>, covered: &BTreeSet<String>) -> BTreeSet<String> {
    universe.iter().filter(|id| !covered.contains(**id)).map(|id| (*id).to_owned()).collect()
}

/// A claimed id that names nothing is a hard error, not a silent zero.
///
/// This is what makes `cover!`'s `hits:` list trustworthy without a second mechanism to verify
/// it. A typo, or an operation that a spec bump renamed out from under a test, would otherwise
/// credit nothing at all while the test kept passing — the budget would look like it had grown
/// and the cause would be invisible.
fn validate_ids(
    journals: &[(Plane, Record)],
    ops: &BTreeMap<String, Entry>,
    porcelain: &BTreeSet<String>,
) -> Result<()> {
    let mut bad: Vec<String> = Vec::new();
    for (_, r) in journals {
        let known = match r.kind.as_str() {
            "raw" => ops.contains_key(&r.id),
            "porcelain" => porcelain.contains(&r.id),
            other => bail!("journal line at {} has unknown kind {other:?}", r.site),
        };
        if !known {
            bad.push(format!("  {} {:?} claimed at {}", r.kind, r.id, r.site));
        }
    }
    bad.sort();
    bad.dedup();
    if !bad.is_empty() {
        bail!(
            "these coverage claims name nothing that exists:\n{}\n\n\
             A raw id must be an operationId from spec/name-lock.toml; a porcelain id must be \
             a leaf command path such as \"pr list\". If a spec bump renamed the operation, the \
             rename is in that file's diff.",
            bad.join("\n"),
        );
    }
    Ok(())
}

/// An exemption for an operation that no longer exists is worse than no exemption: it silently
/// shrinks the denominator forever. The same argument `name_lock` makes about removals.
fn check_unreachable_are_real(
    unreachable: &BTreeMap<String, Unreachable>,
    ops: &BTreeMap<String, Entry>,
) -> Result<()> {
    let stale: Vec<&String> = unreachable.keys().filter(|id| !ops.contains_key(*id)).collect();
    if !stale.is_empty() {
        bail!(
            "{UNREACHABLE} holds entries for operations that no longer exist: {stale:?}\n\
             Delete them — an exemption for a removed operation shrinks the denominator for free."
        );
    }
    Ok(())
}

fn load_ops(root: &Path) -> Result<BTreeMap<String, Entry>> {
    let path = spec::name_lock_path(root);
    let text = std::fs::read_to_string(&path)
        .map_err(|e| format!("could not read {}: {e}", path.display()))?;
    toml::from_str(&text).map_err(|e| format!("{} is malformed: {e}", path.display()).into())
}

fn load_inventory(dir: &Path) -> Result<BTreeSet<String>> {
    let path = dir.join(INVENTORY);
    let text = std::fs::read_to_string(&path).map_err(|e| {
        format!(
            "could not read the porcelain inventory at {}: {e}\n\
             It is written by crates/gea/tests/porcelain_inventory.rs when GEA_COVERAGE_DIR is \
             set. `mise run coverage-check` runs that test first.",
            path.display()
        )
    })?;
    Ok(serde_json::from_str(&text)?)
}

fn load_unreachable(root: &Path) -> Result<BTreeMap<String, Unreachable>> {
    let path = root.join(UNREACHABLE);
    if !path.exists() {
        return Ok(BTreeMap::new());
    }
    let text = std::fs::read_to_string(&path)?;
    let parsed: UnreachableFile =
        toml::from_str(&text).map_err(|e| format!("{UNREACHABLE} is malformed: {e}"))?;
    Ok(parsed.unreachable)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn rec(kind: &str, id: &str) -> Record {
        Record { kind: kind.into(), id: id.into(), site: "tests/x.rs:1".into() }
    }

    /// Bug this prevents: journals from the two planes being unioned, so one hermetic contract
    /// run would satisfy the live budget and the Docker suite could stop running unnoticed.
    #[test]
    fn a_contract_record_never_counts_toward_live_coverage() {
        let journals =
            vec![(Plane::Contract, rec("raw", "repoGet")), (Plane::Live, rec("raw", "repoDelete"))];
        assert_eq!(covered(&journals, Plane::Live, "raw"), BTreeSet::from(["repoDelete".into()]));
        assert_eq!(covered(&journals, Plane::Contract, "raw"), BTreeSet::from(["repoGet".into()]));
    }

    /// Bug this prevents: a porcelain path landing in the raw set, which would credit 243
    /// operations that were never driven.
    #[test]
    fn the_two_kinds_do_not_bleed_into_each_other() {
        let journals =
            vec![(Plane::Live, rec("raw", "repoGet")), (Plane::Live, rec("porcelain", "pr list"))];
        assert_eq!(covered(&journals, Plane::Live, "raw").len(), 1);
        assert_eq!(covered(&journals, Plane::Live, "porcelain").len(), 1);
    }

    /// Bug this prevents: an unrecognised filename being treated as live coverage.
    #[test]
    fn only_the_two_known_journal_prefixes_are_read() {
        assert!(plane_of("live-42.jsonl").is_some());
        assert!(plane_of("contract-42.jsonl").is_some());
        assert!(plane_of("porcelain-inventory.json").is_none());
        assert!(plane_of("notes.jsonl").is_none());
    }

    /// Bug this prevents: a typo in a `hits:` list crediting nothing while the test passes, so
    /// the budget appears to grow for no visible reason.
    #[test]
    fn an_unknown_operation_id_is_refused_rather_than_ignored() {
        let ops = BTreeMap::from([(
            "repoGet".to_owned(),
            Entry { group: "repo".into(), command: "get".into(), fn_name: "get".into() },
        )]);
        let porcelain = BTreeSet::from(["pr list".to_owned()]);
        let journals = vec![(Plane::Live, rec("raw", "repoGett"))];
        let err = validate_ids(&journals, &ops, &porcelain).unwrap_err().to_string();
        assert!(err.contains("repoGett"), "{err}");
    }

    /// Bug this prevents: the gap being computed against the full operation table rather than
    /// against the table minus exemptions, which would give the budget a floor it can never
    /// drain past.
    #[test]
    fn exempt_operations_leave_the_denominator_instead_of_counting_as_covered() {
        let all = BTreeSet::from(["a", "b", "c"]);
        let exempt = BTreeSet::from(["c"]);
        let universe: BTreeSet<&str> = all.difference(&exempt).copied().collect();
        let done = BTreeSet::from(["a".to_owned()]);
        assert_eq!(gap(&universe, &done), BTreeSet::from(["b".to_owned()]));
    }
}
