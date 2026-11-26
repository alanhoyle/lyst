//! Disk usage of a file or directory tree, totaled on a background thread.

use std::collections::HashSet;
use std::fs;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering::Relaxed};
use std::thread;

#[derive(Default)]
struct Progress {
    /// Space allocated on disk, like `du`.
    disk: AtomicU64,
    /// Sum of file lengths, like `ls -l`.
    bytes: AtomicU64,
    files: AtomicU64,
    dirs: AtomicU64,
    /// Entries that couldn't be read (permission denied, vanished, ...).
    errors: AtomicU64,
    done: AtomicBool,
    cancel: AtomicBool,
}

/// Totals so far, or final once `done` is set.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct Totals {
    pub disk: u64,
    pub bytes: u64,
    pub files: u64,
    pub dirs: u64,
    pub errors: u64,
    pub done: bool,
}

/// A running (or finished) count. Dropping it stops the count.
pub struct Usage {
    pub path: PathBuf,
    progress: Arc<Progress>,
    /// Set once the final totals have been drawn.
    pub reported: bool,
}

impl Usage {
    pub fn start(path: &Path) -> Usage {
        let progress = Arc::new(Progress::default());
        let p = Arc::clone(&progress);
        let root = path.to_path_buf();
        thread::spawn(move || {
            walk(&root, &p);
            p.done.store(true, Relaxed);
        });
        Usage { path: path.to_path_buf(), progress, reported: false }
    }

    pub fn totals(&self) -> Totals {
        let p = &self.progress;
        Totals {
            disk: p.disk.load(Relaxed),
            bytes: p.bytes.load(Relaxed),
            files: p.files.load(Relaxed),
            dirs: p.dirs.load(Relaxed),
            errors: p.errors.load(Relaxed),
            done: p.done.load(Relaxed),
        }
    }
}

impl Drop for Usage {
    fn drop(&mut self) {
        self.progress.cancel.store(true, Relaxed);
    }
}

/// Space a file takes on disk.
#[cfg(unix)]
fn allocated(m: &fs::Metadata) -> u64 {
    use std::os::unix::fs::MetadataExt;
    m.blocks() * 512
}

#[cfg(not(unix))]
fn allocated(m: &fs::Metadata) -> u64 {
    m.len()
}

/// Identity of a file with several hard links, so it's counted once.
#[cfg(unix)]
fn shared_id(m: &fs::Metadata) -> Option<(u64, u64)> {
    use std::os::unix::fs::MetadataExt;
    (!m.is_dir() && m.nlink() > 1).then(|| (m.dev(), m.ino()))
}

#[cfg(not(unix))]
fn shared_id(_: &fs::Metadata) -> Option<(u64, u64)> {
    None
}

/// Adds up `root` and everything under it. Symbolic links are counted
/// themselves, not followed.
fn walk(root: &Path, p: &Progress) {
    let mut seen = HashSet::new();
    let mut stack = vec![root.to_path_buf()];
    while let Some(path) = stack.pop() {
        if p.cancel.load(Relaxed) {
            return;
        }
        let Ok(m) = fs::symlink_metadata(&path) else {
            p.errors.fetch_add(1, Relaxed);
            continue;
        };
        if shared_id(&m).is_some_and(|id| !seen.insert(id)) {
            continue;
        }
        p.disk.fetch_add(allocated(&m), Relaxed);
        if !m.is_dir() {
            p.bytes.fetch_add(m.len(), Relaxed);
            p.files.fetch_add(1, Relaxed);
            continue;
        }
        if path != root {
            p.dirs.fetch_add(1, Relaxed);
        }
        match fs::read_dir(&path) {
            Ok(rd) => {
                for e in rd {
                    match e {
                        Ok(e) => stack.push(e.path()),
                        Err(_) => {
                            p.errors.fetch_add(1, Relaxed);
                        }
                    }
                }
            }
            Err(_) => {
                p.errors.fetch_add(1, Relaxed);
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::time::{Duration, Instant};

    fn finish(u: &Usage) -> Totals {
        let deadline = Instant::now() + Duration::from_secs(10);
        loop {
            let t = u.totals();
            if t.done {
                return t;
            }
            assert!(Instant::now() < deadline, "disk usage never finished");
            thread::sleep(Duration::from_millis(5));
        }
    }

    #[test]
    fn totals_a_tree() {
        let dir = std::env::temp_dir().join(format!("lyst-usage-{}", std::process::id()));
        let _ = fs::remove_dir_all(&dir);
        fs::create_dir_all(dir.join("a/b")).unwrap();
        fs::write(dir.join("one"), vec![b'x'; 10_000]).unwrap();
        fs::write(dir.join("a/two"), vec![b'y'; 5_000]).unwrap();
        fs::write(dir.join("a/b/three"), "").unwrap();
        #[cfg(unix)]
        {
            // A hard link is counted once; a symlink isn't followed.
            fs::hard_link(dir.join("one"), dir.join("a/one-again")).unwrap();
            std::os::unix::fs::symlink(dir.join("a"), dir.join("link")).unwrap();
        }

        let t = finish(&Usage::start(&dir));
        assert_eq!(t.dirs, 2);
        assert_eq!(t.errors, 0);
        #[cfg(unix)]
        assert_eq!(t.files, 4, "three files and the symlink");
        assert!(t.bytes >= 15_000, "{t:?}");
        assert!(t.bytes < 20_000, "the hard link wasn't counted twice: {t:?}");
        assert!(t.disk >= 15_000, "{t:?}");

        let file = finish(&Usage::start(&dir.join("one")));
        assert_eq!((file.files, file.dirs, file.bytes), (1, 0, 10_000));

        let missing = finish(&Usage::start(&dir.join("nope")));
        assert_eq!(missing.errors, 1);
        let _ = fs::remove_dir_all(&dir);
    }
}
