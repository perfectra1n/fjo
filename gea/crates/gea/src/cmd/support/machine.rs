//! The compiled `--json` / `--jq` / `--template` triad.
//!
//! Six modules each held these same three fields and the same `pipeline()` built from them.
//! Owning the compiled [`Filter`] and [`Template`] separately from the borrowed [`Pipeline`] is
//! the reason it has to be a struct at all: a `--jq` expression is compiled **once**, before the
//! first request, and then applied to every page.
//!
//! Compiling early is not an optimisation. A typo in `--jq` has to be a usage error *before* the
//! command sends anything, or the user gets a mutation followed by a filter error.

use std::io::Write;

use gitea_core::error::Result;
use serde_json::Value;

use gitea_client::meta_types::FieldSpec as GenSpec;

use crate::global::GlobalOpts;
use crate::output::{Filter, Pipeline, Template, Term, project};
use crate::runtime::Runtime;

/// `--json`'s field selection plus the compiled `--jq` and `--template`.
#[derive(Debug, Default)]
pub struct Triad {
    fields: Option<Vec<String>>,
    filter: Option<Filter>,
    template: Option<Template>,
}

impl Triad {
    /// Compile `--jq` and `--template`, carrying an already-resolved `--json` selection.
    ///
    /// The selection is passed in rather than resolved here because *which* field table to
    /// validate against is the caller's business, and because a bare `--json` has to be answered
    /// and the command stopped — a decision a constructor cannot express.
    pub fn compile(globals: &GlobalOpts, fields: Option<Vec<String>>) -> Result<Self> {
        Ok(Self {
            fields,
            filter: globals.jq.as_deref().map(Filter::compile).transpose()?,
            template: globals.template.as_deref().map(Template::parse).transpose()?,
        })
    }

    /// Resolve `--json` against a **hand-declared** field table, then compile the rest.
    ///
    /// `Ok(None)` means bare `--json` was answered on stdout and the command is finished — field
    /// discovery short-circuits before any HTTP request, so it needs neither a token nor a
    /// network. See `docs/output.md`, divergence 2.
    ///
    /// The table is local, not generated, because `auth status`, `config list` and `status`
    /// describe **gea's own state**: there is no operation in `gitea_client::fields` to borrow
    /// names from. `docs/porcelain-conventions.md` forbids inventing field names, and this is the
    /// case it allows — the object is not the API's, so there are no API names to be faithful to.
    /// Declaring one locally keeps the two properties that matter anyway: bare `--json` lists the
    /// names, and an unknown one is a usage error carrying a suggestion instead of a silently
    /// empty column.
    pub fn for_local_table(
        globals: &GlobalOpts,
        table: &[project::FieldSpec],
    ) -> Result<Option<Self>> {
        let fields = match globals.json.as_deref() {
            None => None,
            Some(raw) => match project::resolve(raw, table)? {
                project::Selection::Discover => {
                    let term = Term::detect();
                    let mut out = std::io::stdout().lock();
                    project::write_field_list(table, &term, &mut out)?;
                    out.flush()?;
                    return Ok(None);
                }
                project::Selection::Fields(f) => Some(f),
            },
        };
        Self::compile(globals, fields).map(Some)
    }

    pub fn pipeline(&self) -> Pipeline<'_> {
        Pipeline::new()
            .fields(self.fields.as_deref())
            .jq(self.filter.as_ref())
            .template(self.template.as_ref())
    }

    /// True when the caller asked for machine-readable output, so the human view is skipped.
    ///
    /// All three flags, not just `--json`: a command that checked only `--json` would print the
    /// human table *and* the filtered document for `--jq`.
    pub fn is_explicit(&self) -> bool {
        self.fields.is_some() || self.filter.is_some() || self.template.is_some()
    }

    /// Render one document through the pipeline.
    pub fn render(&self, value: Value, term: &Term, out: &mut impl Write) -> Result<()> {
        self.pipeline().render(value, term, out)?;
        out.flush()?;
        Ok(())
    }
}

/// What the `--json`/`--jq`/`--template` triad asked for, as three states rather than two.
///
/// The two-state form ([`Triad::for_local_table`], which returns `Option`) cannot say "the user
/// asked for none of the three, render the human view" separately from "bare `--json` was already
/// answered, stop". A command that confuses those either prints a table after the field list or
/// prints nothing at all.
pub enum Wanted {
    /// No machine output: render the human view.
    Human,
    /// Machine output, with everything compiled.
    Machine(Triad),
    /// Bare `--json`: the field list has already been written to stdout. Return `Ok(())` now,
    /// without making a request.
    Listed,
}

/// Decide, and validate, before anything is requested.
///
/// `table` is the **generated** field table for the resource being printed — `--json` names are
/// the API's own, so an unknown one is a usage error carrying a suggestion rather than a silently
/// empty column.
pub fn plan(globals: &GlobalOpts, table: &'static [GenSpec]) -> Result<Wanted> {
    let fields = match globals.json.as_deref() {
        None => None,
        Some(raw) => {
            let available = super::fields::for_table(table);
            match project::resolve(raw, &available)? {
                project::Selection::Discover => {
                    let mut out = std::io::stdout().lock();
                    project::write_field_list(&available, &Term::detect(), &mut out)?;
                    out.flush()?;
                    return Ok(Wanted::Listed);
                }
                project::Selection::Fields(f) => Some(f),
            }
        }
    };
    let triad = Triad::compile(globals, fields)?;
    if triad.is_explicit() { Ok(Wanted::Machine(triad)) } else { Ok(Wanted::Human) }
}

/// Write one JSON document through a compiled triad, to wherever `--output` points.
pub fn emit(rt: &Runtime, globals: &GlobalOpts, triad: &Triad, value: Value) -> Result<()> {
    let mut out = super::writer(globals)?;
    triad.render(value, rt.term(), &mut out)
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Bare `--json` must answer on stdout and stop, without a token or a network.
    #[test]
    fn bare_json_lists_a_local_field_table_and_stops() {
        const TABLE: &[project::FieldSpec] =
            &[project::FieldSpec { name: "host", kind: project::FieldKind::Str, doc: "the host" }];
        let globals = GlobalOpts { json: Some(String::new()), ..GlobalOpts::default() };
        assert!(Triad::for_local_table(&globals, TABLE).unwrap().is_none());

        let globals = GlobalOpts { json: Some("host".to_owned()), ..GlobalOpts::default() };
        assert!(Triad::for_local_table(&globals, TABLE).unwrap().unwrap().is_explicit());

        // An unknown field is a usage error, not an empty column.
        let globals = GlobalOpts { json: Some("hots".to_owned()), ..GlobalOpts::default() };
        assert_eq!(Triad::for_local_table(&globals, TABLE).unwrap_err().exit_code(), 2);

        // None of the three flags means the human view: nothing is explicit.
        assert!(
            !Triad::for_local_table(&GlobalOpts::default(), TABLE).unwrap().unwrap().is_explicit()
        );
    }

    /// Bug this prevents: `--json head_branch` being accepted and silently producing nulls
    /// because nothing validated it against the API's own field names.
    #[test]
    fn json_field_names_are_validated_against_the_generated_table() {
        let table = gitea_client::fields::FIELDS_PULL_REQUEST;
        let g = GlobalOpts { json: Some("number,head".to_owned()), ..GlobalOpts::default() };
        assert!(matches!(plan(&g, table), Ok(Wanted::Machine(_))));
        assert!(matches!(plan(&GlobalOpts::default(), table), Ok(Wanted::Human)));

        let g = GlobalOpts { json: Some("headRefName".to_owned()), ..GlobalOpts::default() };
        let Err(e) = plan(&g, table) else { panic!("an unknown --json field must be refused") };
        assert_eq!(e.exit_code(), 2, "an unknown --json field is a usage error");
    }
}
