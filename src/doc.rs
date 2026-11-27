//! File loading and line indexing.

use memmap2::Mmap;
use std::fs::File;
use std::io::{self, Read};
use std::path::{Path, PathBuf};

enum Data {
    Mapped(Mmap),
    Owned(Vec<u8>),
}

fn slice(d: &Data) -> &[u8] {
    match d {
        Data::Mapped(m) => &m[..],
        Data::Owned(v) => v,
    }
}

/// An immutable view of a file's bytes plus an index of line start offsets.
pub struct Document {
    pub name: String,
    /// Name of the input preprocessor that produced this content, if any.
    pub filter: Option<String>,
    /// Set when this is an archive's table of contents ("zip", "tar.gz").
    pub archive: Option<&'static str>,
    path: Option<PathBuf>,
    data: Data,
    /// Byte offset of the start of every line. Always contains at least `0`.
    line_starts: Vec<usize>,
}

impl Document {
    pub fn open(path: &Path) -> io::Result<Self> {
        let data = load(path)?;
        let mut d = Document {
            name: path.display().to_string(),
            filter: None,
            archive: None,
            path: Some(path.to_owned()),
            data,
            line_starts: vec![0],
        };
        d.index_from(0);
        Ok(d)
    }

    pub fn from_bytes(name: impl Into<String>, bytes: Vec<u8>) -> Self {
        let mut d = Document {
            name: name.into(),
            filter: None,
            archive: None,
            path: None,
            data: Data::Owned(bytes),
            line_starts: vec![0],
        };
        d.index_from(0);
        d
    }

    pub fn bytes(&self) -> &[u8] {
        slice(&self.data)
    }

    pub fn len(&self) -> usize {
        self.bytes().len()
    }

    pub fn line_count(&self) -> usize {
        self.line_starts.len()
    }

    /// Returns `(start_offset, content)` for line `i`, without the line terminator.
    pub fn line(&self, i: usize) -> (usize, &[u8]) {
        let data = self.bytes();
        let start = self.line_starts[i];
        let mut end = match self.line_starts.get(i + 1) {
            Some(&next) => next - 1,
            None if data.last() == Some(&b'\n') => data.len() - 1,
            None => data.len(),
        };
        if end > start && data[end - 1] == b'\r' {
            end -= 1;
        }
        (start, &data[start..end])
    }

    pub fn line_start(&self, i: usize) -> usize {
        self.line_starts[i]
    }

    /// Offset one past the end of line `i`, including its terminator.
    pub fn line_end(&self, i: usize) -> usize {
        self.line_starts.get(i + 1).copied().unwrap_or(self.len())
    }

    /// Index of the line containing byte `off`.
    pub fn line_of_offset(&self, off: usize) -> usize {
        self.line_starts.partition_point(|&s| s <= off).saturating_sub(1)
    }

    pub fn filtered(mut self, filter: Option<String>) -> Self {
        self.filter = filter;
        self
    }

    /// True if the start of the file contains ANSI escape sequences (CSI).
    pub fn has_ansi(&self) -> bool {
        let d = self.bytes();
        memchr::memmem::find(&d[..d.len().min(64 * 1024)], b"\x1b[").is_some()
    }

    /// Heuristic: a NUL byte near the start means this is binary.
    pub fn looks_binary(&self) -> bool {
        if self.has_ansi() {
            return false;
        }
        let d = self.bytes();
        memchr::memchr(0, &d[..d.len().min(8192)]).is_some()
    }

    /// Re-reads the file if its size changed. Returns true if anything changed.
    pub fn reload(&mut self) -> io::Result<bool> {
        let Some(path) = self.path.clone() else {
            return Ok(false);
        };
        let new_len = std::fs::metadata(&path)?.len() as usize;
        let old_len = self.len();
        if new_len == old_len {
            return Ok(false);
        }
        self.data = load(&path)?;
        if self.len() > old_len {
            // Appended: existing line starts are still valid; rescan from the last one.
            let last = *self.line_starts.last().unwrap();
            self.index_from(last);
        } else {
            self.line_starts = vec![0];
            self.index_from(0);
        }
        Ok(true)
    }

    fn index_from(&mut self, start: usize) {
        let data = slice(&self.data);
        let len = data.len();
        self.line_starts.extend(
            memchr::memchr_iter(b'\n', &data[start..])
                .map(|p| start + p + 1)
                .filter(|&s| s < len),
        );
    }
}

/// Smallest file that gets memory-mapped. On Windows a mapped file can't be
/// truncated or replaced by other processes (editors saving, log rotation), so
/// only files too big to comfortably copy into memory are mapped there.
#[cfg(windows)]
const MAP_THRESHOLD: u64 = 64 * 1024 * 1024;
#[cfg(not(windows))]
const MAP_THRESHOLD: u64 = 1;

fn load(path: &Path) -> io::Result<Data> {
    let mut f = File::open(path)?;
    let meta = f.metadata()?;
    if !meta.is_file() || meta.len() < MAP_THRESHOLD {
        // Pipes, devices, and empty files can't (or needn't) be mapped, and
        // on Windows neither are files below the threshold.
        let mut v = Vec::new();
        f.read_to_end(&mut v)?;
        return Ok(Data::Owned(v));
    }
    // SAFETY: the mapping is read-only. If another process truncates the file
    // while it is mapped, reads past the new end may fault; this is the same
    // trade-off every mmap-based pager makes. (On Windows the OS refuses the
    // truncation instead, which is why small files aren't mapped there.)
    let m = unsafe { Mmap::map(&f)? };
    Ok(Data::Mapped(m))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn doc(s: &str) -> Document {
        Document::from_bytes("t", s.as_bytes().to_vec())
    }

    #[test]
    fn empty_file_has_one_empty_line() {
        let d = doc("");
        assert_eq!(d.line_count(), 1);
        assert_eq!(d.line(0), (0, &b""[..]));
    }

    #[test]
    fn trailing_newline_does_not_add_a_line() {
        let d = doc("a\nb\n");
        assert_eq!(d.line_count(), 2);
        assert_eq!(d.line(1), (2, &b"b"[..]));
    }

    #[test]
    fn crlf_is_stripped() {
        let d = doc("one\r\ntwo\r\n");
        assert_eq!(d.line(0).1, b"one");
        assert_eq!(d.line(1).1, b"two");
    }

    #[test]
    fn no_trailing_newline() {
        let d = doc("a\nbc");
        assert_eq!(d.line_count(), 2);
        assert_eq!(d.line(1).1, b"bc");
        assert_eq!(d.line_of_offset(3), 1);
        assert_eq!(d.line_of_offset(1), 0);
    }

    #[test]
    fn line_bounds_include_terminators() {
        let d = doc("ab\r\ncd\nef");
        assert_eq!((d.line_start(1), d.line_end(1)), (4, 7));
        assert_eq!(d.line_end(0), 4, "line_end includes the CRLF");
        assert_eq!(d.line_end(2), d.len());
        // The newline byte itself belongs to the line it ends.
        assert_eq!(d.line_of_offset(6), 1);
        assert_eq!(d.line_of_offset(7), 2);
        assert_eq!(d.line_of_offset(d.len() + 10), 2);
    }

    #[test]
    fn lone_cr_is_kept_and_empty_lines_count() {
        let d = doc("\r\n\n\rx\n");
        assert_eq!(d.line_count(), 3);
        assert_eq!(d.line(0).1, b"");
        assert_eq!(d.line(1).1, b"");
        assert_eq!(d.line(2).1, b"\rx", "only a CR right before LF is stripped");
    }

    #[test]
    fn binary_and_ansi_detection() {
        assert!(!doc("plain text\n").looks_binary());
        assert!(Document::from_bytes("t", b"ELF\0\x01\x02".to_vec()).looks_binary());
        let colored = Document::from_bytes("t", b"\x1b[31mred\x1b[0m\0".to_vec());
        assert!(colored.has_ansi());
        assert!(!colored.looks_binary(), "ANSI output wins over a stray NUL");
        // A NUL past the sniffing window doesn't count.
        let mut late = vec![b'a'; 9000];
        late.push(0);
        assert!(!Document::from_bytes("t", late).looks_binary());
    }

    #[test]
    fn reload_handles_append_truncate_and_no_change() {
        let path = std::env::temp_dir().join(format!("lyst-doc-reload-{}.txt", std::process::id()));
        std::fs::write(&path, "a\nb").unwrap();
        let mut d = Document::open(&path).unwrap();
        assert_eq!(d.line_count(), 2);
        assert!(!d.reload().unwrap(), "same size means unchanged");

        // Appending continues the unterminated last line.
        std::fs::write(&path, "a\nbc\nd\n").unwrap();
        assert!(d.reload().unwrap());
        assert_eq!(d.line_count(), 3);
        assert_eq!(d.line(1).1, b"bc");
        assert_eq!(d.line(2).1, b"d");

        // Shrinking rebuilds the index from scratch.
        std::fs::write(&path, "z").unwrap();
        assert!(d.reload().unwrap());
        assert_eq!(d.line_count(), 1);
        assert_eq!(d.line(0).1, b"z");

        // In-memory documents never reload.
        assert!(!doc("x").reload().unwrap());
        let _ = std::fs::remove_file(&path);
    }

    #[test]
    fn open_empty_file_and_missing_file() {
        let path = std::env::temp_dir().join(format!("lyst-doc-empty-{}.txt", std::process::id()));
        std::fs::write(&path, "").unwrap();
        let d = Document::open(&path).unwrap();
        assert_eq!((d.len(), d.line_count()), (0, 1));
        let _ = std::fs::remove_file(&path);
        assert!(Document::open(&path).is_err());
    }
}
