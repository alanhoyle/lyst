//! Zip and tar (plain or gzipped) archives: a table of contents shown in
//! place of the archive's bytes, and the files inside opened one at a time.
//!
//! Listing reads zip's central directory, or tar's headers with the
//! contents skipped. Opening a file reads just that file: from a plain tar
//! it's a byte range of the archive (memory-mapped, any size); from a zip
//! it's decompressed on its own; from a tar.gz everything before it has to
//! be decompressed too, since gzip can't be read from the middle.
//!
//! A listing is tab-separated text: `#` summary lines, a header row, then
//! one row per entry with the name last, so table view can show it in full.

use std::fs::File;
use std::io::{self, BufReader, Cursor, Read, Seek, SeekFrom};
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering::Relaxed};

/// Largest file opened from a zip or tar.gz, which is decompressed into
/// memory. (A plain tar's files are mapped from disk, at any size.)
pub const MAX_IN_MEMORY: u64 = 1 << 30;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Kind {
    Zip,
    Tar,
    TarGz,
}

impl Kind {
    pub fn label(self) -> &'static str {
        match self {
            Kind::Zip => "zip",
            Kind::Tar => "tar",
            Kind::TarGz => "tar.gz",
        }
    }
}

/// Where an archive's bytes are.
#[derive(Clone, Debug)]
pub enum Origin {
    /// A file, or a byte range of one (a tar inside a tar).
    File { path: PathBuf, start: u64, len: u64 },
    /// In memory: a file taken out of a compressed archive.
    Bytes(Arc<[u8]>),
}

trait ReadSeek: Read + Seek + Send {}
impl<T: Read + Seek + Send> ReadSeek for T {}

impl Origin {
    pub fn file(path: &Path) -> io::Result<Origin> {
        Ok(Origin::File { path: path.to_path_buf(), start: 0, len: std::fs::metadata(path)?.len() })
    }

    fn len(&self) -> u64 {
        match self {
            Origin::File { len, .. } => *len,
            Origin::Bytes(b) => b.len() as u64,
        }
    }

    fn reader(&self) -> io::Result<Box<dyn ReadSeek + '_>> {
        Ok(match self {
            Origin::File { path, start, len } => Box::new(BufReader::new(Window::new(File::open(path)?, *start, *len)?)),
            Origin::Bytes(b) => Box::new(Cursor::new(&b[..])),
        })
    }
}

/// Part of a file, read and seeked as if it were the whole file.
struct Window {
    file: File,
    start: u64,
    len: u64,
    pos: u64,
}

impl Window {
    fn new(mut file: File, start: u64, len: u64) -> io::Result<Window> {
        file.seek(SeekFrom::Start(start))?;
        Ok(Window { file, start, len, pos: 0 })
    }
}

impl Read for Window {
    fn read(&mut self, buf: &mut [u8]) -> io::Result<usize> {
        let room = self.len.saturating_sub(self.pos).min(buf.len() as u64) as usize;
        let n = self.file.read(&mut buf[..room])?;
        self.pos += n as u64;
        Ok(n)
    }
}

impl Seek for Window {
    fn seek(&mut self, to: SeekFrom) -> io::Result<u64> {
        let pos = match to {
            SeekFrom::Start(n) => Some(n),
            SeekFrom::Current(d) => self.pos.checked_add_signed(d),
            SeekFrom::End(d) => self.len.checked_add_signed(d),
        }
        .ok_or_else(|| io::Error::new(io::ErrorKind::InvalidInput, "seek before the start"))?;
        self.file.seek(SeekFrom::Start(self.start + pos))?;
        self.pos = pos;
        Ok(pos)
    }
}

/// Recognizes an archive from its first bytes, whatever its name.
pub fn detect(origin: &Origin) -> Option<Kind> {
    let mut r = origin.reader().ok()?;
    let head = read_up_to(&mut r, 512);
    if head.starts_with(b"PK\x03\x04") || head.starts_with(b"PK\x05\x06") {
        Some(Kind::Zip)
    } else if is_tar(&head) {
        Some(Kind::Tar)
    } else if head.starts_with(&[0x1f, 0x8b]) {
        // Only a tar inside counts; other .gz files go to the preprocessor.
        r.seek(SeekFrom::Start(0)).ok()?;
        let inner = read_up_to(&mut flate2::read::MultiGzDecoder::new(r), 512);
        is_tar(&inner).then_some(Kind::TarGz)
    } else {
        None
    }
}

/// POSIX and GNU tar headers say "ustar" at offset 257.
fn is_tar(head: &[u8]) -> bool {
    head.get(257..262) == Some(b"ustar")
}

fn read_up_to(r: &mut impl Read, n: usize) -> Vec<u8> {
    let mut v = Vec::with_capacity(n);
    let _ = r.take(n as u64).read_to_end(&mut v);
    v
}

/// One file, directory or link in an archive.
#[derive(Clone, Debug)]
pub struct Entry {
    /// Its path inside the archive.
    pub name: String,
    pub size: u64,
    pub is_dir: bool,
    /// What a link points to.
    pub link: Option<String>,
    /// The link is a hard link, whose target is a path in the archive.
    pub hard: bool,
    /// zip: index in the central directory. tar: position among entries.
    seq: usize,
    /// tar: where the contents start, when they're stored as is (not sparse).
    offset: Option<u64>,
}

/// An archive's entries, matched to the rows of its listing.
#[derive(Debug)]
pub struct Index {
    pub kind: Kind,
    pub origin: Origin,
    pub entries: Vec<Entry>,
    /// Listing line of the first entry; entries follow one per line.
    pub first_row: usize,
}

impl Index {
    /// The entry shown on a line of the listing.
    pub fn entry_at(&self, line: usize) -> Option<&Entry> {
        self.entries.get(line.checked_sub(self.first_row)?)
    }

    /// The entry whose contents `e` shows: itself, or what its link points
    /// to. Directories and links that lead outside the archive are errors.
    pub fn resolve<'a>(&'a self, e: &'a Entry) -> Result<&'a Entry, String> {
        let mut cur = e;
        for _ in 0..16 {
            let Some(target) = &cur.link else {
                return if cur.is_dir { Err(format!("{} is a directory", cur.name)) } else { Ok(cur) };
            };
            let path = if cur.hard || target.starts_with('/') {
                normalize(target)
            } else {
                let parent = cur.name.trim_end_matches('/').rsplit_once('/').map_or("", |(p, _)| p);
                normalize(&format!("{parent}/{target}"))
            };
            cur = self
                .entries
                .iter()
                .find(|x| normalize(&x.name) == path)
                .ok_or_else(|| format!("{} links to {target}, which isn't in the archive", cur.name))?;
        }
        Err(format!("{}: too many links", e.name))
    }
}

/// A path inside an archive with `.`, `..` and repeated or edge `/` resolved.
fn normalize(path: &str) -> String {
    let mut parts: Vec<&str> = Vec::new();
    for p in path.split('/') {
        match p {
            "" | "." => {}
            ".." => {
                parts.pop();
            }
            _ => parts.push(p),
        }
    }
    parts.join("/")
}

/// The listing text for an archive, and its index. Stops with an
/// `Interrupted` error once `cancel` is set. An archive that can't be read
/// at all is an error; one that breaks partway lists what came before,
/// with a note.
pub fn listing(origin: &Origin, kind: Kind, cancel: &AtomicBool) -> io::Result<(Vec<u8>, Index)> {
    let reader = Cancelable { inner: origin.reader()?, cancel };
    let mut out = Listing::default();
    let result = match kind {
        Kind::Zip => list_zip(reader, &mut out),
        Kind::Tar => list_tar(tar::Archive::new(reader).entries_with_seek()?, true, &mut out),
        Kind::TarGz => {
            let gz = flate2::read::MultiGzDecoder::new(reader);
            list_tar(tar::Archive::new(gz).entries()?, false, &mut out)
        }
    };
    match result {
        Err(e) if e.kind() == io::ErrorKind::Interrupted => return Err(e),
        Err(e) if out.entries.is_empty() => return Err(e),
        Err(e) => out.notes.push(format!("Listing stopped early: {e}")),
        Ok(()) => {}
    }
    let (text, first_row) = out.render(kind, origin.len());
    let index = Index { kind, origin: origin.clone(), entries: out.entries, first_row };
    Ok((text, index))
}

/// A file's contents, taken from an archive.
pub enum Contents {
    Bytes(Vec<u8>),
    /// Stored as is in a tar file on disk: this range of it.
    Range { path: PathBuf, start: u64, len: u64 },
}

/// Reads the contents of `e`, an entry of the archive at `origin`.
pub fn open(origin: &Origin, kind: Kind, e: &Entry, cancel: &AtomicBool) -> io::Result<Contents> {
    let reader = Cancelable { inner: origin.reader()?, cancel };
    let bytes = match (kind, e.offset) {
        (Kind::Tar, Some(off)) => match origin {
            Origin::File { path, start, .. } => {
                return Ok(Contents::Range { path: path.clone(), start: start + off, len: e.size });
            }
            Origin::Bytes(b) => {
                let end = off.checked_add(e.size).filter(|&end| end <= b.len() as u64);
                let end = end.ok_or_else(|| io::Error::new(io::ErrorKind::UnexpectedEof, "the archive is cut short"))?;
                b[off as usize..end as usize].to_vec()
            }
        },
        (Kind::Tar, None) => nth_tar_entry(tar::Archive::new(reader).entries_with_seek()?, e)?,
        (Kind::TarGz, _) => {
            let gz = flate2::read::MultiGzDecoder::new(reader);
            nth_tar_entry(tar::Archive::new(gz).entries()?, e)?
        }
        (Kind::Zip, _) => {
            let mut zip = zip::ZipArchive::new(reader).map_err(zip_error)?;
            let mut f = zip.by_index(e.seq).map_err(zip_error)?;
            let size = f.size();
            read_limited(&mut f, size)?
        }
    };
    Ok(Contents::Bytes(bytes))
}

/// The contents of the tar entry `e`, read by going through the entries
/// in order (a tar.gz, or a sparse file).
fn nth_tar_entry<R: Read>(entries: tar::Entries<'_, R>, e: &Entry) -> io::Result<Vec<u8>> {
    for (i, entry) in entries.enumerate() {
        let mut entry = entry?;
        if i == e.seq {
            return read_limited(&mut entry, e.size);
        }
    }
    Err(io::Error::new(io::ErrorKind::UnexpectedEof, "the archive ends before this file"))
}

/// Reads a file decompressed into memory, refusing one over MAX_IN_MEMORY.
fn read_limited(r: &mut impl Read, size: u64) -> io::Result<Vec<u8>> {
    let too_big = || {
        io::Error::other(format!(
            "too large to open from a compressed archive ({}; the limit is {})",
            crate::browser::human_size(size),
            crate::browser::human_size(MAX_IN_MEMORY)
        ))
    };
    if size > MAX_IN_MEMORY {
        return Err(too_big());
    }
    let mut v = Vec::with_capacity(size as usize);
    r.take(MAX_IN_MEMORY + 1).read_to_end(&mut v)?;
    if v.len() as u64 > MAX_IN_MEMORY {
        return Err(too_big());
    }
    Ok(v)
}

/// Rows and entries collected so far, and totals for the summary.
#[derive(Default)]
struct Listing {
    header: &'static str,
    rows: Vec<u8>,
    entries: Vec<Entry>,
    files: u64,
    dirs: u64,
    bytes: u64,
    /// zip only: the total compressed size of the entries.
    packed: Option<u64>,
    notes: Vec<String>,
}

impl Listing {
    fn row(&mut self, fields: &[&[u8]], entry: Entry) {
        for (i, f) in fields.iter().enumerate() {
            if i > 0 {
                self.rows.push(b'\t');
            }
            clean_into(&mut self.rows, f);
        }
        self.rows.push(b'\n');
        if entry.is_dir {
            self.dirs += 1;
        } else {
            self.files += 1;
            self.bytes += entry.size;
        }
        self.entries.push(entry);
    }

    /// The listing text, and the line its first entry is on.
    fn render(&self, kind: Kind, archive_len: u64) -> (Vec<u8>, usize) {
        let size = |n: u64| if n < 100_000 { format!("{n} bytes") } else { crate::browser::human_size(n) };
        let plural = |n: u64, one: &str, many: &str| format!("{n} {}", if n == 1 { one } else { many });
        let mut summary = format!(
            "# {} archive: {}, {}, {} of contents",
            kind.label(),
            plural(self.files, "file", "files"),
            plural(self.dirs, "directory", "directories"),
            size(self.bytes)
        );
        match (kind, self.packed) {
            (Kind::Zip, Some(p)) => summary += &format!(" ({} compressed)", size(p)),
            (Kind::TarGz, _) => summary += &format!(" ({} compressed)", size(archive_len)),
            _ => {}
        }
        let mut out = summary.into_bytes();
        out.push(b'\n');
        for n in &self.notes {
            out.extend_from_slice(format!("# {n}\n").as_bytes());
        }
        out.extend_from_slice(self.header.as_bytes());
        out.push(b'\n');
        out.extend_from_slice(&self.rows);
        (out, self.notes.len() + 2)
    }
}

/// Copies a field, showing tabs and line breaks in names as `\t`, `\n`, `\r`
/// so they can't split the row.
fn clean_into(out: &mut Vec<u8>, field: &[u8]) {
    for &b in field {
        match b {
            b'\t' => out.extend_from_slice(b"\\t"),
            b'\n' => out.extend_from_slice(b"\\n"),
            b'\r' => out.extend_from_slice(b"\\r"),
            _ => out.push(b),
        }
    }
}

fn list_zip<R: Read + Seek>(reader: R, out: &mut Listing) -> io::Result<()> {
    out.header = "Size\tCompressed\tModified\tName";
    out.packed = Some(0);
    let mut zip = zip::ZipArchive::new(reader).map_err(zip_error)?;
    for i in 0..zip.len() {
        // by_index_raw reads the entry's headers without decompressing it.
        let f = zip.by_index_raw(i).map_err(zip_error)?;
        let name = match f.name() {
            Ok(n) => n.into_owned(),
            Err(_) => String::from_utf8_lossy(f.name_raw()).into_owned(),
        };
        let modified = f
            .last_modified()
            .map(|d| format!("{:04}-{:02}-{:02} {:02}:{:02}", d.year(), d.month(), d.day(), d.hour(), d.minute()))
            .unwrap_or_default();
        if !f.is_dir() {
            *out.packed.get_or_insert(0) += f.compressed_size();
        }
        let (size, packed) = (f.size().to_string(), f.compressed_size().to_string());
        let entry = Entry { name: name.clone(), size: f.size(), is_dir: f.is_dir(), link: None, hard: false, seq: i, offset: None };
        out.row(&[size.as_bytes(), packed.as_bytes(), modified.as_bytes(), name.as_bytes()], entry);
    }
    Ok(())
}

fn zip_error(e: zip::result::ZipError) -> io::Error {
    match e {
        zip::result::ZipError::Io(e) => e,
        e => io::Error::new(io::ErrorKind::InvalidData, e),
    }
}

/// `seekable`: the contents can be read later at their offset.
fn list_tar<R: Read>(entries: tar::Entries<'_, R>, seekable: bool, out: &mut Listing) -> io::Result<()> {
    out.header = "Mode\tOwner\tSize\tModified\tName";
    for (seq, entry) in entries.enumerate() {
        let e = entry?;
        let h = e.header();
        let t = h.entry_type();
        if t.is_pax_global_extensions() {
            continue;
        }
        let kind = if t.is_dir() {
            'd'
        } else if t.is_symlink() {
            'l'
        } else if t.is_hard_link() {
            'h'
        } else if t.is_character_special() {
            'c'
        } else if t.is_block_special() {
            'b'
        } else if t.is_fifo() {
            'p'
        } else {
            '-'
        };
        let mode = format!("{kind}{}", crate::usage::perms(h.mode().unwrap_or(0)));
        let user = h.username_bytes().filter(|u| !u.is_empty()).map(|u| String::from_utf8_lossy(u).into_owned());
        let group = h.groupname_bytes().filter(|g| !g.is_empty()).map(|g| String::from_utf8_lossy(g).into_owned());
        let owner = format!(
            "{}:{}",
            user.unwrap_or_else(|| h.uid().map_or("?".into(), |u| u.to_string())),
            group.unwrap_or_else(|| h.gid().map_or("?".into(), |g| g.to_string()))
        );
        let modified = h
            .mtime()
            .ok()
            .and_then(|s| chrono::DateTime::from_timestamp(s as i64, 0))
            .map(|t| t.with_timezone(&chrono::Local).format("%Y-%m-%d %H:%M").to_string())
            .unwrap_or_default();
        let name = String::from_utf8_lossy(&e.path_bytes()).into_owned();
        let link = e.link_name_bytes().map(|l| String::from_utf8_lossy(&l).into_owned());
        let mut shown = name.clone().into_bytes();
        if let Some(target) = &link {
            shown.extend_from_slice(if t.is_hard_link() { b" link to " } else { b" -> " });
            shown.extend_from_slice(target.as_bytes());
        }
        let size = e.size().to_string();
        let offset = (seekable && !t.is_gnu_sparse()).then(|| e.raw_file_position());
        let entry = Entry { name, size: e.size(), is_dir: t.is_dir(), link, hard: t.is_hard_link(), seq, offset };
        out.row(&[mode.as_bytes(), owner.as_bytes(), size.as_bytes(), modified.as_bytes(), &shown], entry);
    }
    Ok(())
}

/// A reader that fails with `Interrupted` once `cancel` is set, so a long
/// listing or decompression stops at the next read.
struct Cancelable<'a, R> {
    inner: R,
    cancel: &'a AtomicBool,
}

impl<R: Read> Read for Cancelable<'_, R> {
    fn read(&mut self, buf: &mut [u8]) -> io::Result<usize> {
        if self.cancel.load(Relaxed) {
            return Err(io::Error::new(io::ErrorKind::Interrupted, "Canceled"));
        }
        self.inner.read(buf)
    }
}

impl<R: Seek> Seek for Cancelable<'_, R> {
    fn seek(&mut self, pos: SeekFrom) -> io::Result<u64> {
        self.inner.seek(pos)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Write;

    fn temp(name: &str) -> PathBuf {
        let dir = std::env::temp_dir().join(format!("lyst-archive-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        dir.join(name)
    }

    fn no() -> AtomicBool {
        AtomicBool::new(false)
    }

    fn list(path: &Path) -> (Option<Kind>, String, Option<Index>) {
        let origin = Origin::file(path).unwrap();
        let kind = detect(&origin);
        match kind.map(|k| listing(&origin, k, &no()).unwrap()) {
            Some((text, index)) => (kind, String::from_utf8(text).unwrap(), Some(index)),
            None => (None, String::new(), None),
        }
    }

    /// The contents of the entry on listing line `line`, following links.
    fn read(index: &Index, line: usize) -> Result<Vec<u8>, String> {
        let e = index.resolve(index.entry_at(line).unwrap())?;
        match open(&index.origin, index.kind, e, &no()).map_err(|e| e.to_string())? {
            Contents::Bytes(b) => Ok(b),
            Contents::Range { path, start, len } => {
                let mut f = File::open(path).unwrap();
                f.seek(SeekFrom::Start(start)).unwrap();
                Ok(read_up_to(&mut f, len as usize))
            }
        }
    }

    fn tar_bytes() -> Vec<u8> {
        let mut b = tar::Builder::new(Vec::new());
        let mut h = tar::Header::new_gnu();
        h.set_entry_type(tar::EntryType::Directory);
        h.set_mode(0o755);
        h.set_size(0);
        h.set_mtime(0);
        h.set_username("ann").unwrap();
        h.set_groupname("staff").unwrap();
        h.set_cksum();
        b.append_data(&mut h, "docs/", io::empty()).unwrap();
        let mut h = tar::Header::new_gnu();
        h.set_mode(0o644);
        h.set_size(5);
        h.set_mtime(0);
        h.set_uid(501);
        h.set_gid(20);
        // Longer than a tar header's 100 bytes, and with a tab in it.
        let long = format!("docs/{}\tx.txt", "n".repeat(120));
        b.append_data(&mut h, &long, &b"hello"[..]).unwrap();
        let mut h = tar::Header::new_gnu();
        h.set_mode(0o777);
        h.set_size(0);
        h.set_mtime(0);
        h.set_entry_type(tar::EntryType::Symlink);
        b.append_link(&mut h, "latest", &long).unwrap();
        let mut h = tar::Header::new_gnu();
        h.set_mode(0o644);
        h.set_size(6);
        h.set_mtime(0);
        b.append_data(&mut h, "docs/b.txt", &b"world\n"[..]).unwrap();
        b.into_inner().unwrap()
    }

    #[test]
    fn lists_and_opens_a_tar() {
        let p = temp("a.tar");
        std::fs::write(&p, tar_bytes()).unwrap();
        let (kind, text, index) = list(&p);
        let index = index.unwrap();
        assert_eq!(kind, Some(Kind::Tar));
        let lines: Vec<&str> = text.lines().collect();
        assert_eq!(lines[0], "# tar archive: 3 files, 1 directory, 11 bytes of contents");
        assert_eq!(lines[1], "Mode\tOwner\tSize\tModified\tName");
        assert_eq!(index.first_row, 2);
        assert!(lines[2].starts_with("drwxr-xr-x\tann:staff\t0\t") && lines[2].ends_with("\tdocs/"), "{}", lines[2]);
        assert!(lines[3].starts_with("-rw-r--r--\t501:20\t5\t"), "ids without names: {}", lines[3]);
        assert!(lines[3].ends_with(&format!("\tdocs/{}\\tx.txt", "n".repeat(120))), "long name, tab shown as \\t");
        assert!(lines[4].starts_with("lrwxrwxrwx") && lines[4].contains("\tlatest -> docs/"), "{}", lines[4]);
        assert_eq!(lines.len(), 6);

        assert_eq!(read(&index, 3).unwrap(), b"hello");
        assert_eq!(read(&index, 5).unwrap(), b"world\n");
        assert_eq!(read(&index, 4).unwrap(), b"hello", "the link opens its target");
        assert_eq!(read(&index, 2).unwrap_err(), "docs/ is a directory");
        assert!(index.entry_at(1).is_none() && index.entry_at(6).is_none(), "header and past the end");
    }

    #[test]
    fn opens_from_a_gzipped_tar_and_not_other_gzip() {
        let p = temp("a.tgz");
        let mut gz = flate2::write::GzEncoder::new(Vec::new(), flate2::Compression::default());
        gz.write_all(&tar_bytes()).unwrap();
        let bytes = gz.finish().unwrap();
        std::fs::write(&p, &bytes).unwrap();
        let (kind, text, index) = list(&p);
        assert_eq!(kind, Some(Kind::TarGz));
        assert!(
            text.starts_with(&format!("# tar.gz archive: 3 files, 1 directory, 11 bytes of contents ({} bytes compressed)\n", bytes.len())),
            "{text}"
        );
        let index = index.unwrap();
        assert_eq!(read(&index, 5).unwrap(), b"world\n");
        assert_eq!(read(&index, 4).unwrap(), b"hello");

        let p = temp("calls.vcf.gz");
        let mut gz = flate2::write::GzEncoder::new(Vec::new(), flate2::Compression::default());
        gz.write_all(b"##fileformat=VCFv4.2\n").unwrap();
        std::fs::write(&p, gz.finish().unwrap()).unwrap();
        assert_eq!(detect(&Origin::file(&p).unwrap()), None, "left to the preprocessor");
    }

    fn zip_with(files: &[(&str, &[u8], zip::CompressionMethod)]) -> Vec<u8> {
        let mut z = zip::ZipWriter::new(Cursor::new(Vec::new()));
        let stamp = zip::DateTime::from_date_and_time(2026, 11, 27, 9, 30, 0).unwrap();
        for (name, data, method) in files {
            let opts = zip::write::SimpleFileOptions::default().compression_method(*method).last_modified_time(stamp);
            if name.ends_with('/') {
                z.add_directory(*name, opts).unwrap();
            } else {
                z.start_file(*name, opts).unwrap();
                z.write_all(data).unwrap();
            }
        }
        z.finish().unwrap().into_inner()
    }

    #[test]
    fn lists_and_opens_a_zip() {
        use zip::CompressionMethod::{Deflated, Stored};
        let p = temp("a.zip");
        let text = "fn main() {}\n".repeat(50);
        std::fs::write(&p, zip_with(&[("src/", b"", Stored), ("src/main.rs", text.as_bytes(), Deflated)])).unwrap();
        let (kind, listing, index) = list(&p);
        assert_eq!(kind, Some(Kind::Zip));
        let lines: Vec<&str> = listing.lines().collect();
        assert!(lines[0].starts_with("# zip archive: 1 file, 1 directory, 650 bytes of contents ("), "{}", lines[0]);
        assert_eq!(lines[1], "Size\tCompressed\tModified\tName");
        assert_eq!(lines[2], "0\t0\t2026-11-27 09:30\tsrc/");
        assert!(lines[3].starts_with("650\t") && lines[3].ends_with("\t2026-11-27 09:30\tsrc/main.rs"), "{}", lines[3]);
        let index = index.unwrap();
        assert_eq!(read(&index, 3).unwrap(), text.as_bytes(), "decompressed");
    }

    #[test]
    fn archives_inside_archives() {
        // A zip stored in a tar, then that tar in a tar.gz's place in memory.
        use zip::CompressionMethod::Deflated;
        let inner = zip_with(&[("hi.txt", b"nested\n", Deflated)]);
        let mut b = tar::Builder::new(Vec::new());
        let mut h = tar::Header::new_gnu();
        h.set_mode(0o644);
        h.set_size(inner.len() as u64);
        b.append_data(&mut h, "inner.zip", &inner[..]).unwrap();
        let tar = b.into_inner().unwrap();
        let p = temp("outer.tar");
        std::fs::write(&p, &tar).unwrap();
        let (_, _, index) = list(&p);
        let index = index.unwrap();
        // From disk: a range of the outer file.
        let Contents::Range { path, start, len } = open(&index.origin, index.kind, &index.entries[0], &no()).unwrap() else {
            panic!("a plain tar's file is read in place")
        };
        let range = Origin::File { path, start, len };
        assert_eq!(detect(&range), Some(Kind::Zip));
        let (_, inner_index) = listing(&range, Kind::Zip, &no()).unwrap();
        assert_eq!(read(&inner_index, 2).unwrap(), b"nested\n");
        // From memory: the same tar as bytes.
        let mem = Origin::Bytes(Arc::from(&tar[..]));
        let (_, mem_index) = listing(&mem, Kind::Tar, &no()).unwrap();
        let Contents::Bytes(zip) = open(&mem, Kind::Tar, &mem_index.entries[0], &no()).unwrap() else { panic!() };
        assert_eq!(zip, inner);
    }

    #[test]
    fn truncated_canceled_and_too_large() {
        // A tar cut off partway lists what came before it.
        let p = temp("cut.tar");
        let full = tar_bytes();
        std::fs::write(&p, &full[..512 * 3 + 100]).unwrap();
        let (text, _) = listing(&Origin::file(&p).unwrap(), Kind::Tar, &no()).unwrap();
        let text = String::from_utf8(text).unwrap();
        assert!(text.contains("\tdocs/\n") && text.contains("# Listing stopped early"), "{text}");
        // Not an archive at all: an error, so the file is shown as usual.
        let p = temp("not.zip");
        std::fs::write(&p, b"PK\x03\x04 but nothing more").unwrap();
        assert!(listing(&Origin::file(&p).unwrap(), Kind::Zip, &no()).is_err());
        let p = temp("b.tar");
        std::fs::write(&p, tar_bytes()).unwrap();
        let err = listing(&Origin::file(&p).unwrap(), Kind::Tar, &AtomicBool::new(true)).unwrap_err();
        assert_eq!(err.kind(), io::ErrorKind::Interrupted);
        let err = read_limited(&mut io::empty(), MAX_IN_MEMORY + 1).unwrap_err();
        assert!(err.to_string().starts_with("too large to open from a compressed archive"), "{err}");
    }

    #[test]
    fn normalizes_paths() {
        assert_eq!(normalize("./a//b/../c/"), "a/c");
        assert_eq!(normalize("/etc/x"), "etc/x");
        assert_eq!(normalize("../../x"), "x");
    }
}
