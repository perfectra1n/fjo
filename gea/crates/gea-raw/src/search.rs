//! `gea raw search <terms>` — the answer to "506 commands are undiscoverable".
//!
//! The matcher is hand-rolled, and deliberately so: `fuzzy-matcher` is unmaintained and banned
//! in `deny.toml`, and the job is small. Ranking is substring-first with a subsequence
//! fallback, over the fields a person actually remembers — the command name, the group, the
//! one-line summary, and the URL path. Every term must match *something* (an AND), because a
//! search for `pull merge` that returns every pull-request operation is no better than
//! `--help`.

use gitea_client::meta_types::{BodyField, OpMeta};

/// How many hits `gea raw search` prints. Fifteen fits a terminal without a pager.
pub const DEFAULT_LIMIT: usize = 15;

#[derive(Debug, Clone, Copy)]
pub struct Hit<'a> {
    pub op: &'a OpMeta,
    pub score: u32,
}

/// The `search` leaf, built alongside the group stubs so `gea raw search …` needs no group.
pub fn command() -> clap::Command {
    clap::Command::new("search")
        .about("Find an operation by name, summary, or URL path")
        .long_about(
            "Rank all operations against the given words and print the best matches as full \
             invocations, ready to copy.\n\nExample: gea raw search pull request",
        )
        .arg(
            clap::Arg::new("terms")
                .value_name("WORDS")
                .num_args(1..)
                .required(true)
                .help("Words to match against the command name, summary, and path"),
        )
}

/// Fields searched, with their weights. The command name outranks the summary because someone
/// half-remembering `create-pull-request` should not be buried under operations whose
/// *description* happens to mention pull requests.
const FIELDS: usize = 4;

fn haystacks(op: &OpMeta) -> [(String, u32); FIELDS] {
    [
        (normalize(op.command), 6),
        (normalize(op.group), 3),
        (normalize(op.summary), 3),
        (normalize(op.path), 2),
    ]
}

/// Lowercase, and turn the separators that split words in identifiers and paths into spaces.
///
/// This is what lets the single term `pull request` match the command `create-pull-request`
/// as a *substring* rather than a weak subsequence, which is a large difference in rank.
fn normalize(s: &str) -> String {
    s.chars()
        .map(|c| match c {
            '-' | '_' | '/' | '{' | '}' | '.' => ' ',
            c => c.to_ascii_lowercase(),
        })
        .collect()
}

/// Rank `ops` and return the best `limit`, best first.
pub fn search<'a>(ops: &'a [OpMeta], terms: &[String], limit: usize) -> Vec<Hit<'a>> {
    let needles: Vec<String> =
        terms.iter().map(|t| normalize(t)).filter(|t| !t.trim().is_empty()).collect();
    if needles.is_empty() {
        return Vec::new();
    }

    let mut hits: Vec<Hit<'a>> = Vec::new();
    for op in ops {
        let fields = haystacks(op);
        let mut total = 0;
        let mut matched_all = true;
        for needle in &needles {
            let best = fields.iter().map(|(hay, w)| score(hay, needle, *w)).max().unwrap_or(0);
            if best == 0 {
                matched_all = false;
                break;
            }
            total += best;
        }
        if matched_all {
            hits.push(Hit { op, score: total });
        }
    }
    // Ties break on (group, command) so the output is stable run to run — a search whose order
    // shuffles between invocations reads as a bug.
    hits.sort_by(|a, b| {
        b.score
            .cmp(&a.score)
            .then_with(|| (a.op.group, a.op.command).cmp(&(b.op.group, b.op.command)))
    });
    hits.truncate(limit);
    hits
}

/// Score one needle against one haystack.
///
/// A substring beats a subsequence by a wide margin, a match at a word boundary beats one in
/// the middle, and a match at position zero beats both — so `list` ranks `list-branches` above
/// `download-artifact-list`.
fn score(hay: &str, needle: &str, weight: u32) -> u32 {
    match hay.find(needle) {
        Some(0) => weight * 6,
        Some(at) => {
            let at_word_start = hay.as_bytes()[at - 1] == b' ';
            if at_word_start { weight * 5 } else { weight * 4 }
        }
        None if is_subsequence(hay, needle) => weight,
        None => 0,
    }
}

/// Are `needle`'s characters present in `hay`, in order but not necessarily adjacent? This is
/// what makes `crpr` find `create-pull-request`.
fn is_subsequence(hay: &str, needle: &str) -> bool {
    let mut chars = hay.chars();
    needle.chars().all(|n| chars.any(|h| h == n))
}

/// The command line a hit corresponds to, with path parameters as placeholders — copyable
/// as-is once the placeholders are filled in.
pub fn invocation(op: &OpMeta) -> String {
    let mut s = format!("gea raw {} {}", op.group, op.command);
    for p in op.path_params() {
        s.push_str(&format!(" <{}>", crate::build::value_name(p.wire)));
    }
    // Enough of a nudge that the user knows a body is expected before they get a 422.
    if let Some(b) = op.body
        && b.required
    {
        s.push_str(&required_body_hint(b.fields));
    }
    s
}

fn required_body_hint(fields: &'static [BodyField]) -> String {
    let required: Vec<String> =
        fields.iter().filter(|f| f.required).map(|f| format!(" --{} …", f.flag)).collect();
    required.concat()
}

/// Render hits for the terminal: the invocation, then the method, path, and summary indented
/// under it.
pub fn render(hits: &[Hit<'_>], terms: &[String]) -> String {
    if hits.is_empty() {
        return format!(
            "no operation matches {:?}\n\ntry fewer or different words, or `gea raw --help` \
             to browse the groups\n",
            terms.join(" ")
        );
    }
    let mut out = String::new();
    for h in hits {
        out.push_str(&invocation(h.op));
        out.push('\n');
        out.push_str(&format!("    {} {}", h.op.method, h.op.path));
        if !h.op.summary.is_empty() {
            out.push_str(&format!("  --  {}", h.op.summary));
        }
        out.push('\n');
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::fixtures::OPS;

    fn find(words: &[&str]) -> Vec<String> {
        let terms: Vec<String> = words.iter().map(|s| (*s).to_owned()).collect();
        search(OPS, &terms, DEFAULT_LIMIT)
            .into_iter()
            .map(|h| format!("{} {}", h.op.group, h.op.command))
            .collect()
    }

    /// The command name outranks a summary mention, so the operation whose *name* is
    /// "create-pull-request" comes first for those words.
    #[test]
    fn command_names_outrank_summary_mentions() {
        let hits = find(&["pull", "request"]);
        assert_eq!(hits.first().map(String::as_str), Some("repo create-pull-request"), "{hits:?}");
    }

    /// A multi-word term must still match a kebab-cased command as a substring, which is why
    /// `normalize` folds `-` to a space.
    #[test]
    fn a_multi_word_term_matches_a_kebab_case_command() {
        let hits = find(&["pull request"]);
        assert_eq!(hits.first().map(String::as_str), Some("repo create-pull-request"), "{hits:?}");
    }

    #[test]
    fn every_term_must_match_something() {
        assert!(find(&["pull", "zzzzzz"]).is_empty());
        assert!(find(&[]).is_empty());
        assert!(find(&["   "]).is_empty());
    }

    #[test]
    fn the_url_path_is_searchable_because_that_is_what_api_docs_show() {
        let hits = find(&["contents"]);
        assert!(hits.contains(&"repo get-contents".to_owned()), "{hits:?}");
    }

    #[test]
    fn a_subsequence_still_matches_when_nothing_else_does() {
        assert!(is_subsequence("create pull request", "crpr"));
        assert!(!is_subsequence("create pull request", "rrrr"));
        let hits = find(&["crpr"]);
        assert!(hits.contains(&"repo create-pull-request".to_owned()), "{hits:?}");
    }

    #[test]
    fn results_are_capped_and_ordered_deterministically() {
        let terms = vec!["o".to_owned()];
        let a = search(OPS, &terms, 2);
        let b = search(OPS, &terms, 2);
        assert!(a.len() <= 2);
        assert_eq!(
            a.iter().map(|h| h.op.op_id).collect::<Vec<_>>(),
            b.iter().map(|h| h.op.op_id).collect::<Vec<_>>()
        );
    }

    #[test]
    fn an_invocation_shows_path_placeholders_and_required_body_flags() {
        let op = crate::fixtures::op("repo", "create-pull-request");
        assert_eq!(invocation(op), "gea raw repo create-pull-request <OWNER> <REPO> --title …");
        let op = crate::fixtures::op("repo", "download-pull-diff-or-patch");
        assert_eq!(
            invocation(op),
            "gea raw repo download-pull-diff-or-patch <OWNER> <REPO> <INDEX> <DIFF_TYPE>"
        );
    }

    #[test]
    fn no_match_says_what_to_do_next() {
        let terms = vec!["zzzzzz".to_owned()];
        let text = render(&search(OPS, &terms, DEFAULT_LIMIT), &terms);
        assert!(text.contains("no operation matches"), "{text}");
        assert!(text.contains("gea raw --help"), "{text}");
    }

    #[test]
    fn rendered_hits_show_the_method_and_path() {
        let terms = vec!["contents".to_owned()];
        let text = render(&search(OPS, &terms, DEFAULT_LIMIT), &terms);
        assert!(text.contains("gea raw repo get-contents <OWNER> <REPO> <FILEPATH>"), "{text}");
        assert!(text.contains("GET /repos/{owner}/{repo}/contents/{filepath}"), "{text}");
    }
}
