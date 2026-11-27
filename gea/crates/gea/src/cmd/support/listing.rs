//! The output facade that owns its destination.
//!
//! Where [`super::emit::Emit`] borrows a writer, this one opens whatever `--output` selected.
//! Every entry point has a `*_to` twin that writes into an arbitrary sink, so the renderers stay
//! snapshot-testable without a [`Runtime`] — which would need a configured host and a credential
//! store.

use std::io::Write;

use gitea_core::error::Result;
use serde_json::Value;

use super::machine::Triad;
use super::{fields, note, truncation_banner, usage};
use crate::global::GlobalOpts;
use crate::output::{self, Selection, Table, Term, project};
use crate::runtime::Runtime;

/// Where a command's `--json` field table comes from.
#[derive(Debug, Clone, Copy)]
pub enum Fields {
    /// The generated table for an operation id, e.g. `"orgGetAll"`. Preferred: the names are
    /// then the API's own, and they cannot drift from the response.
    Op(&'static str),
    /// A hand-written table, for output no single operation describes — `gea workflow list`
    /// is assembled from the contents API, and `gea search topics` unwraps a response
    /// envelope. Names are still copied from the model, never invented.
    Custom(&'static [project::FieldSpec]),
    /// The command produces no JSON document at all (log text, a browser URL). `--json` on one
    /// of these is a usage error rather than an empty listing.
    None,
}

impl Fields {
    fn specs(self) -> Vec<project::FieldSpec> {
        match self {
            Self::Op(op_id) => fields::for_op(op_id),
            Self::Custom(specs) => specs.to_vec(),
            Self::None => Vec::new(),
        }
    }
}

/// Handle a bare `--json`, and validate a named field list early.
///
/// Returns `true` when the field listing was printed and the caller must return `Ok(())`
/// **without making a request**.
pub fn discover(globals: &GlobalOpts, fields: Fields) -> Result<bool> {
    let Some(raw) = globals.json.as_deref() else { return Ok(false) };
    let specs = fields.specs();
    if specs.is_empty() {
        return Err(usage("this command has no selectable --json fields. Remove --json."));
    }
    match project::resolve(raw, &specs)? {
        Selection::Discover => {
            let term = Term::detect();
            let mut out = std::io::stdout().lock();
            project::write_field_list(&specs, &term, &mut out)?;
            out.flush()?;
            Ok(true)
        }
        // Validated here so an unknown field is reported before the request rather than after
        // it: the answer does not depend on the response, and neither should the error.
        Selection::Fields(_) => Ok(false),
    }
}

/// A collection to print.
pub struct Listing<'a> {
    pub fields: Fields,
    /// The array as the API sent it, for `--json`/`--jq`/`--template`.
    pub value: Value,
    /// How many rows the human table will hold.
    pub count: usize,
    /// The collection's total size when the server told us (`total_count`), for the banner.
    pub total: Option<i64>,
    /// Plural noun for the banner and the empty note: `"runs"`, `"organizations"`.
    pub noun: &'a str,
}

/// Print a collection: machine output if asked for, else the human table.
pub fn list(
    rt: &Runtime,
    globals: &GlobalOpts,
    listing: Listing<'_>,
    build: impl FnOnce(&mut Table),
) -> Result<()> {
    let dest = output::dest_for(globals.output.as_deref());
    let mut out = output::open_dest(&dest)?;
    list_to(&mut out, rt.term(), globals, listing, build)?;
    out.flush()?;
    Ok(())
}

/// [`list`], writing into an arbitrary sink.
pub fn list_to(
    out: &mut impl Write,
    term: &Term,
    globals: &GlobalOpts,
    listing: Listing<'_>,
    build: impl FnOnce(&mut Table),
) -> Result<()> {
    let triad = compile(globals, listing.fields)?;
    if triad.is_explicit() {
        // An empty collection is `[]` and exit 0, never an error: `if gea run list --json id`
        // must test reachability, not emptiness.
        return triad.pipeline().render(listing.value, term, out);
    }

    let mut table = Table::new(term);
    build(&mut table);
    let total = listing.total.and_then(|t| usize::try_from(t).ok());
    if let Some(banner) = truncation_banner(listing.count, total, listing.noun) {
        table.banner(banner);
    }
    table.render(out)?;
    if listing.count == 0 {
        note(term, &format!("no {} found", listing.noun));
    }
    Ok(())
}

/// Print one object: machine output if asked for, else an aligned label/value block.
///
/// The human form is a headerless [`Table`], not hand-rolled `{:<12}` formatting, so that it
/// pads on a terminal and becomes TSV in a pipe like every other view.
pub fn detail(
    rt: &Runtime,
    globals: &GlobalOpts,
    fields: Fields,
    value: Value,
    rows: Vec<(String, String)>,
) -> Result<()> {
    let dest = output::dest_for(globals.output.as_deref());
    let mut out = output::open_dest(&dest)?;
    detail_to(&mut out, rt.term(), globals, fields, value, rows)?;
    out.flush()?;
    Ok(())
}

pub fn detail_to(
    out: &mut impl Write,
    term: &Term,
    globals: &GlobalOpts,
    fields: Fields,
    value: Value,
    rows: Vec<(String, String)>,
) -> Result<()> {
    let triad = compile(globals, fields)?;
    if triad.is_explicit() {
        return triad.pipeline().render(value, term, out);
    }
    let mut table = Table::new(term);
    for (label, v) in rows {
        table.row([label, v]);
    }
    table.render(out)?;
    Ok(())
}

/// Print text that is not a JSON document — job logs, a URL.
///
/// `--json`/`--jq`/`--template` are refused rather than ignored, with the same wording layer 2
/// uses: a filter that silently does nothing is worse than one that says it cannot apply.
pub fn text(rt: &Runtime, globals: &GlobalOpts, body: &str) -> Result<()> {
    if globals.wants_machine_output() {
        return Err(usage(
            "this command returns plain text; --json, --jq, and --template are not supported",
        ));
    }
    let dest = output::dest_for(globals.output.as_deref());
    let mut out = output::open_dest(&dest)?;
    // Verbatim, with no trailing newline added: log text already ends with one, and appending
    // a byte to a file someone redirected would corrupt a byte-for-byte comparison.
    out.write_all(body.as_bytes())?;
    out.flush()?;
    let _ = rt;
    Ok(())
}

/// Resolve `--json` against `fields` and compile the rest of the triad.
fn compile(globals: &GlobalOpts, fields: Fields) -> Result<Triad> {
    let selected = match globals.json.as_deref() {
        None => None,
        Some(raw) => match project::resolve(raw, &fields.specs())? {
            // Unreachable in the binary: `discover` already handled and returned. Treating it
            // as "no projection" rather than panicking keeps a direct caller (a test) honest
            // instead of crashing.
            Selection::Discover => None,
            Selection::Fields(f) => Some(f),
        },
    };
    Triad::compile(globals, selected)
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn globals(json: Option<&str>) -> GlobalOpts {
        GlobalOpts { json: json.map(str::to_owned), ..GlobalOpts::default() }
    }

    fn render(term: &Term, g: &GlobalOpts) -> String {
        let listing = Listing {
            fields: Fields::Op("orgGetAll"),
            value: json!([{"username": "acme", "visibility": "public"}]),
            count: 1,
            total: Some(4),
            noun: "organizations",
        };
        let mut buf = Vec::new();
        list_to(&mut buf, term, g, listing, |t| {
            t.headers(["NAME", "VISIBILITY"]);
            t.row(["acme", "public"]);
        })
        .unwrap();
        String::from_utf8(buf).unwrap()
    }

    /// Bug this prevents: an op id in one of the command modules being a typo (or renamed by a
    /// spec bump), which would silently turn `--json` into "no selectable fields" for that
    /// command only — an error nobody sees until a user tries the flag.
    #[test]
    fn every_op_id_these_groups_name_has_a_field_table() {
        for op_id in [
            "GetWorkflowRun",
            "getWorkflowJob",
            "getArtifact",
            "getRepoRunner",
            "repoListActionsSecrets",
            "getRepoVariablesList",
            "getRepoVariable",
            "orgGetAll",
            "orgGet",
            "orgListMembers",
            "orgListTeams",
            "orgGetTeam",
            "orgListTeamMembers",
            "orgListTeamRepos",
            "userGet",
            "userCurrentListKeys",
            "userCurrentListGPGKeys",
            "userGetTokens",
            "userCreateToken",
            "userCurrentListStarred",
            "notifyGetList",
            "repoGet",
            "issueSearchIssues",
            "ActionsDispatchWorkflow",
            "repoGetContentsList",
        ] {
            assert!(!fields::for_op(op_id).is_empty(), "no field table for {op_id:?}");
        }
    }

    /// The banner is a terminal-only, "there is more than this" hint. In a pipe it would be a
    /// bogus first record for `cut -f1`.
    #[test]
    fn the_banner_appears_only_on_a_terminal_and_only_when_truncated() {
        let out = render(&Term::tty(80), &globals(None));
        assert!(out.starts_with("Showing 1 of 4 organizations\n"), "{out}");
        assert!(!render(&Term::piped(), &globals(None)).contains("Showing"));
        assert_eq!(truncation_banner(0, Some(0), "organizations"), None);
    }

    /// Bug this prevents: printing the human table *and* the machine output, which is what
    /// happens when a command checks only `--json` and forgets `--jq`/`--template`.
    #[test]
    fn machine_output_replaces_the_table_for_all_three_flags() {
        assert!(!render(&Term::piped(), &globals(Some("username"))).contains("NAME"));
        let g = GlobalOpts { jq: Some(".[].username".to_owned()), ..GlobalOpts::default() };
        assert_eq!(render(&Term::piped(), &g), "acme\n");
        let g = GlobalOpts {
            template: Some("{{range .}}{{.username}}{{end}}".to_owned()),
            ..GlobalOpts::default()
        };
        assert_eq!(render(&Term::piped(), &g), "acme");
    }

    /// An empty collection is success: `[]` under `--json`, an empty table otherwise.
    #[test]
    fn an_empty_collection_is_not_an_error() {
        let mut buf = Vec::new();
        let listing = Listing {
            fields: Fields::Op("orgGetAll"),
            value: json!([]),
            count: 0,
            total: None,
            noun: "organizations",
        };
        list_to(&mut buf, &Term::piped(), &globals(Some("username")), listing, |_| {}).unwrap();
        assert_eq!(String::from_utf8(buf).unwrap(), "[]\n");
    }

    #[test]
    fn bare_json_lists_fields_and_an_unknown_field_is_a_usage_error() {
        assert!(discover(&globals(Some("")), Fields::Op("orgGetAll")).unwrap());
        assert!(!discover(&globals(Some("username")), Fields::Op("orgGetAll")).unwrap());
        let e = discover(&globals(Some("usrname")), Fields::Op("orgGetAll")).unwrap_err();
        assert_eq!(e.exit_code(), 2);
        // A command with no JSON at all must say so rather than offering an empty list.
        let e = discover(&globals(Some("")), Fields::None).unwrap_err();
        assert_eq!(e.exit_code(), 2);
    }
}
