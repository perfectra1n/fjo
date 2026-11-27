//! The output facade that borrows its writer.
//!
//! Borrowing the writer is what makes a command testable: a unit test builds an [`Emit`] over a
//! `Vec<u8>` and asserts on bytes, with no process and no terminal. The other facade,
//! [`super::listing`], opens `--output`'s destination itself; see the module docs on
//! [`super`] for why both exist.

use std::io::Write;

use gitea_core::error::Result;
use serde::Serialize;
use serde_json::Value;

use super::machine::Triad;
use super::{banner, fields, note, usage};
use crate::global::GlobalOpts;
use crate::output::{self, Selection, Table, project};

/// What `--json` resolved to.
#[derive(Debug)]
pub enum Json {
    /// `--json` was absent, or named fields that validated. Carry them into the pipeline.
    Fields(Option<Vec<String>>),
    /// Bare `--json`: the field list has been printed and the command is finished.
    Listed,
}

impl Json {
    /// Validate `--json` against `op_id`'s generated field table, printing the list for a bare
    /// `--json`.
    ///
    /// Call this **first**, before the runtime exists. `op_id` is the operation whose response
    /// shape the command emits — which is not always the operation it calls: `gea admin repo
    /// list` calls `repoSearch` but emits the repositories inside `data`, so it passes `repoGet`.
    pub fn resolve(globals: &GlobalOpts, op_id: &str) -> Result<Self> {
        let Some(raw) = globals.json.as_deref() else { return Ok(Self::Fields(None)) };
        let fields = fields::for_op(op_id);
        if fields.is_empty() {
            return Err(usage(format!(
                "no selectable --json fields for `{op_id}`. Use --jq to filter the response."
            )));
        }
        match project::resolve(raw, &fields)? {
            Selection::Discover => {
                let term = output::Term::detect();
                let mut out = std::io::stdout().lock();
                project::write_field_list(&fields, &term, &mut out)?;
                out.flush()?;
                Ok(Self::Listed)
            }
            Selection::Fields(f) => Ok(Self::Fields(Some(f))),
        }
    }
}

/// One command's output channel: the compiled `--json`/`--jq`/`--template` triad, the terminal
/// description, and somewhere to write.
pub struct Emit<'a> {
    term: output::Term,
    triad: Triad,
    out: &'a mut dyn Write,
}

impl<'a> Emit<'a> {
    /// Compile the output flags. Fails on a bad `--jq` or `--template` **before** the command
    /// sends anything, so a typo in a filter is a usage error rather than a mutation plus an
    /// error.
    pub fn new(
        globals: &GlobalOpts,
        fields: Option<Vec<String>>,
        term: &output::Term,
        out: &'a mut dyn Write,
    ) -> Result<Self> {
        Ok(Self { term: *term, triad: Triad::compile(globals, fields)?, out })
    }

    pub fn term(&self) -> &output::Term {
        &self.term
    }

    /// True when the caller asked for machine-readable output, so the human view is skipped.
    pub fn machine(&self) -> bool {
        self.triad.is_explicit()
    }

    /// A new [`Table`] carrying this command's terminal description.
    pub fn table(&self) -> Table {
        Table::new(&self.term)
    }

    /// Emit one document: through the pipeline if the caller asked for machine output, otherwise
    /// as whatever `human` renders.
    pub fn one<T: Serialize>(&mut self, value: &T, human: impl FnOnce(&mut Table)) -> Result<()> {
        if self.machine() {
            return self.json(value);
        }
        let mut table = self.table();
        human(&mut table);
        table.render(&mut self.out)?;
        self.out.flush()?;
        Ok(())
    }

    /// Emit a collection, with the `Showing N of M` banner and the empty-set contract.
    ///
    /// An empty list is **exit 0**: an empty JSON array, or an empty table plus a one-line note
    /// on stderr for a terminal. `if gea webhook list` must test reachability, not emptiness.
    pub fn many<T: Serialize>(
        &mut self,
        items: &[T],
        total: Option<u64>,
        noun: &str,
        human: impl FnOnce(&mut Table, &[T]),
    ) -> Result<()> {
        if self.machine() {
            return self.json(&items);
        }
        if items.is_empty() {
            note(&self.term, &format!("no {noun}"));
            return Ok(());
        }
        let mut table = self.table();
        if self.term.tty {
            table.banner(banner(items.len(), total, noun));
        }
        human(&mut table, items);
        table.render(&mut self.out)?;
        self.out.flush()?;
        Ok(())
    }

    /// The machine path on its own, for the rare response that has no sensible table (a bare
    /// array of strings).
    pub fn json<T: Serialize>(&mut self, value: &T) -> Result<()> {
        let value = serde_json::to_value(value).map_err(|e| {
            // A generated model that will not serialise is a codegen bug, not user error, and
            // saying so beats a bare serde message.
            usage(format!("could not serialise the response for output: {e}"))
        })?;
        self.render_machine(value)
    }

    /// The pipeline half, with the borrows split by field.
    ///
    /// `Triad::pipeline` borrows `self.triad` immutably (it holds references to the compiled
    /// filter and template) while writing needs `self.out` mutably, so the two have to be taken
    /// as disjoint field borrows rather than through `&self`/`&mut self`.
    fn render_machine(&mut self, value: Value) -> Result<()> {
        let Self { term, triad, out } = self;
        triad.render(value, term, out)
    }

    /// Emit an already-built [`Value`], for the commands that assemble their own view.
    pub fn value(&mut self, value: Value, human: impl FnOnce(&mut Table)) -> Result<()> {
        if self.machine() {
            return self.render_machine(value);
        }
        let mut table = self.table();
        human(&mut table);
        table.render(&mut self.out)?;
        self.out.flush()?;
        Ok(())
    }

    /// A one-line confirmation of a mutation, on stderr, terminal only.
    ///
    /// stderr because stdout is the machine channel: `gea topic add rust --json topics | jq`
    /// must not receive prose. Terminal only because a script does not read it.
    pub fn done(&self, message: &str) {
        note(&self.term, message);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Bug this prevents: `--json nosuchfield` reaching the network and failing at the server, or
    /// bare `--json` on a typed operation being treated as a usage error.
    #[test]
    fn json_resolution_lists_validates_and_refuses() {
        let listed = GlobalOpts { json: Some(String::new()), ..GlobalOpts::default() };
        assert!(matches!(Json::resolve(&listed, "repoListHooks"), Ok(Json::Listed)));

        let named = GlobalOpts { json: Some("id,active".into()), ..GlobalOpts::default() };
        let Ok(Json::Fields(Some(f))) = Json::resolve(&named, "repoListHooks") else {
            panic!("named fields should resolve")
        };
        assert_eq!(f, vec!["id".to_owned(), "active".to_owned()]);

        let wrong = GlobalOpts { json: Some("activ".into()), ..GlobalOpts::default() };
        assert_eq!(Json::resolve(&wrong, "repoListHooks").unwrap_err().exit_code(), 2);

        // An operation with no typed response must say so and point at `--jq`.
        let e = Json::resolve(&listed, "adminUnadoptedList").unwrap_err();
        assert!(e.to_string().contains("--jq"), "{e}");
    }

    /// Bug this prevents: an empty result set being an error, or printing a header-only table
    /// into a pipe where it becomes a bogus record.
    #[test]
    fn an_empty_list_is_success_and_writes_nothing_to_stdout() {
        let mut buf: Vec<u8> = Vec::new();
        let mut e =
            Emit::new(&GlobalOpts::default(), None, &output::Term::piped(), &mut buf).unwrap();
        e.many::<Value>(&[], Some(0), "webhooks", |_, _| unreachable!()).unwrap();
        assert!(buf.is_empty(), "{buf:?}");

        // ...and `[]` under `--json`, which is what a script tests.
        let g = GlobalOpts { json: Some("id".into()), ..GlobalOpts::default() };
        let mut buf: Vec<u8> = Vec::new();
        let mut e =
            Emit::new(&g, Some(vec!["id".into()]), &output::Term::piped(), &mut buf).unwrap();
        e.many::<Value>(&[], None, "webhooks", |_, _| unreachable!()).unwrap();
        assert_eq!(String::from_utf8(buf).unwrap(), "[]\n");
    }
}
