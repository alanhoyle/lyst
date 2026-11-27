//! Path helpers that behave the same on every platform.

use std::io;
use std::path::{Path, PathBuf};

/// `fs::canonicalize`, except that on Windows the result is an ordinary
/// path (`C:\x`, `\\server\share\x`) rather than the `\\?\` form. In that
/// form `/` isn't a separator, so `C:\x` + `/y` would name nothing, and it
/// would also show up on screen.
pub fn canonicalize(path: &Path) -> io::Result<PathBuf> {
    std::fs::canonicalize(path).map(plain)
}

/// The home directory: `$HOME`, or on Windows (where it's usually unset)
/// `%USERPROFILE%`.
pub fn home() -> Option<PathBuf> {
    let var = |k| std::env::var_os(k).filter(|v| !v.is_empty()).map(PathBuf::from);
    var("HOME").or_else(|| if cfg!(windows) { var("USERPROFILE") } else { None })
}

/// Removes a `\\?\` prefix when the path means the same without it. Paths
/// long enough to need it keep it.
fn plain(p: PathBuf) -> PathBuf {
    let Some(s) = p.to_str() else { return p };
    let short = s.len() < 260;
    if let Some(rest) = s.strip_prefix(r"\\?\UNC\") {
        if short {
            return PathBuf::from(format!(r"\\{rest}"));
        }
    } else if let Some(rest) = s.strip_prefix(r"\\?\") {
        let b = rest.as_bytes();
        let drive = b.len() >= 3 && b[0].is_ascii_alphabetic() && b[1] == b':' && b[2] == b'\\';
        if drive && short {
            return PathBuf::from(rest);
        }
    }
    p
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn plain_drops_the_verbatim_prefix() {
        let p = |s: &str| plain(PathBuf::from(s));
        assert_eq!(p(r"\\?\C:\Users\me"), PathBuf::from(r"C:\Users\me"));
        assert_eq!(p(r"\\?\UNC\server\share\x"), PathBuf::from(r"\\server\share\x"));
        assert_eq!(p(r"\\?\Volume{abc}\x"), PathBuf::from(r"\\?\Volume{abc}\x"), "no drive letter");
        let long = format!(r"\\?\C:\{}", "a".repeat(300));
        assert_eq!(p(&long), PathBuf::from(&long), "too long without it");
        assert_eq!(p("/usr/local"), PathBuf::from("/usr/local"));
    }
}
