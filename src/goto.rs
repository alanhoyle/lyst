//! The browser's "go to directory" prompt: suggestions and Tab completion.

use std::fs;
use std::path::{Path, PathBuf};

/// Most suggestions kept for one directory.
const MAX_MATCHES: usize = 500;

pub struct DirPrompt {
    pub input: String,
    /// Directory that relative paths start from.
    base: PathBuf,
    show_hidden: bool,
    /// Directories that complete the input, each as the full input text
    /// ending in `/`.
    pub matches: Vec<String>,
    /// The highlighted suggestion, whose text is then in `input`.
    pub sel: Option<usize>,
}

impl DirPrompt {
    pub fn new(base: &Path, show_hidden: bool) -> Self {
        let mut p = DirPrompt { input: String::new(), base: base.to_path_buf(), show_hidden, matches: Vec::new(), sel: None };
        p.refresh();
        p
    }

    /// The path the input names: `~` is the home directory, and relative
    /// paths start at the browser's directory.
    pub fn target(&self) -> PathBuf {
        expand(&self.input, &self.base)
    }

    /// Recomputes the suggestions for the input as typed.
    fn refresh(&mut self) {
        self.sel = None;
        self.matches = suggestions(&self.input, &self.base, self.show_hidden);
    }

    pub fn push(&mut self, c: char) {
        self.input.push(c);
        self.refresh();
    }

    pub fn backspace(&mut self) {
        self.input.pop();
        self.refresh();
    }

    /// Ctrl-W: deletes back to the previous `/`, or the `/` itself.
    pub fn delete_word(&mut self) {
        if self.input.ends_with('/') {
            self.input.pop();
        }
        let keep = self.input.rfind('/').map_or(0, |i| i + 1);
        self.input.truncate(keep);
        self.refresh();
    }

    pub fn clear(&mut self) {
        self.input.clear();
        self.refresh();
    }

    /// Tab: completes as far as the suggestions agree, goes into the only
    /// match, or else steps through the suggestions.
    pub fn complete(&mut self) {
        if self.sel.is_some() {
            return self.step(1);
        }
        match self.matches.len() {
            0 => {}
            1 => {
                self.input = self.matches[0].clone();
                self.refresh();
            }
            _ => {
                let common = common_prefix(&self.matches);
                if common.chars().count() > self.input.chars().count() {
                    self.input = common;
                    self.refresh();
                } else {
                    self.step(1);
                }
            }
        }
    }

    /// Highlights the next (or, for -1, previous) suggestion and puts it in
    /// the input, wrapping around.
    pub fn step(&mut self, by: isize) {
        let n = self.matches.len();
        if n == 0 {
            return;
        }
        let i = match self.sel {
            None if by < 0 => n - 1,
            None => 0,
            Some(i) => (i as isize + by).rem_euclid(n as isize) as usize,
        };
        self.sel = Some(i);
        self.input = self.matches[i].clone();
    }
}

fn home() -> Option<PathBuf> {
    std::env::var_os("HOME").map(PathBuf::from)
}

fn expand(input: &str, base: &Path) -> PathBuf {
    let rest = if input == "~" { Some("") } else { input.strip_prefix("~/") };
    if let (Some(rest), Some(h)) = (rest, home()) {
        return h.join(rest);
    }
    base.join(input)
}

/// Directories whose names start with the input's last component (ignoring
/// case), as complete inputs. Hidden ones are left out unless shown in the
/// browser or the component starts with `.`.
fn suggestions(input: &str, base: &Path, show_hidden: bool) -> Vec<String> {
    if input == "~" {
        return vec!["~/".into()];
    }
    let split = input.rfind('/').map_or(0, |i| i + 1);
    let (dir, prefix) = input.split_at(split);
    let prefix = prefix.to_lowercase();
    let Ok(rd) = fs::read_dir(expand(dir, base)) else { return Vec::new() };
    let mut names: Vec<String> = rd
        .flatten()
        .filter_map(|e| {
            let name = e.file_name().into_string().ok()?;
            let hidden = name.starts_with('.') && !show_hidden && !prefix.starts_with('.');
            // Follows symbolic links, so a link to a directory counts.
            let is_dir = fs::metadata(e.path()).is_ok_and(|m| m.is_dir());
            (is_dir && !hidden && name.to_lowercase().starts_with(&prefix)).then_some(name)
        })
        .collect();
    names.sort_by_key(|n| n.to_lowercase());
    names.truncate(MAX_MATCHES);
    names.into_iter().map(|n| format!("{dir}{n}/")).collect()
}

/// Longest prefix shared by all the strings, ignoring case; taken from the
/// first one.
fn common_prefix(all: &[String]) -> String {
    let Some(first) = all.first() else { return String::new() };
    let mut len = first.len();
    for s in &all[1..] {
        len = first
            .char_indices()
            .zip(s.chars())
            .take_while(|((_, a), b)| a.to_lowercase().eq(b.to_lowercase()))
            .last()
            .map_or(0, |((i, a), _)| i + a.len_utf8())
            .min(len);
    }
    first[..len].to_string()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn tree(tag: &str) -> PathBuf {
        let dir = std::env::temp_dir().join(format!("lyst-goto-{tag}-{}", std::process::id()));
        let _ = fs::remove_dir_all(&dir);
        for d in ["src/lib", "src/bin", "Samples", "scratch", ".git", "docs"] {
            fs::create_dir_all(dir.join(d)).unwrap();
        }
        fs::write(dir.join("setup.py"), "").unwrap();
        dir
    }

    #[test]
    fn suggests_directories_only_and_hides_dotfiles() {
        let dir = tree("suggest");
        let p = DirPrompt::new(&dir, false);
        assert_eq!(p.matches, ["docs/", "Samples/", "scratch/", "src/"]);
        assert_eq!(suggestions("s", &dir, false), ["Samples/", "scratch/", "src/"], "no setup.py");
        assert_eq!(suggestions("S", &dir, false), ["Samples/", "scratch/", "src/"], "ignores case");
        assert_eq!(suggestions(".", &dir, false), [".git/"]);
        assert!(suggestions("", &dir, true).contains(&".git/".to_string()));
        assert_eq!(suggestions("src/", &dir, false), ["src/bin/", "src/lib/"]);
        assert!(suggestions("nope/", &dir, false).is_empty());
        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn tab_completes_then_cycles() {
        let dir = tree("tab");
        let mut p = DirPrompt::new(&dir, false);
        p.push('d');
        p.complete();
        assert_eq!(p.input, "docs/", "the only match");
        assert!(p.matches.is_empty(), "docs has no subdirectories");

        p.clear();
        p.push('s');
        p.push('r');
        p.complete();
        assert_eq!(p.input, "src/");
        assert_eq!(p.matches, ["src/bin/", "src/lib/"]);
        // Nothing more in common: Tab steps through the suggestions.
        p.complete();
        assert_eq!((p.input.as_str(), p.sel), ("src/bin/", Some(0)));
        p.complete();
        assert_eq!(p.input, "src/lib/");
        p.complete();
        assert_eq!(p.input, "src/bin/", "wraps around");
        p.step(-1);
        assert_eq!(p.input, "src/lib/");
        // Typing goes on from the highlighted suggestion.
        p.push('x');
        assert_eq!((p.input.as_str(), p.sel), ("src/lib/x", None));

        p.delete_word();
        assert_eq!(p.input, "src/lib/");
        p.delete_word();
        assert_eq!(p.input, "src/");
        p.backspace();
        p.backspace();
        p.backspace();
        p.push('c');
        assert_eq!(p.input, "sc");
        p.complete();
        assert_eq!(p.input, "scratch/");
        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn common_prefix_ignores_case() {
        let v = |a: &[&str]| a.iter().map(|s| s.to_string()).collect::<Vec<_>>();
        assert_eq!(common_prefix(&v(&["Samples/", "sample/"])), "Sample");
        assert_eq!(common_prefix(&v(&["abc/", "xyz/"])), "");
        assert_eq!(common_prefix(&v(&["only/"])), "only/");
    }

    #[test]
    fn targets() {
        let base = Path::new("/base");
        assert_eq!(expand("src", base), Path::new("/base/src"));
        assert_eq!(expand("/etc", base), Path::new("/etc"));
        assert_eq!(expand("..", base), Path::new("/base/.."));
        if let Some(h) = home() {
            assert_eq!(expand("~", base), h);
            assert_eq!(expand("~/x", base), h.join("x"));
        }
        assert_eq!(suggestions("~", base, false), ["~/"]);
    }
}
