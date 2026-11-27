//! Tables of contents for zip and tar (plain or gzipped) archives, shown
//! instead of the archive's bytes. Nothing is extracted: zip's central
//! directory is read, and tar headers are read with the contents skipped.
//!
//! A listing is tab-separated text: `#` summary lines, a header row, then
//! one row per entry with the name last, so table view can show it in full.

use std::fs::File;
use std::io::{self, BufReader, Read, Seek, SeekFrom};
use std::path::Path;
use std::sync::atomic::{AtomicBool, Ordering::Relaxed};

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

/// Recognizes an archive from its first bytes, whatever its name.
pub fn detect(path: &Path) -> Option<Kind> {
    let mut f = File::open(path).ok()?;
    let head = read_up_to(&mut f, 512);
    if head.starts_with(b"PK\x03\x04") || head.starts_with(b"PK\x05\x06") {
        Some(Kind::Zip)
    } else if is_tar(&head) {
        Some(Kind::Tar)
    } else if head.starts_with(&[0x1f, 0x8b]) {
        // Only a tar inside counts; other .gz files go to the preprocessor.
        f.seek(SeekFrom::Start(0)).ok()?;
        let inner = read_up_to(&mut flate2::read::MultiGzDecoder::new(BufReader::new(f)), 512);
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

/// The listing for an archive. Stops with an `Interrupted` error once
/// `cancel` is set. An archive that can't be read at all is an error; one
/// that breaks partway lists what came before, with a note.
pub fn listing(path: &Path, kind: Kind, cancel: &AtomicBool) -> io::Result<Vec<u8>> {
    let file = File::open(path)?;
    let compressed = file.metadata()?.len();
    let reader = Cancelable { inner: file, cancel };
    let mut out = Listing::default();
    let result = match kind {
        Kind::Zip => list_zip(BufReader::new(reader), &mut out),
        Kind::Tar => list_tar(tar::Archive::new(BufReader::new(reader)).entries_with_seek()?, &mut out),
        Kind::TarGz => {
            let gz = flate2::read::MultiGzDecoder::new(BufReader::new(reader));
            list_tar(tar::Archive::new(gz).entries()?, &mut out)
        }
    };
    match result {
        Err(e) if e.kind() == io::ErrorKind::Interrupted => return Err(e),
        Err(e) if out.rows.is_empty() => return Err(e),
        Err(e) => out.notes.push(format!("Listing stopped early: {e}")),
        Ok(()) => {}
    }
    Ok(out.render(kind, compressed))
}

/// Rows collected so far, and totals for the summary.
#[derive(Default)]
struct Listing {
    header: &'static str,
    rows: Vec<u8>,
    files: u64,
    dirs: u64,
    bytes: u64,
    /// zip only: the total compressed size of the entries.
    packed: Option<u64>,
    notes: Vec<String>,
}

impl Listing {
    fn row(&mut self, fields: &[&[u8]]) {
        for (i, f) in fields.iter().enumerate() {
            if i > 0 {
                self.rows.push(b'\t');
            }
            clean_into(&mut self.rows, f);
        }
        self.rows.push(b'\n');
    }

    fn render(self, kind: Kind, compressed: u64) -> Vec<u8> {
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
            (Kind::TarGz, _) => summary += &format!(" ({} compressed)", size(compressed)),
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
        out
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
            Ok(n) => n.into_owned().into_bytes(),
            Err(_) => f.name_raw().to_vec(),
        };
        let modified = f
            .last_modified()
            .map(|d| format!("{:04}-{:02}-{:02} {:02}:{:02}", d.year(), d.month(), d.day(), d.hour(), d.minute()))
            .unwrap_or_default();
        if f.is_dir() {
            out.dirs += 1;
        } else {
            out.files += 1;
            out.bytes += f.size();
            *out.packed.get_or_insert(0) += f.compressed_size();
        }
        let (size, packed) = (f.size().to_string(), f.compressed_size().to_string());
        out.row(&[size.as_bytes(), packed.as_bytes(), modified.as_bytes(), &name]);
    }
    Ok(())
}

fn zip_error(e: zip::result::ZipError) -> io::Error {
    match e {
        zip::result::ZipError::Io(e) => e,
        e => io::Error::new(io::ErrorKind::InvalidData, e),
    }
}

fn list_tar<R: Read>(entries: tar::Entries<'_, R>, out: &mut Listing) -> io::Result<()> {
    out.header = "Mode\tOwner\tSize\tModified\tName";
    for entry in entries {
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
        let mut name = e.path_bytes().into_owned();
        if let Some(target) = e.link_name_bytes() {
            name.extend_from_slice(if t.is_hard_link() { b" link to " } else { b" -> " });
            name.extend_from_slice(&target);
        }
        if t.is_dir() {
            out.dirs += 1;
        } else {
            out.files += 1;
            out.bytes += e.size();
        }
        let size = e.size().to_string();
        out.row(&[mode.as_bytes(), owner.as_bytes(), size.as_bytes(), modified.as_bytes(), &name]);
    }
    Ok(())
}

/// A reader that fails with `Interrupted` once `cancel` is set, so a long
/// listing stops at the next read.
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
    use std::path::PathBuf;

    fn temp(name: &str) -> PathBuf {
        let dir = std::env::temp_dir().join(format!("lyst-archive-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        dir.join(name)
    }

    fn list(path: &Path) -> (Option<Kind>, String) {
        let kind = detect(path);
        let text = kind.map(|k| String::from_utf8(listing(path, k, &AtomicBool::new(false)).unwrap()).unwrap());
        (kind, text.unwrap_or_default())
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
        b.append_link(&mut h, "latest", "docs/").unwrap();
        b.into_inner().unwrap()
    }

    #[test]
    fn lists_a_tar() {
        let p = temp("a.tar");
        std::fs::write(&p, tar_bytes()).unwrap();
        let (kind, text) = list(&p);
        assert_eq!(kind, Some(Kind::Tar));
        let lines: Vec<&str> = text.lines().collect();
        assert_eq!(lines[0], "# tar archive: 2 files, 1 directory, 5 bytes of contents");
        assert_eq!(lines[1], "Mode\tOwner\tSize\tModified\tName");
        assert!(lines[2].starts_with("drwxr-xr-x\tann:staff\t0\t") && lines[2].ends_with("\tdocs/"), "{}", lines[2]);
        assert!(lines[3].starts_with("-rw-r--r--\t501:20\t5\t"), "ids without names: {}", lines[3]);
        assert!(lines[3].ends_with(&format!("\tdocs/{}\\tx.txt", "n".repeat(120))), "long name, tab shown as \\t");
        assert!(lines[4].starts_with("lrwxrwxrwx") && lines[4].ends_with("\tlatest -> docs/"), "{}", lines[4]);
        assert_eq!(lines.len(), 5);
    }

    #[test]
    fn lists_a_gzipped_tar_but_not_other_gzip() {
        let p = temp("a.tgz");
        let mut gz = flate2::write::GzEncoder::new(Vec::new(), flate2::Compression::default());
        gz.write_all(&tar_bytes()).unwrap();
        let bytes = gz.finish().unwrap();
        std::fs::write(&p, &bytes).unwrap();
        let (kind, text) = list(&p);
        assert_eq!(kind, Some(Kind::TarGz));
        assert!(text.starts_with(&format!("# tar.gz archive: 2 files, 1 directory, 5 bytes of contents ({} bytes compressed)\n", bytes.len())), "{text}");

        let p = temp("calls.vcf.gz");
        let mut gz = flate2::write::GzEncoder::new(Vec::new(), flate2::Compression::default());
        gz.write_all(b"##fileformat=VCFv4.2\n").unwrap();
        std::fs::write(&p, gz.finish().unwrap()).unwrap();
        assert_eq!(detect(&p), None, "left to the preprocessor");
    }

    #[test]
    fn lists_a_zip() {
        let p = temp("a.zip");
        let mut z = zip::ZipWriter::new(std::fs::File::create(&p).unwrap());
        let opts = zip::write::SimpleFileOptions::default()
            .compression_method(zip::CompressionMethod::Stored)
            .last_modified_time(zip::DateTime::from_date_and_time(2026, 11, 27, 9, 30, 0).unwrap());
        z.add_directory("src/", opts).unwrap();
        z.start_file("src/main.rs", opts).unwrap();
        z.write_all(b"fn main() {}\n").unwrap();
        z.finish().unwrap();
        let (kind, text) = list(&p);
        assert_eq!(kind, Some(Kind::Zip));
        assert_eq!(
            text,
            "# zip archive: 1 file, 1 directory, 13 bytes of contents (13 bytes compressed)\n\
             Size\tCompressed\tModified\tName\n\
             0\t0\t2026-11-27 09:30\tsrc/\n\
             13\t13\t2026-11-27 09:30\tsrc/main.rs\n"
        );
    }

    #[test]
    fn truncated_and_canceled() {
        // A tar cut off partway lists what came before it.
        let p = temp("cut.tar");
        let full = tar_bytes();
        std::fs::write(&p, &full[..512 * 3 + 100]).unwrap();
        let text = String::from_utf8(listing(&p, Kind::Tar, &AtomicBool::new(false)).unwrap()).unwrap();
        assert!(text.contains("\tdocs/\n"), "{text}");
        // Not an archive at all: an error, so the file is shown as usual.
        let p = temp("not.zip");
        std::fs::write(&p, b"PK\x03\x04 but nothing more").unwrap();
        assert!(listing(&p, Kind::Zip, &AtomicBool::new(false)).is_err());
        let p = temp("b.tar");
        std::fs::write(&p, tar_bytes()).unwrap();
        let err = listing(&p, Kind::Tar, &AtomicBool::new(true)).unwrap_err();
        assert_eq!(err.kind(), io::ErrorKind::Interrupted);
    }
}
