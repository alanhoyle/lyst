//! Recently visited directories, most recent first, remembered between runs
//! in `$XDG_STATE_HOME/lyst/dirs` (`~/.local/state/lyst/dirs`; on Windows
//! `%LOCALAPPDATA%\lyst\dirs`). This is the only file lyst writes.

use std::fs;
use std::path::{Path, PathBuf};

/// Most directories kept.
const MAX: usize = 100;

/// Where the history lives. Tests never touch the real file.
fn file() -> Option<PathBuf> {
    if cfg!(test) {
        return None;
    }
    let var = |k| std::env::var_os(k).filter(|v| !v.is_empty()).map(PathBuf::from);
    let base = if cfg!(windows) {
        var("LOCALAPPDATA")?
    } else {
        var("XDG_STATE_HOME").or_else(|| var("HOME").map(|h| h.join(".local/state")))?
    };
    Some(base.join("lyst").join("dirs"))
}

fn read(file: &Path) -> Vec<PathBuf> {
    fs::read_to_string(file).map(|s| s.lines().filter(|l| !l.is_empty()).map(PathBuf::from).collect()).unwrap_or_default()
}

fn write(file: &Path, dirs: &[PathBuf]) {
    if let Some(parent) = file.parent() {
        let _ = fs::create_dir_all(parent);
    }
    let text: String = dirs.iter().map(|d| format!("{}\n", d.display())).collect();
    // Write a temporary file and rename it, so a crash can't leave half a list.
    let tmp = file.with_extension("tmp");
    if fs::write(&tmp, text).is_ok() {
        let _ = fs::rename(&tmp, file);
    }
}

fn push(dirs: &mut Vec<PathBuf>, dir: &Path) {
    dirs.retain(|d| d != dir);
    dirs.insert(0, dir.to_path_buf());
    dirs.truncate(MAX);
}

/// Records a visit to `dir` and returns the updated history. Other lyst
/// windows may have added to the file since `current` was read, so it's
/// re-read first; without a file, `current` is used.
pub fn record(current: &[PathBuf], dir: &Path) -> Vec<PathBuf> {
    record_in(file().as_deref(), current, dir)
}

fn record_in(file: Option<&Path>, current: &[PathBuf], dir: &Path) -> Vec<PathBuf> {
    let mut dirs = match file {
        Some(f) if f.exists() => read(f),
        _ => current.to_vec(),
    };
    // Paths that can't be written one per line aren't recorded.
    if dir.to_str().is_some_and(|s| !s.contains('\n')) {
        push(&mut dirs, dir);
        if let Some(f) = file {
            write(f, &dirs);
        }
    }
    dirs
}

/// How a directory is shown and typed: `~/...` under the home directory,
/// with a trailing `/`.
pub fn display(dir: &Path) -> String {
    let home = crate::paths::home();
    let s = match home.as_deref().and_then(|h| dir.strip_prefix(h).ok()) {
        Some(rest) if rest.as_os_str().is_empty() => "~".to_string(),
        Some(rest) => format!("~/{}", rest.display()),
        None => dir.display().to_string(),
    };
    if s.ends_with('/') { s } else { format!("{s}/") }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn records_most_recent_first_without_duplicates() {
        let dir = std::env::temp_dir().join(format!("lyst-history-{}", std::process::id()));
        let _ = fs::remove_dir_all(&dir);
        let f = dir.join("state/lyst/dirs");
        let (a, b) = (Path::new("/a"), Path::new("/b c"));
        assert_eq!(record_in(Some(&f), &[], a), [a]);
        assert_eq!(record_in(Some(&f), &[], b), [b, a]);
        assert_eq!(record_in(Some(&f), &[], a), [a, b], "moves to the front");
        assert_eq!(read(&f), [a, b], "saved");
        // Another window's visit, made since, is kept.
        write(&f, &[PathBuf::from("/other"), a.into(), b.into()]);
        assert_eq!(record_in(Some(&f), &[a.into()], b), [b, Path::new("/other"), a]);

        let many: Vec<PathBuf> = (0..150).map(|i| PathBuf::from(format!("/d{i}"))).collect();
        assert_eq!(record_in(None, &many, a).len(), MAX);
        assert_eq!(record_in(None, &[], Path::new("/x\ny")), Vec::<PathBuf>::new());
        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn display_uses_tilde() {
        if let Some(h) = crate::paths::home() {
            assert_eq!(display(&h.join("src/lyst")), "~/src/lyst/");
            assert_eq!(display(&h), "~/");
        }
        assert_eq!(display(Path::new("/")), "/");
        assert_eq!(display(Path::new("/opt/x")), "/opt/x/");
    }
}
