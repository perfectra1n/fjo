//! `cargo xtask spec-stats` — a self-test of the spec loader.
//!
//! This is not a report. Every number below was established by independent inspection of
//! `spec/gitea-v1.27.2.json` before any loader existed, and [`Stats::verify`] asserts the
//! loader reproduces them. **If the numbers disagree, the loader is wrong**, not the
//! expectations.
//!
//! The reason to spend a milestone on this: 42k lines of generated code inherit whatever the
//! traversal does. A traversal that misses array `items` undercounts `int64` by six, which
//! sounds harmless until six fields deserialize as `i32` and silently truncate a Gitea
//! database id. Miscounts are cheap to find here and expensive to find anywhere later.
//!
//! ## Counting rules
//!
//! Stated explicitly, because "how many `int64` properties are there" has several defensible
//! answers and only one of them is ours:
//!
//! - **operations** — every `(path, method)` pair with an `operationId`.
//! - **tags** — distinct tags used by operations. The spec declares no top-level `tags` array.
//! - **path/query/body/formData params** — parameters on operations, counted per operation
//!   (so a shared parameter used by ten operations counts ten times, which is what matters
//!   for "how many flags will layer 2 have").
//! - **enum / date-time / int64 / uint64** — schema nodes anywhere inside `definitions`,
//!   found by [`crate::swagger::Schema::walk`]. This includes array `items`, map values, and
//!   definitions that are themselves a scalar alias (`Duration`, `TimeStamp`). Counting only
//!   top-level properties undercounts int64 against the 174 this reports.
//! - **defs with required** — definitions declaring a non-empty `required` list, top level
//!   only. Only 38 of 222 do, which is why nearly every generated field is `#[serde(default)]`.

use std::collections::BTreeMap;

use crate::Result;
use crate::swagger::{ParamIn, Spec};

/// The version these expectations were verified against.
pub const EXPECTED_VERSION: &str = "1.27.3";

pub struct Stats {
    pub paths: usize,
    pub operations: usize,
    pub definitions: usize,
    pub tags: usize,
    pub path_params: usize,
    pub query_params: usize,
    pub body_params: usize,
    pub form_data_params: usize,
    pub enum_properties: usize,
    pub date_time_props: usize,
    pub int64_props: usize,
    pub uint64_props: usize,
    pub defs_with_required: usize,
    /// Must be zero. "No polymorphism anywhere" is the assumption that makes a hand-rolled
    /// generator tractable; the moment it stops holding, codegen must stop too.
    pub polymorphism: usize,
    pub tag_counts: BTreeMap<String, usize>,
}

/// The independently verified counts for Gitea v1.27.3.
struct Expected {
    paths: usize,
    operations: usize,
    definitions: usize,
    tags: usize,
    path_params: usize,
    query_params: usize,
    body_params: usize,
    form_data_params: usize,
    enum_properties: usize,
    date_time_props: usize,
    int64_props: usize,
    uint64_props: usize,
    defs_with_required: usize,
    polymorphism: usize,
    tag_counts: &'static [(&'static str, usize)],
}

const EXPECTED: Expected = Expected {
    paths: 308,
    operations: 482,
    definitions: 222,
    tags: 9,
    path_params: 956,
    query_params: 412,
    body_params: 121,
    form_data_params: 3,
    enum_properties: 37,
    date_time_props: 101,
    int64_props: 174,
    uint64_props: 2,
    defs_with_required: 38,
    polymorphism: 0,
    tag_counts: &[
        ("admin", 32),
        ("issue", 72),
        ("miscellaneous", 14),
        ("notification", 7),
        ("organization", 67),
        ("package", 9),
        ("repository", 202),
        ("settings", 4),
        ("user", 76),
    ],
};

impl Stats {
    pub fn compute(spec: &Spec) -> Self {
        let mut s = Stats {
            paths: spec.paths.len(),
            operations: 0,
            definitions: spec.definitions.len(),
            tags: 0,
            path_params: 0,
            query_params: 0,
            body_params: 0,
            form_data_params: 0,
            enum_properties: 0,
            date_time_props: 0,
            int64_props: 0,
            uint64_props: 0,
            defs_with_required: 0,
            polymorphism: 0,
            tag_counts: BTreeMap::new(),
        };

        for item in spec.paths.values() {
            for (_method, op) in item.operations() {
                s.operations += 1;
                for tag in &op.tags {
                    *s.tag_counts.entry(tag.clone()).or_default() += 1;
                }
                for p in item.parameters.iter().chain(&op.parameters) {
                    match p.location {
                        ParamIn::Path => s.path_params += 1,
                        ParamIn::Query => s.query_params += 1,
                        ParamIn::Body => s.body_params += 1,
                        ParamIn::FormData => s.form_data_params += 1,
                        ParamIn::Header => {}
                    }
                    if let Some(schema) = &p.schema {
                        s.polymorphism +=
                            schema.walk().iter().map(|n| n.polymorphism_keys()).sum::<usize>();
                    }
                }
            }
        }
        s.tags = s.tag_counts.len();

        for def in spec.definitions.values() {
            if !def.required.is_empty() {
                s.defs_with_required += 1;
            }
            for node in def.walk() {
                if node.enum_values.is_some() {
                    s.enum_properties += 1;
                }
                match node.format.as_deref() {
                    Some("date-time") => s.date_time_props += 1,
                    Some("int64") => s.int64_props += 1,
                    Some("uint64") => s.uint64_props += 1,
                    _ => {}
                }
                s.polymorphism += node.polymorphism_keys();
            }
        }
        for resp in spec.responses.values() {
            if let Some(schema) = &resp.schema {
                s.polymorphism +=
                    schema.walk().iter().map(|n| n.polymorphism_keys()).sum::<usize>();
            }
        }

        s
    }

    pub fn render(&self) -> String {
        let mut out = String::new();
        out.push_str(&row(&[
            ("paths", self.paths),
            ("operations", self.operations),
            ("definitions", self.definitions),
        ]));
        out.push_str(&row(&[
            ("tags", self.tags),
            ("path params", self.path_params),
            ("query params", self.query_params),
        ]));
        out.push_str(&row(&[
            ("body params", self.body_params),
            ("formData params", self.form_data_params),
            ("enum properties", self.enum_properties),
        ]));
        out.push_str(&row(&[
            ("date-time props", self.date_time_props),
            ("int64 props", self.int64_props),
            ("uint64 props", self.uint64_props),
        ]));
        out.push_str(&format!(
            "{}      {}\n",
            cell("defs with required", self.defs_with_required, 20),
            cell("allOf/oneOf/anyOf/not/discriminator", self.polymorphism, 35),
        ));

        out.push_str("\noperations per tag\n");
        // Sorted by count descending, then name, so the shape of the API is readable at a
        // glance and the output is still deterministic.
        let mut by_count: Vec<(&str, usize)> =
            self.tag_counts.iter().map(|(k, v)| (k.as_str(), *v)).collect();
        by_count.sort_by(|a, b| b.1.cmp(&a.1).then(a.0.cmp(b.0)));
        for (tag, n) in by_count {
            out.push_str(&format!("  {}\n", cell(tag, n, 20)));
        }
        out
    }

    /// The self-test. Collects *every* mismatch before failing, because a broken traversal
    /// usually breaks several counts at once and fixing them one error message at a time is
    /// needlessly slow.
    pub fn verify(&self, version: &str) -> Result<()> {
        if version != EXPECTED_VERSION {
            bail!(
                "the verified counts in xtask/src/stats.rs are for Gitea {EXPECTED_VERSION}, \
                 but the vendored spec is {version}.\n  \
                 Inspect the new specification and update the expected counts in \
                 crates/xtask/src/stats.rs before continuing."
            );
        }

        let mut bad: Vec<String> = Vec::new();
        let mut check = |what: &str, got: usize, want: usize| {
            if got != want {
                bad.push(format!("  {what:<36} expected {want:>5}, got {got:>5}"));
            }
        };
        check("paths", self.paths, EXPECTED.paths);
        check("operations", self.operations, EXPECTED.operations);
        check("definitions", self.definitions, EXPECTED.definitions);
        check("tags", self.tags, EXPECTED.tags);
        check("path params", self.path_params, EXPECTED.path_params);
        check("query params", self.query_params, EXPECTED.query_params);
        check("body params", self.body_params, EXPECTED.body_params);
        check("formData params", self.form_data_params, EXPECTED.form_data_params);
        check("enum properties", self.enum_properties, EXPECTED.enum_properties);
        check("date-time props", self.date_time_props, EXPECTED.date_time_props);
        check("int64 props", self.int64_props, EXPECTED.int64_props);
        check("uint64 props", self.uint64_props, EXPECTED.uint64_props);
        check("defs with required", self.defs_with_required, EXPECTED.defs_with_required);
        check("allOf/oneOf/anyOf/not/discriminator", self.polymorphism, EXPECTED.polymorphism);
        for (tag, want) in EXPECTED.tag_counts {
            check(&format!("tag {tag}"), self.tag_counts.get(*tag).copied().unwrap_or(0), *want);
        }
        for tag in self.tag_counts.keys() {
            if !EXPECTED.tag_counts.iter().any(|(t, _)| t == tag) {
                bad.push(format!("  tag {tag:<32} unexpected: not in the verified set"));
            }
        }

        if bad.is_empty() {
            eprintln!("verified: all counts match the independently confirmed values");
            return Ok(());
        }
        bail!(
            "the spec loader disagrees with the independently verified counts:\n{}\n\
             \n  These numbers are the expectation and the loader is the suspect. Before \
             touching stats.rs, check the traversal: the usual culprits are missing array \
             `items` / map values (undercounts int64) and counting path-level parameters twice.",
            bad.join("\n"),
        );
    }
}

/// One label/value cell. Values are right-aligned in 4 columns; the widest real count is
/// 3 digits, and the extra column keeps the table from reflowing if one crosses 1000.
fn cell(label: &str, value: usize, label_width: usize) -> String {
    format!("{label:<label_width$}{value:>4}")
}

fn row(cells: &[(&str, usize)]) -> String {
    let mut line = cell(cells[0].0, cells[0].1, 20);
    for (label, value) in &cells[1..] {
        line.push_str("      ");
        line.push_str(&cell(label, *value, 18));
    }
    line.push('\n');
    line
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The layout the milestone specifies, verbatim. A test rather than a comment because the
    /// column widths are the sort of thing a well-meaning cleanup silently reflows.
    const GOLDEN: &str = "\
paths                308      operations         482      definitions        222
tags                   9      path params        956      query params       412
body params          121      formData params      3      enum properties     37
date-time props      101      int64 props        174      uint64 props         2
defs with required    38      allOf/oneOf/anyOf/not/discriminator   0
";

    fn known_good() -> Stats {
        Stats {
            paths: EXPECTED.paths,
            operations: EXPECTED.operations,
            definitions: EXPECTED.definitions,
            tags: EXPECTED.tags,
            path_params: EXPECTED.path_params,
            query_params: EXPECTED.query_params,
            body_params: EXPECTED.body_params,
            form_data_params: EXPECTED.form_data_params,
            enum_properties: EXPECTED.enum_properties,
            date_time_props: EXPECTED.date_time_props,
            int64_props: EXPECTED.int64_props,
            uint64_props: EXPECTED.uint64_props,
            defs_with_required: EXPECTED.defs_with_required,
            polymorphism: EXPECTED.polymorphism,
            tag_counts: EXPECTED.tag_counts.iter().map(|(k, v)| ((*k).to_owned(), *v)).collect(),
        }
    }

    #[test]
    fn renders_the_specified_layout() {
        let rendered = known_good().render();
        let table: String = rendered.lines().take(5).map(|l| format!("{l}\n")).collect();
        assert_eq!(table, GOLDEN);
    }

    #[test]
    fn verify_accepts_the_known_good_numbers() {
        known_good().verify(EXPECTED_VERSION).unwrap();
    }

    #[test]
    fn verify_rejects_a_single_wrong_count() {
        // The bug this prevents: a loader that undercounts int64 by six (the array-items
        // traversal bug) shipping because nothing compared it to a known-good value.
        let mut s = known_good();
        s.int64_props = EXPECTED.int64_props - 6;
        let err = s.verify(EXPECTED_VERSION).unwrap_err().to_string();
        assert!(err.contains("int64 props"), "{err}");
        assert!(err.contains(&EXPECTED.int64_props.to_string()), "{err}");
    }

    #[test]
    fn verify_rejects_a_wrong_tag_count() {
        let mut s = known_good();
        s.tag_counts.insert("repository".into(), 201);
        assert!(s.verify(EXPECTED_VERSION).is_err());
    }

    #[test]
    fn verify_rejects_an_unknown_tag() {
        let mut s = known_good();
        s.tags += 1;
        s.tag_counts.insert("quantum".into(), 3);
        let err = s.verify(EXPECTED_VERSION).unwrap_err().to_string();
        assert!(err.contains("quantum"), "{err}");
    }

    #[test]
    fn verify_refuses_to_bless_a_different_spec_version() {
        // A spec bump must not silently re-baseline the self-test.
        assert!(known_good().verify("17.0.0").is_err());
    }
}
