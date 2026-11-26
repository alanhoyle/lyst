//! Table view for delimited text: CSV, TSV, and tab-separated
//! bioinformatics formats (VCF, BED, GTF/GFF, SAM).
//!
//! Each line is one row. CSV quoting is understood within a line, but a
//! quoted field containing a line break ends the row at the break.

use crate::doc::Document;
use crate::encoding::{Cell, CellKind};
use memchr::memchr;

/// Widest a column grows before its fields are cut off with `…`.
pub const MAX_WIDTH: usize = 40;
/// Between columns.
pub const SEP: &str = " │ ";
const SEP_WIDTH: usize = 3;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Header {
    /// The first non-metadata line names the columns.
    First,
    /// A `#`-prefixed line just before the data names them (`#CHROM`, `#chrom`).
    Hash,
    None,
}

/// How a file's lines split into columns.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Format {
    pub label: &'static str,
    delim: u8,
    /// CSV-style `"quoted, fields"` with `""` for a literal quote.
    quotes: bool,
    /// Lines starting with any of these are shown as plain text.
    meta: &'static [&'static [u8]],
    header: Header,
}

const fn fmt(label: &'static str, delim: u8, quotes: bool, meta: &'static [&'static [u8]], header: Header) -> Format {
    Format { label, delim, quotes, meta, header }
}

const CSV: Format = fmt("CSV", b',', true, &[], Header::First);
const TSV: Format = fmt("TSV", b'\t', false, &[], Header::First);
const VCF: Format = fmt("VCF", b'\t', false, &[b"##"], Header::Hash);
const BED: Format = fmt("BED", b'\t', false, &[b"#", b"track", b"browser"], Header::Hash);
const GTF: Format = fmt("GTF", b'\t', false, &[b"#"], Header::Hash);
const GFF: Format = fmt("GFF", b'\t', false, &[b"#"], Header::Hash);
const SAM: Format = fmt("SAM", b'\t', false, &[b"@"], Header::None);

const COMPRESSED: [&str; 5] = [".gz", ".bgz", ".bz2", ".xz", ".zst"];

/// True for a file lyst shows as a table that needs no decompressing. Such
/// files skip the input preprocessor: lyst lays them out itself, and
/// lesspipe would otherwise hand over its own already-formatted table.
pub fn reads_directly(name: &str) -> bool {
    let lower = name.to_lowercase();
    Format::for_name(&lower).is_some() && !COMPRESSED.iter().any(|z| lower.ends_with(z))
}

impl Format {
    /// The format implied by a file name, looking past compression suffixes
    /// (a preprocessor may have decompressed `calls.vcf.gz`).
    pub fn for_name(name: &str) -> Option<Format> {
        let lower = name.to_lowercase();
        let mut stem = lower.as_str();
        for z in COMPRESSED {
            stem = stem.strip_suffix(z).unwrap_or(stem);
        }
        let ext = stem.rsplit_once('.').map(|(_, e)| e)?;
        Some(match ext {
            "csv" => CSV,
            "tsv" | "tab" => TSV,
            "vcf" => VCF,
            "bed" | "bedgraph" | "narrowpeak" | "broadpeak" => BED,
            "gtf" => GTF,
            "gff" | "gff3" => GFF,
            "sam" => SAM,
            _ => return None,
        })
    }

    /// Guesses the separator from the first lines: the one found on the
    /// most lines, preferring a consistent count per line.
    pub fn sniff(doc: &Document) -> Option<Format> {
        let lines: Vec<&[u8]> = (0..doc.line_count().min(200))
            .map(|i| doc.line(i).1)
            .filter(|l| !l.is_empty() && !l.starts_with(b"#"))
            .take(50)
            .collect();
        let candidates = [
            (b'\t', "TSV", false),
            (b',', "CSV", true),
            (b';', "SSV", true),
            (b'|', "PSV", false),
        ];
        let mut best: Option<(usize, usize, Format)> = None;
        for (d, label, quotes) in candidates {
            let counts: Vec<usize> = lines.iter().map(|l| l.iter().filter(|&&b| b == d).count()).collect();
            let present = counts.iter().filter(|&&c| c > 0).count();
            if present == 0 {
                continue;
            }
            let consistent = counts.iter().filter(|&&c| c == counts[0]).count();
            let score = (present, consistent);
            if best.is_none_or(|(p, c, _)| score > (p, c)) {
                best = Some((present, consistent, fmt(label, d, quotes, &[], Header::First)));
            }
        }
        best.map(|(_, _, f)| f)
    }

    pub fn same_separator(&self, other: &Format) -> bool {
        self.delim == other.delim
    }

    fn is_meta(&self, line: &[u8]) -> bool {
        line.is_empty() || self.meta.iter().any(|m| line.starts_with(m))
    }
}

/// A field's bytes within its line.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Field {
    /// Where the field begins, including any opening quote.
    raw_start: usize,
    /// The displayed contents (inside quotes, for a quoted field).
    start: usize,
    end: usize,
    quoted: bool,
}

pub fn split(line: &[u8], delim: u8, quotes: bool) -> Vec<Field> {
    let mut fields = Vec::new();
    let mut i = 0;
    loop {
        let raw_start = i;
        let (start, end, quoted, rest) = if quotes && line.get(i) == Some(&b'"') {
            let start = i + 1;
            let mut j = start;
            let end = loop {
                match line.get(j) {
                    None => break j,
                    Some(b'"') if line.get(j + 1) == Some(&b'"') => j += 2,
                    Some(b'"') => {
                        j += 1;
                        break j - 1;
                    }
                    Some(_) => j += 1,
                }
            };
            // Anything between the closing quote and the separator is dropped.
            (start, end, true, j)
        } else {
            let end = memchr(delim, &line[i..]).map_or(line.len(), |p| i + p);
            (i, end, false, end)
        };
        fields.push(Field { raw_start, start, end, quoted });
        match memchr(delim, &line[rest.min(line.len())..]) {
            Some(p) => i = rest + p + 1,
            None => break,
        }
    }
    fields
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum RowKind {
    /// Shown as plain text (`##` lines, comments, blank lines).
    Meta,
    Header,
    Data,
}

/// A screen cell of a table row; `col` is `None` for separators.
#[derive(Clone, Copy, Debug)]
pub struct TCell {
    pub cell: Cell,
    pub col: Option<usize>,
}

/// Lays out bytes as cells: `(bytes, max_cols) -> cells`.
pub type Lay<'a> = dyn Fn(&[u8], usize) -> Vec<Cell> + 'a;

pub struct Table {
    pub format: Format,
    /// The line naming the columns, if any.
    pub header: Option<usize>,
    /// Display width of each column. Only grows, so columns stay put as you
    /// scroll back over rows already seen.
    widths: Vec<usize>,
    /// Color each column differently.
    pub colors: bool,
}

impl Table {
    pub fn new(doc: &Document, format: Format, lay: &Lay) -> Self {
        let limit = doc.line_count().min(10_000);
        let header = match format.header {
            Header::First => (0..limit).find(|&i| !format.is_meta(doc.line(i).1)),
            Header::Hash => {
                let mut h = None;
                for i in 0..limit {
                    let l = doc.line(i).1;
                    if l.starts_with(b"#") && !l.starts_with(b"##") && memchr(format.delim, l).is_some() {
                        h = Some(i);
                    } else if !format.is_meta(l) {
                        break;
                    }
                }
                h
            }
            Header::None => None,
        };
        let mut t = Table { format, header, widths: Vec::new(), colors: true };
        // Size the columns from the start of the file; rows seen later can widen them.
        let mut bytes = 0;
        for i in 0..doc.line_count().min(1000) {
            let l = doc.line(i).1;
            bytes += l.len();
            if bytes > 1 << 20 {
                break;
            }
            if t.kind(i, l) != RowKind::Meta {
                t.measure(l, lay);
            }
        }
        t
    }

    pub fn kind(&self, line: usize, bytes: &[u8]) -> RowKind {
        if self.header == Some(line) {
            RowKind::Header
        } else if self.format.is_meta(bytes) {
            RowKind::Meta
        } else {
            RowKind::Data
        }
    }

    fn split(&self, line: &[u8]) -> Vec<Field> {
        split(line, self.format.delim, self.format.quotes)
    }

    /// Widens columns to fit this row.
    pub fn measure(&mut self, line: &[u8], lay: &Lay) {
        for (k, f) in self.split(line).iter().enumerate() {
            let w = field_cells(line, f, lay).iter().map(|c| c.width as usize).sum::<usize>().clamp(1, MAX_WIDTH);
            match self.widths.get_mut(k) {
                Some(cur) => *cur = (*cur).max(w),
                None => self.widths.push(w),
            }
        }
    }

    /// Screen column where each column starts.
    pub fn column_starts(&self) -> Vec<usize> {
        let mut x = 0;
        self.widths
            .iter()
            .map(|w| {
                let s = x;
                x += w + SEP_WIDTH;
                s
            })
            .collect()
    }

    /// Lays out one row: fields padded or cut to their column widths, with
    /// separators between them. Every cell keeps its byte offset in the line.
    pub fn row_cells(&self, line: &[u8], lay: &Lay) -> Vec<TCell> {
        let fields = self.split(line);
        let mut out = Vec::new();
        let plain = |ch, byte| Cell { ch, width: 1, kind: CellKind::Text, byte, ansi: Default::default() };
        for (k, f) in fields.iter().enumerate() {
            if k > 0 {
                let byte = f.raw_start.saturating_sub(1);
                out.extend(SEP.chars().map(|ch| TCell { cell: plain(ch, byte), col: None }));
            }
            let cells = field_cells(line, f, lay);
            let natural: usize = cells.iter().map(|c| c.width as usize).sum();
            let width = self.widths.get(k).copied().unwrap_or(natural.clamp(1, MAX_WIDTH));
            let col = Some(k);
            let mut used = 0;
            if natural > width {
                for c in &cells {
                    if used + c.width as usize > width - 1 {
                        out.push(TCell { cell: Cell { ch: '…', width: 1, ..*c }, col });
                        used += 1;
                        break;
                    }
                    used += c.width as usize;
                    out.push(TCell { cell: *c, col });
                }
            } else {
                out.extend(cells.iter().map(|&cell| TCell { cell, col }));
                used = natural;
            }
            if k + 1 < fields.len() {
                out.extend((used..width).map(|_| TCell { cell: plain(' ', f.end), col }));
            }
        }
        out
    }
}

/// Cells for a field's displayed text, with `""` shown as one quote.
fn field_cells(line: &[u8], f: &Field, lay: &Lay) -> Vec<Cell> {
    let inner = &line[f.start..f.end];
    // Long fields are cut at MAX_WIDTH anyway; don't lay out megabytes.
    let mut cells = lay(inner, MAX_WIDTH * 2 + 2);
    for c in &mut cells {
        c.byte += f.start;
    }
    if f.quoted {
        let mut skip = Vec::new();
        let mut k = 0;
        while k < inner.len() {
            if inner[k] == b'"' && inner.get(k + 1) == Some(&b'"') {
                skip.push(f.start + k + 1);
                k += 2;
            } else {
                k += 1;
            }
        }
        cells.retain(|c| !skip.contains(&c.byte));
    }
    cells
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::encoding::{self, Encoding};

    fn lay(b: &[u8], max: usize) -> Vec<Cell> {
        encoding::layout(b, Encoding::Utf8, 8, max, false)
    }

    fn texts(line: &str, delim: u8, quotes: bool) -> Vec<String> {
        let b = line.as_bytes();
        split(b, delim, quotes)
            .iter()
            .map(|f| field_cells(b, f, &lay).iter().map(|c| c.ch).collect())
            .collect()
    }

    fn row(t: &Table, line: &str) -> String {
        t.row_cells(line.as_bytes(), &lay).iter().map(|c| c.cell.ch).collect()
    }

    fn doc(name: &str, s: &str) -> Document {
        Document::from_bytes(name, s.as_bytes().to_vec())
    }

    #[test]
    fn splitting_plain_and_quoted_fields() {
        assert_eq!(texts("a,b,,c,", b',', true), ["a", "b", "", "c", ""]);
        assert_eq!(texts("", b',', true), [""]);
        assert_eq!(texts(r#""x, y",2"#, b',', true), ["x, y", "2"]);
        assert_eq!(texts(r#""say ""hi""",z"#, b',', true), [r#"say "hi""#, "z"]);
        // An unterminated quote runs to the end of the line.
        assert_eq!(texts(r#""open,still"#, b',', true), ["open,still"]);
        // Without quoting, quotes are ordinary text.
        assert_eq!(texts("\"a\tb\"\tc", b'\t', false), ["\"a", "b\"", "c"]);
    }

    #[test]
    fn unquoting_keeps_byte_offsets() {
        let b = br#"1,"a""b""#;
        let f = split(b, b',', true);
        let cells = field_cells(b, &f[1], &lay);
        let bytes: Vec<usize> = cells.iter().map(|c| c.byte).collect();
        assert_eq!(bytes, [3, 4, 6], "a, the first quote of the pair, b");
    }

    #[test]
    fn formats_from_file_names() {
        let label = |n| Format::for_name(n).map(|f| f.label);
        assert_eq!(label("data.CSV"), Some("CSV"));
        assert_eq!(label("x.tsv"), Some("TSV"));
        assert_eq!(label("calls.vcf.gz"), Some("VCF"));
        assert_eq!(label("peaks.narrowPeak"), Some("BED"));
        assert_eq!(label("genes.gff3.bgz"), Some("GFF"));
        assert_eq!(label("reads.sam"), Some("SAM"));
        assert_eq!(label("notes.txt"), None);
        assert_eq!(label("Makefile"), None);
    }

    #[test]
    fn only_uncompressed_table_files_skip_preprocessing() {
        assert!(reads_directly("/data/people.CSV"));
        assert!(reads_directly("calls.vcf"));
        assert!(!reads_directly("calls.vcf.gz"));
        assert!(!reads_directly("notes.txt"));
        assert!(!reads_directly("archive.tar.gz"));
    }

    #[test]
    fn sniffing_the_separator() {
        let label = |s| Format::sniff(&doc("x", s)).map(|f| f.label);
        assert_eq!(label("a,b,c\n1,2,3\n"), Some("CSV"));
        assert_eq!(label("a\tb\n1\t2\n"), Some("TSV"));
        assert_eq!(label("a;b;c\n1;2,5;3\n4;5;6\n"), Some("SSV"), "semicolons on every line beat a stray comma");
        assert_eq!(label("# a,b\nx|y\n"), Some("PSV"), "comment lines are ignored");
        assert_eq!(label("just words\nhere\n"), None);
    }

    #[test]
    fn headers_and_metadata() {
        let vcf = doc("x.vcf", "##fileformat=VCFv4.2\n##INFO=<x>\n#CHROM\tPOS\tID\nchr1\t100\trs1\n");
        let t = Table::new(&vcf, VCF, &lay);
        assert_eq!(t.header, Some(2));
        assert_eq!(t.kind(0, vcf.line(0).1), RowKind::Meta);
        assert_eq!(t.kind(3, vcf.line(3).1), RowKind::Data);

        let bed = doc("x.bed", "track name=x\n#chrom\tstart\tend\nchr1\t1\t5\n");
        assert_eq!(Table::new(&bed, BED, &lay).header, Some(1));
        let bed = doc("x.bed", "# just a comment\nchr1\t1\t5\n");
        assert_eq!(Table::new(&bed, BED, &lay).header, None);

        let csv = doc("x.csv", "name,age\nann,3\n");
        assert_eq!(Table::new(&csv, CSV, &lay).header, Some(0));
        let sam = doc("x.sam", "@HD\tVN:1.6\nr1\t0\tchr1\n");
        let t = Table::new(&sam, SAM, &lay);
        assert_eq!(t.header, None);
        assert_eq!(t.kind(0, sam.line(0).1), RowKind::Meta);
    }

    #[test]
    fn rows_are_padded_into_columns() {
        let d = doc("x.csv", "name,n\nann,3\nbartholomew,12\n");
        let t = Table::new(&d, CSV, &lay);
        assert_eq!(row(&t, "name,n"), "name        │ n");
        assert_eq!(row(&t, "ann,3"), "ann         │ 3");
        assert_eq!(t.column_starts(), [0, 14]);
        // Extra fields beyond the known columns still show.
        assert_eq!(row(&t, "x,y,z"), "x           │ y  │ z");
    }

    #[test]
    fn long_fields_are_cut_and_columns_grow() {
        let long = "x".repeat(60);
        let d = doc("x.tsv", &format!("a\tb\n{long}\tq\n"));
        let mut t = Table::new(&d, TSV, &lay);
        let r = row(&t, &format!("{long}\tq"));
        assert_eq!(r, format!("{}… │ q", "x".repeat(MAX_WIDTH - 1)));
        // A row seen later widens its column.
        t.measure(b"a\twider\tz", &lay);
        assert!(row(&t, "a\tb\tc").ends_with("│ b     │ c"));
    }
}
