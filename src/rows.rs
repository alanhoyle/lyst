//! Filtering and sorting a document's lines. The result is a list of the
//! original line numbers to show, in order, and a new document holding just
//! those lines.

use crate::doc::Document;
use crate::search::Search;
use crate::table::Table;
use std::cmp::Ordering;

/// One step applied to the original lines.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Op {
    /// Keep lines matching (or, with `invert`, not matching) the pattern.
    Filter { pattern: String, regex: bool, invert: bool },
    /// Sort table rows by a column (0-based).
    Sort { col: usize, desc: bool },
}

impl Op {
    /// Parses filter input: a leading `!` keeps the lines that don't match.
    pub fn filter(input: &str, regex: bool) -> Result<Op, String> {
        let (invert, pattern) = match input.strip_prefix('!') {
            Some(rest) => (true, rest),
            None => (false, input),
        };
        Search::new(pattern, regex)?;
        Ok(Op::Filter { pattern: pattern.to_string(), regex, invert })
    }

    pub fn is_filter(&self) -> bool {
        matches!(self, Op::Filter { .. })
    }

    /// Short description for the status bar and file info.
    pub fn label(&self) -> String {
        match self {
            Op::Filter { pattern, invert: false, .. } => format!("lines matching \"{pattern}\""),
            Op::Filter { pattern, invert: true, .. } => format!("lines not matching \"{pattern}\""),
            Op::Sort { col, desc } => format!("sorted by column {} {}", col + 1, if *desc { "↓" } else { "↑" }),
        }
    }
}

/// The original line numbers left after applying `ops` in order. A table's
/// header and metadata lines survive filters and stay above sorted rows.
pub fn apply(doc: &Document, table: Option<&Table>, ops: &[Op]) -> Result<Vec<usize>, String> {
    let fixed = |i: usize| table.is_some_and(|t| t.header == Some(i) || t.is_meta(doc.line(i).1));
    let mut lines: Vec<usize> = (0..doc.line_count()).collect();
    for op in ops {
        match op {
            Op::Filter { pattern, regex, invert } => {
                let s = Search::new(pattern, *regex)?;
                let header = table.and_then(|t| t.header);
                lines.retain(|&i| Some(i) == header || s.is_match(doc.line(i).1) != *invert);
            }
            Op::Sort { col, desc } => {
                let Some(t) = table else { return Err("Sorting needs table view (D)".into()) };
                let (mut keep, mut rows): (Vec<usize>, Vec<usize>) = lines.iter().partition(|&&i| fixed(i));
                rows.sort_by(|&a, &b| {
                    let o = compare(t.field(doc.line(a).1, *col), t.field(doc.line(b).1, *col));
                    if *desc { o.reverse() } else { o }
                });
                keep.extend(rows);
                lines = keep;
            }
        }
    }
    Ok(lines)
}

/// A document of the given original lines, in order.
pub fn build(doc: &Document, lines: &[usize]) -> Document {
    let mut bytes = Vec::with_capacity(lines.iter().map(|&i| doc.line(i).1.len() + 1).sum());
    for &i in lines {
        bytes.extend_from_slice(doc.line(i).1);
        bytes.push(b'\n');
    }
    Document::from_bytes(doc.name.clone(), bytes).filtered(doc.filter.clone())
}

/// Orders table fields: numbers by value, before text; text naturally
/// (`chr2` before `chr10`), ignoring case; empty fields last.
pub fn compare(a: &[u8], b: &[u8]) -> Ordering {
    let (a, b) = (a.trim_ascii(), b.trim_ascii());
    match (a.is_empty(), b.is_empty()) {
        (true, true) => return Ordering::Equal,
        (true, false) => return Ordering::Greater,
        (false, true) => return Ordering::Less,
        _ => {}
    }
    let num = |s: &[u8]| std::str::from_utf8(s).ok()?.parse::<f64>().ok().filter(|n| !n.is_nan());
    match (num(a), num(b)) {
        (Some(x), Some(y)) => x.total_cmp(&y),
        (Some(_), None) => Ordering::Less,
        (None, Some(_)) => Ordering::Greater,
        (None, None) => natural(a, b),
    }
}

/// Compares runs of digits by value and everything else ignoring case.
fn natural(a: &[u8], b: &[u8]) -> Ordering {
    let (mut i, mut j) = (0, 0);
    while i < a.len() && j < b.len() {
        if a[i].is_ascii_digit() && b[j].is_ascii_digit() {
            let si = i;
            let sj = j;
            while i < a.len() && a[i].is_ascii_digit() {
                i += 1;
            }
            while j < b.len() && b[j].is_ascii_digit() {
                j += 1;
            }
            let (x, y) = (trim_zeros(&a[si..i]), trim_zeros(&b[sj..j]));
            let o = x.len().cmp(&y.len()).then_with(|| x.cmp(y));
            if o != Ordering::Equal {
                return o;
            }
        } else {
            let o = a[i].to_ascii_lowercase().cmp(&b[j].to_ascii_lowercase());
            if o != Ordering::Equal {
                return o;
            }
            i += 1;
            j += 1;
        }
    }
    (a.len() - i).cmp(&(b.len() - j)).then_with(|| a.cmp(b))
}

fn trim_zeros(d: &[u8]) -> &[u8] {
    let n = d.iter().take_while(|&&c| c == b'0').count().min(d.len().saturating_sub(1));
    &d[n..]
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::encoding::{self, Encoding};
    use crate::table::Format;

    fn table(doc: &Document) -> Table {
        let f = Format::for_name(&doc.name).unwrap();
        Table::new(doc, f, &|b: &[u8], m| encoding::layout(b, Encoding::Utf8, 8, m, false))
    }

    fn texts(doc: &Document, lines: &[usize]) -> Vec<String> {
        lines.iter().map(|&i| String::from_utf8_lossy(doc.line(i).1).into_owned()).collect()
    }

    #[test]
    fn natural_and_numeric_order() {
        let mut v: Vec<&[u8]> = vec![b"chr10", b"chr2", b"", b"Chr1", b"x", b"10", b"-3.5", b"2", b"chrX", b"007"];
        v.sort_by(|a, b| compare(a, b));
        let v: Vec<_> = v.iter().map(|s| String::from_utf8_lossy(s).into_owned()).collect();
        assert_eq!(v, ["-3.5", "2", "007", "10", "Chr1", "chr2", "chr10", "chrX", "x", ""]);
    }

    #[test]
    fn filters_keep_the_header_and_can_invert() {
        let doc = Document::from_bytes("t.csv", b"name,n\nann,3\nbob,10\nanna,2\n".to_vec());
        let t = table(&doc);
        let only = |ops: &[Op]| texts(&doc, &apply(&doc, Some(&t), ops).unwrap());
        assert_eq!(only(&[Op::filter("ann", false).unwrap()]), ["name,n", "ann,3", "anna,2"]);
        assert_eq!(only(&[Op::filter("!ann", false).unwrap()]), ["name,n", "bob,10"]);
        assert!(Op::filter("(", true).is_err());
        // Without a table, nothing is special.
        let plain = Document::from_bytes("log", b"a 1\nb 2\na 3\n".to_vec());
        assert_eq!(texts(&plain, &apply(&plain, None, &[Op::filter("a", false).unwrap()]).unwrap()), ["a 1", "a 3"]);
    }

    #[test]
    fn sorts_rows_below_metadata_and_header() {
        let vcf = b"##fileformat=VCFv4.2\n#CHROM\tPOS\nchr10\t5\nchr2\t30\nchr2\t4\n";
        let doc = Document::from_bytes("v.vcf", vcf.to_vec());
        let t = table(&doc);
        let sorted = |ops: &[Op]| texts(&doc, &apply(&doc, Some(&t), ops).unwrap());
        assert_eq!(
            sorted(&[Op::Sort { col: 0, desc: false }]),
            ["##fileformat=VCFv4.2", "#CHROM\tPOS", "chr2\t30", "chr2\t4", "chr10\t5"],
            "stable: equal keys keep their order"
        );
        assert_eq!(
            sorted(&[Op::Sort { col: 1, desc: true }]),
            ["##fileformat=VCFv4.2", "#CHROM\tPOS", "chr2\t30", "chr10\t5", "chr2\t4"]
        );
        // Filter then sort.
        let ops = [Op::filter("chr2", false).unwrap(), Op::Sort { col: 1, desc: false }];
        assert_eq!(sorted(&ops), ["#CHROM\tPOS", "chr2\t4", "chr2\t30"]);

        let lines = apply(&doc, Some(&t), &ops).unwrap();
        let built = build(&doc, &lines);
        assert_eq!(built.bytes(), b"#CHROM\tPOS\nchr2\t4\nchr2\t30\n");
        assert_eq!(built.name, "v.vcf");
        assert!(apply(&doc, None, &[Op::Sort { col: 0, desc: false }]).is_err());
    }
}
