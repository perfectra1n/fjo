//! The one adapter from the generated field tables to [`crate::output::project`].
//!
//! `gitea_client::meta_types::FieldSpec` and [`crate::output::project::FieldSpec`] are
//! structurally identical and deliberately distinct: `gea::output` is written against no
//! generated code at all, so that field discovery keeps working — and keeps being testable —
//! in a build where the generated crate is absent. One conversion is the whole price of that
//! independence.
//!
//! There used to be five copies of it (six counting layer 2's). They had **diverged**, and the
//! divergence was visible to users in the bare-`--json` listing: three rendered a `Vec<String>`
//! field as `[string]`, two as `[json]`, and one flattened both arrays and maps to `json`. The
//! recursive form kept here is the one `crate::raw` uses, so layer 2 and layer 3 now describe
//! the same field the same way.

use gitea_client::meta_types::{self, FieldSpec as GenSpec};

use crate::output::project;

/// The generated field table for an operation id, e.g. `"repoListHooks"`.
///
/// An unknown id yields an empty table rather than an error: the caller decides whether that is
/// "this command has no JSON document" (a usage error) or simply nothing to select.
pub fn for_op(op_id: &str) -> Vec<project::FieldSpec> {
    let table = gitea_client::fields::OP_FIELDS
        .binary_search_by(|(id, _)| (*id).cmp(op_id))
        .map(|i| gitea_client::fields::OP_FIELDS[i].1)
        .unwrap_or(&[]);
    for_table(table)
}

/// The generated field table for a resource, e.g. `fields::FIELDS_PULL_REQUEST`.
///
/// Keyed by the table rather than by an operation id because a porcelain command usually knows
/// which resource it prints, while the operation it called may return an envelope around it.
pub fn for_table(table: &'static [GenSpec]) -> Vec<project::FieldSpec> {
    table
        .iter()
        .map(|f| project::FieldSpec { name: f.name, kind: kind_of(&f.kind), doc: f.doc })
        .collect()
}

fn kind_of(kind: &'static meta_types::FieldKind) -> project::FieldKind {
    use meta_types::FieldKind as G;
    use project::FieldKind as O;
    match kind {
        G::Bool => O::Bool,
        G::Int => O::Int,
        G::Float => O::Float,
        G::Str => O::Str,
        G::DateTime => O::DateTime,
        G::Enum(values) => O::Enum(values),
        // Only top-level fields are selectable, so a nested table would never be read; the
        // listing needs the *label* ("object"), which does not depend on it.
        G::Object(_) => O::Object(&[]),
        G::Array(inner) => O::Array(inner_of(inner)),
        G::Map(inner) => O::Map(inner_of(inner)),
        G::Json => O::Json,
    }
}

/// The element kind of an array or map, for a label like `[string]`.
///
/// A composite element collapses to `json`, matching `raw.rs`: `[object]` would suggest the
/// elements can be selected with `--json`, and they cannot — that is `--jq`'s job.
fn inner_of(kind: &'static meta_types::FieldKind) -> &'static project::FieldKind {
    use meta_types::FieldKind as G;
    use project::FieldKind as O;
    match kind {
        G::Bool => &O::Bool,
        G::Int => &O::Int,
        G::Float => &O::Float,
        G::Str => &O::Str,
        G::DateTime => &O::DateTime,
        _ => &O::Json,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Bug this prevents: field discovery reaching for a table that is not there, so a command
    /// silently claims it has no selectable fields.
    #[test]
    fn a_real_operation_has_a_real_field_table() {
        assert!(for_op("repoListHooks").iter().any(|f| f.name == "events"));
        assert!(for_op("repoListKeys").iter().any(|f| f.name == "read_only"));
        assert!(for_op("adminSearchUsers").iter().any(|f| f.name == "is_admin"));
        assert!(for_op("adminCronList").iter().any(|f| f.name == "schedule"));
        assert!(for_op("notAnOperation").is_empty());
    }

    /// The divergence this file was written to end: a `Vec<String>` field must describe itself
    /// as `[string]`, not `[json]` and not `json`. Three of the six copies got this right and
    /// three did not, so the same field was documented differently depending on which group's
    /// command you asked.
    #[test]
    fn an_array_of_strings_says_so_rather_than_collapsing_to_json() {
        let hooks = for_op("repoListHooks");
        let events = hooks.iter().find(|f| f.name == "events").expect("events");
        assert_eq!(events.kind.label(), "[string]");
    }

    /// Bug this prevents: the adapter panicking on a field kind the generator emits, on a
    /// `--json` that only asked what exists.
    #[test]
    fn every_pull_request_field_kind_has_a_label() {
        for f in for_table(gitea_client::fields::FIELDS_PULL_REQUEST) {
            assert!(!f.kind.label().is_empty(), "{}", f.name);
        }
    }
}
