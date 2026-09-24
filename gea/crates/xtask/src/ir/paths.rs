//! Path template tokenization.
//!
//! A path template becomes a `Vec<Chunk>` of literals and parameters, so the client emitter can
//! build a URL by concatenation with **per-parameter** encoding, rather than doing string
//! replacement on `{name}` at runtime.
//!
//! The two templates that make this necessary rather than pedantic:
//!
//! ```text
//! /repos/{owner}/{repo}/pulls/{index}.{diffType}
//! /repos/{owner}/{repo}/git/commits/{sha}.{diffType}
//! ```
//!
//! Two parameters share one path segment, separated by a literal `.`. A naive renderer that
//! splits the template on `/` sees `{index}.{diffType}` as one segment and either encodes the
//! whole thing or — worse — treats `index.diffType` as a parameter name and silently produces
//! `/pulls/.` . Tokenizing to `[… Param(index), Lit("."), Param(diff_type)]` makes the dot a
//! separator by construction, and both paths are named in the tests below so nobody can
//! "simplify" this back into a split on `/`.

use serde::Serialize;

use crate::Result;
use crate::ir::names::{Ident, ident_snake};

#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub enum Chunk {
    Lit(String),
    Param(Ident),
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct PathTemplate {
    /// The template exactly as the spec writes it. Kept for error messages, `--dry-run` output
    /// and layer-2's search index, all of which want what the API docs say.
    pub raw: String,
    pub chunks: Vec<Chunk>,
    /// Parameter wire names in path order. Path parameters become positional arguments in this
    /// order, so it is part of the generated public API.
    pub params: Vec<String>,
}

impl PathTemplate {
    pub fn param_idents(&self) -> Vec<&Ident> {
        self.chunks
            .iter()
            .filter_map(|c| match c {
                Chunk::Param(i) => Some(i),
                Chunk::Lit(_) => None,
            })
            .collect()
    }
}

/// Splits a path template into literal and parameter chunks.
///
/// Rejects unbalanced braces and empty parameter names rather than guessing. A malformed
/// template would otherwise produce a URL that 404s at runtime, which is a much longer path to
/// the same information.
pub fn tokenize(raw: &str) -> Result<PathTemplate> {
    let mut chunks = Vec::new();
    let mut params = Vec::new();
    let mut lit = String::new();
    let mut rest = raw;

    while let Some(open) = rest.find('{') {
        lit.push_str(&rest[..open]);
        let after = &rest[open + 1..];
        let Some(close) = after.find('}') else {
            bail!("path template {raw:?} has an unclosed '{{'");
        };
        let name = &after[..close];
        if name.is_empty() {
            bail!("path template {raw:?} contains an empty parameter name '{{}}'");
        }
        if name.contains('{') {
            bail!("path template {raw:?} has a nested '{{' inside a parameter name");
        }

        if !lit.is_empty() {
            chunks.push(Chunk::Lit(std::mem::take(&mut lit)));
        }
        chunks.push(Chunk::Param(ident_snake(name)));
        params.push(name.to_owned());
        rest = &after[close + 1..];
    }

    if rest.contains('}') {
        bail!("path template {raw:?} has a '}}' with no matching '{{'");
    }
    lit.push_str(rest);
    if !lit.is_empty() {
        chunks.push(Chunk::Lit(lit));
    }

    Ok(PathTemplate { raw: raw.to_owned(), chunks, params })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn lit(s: &str) -> Chunk {
        Chunk::Lit(s.to_owned())
    }
    fn param(s: &str) -> Chunk {
        Chunk::Param(Ident::new(s))
    }

    #[test]
    fn ordinary_path() {
        let t = tokenize("/repos/{owner}/{repo}/pulls").unwrap();
        assert_eq!(
            t.chunks,
            vec![lit("/repos/"), param("owner"), lit("/"), param("repo"), lit("/pulls")]
        );
        assert_eq!(t.params, ["owner", "repo"]);
    }

    /// Named after the path, because it is one of exactly two in the spec that break a naive
    /// renderer. `gea raw repo download-pull-diff-or-patch o r 1 diff` must produce
    /// `/pulls/1.diff`, not `/pulls/1%2Ediff` and not `/pulls/1.{diffType}`.
    #[test]
    fn repos_owner_repo_pulls_index_dot_difftype() {
        let t = tokenize("/repos/{owner}/{repo}/pulls/{index}.{diffType}").unwrap();
        assert_eq!(
            t.chunks,
            vec![
                lit("/repos/"),
                param("owner"),
                lit("/"),
                param("repo"),
                lit("/pulls/"),
                param("index"),
                // The dot is a separator chunk, which is the entire point.
                lit("."),
                param("diff_type"),
            ]
        );
        assert_eq!(t.params, ["owner", "repo", "index", "diffType"]);
    }

    /// The second dotted path. Same shape, different prefix; both are tested by name so a
    /// refactor cannot quietly handle one and break the other.
    #[test]
    fn repos_owner_repo_git_commits_sha_dot_difftype() {
        let t = tokenize("/repos/{owner}/{repo}/git/commits/{sha}.{diffType}").unwrap();
        assert_eq!(
            t.chunks,
            vec![
                lit("/repos/"),
                param("owner"),
                lit("/"),
                param("repo"),
                lit("/git/commits/"),
                param("sha"),
                lit("."),
                param("diff_type"),
            ]
        );
    }

    #[test]
    fn hyphenated_wire_names_become_snake_case_idents() {
        // `/user/{user-id}` and friends: the wire name is not a legal Rust identifier.
        let t = tokenize("/activitypub/user-id/{user-id}/inbox").unwrap();
        assert_eq!(t.param_idents().iter().map(|i| i.as_str()).collect::<Vec<_>>(), ["user_id"]);
        assert_eq!(t.params, ["user-id"], "the wire name is preserved for URL building");
    }

    #[test]
    fn a_path_with_no_parameters_is_one_literal() {
        let t = tokenize("/version").unwrap();
        assert_eq!(t.chunks, vec![lit("/version")]);
        assert!(t.params.is_empty());
    }

    #[test]
    fn malformed_templates_are_errors_not_guesses() {
        assert!(tokenize("/repos/{owner").is_err());
        assert!(tokenize("/repos/owner}").is_err());
        assert!(tokenize("/repos/{}/x").is_err());
    }
}
