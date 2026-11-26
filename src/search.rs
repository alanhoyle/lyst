//! Text search over document bytes.

use crate::doc::Document;
use regex::bytes::{Regex, RegexBuilder};

pub type Match = (usize, usize);

pub struct Search {
    pub pattern: String,
    re: Regex,
}

impl Search {
    /// Literal by default; `regex` enables regular expressions. Case is
    /// ignored unless the pattern contains an uppercase letter ("smart case").
    pub fn new(pattern: &str, regex: bool) -> Result<Self, String> {
        if pattern.is_empty() {
            return Err("empty pattern".into());
        }
        let src = if regex { pattern.to_string() } else { regex::escape(pattern) };
        let re = RegexBuilder::new(&src)
            .case_insensitive(!pattern.chars().any(char::is_uppercase))
            .multi_line(true)
            .crlf(true)
            .build()
            .map_err(|e| e.to_string())?;
        Ok(Search { pattern: pattern.to_string(), re })
    }

    /// First match starting at or after `from`.
    pub fn forward(&self, data: &[u8], from: usize) -> Option<Match> {
        if from > data.len() {
            return None;
        }
        self.re.find_at(data, from).map(|m| (m.start(), m.end()))
    }

    /// Last match starting strictly before `before`, scanning line by line.
    pub fn backward(&self, doc: &Document, before: usize) -> Option<Match> {
        if before == 0 {
            return None;
        }
        let first = doc.line_of_offset(before - 1);
        for i in (0..=first).rev() {
            let (start, line) = doc.line(i);
            let hit = self
                .re
                .find_iter(line)
                .map(|m| (start + m.start(), start + m.end()))
                .take_while(|&(s, _)| s < before)
                .last();
            if hit.is_some() {
                return hit;
            }
        }
        None
    }

    /// Non-empty matches within `hay`, relative to it.
    pub fn matches_in(&self, hay: &[u8]) -> Vec<Match> {
        self.re
            .find_iter(hay)
            .filter(|m| !m.is_empty())
            .take(4096)
            .map(|m| (m.start(), m.end()))
            .collect()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn smart_case_and_literal() {
        let d = b"Foo foo a.b axb";
        let s = Search::new("foo", false).unwrap();
        assert_eq!(s.forward(d, 0), Some((0, 3)));
        let s = Search::new("foo", false).unwrap();
        assert_eq!(s.forward(d, 1), Some((4, 7)));
        let s = Search::new("Foo", false).unwrap();
        assert_eq!(s.forward(d, 1), None);
        let s = Search::new("a.b", false).unwrap();
        assert_eq!(s.matches_in(d), vec![(8, 11)]);
        let s = Search::new("a.b", true).unwrap();
        assert_eq!(s.matches_in(d).len(), 2);
    }

    #[test]
    fn backward_crosses_lines() {
        let doc = Document::from_bytes("t", b"cat\ndog\ncat dog\nbird\n".to_vec());
        let s = Search::new("cat", false).unwrap();
        assert_eq!(s.backward(&doc, doc.len()), Some((8, 11)));
        assert_eq!(s.backward(&doc, 8), Some((0, 3)));
        assert_eq!(s.backward(&doc, 0), None);
    }

    #[test]
    fn anchors_work_per_line_with_crlf() {
        let s = Search::new("^b.*d$", true).unwrap();
        assert_eq!(s.forward(b"abc\r\nbird\r\n", 0), Some((5, 9)));
    }

    #[test]
    fn bad_patterns_are_errors() {
        assert!(Search::new("", false).is_err());
        assert!(Search::new("(unclosed", true).is_err());
        // The same text is fine as a literal.
        assert!(Search::new("(unclosed", false).is_ok());
    }

    #[test]
    fn forward_at_and_past_the_end() {
        let s = Search::new("x", false).unwrap();
        assert_eq!(s.forward(b"abx", 2), Some((2, 3)));
        assert_eq!(s.forward(b"abx", 3), None);
        assert_eq!(s.forward(b"abx", 99), None);
    }

    #[test]
    fn backward_picks_last_match_before_cursor_on_same_line() {
        let doc = Document::from_bytes("t", b"ab ab ab\n".to_vec());
        let s = Search::new("ab", false).unwrap();
        assert_eq!(s.backward(&doc, 6), Some((3, 5)));
        assert_eq!(s.backward(&doc, 7), Some((6, 8)), "a match starting before the cursor counts");
        assert_eq!(s.backward(&doc, 1), Some((0, 2)));
    }

    #[test]
    fn uppercase_in_regex_class_turns_off_smart_case() {
        let s = Search::new("[A-Z]+", true).unwrap();
        assert_eq!(s.matches_in(b"abc DEF"), vec![(4, 7)]);
    }

    #[test]
    fn empty_regex_matches_are_not_highlighted() {
        let s = Search::new("x*", true).unwrap();
        assert_eq!(s.matches_in(b"axxb"), vec![(1, 3)]);
    }
}
