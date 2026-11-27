//! Just enough YAML to read a workflow file's shape.
//!
//! # Why a hand-rolled scanner and not a YAML crate
//!
//! `gea` has no YAML dependency, and adding one to answer three questions about a file the
//! *server* also parses would be a poor trade — a workflow's authority is Gitea's own parser,
//! not ours. What this module extracts is used for **description only**: the workflow's display
//! name, whether it can be dispatched (and with which inputs), and which runner labels its jobs
//! ask for. Nothing here decides what gets sent to the server except the *list* of input names
//! offered in a validation message, and a wrong answer there costs a slightly worse error, never
//! a wrong request.
//!
//! The scanner is therefore deliberately shallow and its limits are known:
//!
//! * indentation-based nesting only — no anchors, aliases, merge keys or multi-line scalars;
//! * inline flow sequences (`[a, b]`) are understood, flow *mappings* (`{a: 1}`) are not;
//! * a `#` outside quotes starts a comment.
//!
//! Every one of those limits is safe in the "description only" role. If this ever gains a job
//! that changes what is *sent*, it needs a real parser instead.

/// One `workflow_dispatch` input, as declared in the file.
///
/// `kind` is Gitea's/GitHub's input type: `string` (the default), `boolean`, `number`, or
/// `choice` — and `choice` is the one that matters, because its legal values are only knowable
/// from `options`, which is why they are kept.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub(crate) struct Input {
    pub name: String,
    pub kind: String,
    pub description: String,
    pub default: String,
    pub required: bool,
    pub options: Vec<String>,
}

impl Input {
    /// The type as it should be shown, defaulting the way the Actions schema does.
    pub(crate) fn kind_or_default(&self) -> &str {
        if self.kind.is_empty() { "string" } else { &self.kind }
    }
}

#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub(crate) struct Parsed {
    /// The `name:` at the top of the file. Absent means Gitea falls back to the file name.
    pub name: Option<String>,
    /// The events under `on:`, in file order.
    pub events: Vec<String>,
    /// `Some` when `workflow_dispatch` is declared — i.e. when `gea workflow run` can start it.
    pub dispatch: Option<Vec<Input>>,
    /// Every `runs-on:` label in the file, deduplicated. The labels a runner must carry.
    pub runs_on: Vec<String>,
    /// Job ids, in file order.
    pub jobs: Vec<String>,
}

impl Parsed {
    pub(crate) fn dispatchable(&self) -> bool {
        self.dispatch.is_some()
    }

    /// The name to show: the declared one, or the file's stem, which is what Gitea shows too.
    pub(crate) fn display_name(&self, path: &str) -> String {
        match &self.name {
            Some(n) if !n.is_empty() => n.clone(),
            _ => path.rsplit('/').next().unwrap_or(path).to_owned(),
        }
    }
}

/// One significant line: its indentation, and its key/value or sequence item.
struct Line<'a> {
    indent: usize,
    key: Option<&'a str>,
    value: &'a str,
    item: Option<&'a str>,
}

pub(crate) fn parse(source: &str) -> Parsed {
    let mut out = Parsed::default();
    let lines: Vec<Line<'_>> = source.lines().filter_map(split_line).collect();

    // Indentation of the block we are inside, or `None` when we are not inside it. A block ends
    // at the first line indented no deeper than the key that opened it.
    let mut on_at: Option<usize> = None;
    let mut jobs_at: Option<usize> = None;
    let mut dispatch_at: Option<usize> = None;
    let mut inputs_at: Option<usize> = None;
    let mut input_at: Option<usize> = None;
    let mut options_at: Option<usize> = None;
    let mut runs_on_at: Option<usize> = None;
    let mut on_child_at: Option<usize> = None;

    for line in &lines {
        // Close every block this line has dedented out of. A single line can close several.
        for block in [
            &mut options_at,
            &mut runs_on_at,
            &mut on_child_at,
            &mut input_at,
            &mut inputs_at,
            &mut dispatch_at,
            &mut on_at,
            &mut jobs_at,
        ] {
            if block.is_some_and(|at| line.indent <= at) {
                *block = None;
            }
        }

        if let Some(key) = line.key {
            match (line.indent, key) {
                (0, "name") => out.name = Some(scalar(line.value).to_owned()),
                (0, "on") => {
                    on_at = Some(0);
                    // `on: push` and `on: [push, workflow_dispatch]` put the events on one line.
                    for e in inline_list(line.value) {
                        out.events.push(e.clone());
                        if e == "workflow_dispatch" {
                            out.dispatch = Some(Vec::new());
                        }
                    }
                }
                (0, "jobs") => jobs_at = Some(0),
                (0, _) => {}
                _ => {}
            }
        }

        // `runs-on:` is read wherever it appears: a matrix or a reusable-workflow shape can nest
        // it deeper than a plain job, and every occurrence is a label a runner must carry.
        if line.key == Some("runs-on") {
            push_labels(&mut out.runs_on, inline_list(line.value));
            // `runs-on:` with nothing after it opens a block sequence whose items follow.
            runs_on_at = line.value.is_empty().then_some(line.indent);
        } else if let (Some(at), Some(item)) = (runs_on_at, line.item)
            && line.indent > at
        {
            push_labels(&mut out.runs_on, vec![scalar(item).to_owned()]);
            continue;
        }

        // ---------------------------------------------------------------- the `on:` subtree
        //
        // Only the *first* level below `on:` names events: `push:` is an event, the `branches:`
        // under it is not, and neither is anything under `workflow_dispatch:`.
        if on_at.is_some() && line.indent > 0 {
            let level = *on_child_at.get_or_insert(line.indent);
            if line.indent == level
                && let Some(name) =
                    line.key.map(str::to_owned).or_else(|| line.item.map(|i| scalar(i).to_owned()))
            {
                if name == "workflow_dispatch" {
                    out.dispatch = Some(Vec::new());
                    dispatch_at = Some(line.indent);
                }
                push_labels(&mut out.events, vec![name]);
            }
        }

        // ------------------------------------------------------- the dispatch inputs subtree
        if let (Some(d_at), Some(key)) = (dispatch_at, line.key)
            && line.indent > d_at
            && key == "inputs"
        {
            inputs_at = Some(line.indent);
            continue;
        }
        if let Some(i_at) = inputs_at
            && line.indent > i_at
        {
            let inputs = out.dispatch.get_or_insert_with(Vec::new);
            // The first level below `inputs:` names the inputs; anything deeper describes one.
            let name_level = *input_at.get_or_insert(line.indent);
            if line.indent == name_level {
                if let Some(key) = line.key {
                    inputs.push(Input { name: key.to_owned(), ..Input::default() });
                    options_at = None;
                }
                continue;
            }
            if let Some(current) = inputs.last_mut() {
                if let Some(item) = line.item
                    && options_at.is_some()
                {
                    current.options.push(scalar(item).to_owned());
                    continue;
                }
                match line.key {
                    Some("type") => current.kind = scalar(line.value).to_owned(),
                    Some("description") => current.description = scalar(line.value).to_owned(),
                    Some("default") => current.default = scalar(line.value).to_owned(),
                    Some("required") => current.required = scalar(line.value) == "true",
                    Some("options") => {
                        options_at = Some(line.indent);
                        current.options.extend(inline_list(line.value));
                    }
                    _ => {}
                }
            }
            continue;
        }

        // ------------------------------------------------------------------------- the jobs
        if let (Some(j_at), Some(key)) = (jobs_at, line.key)
            && line.indent == j_at + indent_step(&lines, j_at)
        {
            out.jobs.push(key.to_owned());
        }
    }
    out
}

fn push_labels(into: &mut Vec<String>, labels: Vec<String>) {
    for l in labels {
        if !l.is_empty() && !into.contains(&l) {
            into.push(l);
        }
    }
}

/// The indentation step used just below `at`, so job ids are recognised whether the file is
/// indented by two spaces or by four.
fn indent_step(lines: &[Line<'_>], at: usize) -> usize {
    lines.iter().find(|l| l.indent > at).map(|l| l.indent - at).unwrap_or(2)
}

/// Split one line into indent plus key/value or sequence item, or `None` when it is blank or a
/// comment.
fn split_line(raw: &str) -> Option<Line<'_>> {
    let indent = raw.len() - raw.trim_start().len();
    let content = strip_comment(raw.trim_start());
    if content.is_empty() {
        return None;
    }
    if let Some(item) = content.strip_prefix("- ").or_else(|| content.strip_prefix('-')) {
        let item = item.trim();
        // `- key: value` is both an item and a mapping; the key half is what callers want.
        if let Some((k, v)) = split_key(item) {
            return Some(Line { indent, key: Some(k), value: v, item: Some(item) });
        }
        return Some(Line { indent, key: None, value: "", item: Some(item) });
    }
    match split_key(content) {
        Some((k, v)) => Some(Line { indent, key: Some(k), value: v, item: None }),
        None => Some(Line { indent, key: None, value: content, item: None }),
    }
}

/// `key: value`, with the key unquoted. Quoted keys matter: YAML 1.1 reads a bare `on` as a
/// boolean, so careful authors write `"on":` — and a scanner that missed it would report every
/// such workflow as having no triggers.
fn split_key(content: &str) -> Option<(&str, &str)> {
    let (key, value) = content.split_once(':')?;
    let key = key.trim().trim_matches('"').trim_matches('\'');
    if key.is_empty() || key.contains(' ') && !key.contains('-') {
        // `foo bar: x` is not a key we understand; treat the line as opaque rather than
        // inventing a key with a space in it.
        return None;
    }
    Some((key, value.trim()))
}

fn strip_comment(s: &str) -> &str {
    let bytes = s.as_bytes();
    let mut quote: Option<u8> = None;
    for (i, b) in bytes.iter().enumerate() {
        match (quote, b) {
            (None, b'"') | (None, b'\'') => quote = Some(*b),
            (Some(q), b) if *b == q => quote = None,
            // A `#` only starts a comment at the start of the line or after whitespace, which is
            // what keeps `url: http://x/#frag` intact.
            (None, b'#') if i == 0 || bytes[i - 1].is_ascii_whitespace() => {
                return s[..i].trim_end();
            }
            _ => {}
        }
    }
    s.trim_end()
}

/// A scalar with its quotes removed.
fn scalar(value: &str) -> &str {
    let v = value.trim();
    if v.len() >= 2
        && (v.starts_with('"') && v.ends_with('"') || v.starts_with('\'') && v.ends_with('\''))
    {
        return &v[1..v.len() - 1];
    }
    v
}

/// A value that may be a scalar, or an inline flow sequence `[a, b]`.
fn inline_list(value: &str) -> Vec<String> {
    let v = value.trim();
    if v.is_empty() {
        return Vec::new();
    }
    if let Some(inner) = v.strip_prefix('[').and_then(|s| s.strip_suffix(']')) {
        return inner.split(',').map(|p| scalar(p).to_owned()).filter(|p| !p.is_empty()).collect();
    }
    vec![scalar(v).to_owned()]
}

#[cfg(test)]
mod tests {
    use super::*;

    const CI: &str = r#"
# A comment that must not become a key
name: CI
on:
  push:
    branches: [main]
  workflow_dispatch:
    inputs:
      environment:
        description: 'Where to deploy'
        required: true
        type: choice
        options:
          - staging
          - production
      verbose:
        type: boolean
        default: false
jobs:
  build:
    runs-on: docker
    steps:
      - run: echo hi
  package:
    runs-on: [self-hosted, arm64]
"#;

    #[test]
    fn a_realistic_workflow_yields_its_name_events_inputs_and_labels() {
        let p = parse(CI);
        assert_eq!(p.name.as_deref(), Some("CI"));
        assert!(p.events.contains(&"push".to_owned()), "{:?}", p.events);
        assert!(p.dispatchable());
        assert_eq!(p.jobs, vec!["build", "package"]);
        // The reason this module exists: `runs-on` is a label, and these are the labels a runner
        // must carry for this file to run at all.
        assert_eq!(p.runs_on, vec!["docker", "self-hosted", "arm64"]);

        let inputs = p.dispatch.unwrap();
        assert_eq!(inputs.len(), 2, "{inputs:?}");
        assert_eq!(inputs[0].name, "environment");
        assert_eq!(inputs[0].kind, "choice");
        assert!(inputs[0].required);
        assert_eq!(inputs[0].options, vec!["staging", "production"]);
        assert_eq!(inputs[0].description, "Where to deploy");
        assert_eq!(inputs[1].name, "verbose");
        assert_eq!(inputs[1].kind, "boolean");
        assert_eq!(inputs[1].default, "false");
        assert!(!inputs[1].required);
    }

    /// Bug this prevents: reporting `on: [push, workflow_dispatch]` as not dispatchable, so
    /// `gea workflow list` says a workflow cannot be started when it can.
    #[test]
    fn an_inline_event_list_is_recognised() {
        let p = parse("name: x\non: [push, workflow_dispatch]\njobs:\n  a:\n    runs-on: docker\n");
        assert!(p.dispatchable());
        assert_eq!(p.events, vec!["push", "workflow_dispatch"]);
        assert_eq!(p.dispatch.unwrap().len(), 0, "no inputs were declared");
    }

    /// Bug this prevents: missing `"on":`. YAML 1.1 reads bare `on` as the boolean true, so
    /// careful authors quote it — and a scanner that only matched `on:` would report those files
    /// as having no triggers at all.
    #[test]
    fn a_quoted_on_key_is_still_the_on_key() {
        let p = parse("\"on\":\n  workflow_dispatch:\n");
        assert!(p.dispatchable());
    }

    /// A `#` inside a quoted value is not a comment, and a comment is never a key.
    #[test]
    fn comments_are_stripped_but_urls_survive() {
        assert_eq!(strip_comment("name: CI # the name"), "name: CI");
        assert_eq!(strip_comment("url: 'http://x/#frag'"), "url: 'http://x/#frag'");
        assert_eq!(strip_comment("# whole line"), "");
        let p = parse("# name: NOPE\nname: real\n");
        assert_eq!(p.name.as_deref(), Some("real"));
    }

    /// A file with no `name:` falls back to the file name, which is what Gitea's own UI shows.
    #[test]
    fn a_nameless_workflow_falls_back_to_its_file_name() {
        let p = parse("on: push\njobs:\n  a:\n    runs-on: docker\n");
        assert_eq!(p.display_name(".gitea/workflows/release.yml"), "release.yml");
        assert!(!p.dispatchable());
    }

    /// Four-space indentation is as legal as two, and job ids must be found either way.
    #[test]
    fn four_space_indentation_still_finds_the_jobs() {
        let p = parse("on: push\njobs:\n    build:\n        runs-on: docker\n");
        assert_eq!(p.jobs, vec!["build"]);
    }

    #[test]
    fn an_empty_or_garbage_file_does_not_panic() {
        assert_eq!(parse(""), Parsed::default());
        assert_eq!(parse("::::\n\t\n   \n").name, None);
        assert!(!parse("not: yaml: really").dispatchable());
    }
}
