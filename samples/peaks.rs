//! Reads BED peaks and merges overlapping ones, for trying lyst's syntax
//! highlighting. Standalone: `rustc samples/peaks.rs && ./peaks samples/peaks.bed`.

use std::collections::BTreeMap;
use std::fmt;
use std::fs;
use std::process::ExitCode;

/// A half-open genomic interval, `[start, end)`, as in BED.
#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord)]
struct Peak<'a> {
    chrom: &'a str,
    start: u64,
    end: u64,
    score: u16,
}

#[derive(Debug)]
enum ParseError {
    MissingField { line: usize, field: &'static str },
    BadNumber { line: usize, text: String },
}

impl fmt::Display for ParseError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::MissingField { line, field } => write!(f, "line {line}: missing {field}"),
            Self::BadNumber { line, text } => write!(f, "line {line}: {text:?} is not a number"),
        }
    }
}

fn parse(text: &str) -> Result<Vec<Peak<'_>>, ParseError> {
    let mut peaks = Vec::new();
    for (i, line) in text.lines().enumerate() {
        // Skip headers, comments, and UCSC track/browser lines.
        if line.is_empty() || ["#", "track", "browser"].iter().any(|p| line.starts_with(p)) {
            continue;
        }
        let mut cols = line.split('\t');
        let mut next = |field| cols.next().ok_or(ParseError::MissingField { line: i + 1, field });
        let num = |s: &str| s.parse().map_err(|_| ParseError::BadNumber { line: i + 1, text: s.into() });
        let chrom = next("chrom")?;
        let start = num(next("start")?)?;
        let end = num(next("end")?)?;
        let _name = next("name");
        let score = next("score").map_or(Ok(0), |s| num(s))? as u16;
        peaks.push(Peak { chrom, start, end, score });
    }
    Ok(peaks)
}

/// Merges overlapping or touching peaks per chromosome, keeping the best score.
fn merge<'a>(mut peaks: Vec<Peak<'a>>) -> BTreeMap<&'a str, Vec<Peak<'a>>> {
    peaks.sort();
    let mut out: BTreeMap<&str, Vec<Peak>> = BTreeMap::new();
    for p in peaks {
        let list = out.entry(p.chrom).or_default();
        match list.last_mut() {
            Some(last) if p.start <= last.end => {
                last.end = last.end.max(p.end);
                last.score = last.score.max(p.score);
            }
            _ => list.push(p),
        }
    }
    out
}

fn main() -> ExitCode {
    let path = std::env::args().nth(1).unwrap_or_else(|| "samples/peaks.bed".to_owned());
    let text = match fs::read_to_string(&path) {
        Ok(t) => t,
        Err(e) => {
            eprintln!("{path}: {e}");
            return ExitCode::FAILURE;
        }
    };
    let peaks = match parse(&text) {
        Ok(p) => p,
        Err(e) => {
            eprintln!("{path}: {e}");
            return ExitCode::from(2);
        }
    };
    let total: u64 = peaks.iter().map(|p| p.end - p.start).sum();
    println!("{} peaks covering {total} bp", peaks.len());
    for (chrom, list) in merge(peaks) {
        let spans: Vec<String> = list.iter().map(|p| format!("{}-{}", p.start, p.end)).collect();
        println!("{chrom:>6}: {}", spans.join(", "));
    }
    ExitCode::SUCCESS
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn overlapping_peaks_merge() {
        let text = "chr1\t10\t20\ta\t5\nchr1\t15\t30\tb\t9\nchr2\t0\t5\tc\t1\n";
        let merged = merge(parse(text).unwrap());
        assert_eq!(merged["chr1"].len(), 1);
        assert_eq!((merged["chr1"][0].end, merged["chr1"][0].score), (30, 9));
    }
}
