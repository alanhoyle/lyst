//! Lists every file and directory under a directory, on a background
//! thread, for the browser's recursive find.

use std::fs;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering::Relaxed};
use std::sync::{Arc, Mutex};
use std::thread;

/// Most paths collected; a find in `/` stops here.
pub const LIMIT: usize = 500_000;

#[derive(Default)]
struct Shared {
    /// Paths relative to the root, directories ending in `/`.
    found: Mutex<Vec<String>>,
    done: AtomicBool,
    cancel: AtomicBool,
}

/// A running (or finished) listing. Dropping it stops the walk.
pub struct Walk {
    pub root: PathBuf,
    shared: Arc<Shared>,
}

impl Walk {
    /// Starts listing `root`. Hidden files and directories are skipped
    /// unless `hidden`; symbolic links to directories aren't followed.
    pub fn start(root: &Path, hidden: bool) -> Walk {
        let shared = Arc::new(Shared::default());
        let s = Arc::clone(&shared);
        let r = root.to_path_buf();
        thread::spawn(move || {
            walk(&r, hidden, &s);
            s.done.store(true, Relaxed);
        });
        Walk { root: root.to_path_buf(), shared }
    }

    pub fn done(&self) -> bool {
        self.shared.done.load(Relaxed)
    }

    pub fn count(&self) -> usize {
        self.shared.found.lock().map_or(0, |f| f.len())
    }

    /// Paths containing every space-separated term of `query` (ignoring
    /// case), at most `max`, plus how many matched in all.
    pub fn matching(&self, query: &str, max: usize) -> (Vec<String>, usize) {
        let terms: Vec<String> = query.split_whitespace().map(str::to_lowercase).collect();
        let Ok(found) = self.shared.found.lock() else { return (Vec::new(), 0) };
        let mut out = Vec::new();
        let mut n = 0;
        for p in found.iter() {
            let lower = p.to_lowercase();
            if terms.iter().all(|t| lower.contains(t.as_str())) {
                n += 1;
                if out.len() < max {
                    out.push(p.clone());
                }
            }
        }
        (out, n)
    }
}

impl Drop for Walk {
    fn drop(&mut self) {
        self.shared.cancel.store(true, Relaxed);
    }
}

fn walk(root: &Path, hidden: bool, s: &Shared) {
    // Breadth-first, so nearby files show up first.
    let mut queue = std::collections::VecDeque::from([PathBuf::new()]);
    let mut batch = Vec::new();
    while let Some(rel) = queue.pop_front() {
        if s.cancel.load(Relaxed) {
            return;
        }
        let Ok(rd) = fs::read_dir(root.join(&rel)) else { continue };
        let mut names: Vec<(String, bool)> = rd
            .flatten()
            .filter_map(|e| {
                let name = e.file_name().into_string().ok()?;
                let is_dir = e.file_type().is_ok_and(|t| t.is_dir());
                (hidden || !name.starts_with('.')).then_some((name, is_dir))
            })
            .collect();
        names.sort_by_key(|(n, _)| n.to_lowercase());
        for (name, is_dir) in names {
            let child = rel.join(&name);
            let shown = child.to_string_lossy().into_owned();
            if is_dir {
                batch.push(format!("{shown}/"));
                queue.push_back(child);
            } else {
                batch.push(shown);
            }
        }
        // Publish in batches so the browser's lock is rarely contended.
        if batch.len() >= 256 || queue.is_empty() {
            let Ok(mut found) = s.found.lock() else { return };
            let room = LIMIT.saturating_sub(found.len());
            found.extend(batch.drain(..).take(room));
            if found.len() >= LIMIT {
                return;
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::time::{Duration, Instant};

    #[test]
    fn finds_by_all_terms_and_skips_hidden() {
        let dir = std::env::temp_dir().join(format!("lyst-find-{}", std::process::id()));
        let _ = fs::remove_dir_all(&dir);
        fs::create_dir_all(dir.join("src/deep/er")).unwrap();
        fs::create_dir_all(dir.join(".git")).unwrap();
        for f in ["README.md", "src/main.rs", "src/deep/er/Notes.md", ".git/config"] {
            fs::write(dir.join(f), "").unwrap();
        }
        let w = Walk::start(&dir, false);
        let deadline = Instant::now() + Duration::from_secs(10);
        while !w.done() {
            assert!(Instant::now() < deadline);
            thread::sleep(Duration::from_millis(5));
        }
        let sep = std::path::MAIN_SEPARATOR;
        let p = |s: &str| s.replace('/', &sep.to_string());
        assert_eq!(w.count(), 6, "3 files, 3 directories, nothing hidden");
        assert_eq!(w.matching("md", 10), (vec!["README.md".to_string(), p("src/deep/er/Notes.md")], 2));
        assert_eq!(w.matching("SRC notes", 10).0, [p("src/deep/er/Notes.md")]);
        assert_eq!(w.matching("deep", 1), (vec![format!("{}/", p("src/deep"))], 3), "capped, with the full count");
        assert_eq!(w.matching("", 100).1, 6);

        let all = Walk::start(&dir, true);
        while !all.done() {
            thread::sleep(Duration::from_millis(5));
        }
        assert_eq!(all.matching("config", 10).0, [p(".git/config")]);
        let _ = fs::remove_dir_all(&dir);
    }
}
