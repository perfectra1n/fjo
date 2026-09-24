//! `overrides.toml` — the only place a human gets to disagree with the spec.
//!
//! Every naming and typing decision the generator makes is mechanical, and mechanical rules
//! occasionally produce something inhumane (`get-o-auth2-application`) or something the spec
//! simply does not contain (the variants of a bare Go `string` type). This file is where those
//! get corrected, and it is deliberately the *only* such place:
//!
//! - **Emitters may not read it.** Overrides are applied during lowering, so the IR is the
//!   single thing emitters see. An emitter that consulted overrides could disagree with
//!   another emitter about the same name, which is precisely the class of bug this project
//!   cannot afford across 42k lines.
//! - **Overrides are data, not code.** It is `include_str!`d, so the generator has no runtime
//!   file lookup and cannot behave differently depending on where it was invoked.
//! - **Nothing here disambiguates automatically.** A name collision is a hard error that names
//!   the exact entry to add. Silent auto-disambiguation would let a spec bump quietly rename a
//!   command that users have in scripts.

use std::collections::BTreeMap;

use serde::Deserialize;

use crate::Result;

/// Parsed `overrides.toml`, compiled into the binary.
#[derive(Debug, Default, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Overrides {
    /// `operationId` → the group and command it should land under.
    #[serde(default)]
    pub op: BTreeMap<String, OpOverride>,

    /// Path parameters whose values legitimately contain `/` and must therefore **not** have
    /// it percent-encoded. See [`crate::ir::PathEncoding`].
    #[serde(default)]
    pub path_like: Vec<String>,

    /// `Model.field` → an ID newtype from `gitea_core::types::ids`.
    ///
    /// Curated rather than automatic: newtyping every `*_id` field would mint two hundred
    /// near-identical types nobody asked for. The entries that earn their keep are the ones
    /// where the API is inconsistent about `id` versus `index`, because passing the wrong one
    /// does not error — it operates on a different, real object.
    #[serde(default)]
    pub newtype: BTreeMap<String, String>,

    /// Known values for a named open enum, for definitions the spec declares as a bare
    /// `type: string` with no `enum`.
    #[serde(default)]
    pub enum_values: BTreeMap<String, Vec<String>>,

    /// `Model.field` → the name to give that field's generated open enum, when the default
    /// `<Model><Field>` is not what we want.
    #[serde(default)]
    pub enum_name: BTreeMap<String, String>,

    /// `operationId` → the token scope it really needs, when `tags[0]` plus the HTTP method
    /// derives the wrong one.
    #[serde(default)]
    pub scope: BTreeMap<String, String>,

    /// `Model.field` entries to treat as not-required even though the spec says required.
    ///
    /// Spec `required` is advisory for *deserialization*: if a server stops sending a field we
    /// marked required, a hard decode failure is a worse outcome than a defaulted value.
    #[serde(default)]
    pub demote_required: Vec<String>,

    /// Group name → one-line description, for `gea raw --help`.
    #[serde(default)]
    pub group_doc: BTreeMap<String, String>,

    /// `operationId` → the name of a generated untagged enum accepting *either* the response
    /// model the spec declares *or* a list of it.
    ///
    /// For the handful of routes whose declared response type is simply wrong because the
    /// shape depends on the request. There is one in v1.27.3: `repoGetContents` is declared to
    /// return a single `ContentsResponse`, and returns an **array** of them when the path names
    /// a directory — the operation's own summary says so ("or a list of entries if a dir")
    /// while its `responses` block does not.
    ///
    /// Overriding the type rather than patching the vendored spec keeps `spec/` a verbatim
    /// record of what upstream publishes, which is what makes `update-spec`'s sha256 check
    /// meaningful.
    #[serde(default)]
    pub one_or_many: BTreeMap<String, String>,

    /// `operationId` → `"bytes"` for routes whose success response the spec leaves untyped, or
    /// `"empty"` for routes whose spec declares a body the handler never sends, or `"json"` for
    /// routes that send a JSON body the spec does not describe.
    ///
    /// Gitea declares a few downloads — `repoGetArchive`, the job-log route — as `200` with no
    /// schema under a JSON `produces`, which lowers to `()` and throws the body away. The real
    /// response is an archive or a log, and this says which.
    #[serde(default)]
    pub response_type: BTreeMap<String, String>,
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct OpOverride {
    pub group: String,
    /// Kebab-case, as the user will type it. `fn_name` is derived from this so the CLI and the
    /// SDK cannot drift apart.
    pub command: String,
    /// Optional note explaining the override, printed by `--dump-ir`.
    #[serde(default)]
    pub why: Option<String>,
}

/// The default path-like allowlist, used when `overrides.toml` supplies none.
pub const DEFAULT_PATH_LIKE: [&str; 5] = ["filepath", "treePath", "ref", "path", "filename"];

impl Overrides {
    pub fn load() -> Result<Self> {
        let text = include_str!("overrides.toml");
        let mut o: Overrides = toml::from_str(text)
            .map_err(|e| format!("crates/xtask/src/overrides.toml is malformed: {e}"))?;
        if o.path_like.is_empty() {
            o.path_like = DEFAULT_PATH_LIKE.iter().map(|s| (*s).to_owned()).collect();
        }
        o.path_like.sort();
        o.path_like.dedup();
        Ok(o)
    }

    pub fn is_path_like(&self, param: &str) -> bool {
        self.path_like.iter().any(|p| p == param)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_committed_overrides_parse() {
        // `deny_unknown_fields` means a typo'd section name is a test failure rather than an
        // override that silently does nothing — the worst possible outcome for a file whose
        // entire job is to be consulted.
        let o = Overrides::load().unwrap();
        assert!(!o.op.is_empty());
        assert!(o.is_path_like("filepath"));
        assert!(!o.is_path_like("owner"));
    }

    #[test]
    fn every_pascal_case_operation_id_is_overridden() {
        // The 11 PascalCase ids are Actions and Git endpoints. Without overrides the mangler
        // derives their group from `tags[0]` (`repository`), burying `gea raw run view` at
        // `gea raw repo get-workflow-run`.
        const PASCAL: [&str; 11] = [
            "ActionsDisableWorkflow",
            "ActionsDispatchWorkflow",
            "ActionsEnableWorkflow",
            "ActionsGetWorkflow",
            "ActionsListRepositoryWorkflows",
            "ActionsListWorkflowRuns",
            "GetAnnotatedTag",
            "GetBlob",
            "GetTree",
            "GetWorkflowRun",
            "ListActionTasks",
        ];
        let o = Overrides::load().unwrap();
        for id in PASCAL {
            assert!(o.op.contains_key(id), "{id} needs an [op] entry in overrides.toml");
        }
    }

    #[test]
    fn override_commands_are_kebab_case() {
        // `fn_name` is derived from `command`; an accidental snake_case command would produce
        // a CLI command with an underscore in it, which is not the house style.
        let o = Overrides::load().unwrap();
        for (id, ov) in &o.op {
            assert!(
                !ov.command.contains('_'),
                "{id}: command {:?} must be kebab-case, not snake_case",
                ov.command
            );
            assert!(!ov.command.is_empty(), "{id}: empty command");
        }
    }
}
