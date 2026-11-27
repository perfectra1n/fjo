//! The one table renderer.
//!
//! There is exactly one `Table` in `gea`, and both the default human renderer and the
//! `tablerender` template helper go through it. That is a decision, not an accident:
//!
//! * **No `comfy-table`.** `gh`'s look is space-padded columns with no box drawing. A
//!   box-drawing library would have to be configured back down to that, and its width model
//!   is not ours.
//! * **One implementation.** If `tablerender` had its own width algorithm it would drift
//!   from the default renderer within a release, and users would see two different layouts
//!   for the same data depending on whether they passed `--template`.
//!
//! The TTY/pipe split is the part scripts depend on:
//!
//! | | TTY | not a TTY |
//! | --- | --- | --- |
//! | separator | two spaces | one TAB |
//! | header | dim + underlined | **absent** |
//! | padding | to the column width | none |
//! | truncation | `…` when over budget | **never** |
//! | empty cell | spaces | **an empty field, preserved** |
//!
//! The right-hand column is the contract that makes `gea pr list | cut -f2` work, and it is
//! why every one of those rules is asserted by a test.

use std::io::{self, Write};

use super::color::{display_width, header_style, paint, strip_ansi, truncate_visible};
use super::tty::Term;

/// Gutter between padded columns. Two spaces, like `gh`: one is too tight to scan, and a
/// vertical rule is the box drawing we are avoiding.
const GUTTER: usize = 2;

/// A space-padded (TTY) or TAB-separated (piped) table.
///
/// Cells may already contain ANSI escapes — `{{tablerow (autocolor .state) .title}}` produces
/// them — so every width computation here goes through [`display_width`].
#[derive(Debug, Clone, Default)]
pub struct Table {
    term: Term,
    headers: Vec<String>,
    rows: Vec<Vec<String>>,
    banner: Option<String>,
}

impl Table {
    pub fn new(term: &Term) -> Self {
        Self { term: *term, headers: Vec::new(), rows: Vec::new(), banner: None }
    }

    /// Set the header row.
    ///
    /// Names are stored verbatim; the command layer passes them already uppercased, as `gh`
    /// does. Uppercasing here would mangle a header that a template author wrote on purpose.
    pub fn headers<I, S>(&mut self, headers: I) -> &mut Self
    where
        I: IntoIterator<Item = S>,
        S: Into<String>,
    {
        self.headers = headers.into_iter().map(Into::into).collect();
        self
    }

    pub fn row<I, S>(&mut self, cells: I) -> &mut Self
    where
        I: IntoIterator<Item = S>,
        S: Into<String>,
    {
        self.rows.push(cells.into_iter().map(Into::into).collect());
        self
    }

    /// The `Showing N of M` hook.
    ///
    /// The banner text is *not* built here. Only the command layer knows what "N of M" means
    /// for its resource (and whether `M` is even known — the Gitea API does not always send
    /// `x-total-count`), so hardcoding a phrasing in the renderer would either be wrong or
    /// force every caller to work around it. Printed above the table, on a TTY only, because
    /// a banner in a pipe is an extra line that breaks `head -1`.
    pub fn banner(&mut self, text: impl Into<String>) -> &mut Self {
        self.banner = Some(text.into());
        self
    }

    pub fn is_empty(&self) -> bool {
        self.rows.is_empty()
    }

    pub fn len(&self) -> usize {
        self.rows.len()
    }

    /// Render to a string. Convenience for `tablerender` and for tests.
    pub fn render_to_string(&self) -> String {
        let mut buf = Vec::new();
        self.render(&mut buf).expect("writing to a Vec cannot fail");
        String::from_utf8(buf).expect("cells are UTF-8 and padding is ASCII")
    }

    pub fn render(&self, out: &mut impl Write) -> io::Result<()> {
        if self.term.tty { self.render_padded(out) } else { self.render_tsv(out) }
    }

    /// The machine-readable form: raw TAB-separated fields, no header, no padding, no
    /// truncation, empty cells preserved as empty fields.
    fn render_tsv(&self, out: &mut impl Write) -> io::Result<()> {
        for row in &self.rows {
            for (i, cell) in row.iter().enumerate() {
                if i > 0 {
                    out.write_all(b"\t")?;
                }
                out.write_all(tsv_cell(cell).as_bytes())?;
            }
            out.write_all(b"\n")?;
        }
        Ok(())
    }

    fn render_padded(&self, out: &mut impl Write) -> io::Result<()> {
        if let Some(banner) = &self.banner {
            writeln!(out, "{banner}")?;
            writeln!(out)?;
        }
        if self.rows.is_empty() && self.headers.is_empty() {
            return Ok(());
        }

        let cols = self.column_count();
        let natural = self.natural_widths(cols);
        let widths = fit(&natural, self.term.width);

        if !self.headers.is_empty() {
            let cells: Vec<String> = (0..cols)
                .map(|i| {
                    let text = self.headers.get(i).map(String::as_str).unwrap_or("");
                    paint(&self.term, header_style(), &truncate_visible(text, widths[i]))
                })
                .collect();
            write_padded_row(out, &cells, &widths)?;
        }
        for row in &self.rows {
            let cells: Vec<String> = (0..cols)
                .map(|i| {
                    let text = row.get(i).map(String::as_str).unwrap_or("");
                    truncate_visible(text, widths[i])
                })
                .collect();
            write_padded_row(out, &cells, &widths)?;
        }
        Ok(())
    }

    /// Ragged rows are legal: a template can emit `tablerow` with a different arity per
    /// iteration. The table is as wide as its widest row so no data is silently dropped.
    fn column_count(&self) -> usize {
        self.rows.iter().map(Vec::len).chain([self.headers.len()]).max().unwrap_or(0)
    }

    fn natural_widths(&self, cols: usize) -> Vec<usize> {
        let mut widths = vec![0usize; cols];
        for (i, width) in widths.iter_mut().enumerate() {
            let header = self.headers.get(i).map(String::as_str).unwrap_or("");
            *width = display_width(header);
            for row in &self.rows {
                let cell = row.get(i).map(String::as_str).unwrap_or("");
                *width = (*width).max(display_width(cell));
            }
        }
        widths
    }
}

/// A cell in TSV mode.
///
/// Escapes are stripped and embedded TAB/CR/LF are replaced with a single space. Both are
/// silent-corruption guards: a PR title containing a literal tab would shift every
/// subsequent `cut -f` field by one for that row only, which is far worse to debug than a
/// title with a space in it. This is the one place where TSV mode alters a cell's bytes, and
/// it is why the rule is spelled out here rather than left to the caller.
fn tsv_cell(cell: &str) -> String {
    let plain = strip_ansi(cell);
    if plain.contains(['\t', '\n', '\r']) { plain.replace(['\t', '\n', '\r'], " ") } else { plain }
}

fn write_padded_row(out: &mut impl Write, cells: &[String], widths: &[usize]) -> io::Result<()> {
    let last = cells.iter().rposition(|c| !c.is_empty()).map_or(0, |i| i + 1);
    let mut line = String::new();
    for (i, cell) in cells.iter().enumerate() {
        // Trailing padding is dropped: trailing whitespace shows up as a diff artifact in
        // goldens and as an artifact in a user's `| tee` capture, and it is invisible anyway.
        if i >= last {
            break;
        }
        if i > 0 {
            line.push_str(&" ".repeat(GUTTER));
        }
        line.push_str(cell);
        if i + 1 < last {
            let pad = widths[i].saturating_sub(display_width(cell));
            line.push_str(&" ".repeat(pad));
        }
    }
    writeln!(out, "{}", line.trim_end())
}

/// Fit `natural` column widths into `total` terminal columns.
///
/// If they already fit, they are used as-is — the common case, and the one where any
/// "clever" redistribution would visibly reflow a table for no reason.
///
/// When they do not fit, this is max-min fair allocation: columns narrower than an equal
/// share keep their natural width and donate the surplus, repeatedly, until only columns
/// wider than their share remain; the remaining budget is then split among those in
/// proportion to their natural widths. The effect is that the widest columns absorb the
/// shrinkage, so a table of `NUMBER  STATE  TITLE` truncates the title and leaves the number
/// and state intact — which is what makes a narrow terminal still useful.
fn fit(natural: &[usize], total: usize) -> Vec<usize> {
    let n = natural.len();
    if n == 0 {
        return Vec::new();
    }
    let gutters = GUTTER * (n - 1);
    let sum: usize = natural.iter().sum();
    if sum + gutters <= total {
        return natural.to_vec();
    }
    // Every column keeps at least one column of content, so a very narrow terminal degrades
    // to "…" per column rather than panicking or emitting zero-width columns.
    let avail = total.saturating_sub(gutters).max(n);

    let mut assigned: Vec<Option<usize>> = vec![None; n];
    let mut remaining = avail;
    loop {
        let open: Vec<usize> = (0..n).filter(|i| assigned[*i].is_none()).collect();
        if open.is_empty() {
            break;
        }
        let share = remaining / open.len();
        let fitting: Vec<usize> = open.iter().copied().filter(|i| natural[*i] <= share).collect();
        if fitting.is_empty() {
            // Everyone left wants more than an equal share: divide the rest proportionally.
            let wide_sum: usize = open.iter().map(|i| natural[*i]).sum();
            let mut handed_out = 0;
            for (k, &i) in open.iter().enumerate() {
                let w = if k + 1 == open.len() {
                    // The last column takes the rounding remainder so the row exactly fills
                    // the terminal instead of leaving a one-column gap.
                    remaining - handed_out
                } else {
                    (natural[i] * remaining / wide_sum).max(1)
                };
                handed_out += w;
                assigned[i] = Some(w.max(1));
            }
            break;
        }
        for i in fitting {
            assigned[i] = Some(natural[i]);
            remaining -= natural[i];
        }
    }
    assigned.into_iter().map(|w| w.unwrap_or(1).max(1)).collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn sample(term: &Term) -> Table {
        let mut t = Table::new(term);
        t.headers(["NUMBER", "STATE", "TITLE"]);
        t.row(["1", "open", "add the thing"]);
        // The empty middle cell is the interesting one: TSV must keep it as an empty field.
        t.row(["22", "", "fix the other thing"]);
        t
    }

    /// Bug this prevents: emitting padded, headered output into a pipe. `gh` users write
    /// `gea pr list | cut -f2`; with padding that yields `open   ` and with a header it
    /// yields `STATE` as the first record.
    #[test]
    fn piped_output_is_real_tsv() {
        let out = sample(&Term::piped()).render_to_string();
        assert_eq!(out, "1\topen\tadd the thing\n22\t\tfix the other thing\n");
        // Field 2 of row 2 must be empty, not missing and not padded.
        let row2: Vec<&str> = out.lines().nth(1).unwrap().split('\t').collect();
        assert_eq!(row2, vec!["22", "", "fix the other thing"]);
        assert!(!out.contains("NUMBER"), "no header row when piped");
        assert!(!out.contains("  "), "no padding when piped");
    }

    /// Bug this prevents: the TTY and piped renderers diverging. Same data, both paths, one
    /// golden each.
    #[test]
    fn tty_output_is_padded_with_a_header() {
        let out = sample(&Term::tty(80)).render_to_string();
        assert_eq!(
            out,
            "NUMBER  STATE  TITLE\n1       open   add the thing\n22             fix the other thing\n"
        );
        assert!(!out.contains('\t'), "no tabs on a TTY");
    }

    /// Bug this prevents: measuring CJK/emoji cells with `str::len` or `chars().count()`, so
    /// the column after them is off by the number of double-width characters.
    #[test]
    fn unicode_columns_align() {
        let mut t = Table::new(&Term::tty(80));
        t.row(["日本語", "x"]);
        t.row(["ab", "y"]);
        t.row(["🚀🚀", "z"]);
        let out = t.render_to_string();
        let starts: Vec<usize> = out
            .lines()
            .map(|l| {
                let (first, _) = l.rsplit_once("  ").unwrap();
                display_width(first) + GUTTER
            })
            .collect();
        assert_eq!(starts, vec![8, 8, 8], "second column must start in the same place\n{out}");
    }

    /// Bug this prevents: truncating every column a little instead of the widest a lot, which
    /// mangles a 6-character `NUMBER` column to save space a 90-character title is wasting.
    #[test]
    fn narrow_terminal_shrinks_the_widest_column() {
        let mut t = Table::new(&Term::tty(30));
        t.headers(["NUMBER", "TITLE"]);
        t.row(["1", "a title that is far too long to fit in thirty columns"]);
        let out = t.render_to_string();
        for line in out.lines() {
            assert!(display_width(line) <= 30, "{line:?} is {} wide", display_width(line));
        }
        assert!(out.contains("NUMBER"), "the short column survives intact:\n{out}");
        assert!(out.contains('…'), "the long column is truncated:\n{out}");
    }

    /// Bug this prevents: never truncating when piped. A 400-character body would be cut to
    /// the terminal width even though nobody is looking at a terminal.
    #[test]
    fn piped_output_is_never_truncated() {
        let long = "x".repeat(400);
        let mut t = Table::new(&Term { tty: false, width: 20, ..Term::default() });
        t.row([long.clone()]);
        assert_eq!(t.render_to_string(), format!("{long}\n"));
    }

    /// Bug this prevents: a tab inside a title silently shifting every later `cut -f` field
    /// for that one row — a corruption that is invisible until someone's script misparses.
    #[test]
    fn tsv_cells_cannot_contain_a_tab_or_newline() {
        let mut t = Table::new(&Term::piped());
        t.row(["a\tb", "c\nd"]);
        assert_eq!(t.render_to_string(), "a b\tc d\n");
    }

    /// Bug this prevents: leaking SGR escapes into TSV when `CLICOLOR_FORCE` is set while
    /// piping. The user asked for color on their terminal, not inside a machine-read field.
    #[test]
    fn tsv_strips_escapes() {
        let mut t = Table::new(&Term::piped());
        t.row(["\x1b[32mopen\x1b[0m"]);
        assert_eq!(t.render_to_string(), "open\n");
    }

    /// Bug this prevents: hardcoding `Showing N of M` in the renderer, or printing it into a
    /// pipe where it becomes a bogus first record.
    #[test]
    fn banner_is_a_tty_only_hook() {
        let mut t = sample(&Term::tty(80));
        t.banner("Showing 2 of 40 pull requests");
        assert!(t.render_to_string().starts_with("Showing 2 of 40 pull requests\n\n"));

        let mut t = sample(&Term::piped());
        t.banner("Showing 2 of 40 pull requests");
        assert!(!t.render_to_string().contains("Showing"));
    }

    /// Bug this prevents: `fit` producing a zero width (division by the number of columns, or
    /// saturating subtraction reaching 0) and then panicking in `truncate_visible`.
    #[test]
    fn absurdly_narrow_terminals_do_not_panic() {
        for width in [1usize, 2, 3, 4, 5] {
            let mut t = Table::new(&Term::tty(width));
            t.headers(["AAAA", "BBBB", "CCCC"]);
            t.row(["1111", "2222", "3333"]);
            let out = t.render_to_string();
            assert!(!out.is_empty());
        }
    }

    /// Bug this prevents: reflowing a table that already fits, so adding one short row
    /// visibly rewraps every other row.
    #[test]
    fn widths_that_fit_are_used_verbatim() {
        assert_eq!(fit(&[3, 4, 5], 80), vec![3, 4, 5]);
    }
}
