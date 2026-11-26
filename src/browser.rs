//! Directory browser shown when lyst is started without a file.

use crate::app::{Painter, Theme};
use crossterm::event::{KeyCode, KeyEvent, KeyModifiers, MouseButton, MouseEvent, MouseEventKind};
use crossterm::{cursor, queue, terminal};
use std::cmp::Ordering;
use std::fs;
use std::io::{self, Write};
use std::path::{Path, PathBuf};
use std::time::{Duration, Instant, SystemTime};
use unicode_width::{UnicodeWidthChar, UnicodeWidthStr};

/// Browser settings that persist when the browser is reopened.
#[derive(Clone, Copy)]
pub struct Prefs {
    sort: Sort,
    show_hidden: bool,
    /// Multi-column view instead of one line per file.
    pub wide: bool,
}

impl Prefs {
    pub fn new(wide: bool) -> Self {
        Prefs { sort: Sort::Name, show_hidden: false, wide }
    }
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum Sort {
    Name,
    Ext,
    Size,
    Date,
}

impl Sort {
    fn next(self) -> Self {
        match self {
            Sort::Name => Sort::Ext,
            Sort::Ext => Sort::Size,
            Sort::Size => Sort::Date,
            Sort::Date => Sort::Name,
        }
    }

    fn label(self) -> &'static str {
        match self {
            Sort::Name => "name",
            Sort::Ext => "extension",
            Sort::Size => "size",
            Sort::Date => "date",
        }
    }
}

struct Entry {
    name: String,
    path: PathBuf,
    is_dir: bool,
    is_link: bool,
    size: u64,
    modified: Option<SystemTime>,
}

impl Entry {
    /// Name as shown in the listing.
    fn label(&self) -> String {
        if self.is_dir && self.name != ".." {
            format!("{}/", self.name)
        } else if self.is_link {
            format!("{}@", self.name)
        } else {
            self.name.clone()
        }
    }

    fn ext(&self) -> String {
        Path::new(&self.name)
            .extension()
            .map(|e| e.to_string_lossy().to_lowercase())
            .unwrap_or_default()
    }
}

pub enum Action {
    None,
    Open,
    Quit,
}

pub struct Browser {
    dir: PathBuf,
    entries: Vec<Entry>,
    sel: usize,
    top: usize,
    sort: Sort,
    show_hidden: bool,
    wide: bool,
    typed: String,
    last_typed: Instant,
    /// Entry and time of the last left click, for double-click detection.
    last_click: Option<(usize, Instant)>,
    message: Option<(String, bool)>,
    theme: Theme,
    width: usize,
    height: usize,
}

impl Browser {
    pub fn new(dir: &Path, classic: bool, prefs: Prefs) -> io::Result<Self> {
        let (w, h) = terminal::size().unwrap_or((80, 25));
        let mut b = Browser {
            dir: fs::canonicalize(dir)?,
            entries: Vec::new(),
            sel: 0,
            top: 0,
            sort: prefs.sort,
            show_hidden: prefs.show_hidden,
            wide: prefs.wide,
            typed: String::new(),
            last_typed: Instant::now(),
            last_click: None,
            message: None,
            theme: Theme::new(classic),
            width: w as usize,
            height: h as usize,
        };
        b.load()?;
        // Land on the first real entry rather than "..".
        if b.entries.len() > 1 && b.entries[0].name == ".." {
            b.sel = 1;
        }
        Ok(b)
    }

    fn load(&mut self) -> io::Result<()> {
        let mut entries = Vec::new();
        for e in fs::read_dir(&self.dir)?.flatten() {
            let name = e.file_name().to_string_lossy().into_owned();
            if !self.show_hidden && name.starts_with('.') {
                continue;
            }
            let path = e.path();
            let is_link = e.file_type().is_ok_and(|t| t.is_symlink());
            // Follow symlinks so links to directories behave like directories.
            let Ok(meta) = fs::metadata(&path).or_else(|_| e.metadata()) else { continue };
            entries.push(Entry {
                name,
                path,
                is_dir: meta.is_dir(),
                is_link,
                size: meta.len(),
                modified: meta.modified().ok(),
            });
        }
        let sort = self.sort;
        entries.sort_by(|a, b| {
            b.is_dir.cmp(&a.is_dir).then_with(|| {
                let by_name = || a.name.to_lowercase().cmp(&b.name.to_lowercase());
                match sort {
                    // Directory sizes and extensions aren't meaningful; keep them alphabetical.
                    _ if a.is_dir => Ordering::Equal,
                    Sort::Name => Ordering::Equal,
                    Sort::Ext => a.ext().cmp(&b.ext()),
                    Sort::Size => b.size.cmp(&a.size),
                    Sort::Date => b.modified.cmp(&a.modified),
                }
                .then_with(by_name)
            })
        });
        if let Some(parent) = self.dir.parent() {
            entries.insert(
                0,
                Entry {
                    name: "..".into(),
                    path: parent.to_path_buf(),
                    is_dir: true,
                    is_link: false,
                    size: 0,
                    modified: None,
                },
            );
        }
        self.entries = entries;
        self.sel = self.sel.min(self.entries.len().saturating_sub(1));
        Ok(())
    }

    /// Re-reads the directory, keeping the selection on the same name.
    pub fn refresh(&mut self) {
        let name = self.entries.get(self.sel).map(|e| e.name.clone());
        if let Err(e) = self.load() {
            self.error(format!("{}: {e}", self.dir.display()));
        }
        if let Some(n) = name {
            self.select_name(&n);
        }
    }

    pub fn select_name(&mut self, name: &str) {
        if let Some(i) = self.entries.iter().position(|e| e.name == name) {
            self.sel = i;
        }
    }

    pub fn select_path(&mut self, path: &Path) {
        if path.parent() == Some(self.dir.as_path())
            && let Some(n) = path.file_name()
        {
            self.select_name(&n.to_string_lossy());
        }
    }

    pub fn resize(&mut self, w: u16, h: u16) {
        self.width = w as usize;
        self.height = h as usize;
    }

    pub fn flash(&mut self, s: impl Into<String>) {
        self.message = Some((s.into(), false));
    }

    pub fn error(&mut self, s: impl Into<String>) {
        self.message = Some((s.into(), true));
    }

    /// All plain files in the listing, plus the index of the selected one.
    pub fn file_list(&self) -> (Vec<PathBuf>, usize) {
        let mut idx = 0;
        let mut files = Vec::new();
        for (i, e) in self.entries.iter().enumerate() {
            if !e.is_dir {
                if i == self.sel {
                    idx = files.len();
                }
                files.push(e.path.clone());
            }
        }
        (files, idx)
    }

    pub fn prefs(&self) -> Prefs {
        Prefs { sort: self.sort, show_hidden: self.show_hidden, wide: self.wide }
    }

    /// Column width and column count for the wide view.
    fn grid(&self) -> (usize, usize) {
        if !self.wide {
            return (self.width, 1);
        }
        let longest = self.entries.iter().map(|e| e.label().width()).max().unwrap_or(1);
        let avail = self.width.saturating_sub(1).max(1);
        let colw = (longest + 2).clamp(8, 34).min(avail);
        (colw, (avail / colw).max(1))
    }

    fn rows(&self) -> usize {
        self.height.saturating_sub(2).max(1)
    }

    fn chdir(&mut self, dir: PathBuf, select: Option<String>) {
        let old = std::mem::replace(&mut self.dir, dir);
        if let Err(e) = self.load() {
            let msg = format!("{}: {e}", self.dir.display());
            self.dir = old;
            let _ = self.load();
            return self.error(msg);
        }
        self.sel = usize::from(self.entries.len() > 1 && self.entries[0].name == "..");
        self.top = 0;
        if let Some(n) = select {
            self.select_name(&n);
        }
    }

    fn parent(&mut self) {
        if let Some(p) = self.dir.parent().map(Path::to_path_buf) {
            let here = self.dir.file_name().map(|n| n.to_string_lossy().into_owned());
            self.chdir(p, here);
        }
    }

    fn activate(&mut self) -> Action {
        let Some(e) = self.entries.get(self.sel) else { return Action::None };
        if e.name == ".." {
            self.parent();
        } else if e.is_dir {
            let p = e.path.clone();
            self.chdir(p, None);
        } else {
            return Action::Open;
        }
        Action::None
    }

    /// Jump to the first entry whose name starts with what has been typed.
    /// Repeating a single letter cycles through entries starting with it.
    fn type_ahead(&mut self, c: char) {
        if self.last_typed.elapsed() > Duration::from_millis(1000) {
            self.typed.clear();
        }
        self.last_typed = Instant::now();
        let lc = c.to_lowercase().to_string();
        let starts = |e: &Entry, p: &str| e.name != ".." && e.name.to_lowercase().starts_with(p);
        let repeat = self.typed.chars().all(|t| t.to_lowercase().to_string() == lc) && !self.typed.is_empty();
        self.typed.push(c);
        let typed = self.typed.to_lowercase();
        let n = self.entries.len();
        if !repeat && let Some(i) = self.entries.iter().position(|e| starts(e, &typed)) {
            self.sel = i;
            return;
        }
        // Cycle among entries starting with this letter, after the current one.
        if let Some(i) = (1..=n).map(|d| (self.sel + d) % n).find(|&i| starts(&self.entries[i], &lc)) {
            self.sel = i;
        }
    }

    pub fn on_key(&mut self, k: KeyEvent) -> Action {
        self.message = None;
        let ctrl = k.modifiers.contains(KeyModifiers::CONTROL);
        let page = self.rows().saturating_sub(1).max(1);
        let last = self.entries.len().saturating_sub(1);
        let (_, cols) = self.grid();
        if !matches!(k.code, KeyCode::Char(_)) || ctrl {
            self.typed.clear();
        }
        match k.code {
            KeyCode::Char('c') if ctrl => return Action::Quit,
            KeyCode::Char('s') if ctrl => self.cycle_sort(),
            KeyCode::Char('r') if ctrl => self.refresh(),
            KeyCode::Esc => return Action::Quit,
            KeyCode::Up if self.sel >= cols => self.sel -= cols,
            KeyCode::Up => {}
            // Moving down from a full row into a shorter last row lands on its end.
            KeyCode::Down if self.sel / cols < last / cols => self.sel = (self.sel + cols).min(last),
            KeyCode::Down => {}
            KeyCode::PageUp => self.sel = self.sel.saturating_sub(page * cols),
            KeyCode::PageDown => self.sel = (self.sel + page * cols).min(last),
            KeyCode::Home => self.sel = 0,
            KeyCode::End => self.sel = last,
            KeyCode::Left if self.wide => self.sel = self.sel.saturating_sub(1),
            KeyCode::Right if self.wide => self.sel = (self.sel + 1).min(last),
            KeyCode::Enter | KeyCode::Right => return self.activate(),
            KeyCode::Left | KeyCode::Backspace => self.parent(),
            KeyCode::F(4) => self.wide = !self.wide,
            KeyCode::F(2) => self.cycle_sort(),
            KeyCode::F(3) => {
                self.show_hidden = !self.show_hidden;
                self.refresh();
            }
            KeyCode::F(5) => self.refresh(),
            KeyCode::Char('~') if self.typed.is_empty() => {
                if let Some(home) = std::env::var_os("HOME") {
                    self.chdir(home.into(), None);
                }
            }
            KeyCode::Char(c) if !ctrl => self.type_ahead(c),
            _ => {}
        }
        Action::None
    }

    /// Handles a mouse event; `None` means it was ignored.
    pub fn on_mouse(&mut self, m: MouseEvent) -> Option<Action> {
        let (colw, cols) = self.grid();
        let last = self.entries.len().saturating_sub(1);
        match m.kind {
            MouseEventKind::ScrollDown => self.sel = (self.sel + 3 * cols).min(last),
            MouseEventKind::ScrollUp => self.sel = self.sel.saturating_sub(3 * cols),
            MouseEventKind::Down(MouseButton::Left) => {
                let row = m.row as usize;
                if row == 0 || row > self.rows() {
                    return None;
                }
                // Column 0 is the left margin; each grid cell is `colw` wide.
                let col = if self.wide { ((m.column as usize).saturating_sub(1) / colw).min(cols - 1) } else { 0 };
                let i = (self.top + row - 1) * cols + col;
                if i >= self.entries.len() {
                    return None;
                }
                let double = self
                    .last_click
                    .is_some_and(|(j, t)| j == i && t.elapsed() < Duration::from_millis(400));
                self.sel = i;
                self.message = None;
                self.typed.clear();
                if double {
                    self.last_click = None;
                    return Some(self.activate());
                }
                self.last_click = Some((i, Instant::now()));
            }
            MouseEventKind::Down(MouseButton::Right) => self.parent(),
            _ => return None,
        }
        self.message = None;
        self.typed.clear();
        Some(Action::None)
    }

    fn cycle_sort(&mut self) {
        self.sort = self.sort.next();
        self.refresh();
    }

    pub fn draw(&mut self, out: &mut impl Write) -> io::Result<()> {
        let rows = self.rows();
        let (colw, cols) = self.grid();
        // `top` is the first grid row shown (one entry per row in list view).
        let sel_row = self.sel / cols;
        if sel_row < self.top {
            self.top = sel_row;
        } else if sel_row >= self.top + rows {
            self.top = sel_row + 1 - rows;
        }
        queue!(out, terminal::BeginSynchronizedUpdate, cursor::Hide)?;

        // Status bar: directory on the left, counts and sort on the right.
        let dirs = self.entries.iter().filter(|e| e.is_dir && e.name != "..").count();
        let files = self.entries.iter().filter(|e| !e.is_dir).count();
        let right = format!(
            "{dirs} dirs  {files} files  Sort: {}{} ",
            self.sort.label(),
            if self.show_hidden { "  +hidden" } else { "" }
        );
        let left = format!(" {}", truncate_left(&self.dir.display().to_string(), self.width.saturating_sub(right.width() + 2)));
        let mut p = Painter::new(self.width);
        let bold = crate::app::Style { bold: true, ..self.theme.status };
        p.text(bold, &left);
        p.text(bold, &" ".repeat(self.width.saturating_sub(left.width() + right.width())));
        p.text(self.theme.status, &right);
        p.finish(out, 0, self.theme.status)?;

        if self.wide {
            self.draw_grid(out, colw, cols)?;
        } else {
            self.draw_list(out)?;
        }
        self.draw_bottom(out)?;
        queue!(out, terminal::EndSynchronizedUpdate)?;
        out.flush()
    }

    fn entry_style(&self, i: usize, e: &Entry) -> crate::app::Style {
        match (i == self.sel, e.is_dir) {
            (true, _) => self.theme.select,
            (false, true) => self.theme.dir,
            (false, false) => self.theme.text,
        }
    }

    fn draw_grid(&self, out: &mut impl Write, colw: usize, cols: usize) -> io::Result<()> {
        let cell = colw.saturating_sub(2).max(1);
        for y in 0..self.rows() {
            let mut p = Painter::new(self.width);
            p.text(self.theme.text, " ");
            for c in 0..cols {
                let i = (self.top + y) * cols + c;
                let Some(e) = self.entries.get(i) else { break };
                let style = self.entry_style(i, e);
                let label = truncate_right(&e.label(), cell);
                p.text(style, &label);
                p.text(style, &" ".repeat(cell.saturating_sub(label.width())));
                p.text(self.theme.text, "  ");
            }
            p.finish(out, y + 1, self.theme.text)?;
        }
        Ok(())
    }

    fn draw_list(&self, out: &mut impl Write) -> io::Result<()> {
        let rows = self.rows();
        let meta_w = 26; // "  12.3M  2026-11-25 21:16"
        let name_w = self.width.saturating_sub(meta_w + 2).max(8);
        for y in 0..rows {
            let mut p = Painter::new(self.width);
            let i = self.top + y;
            let fill = if i == self.sel { self.theme.select } else { self.theme.text };
            if let Some(e) = self.entries.get(i) {
                let style = self.entry_style(i, e);
                let name = truncate_right(&e.label(), name_w);
                p.text(style, " ");
                p.text(style, &name);
                p.text(style, &" ".repeat(name_w.saturating_sub(name.width())));
                let size = if e.is_dir { "<DIR>".to_string() } else { human_size(e.size) };
                let date = e.modified.map(format_time).unwrap_or_default();
                let meta_style = if i == self.sel { self.theme.select } else { self.theme.gutter };
                p.text(meta_style, &format!(" {size:>7}  {date:<16}"));
            }
            p.finish(out, y + 1, fill)?;
        }
        Ok(())
    }

    fn draw_bottom(&self, out: &mut impl Write) -> io::Result<()> {
        let y = self.height.saturating_sub(1);
        let mut p = Painter::new(self.width);
        match &self.message {
            Some((m, err)) => p.text(if *err { self.theme.error } else { self.theme.text }, m),
            None if !self.typed.is_empty() && self.last_typed.elapsed() < Duration::from_millis(1000) => {
                p.text(self.theme.text, &format!(" Jump: {}", self.typed))
            }
            None if self.wide => {
                // Columns hide sizes and dates, so show them for the selection.
                if let Some(e) = self.entries.get(self.sel) {
                    let size = if e.is_dir { "<DIR>".to_string() } else { format!("{} bytes", e.size) };
                    let date = e.modified.map(format_time).unwrap_or_default();
                    p.text(self.theme.text, &format!(" {}  {size}  {date}", e.label()));
                }
                p.text(self.theme.hint, "   F4 Details  F2 Sort  F3 Hidden  Esc Exit");
            }
            None => p.text(
                self.theme.hint,
                " Enter View  ← Up  Type to jump  F2 Sort  F3 Hidden  F4 Columns  Esc Exit",
            ),
        }
        p.finish(out, y, self.theme.text)
    }
}

fn human_size(n: u64) -> String {
    if n < 100_000 {
        return n.to_string();
    }
    let mut v = n as f64;
    for unit in ["K", "M", "G", "T", "P"] {
        v /= 1024.0;
        if v < 1000.0 {
            return if v < 10.0 { format!("{v:.1}{unit}") } else { format!("{v:.0}{unit}") };
        }
    }
    format!("{v:.0}E")
}

fn format_time(t: SystemTime) -> String {
    chrono::DateTime::<chrono::Local>::from(t).format("%Y-%m-%d %H:%M").to_string()
}

fn truncate_right(s: &str, w: usize) -> String {
    if s.width() <= w {
        return s.to_string();
    }
    let mut out = String::new();
    let mut used = 0;
    for c in s.chars() {
        let cw = c.width().unwrap_or(0);
        if used + cw + 1 > w {
            break;
        }
        out.push(c);
        used += cw;
    }
    out.push('…');
    out
}

fn truncate_left(s: &str, w: usize) -> String {
    if s.width() <= w {
        return s.to_string();
    }
    let chars: Vec<char> = s.chars().collect();
    let mut used = 1;
    let mut start = chars.len();
    while start > 0 {
        let cw = chars[start - 1].width().unwrap_or(0);
        if used + cw > w {
            break;
        }
        used += cw;
        start -= 1;
    }
    format!("…{}", chars[start..].iter().collect::<String>())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn sizes() {
        assert_eq!(human_size(512), "512");
        assert_eq!(human_size(1_500_000), "1.4M");
        assert_eq!(human_size(50 * 1024 * 1024), "50M");
    }

    #[test]
    fn truncation() {
        assert_eq!(truncate_right("abcdef", 4), "abc…");
        assert_eq!(truncate_left("/a/b/c/d", 5), "…/c/d");
        assert_eq!(truncate_left("/a", 5), "/a");
    }

    #[test]
    fn more_sizes_and_wide_truncation() {
        assert_eq!(human_size(99_999), "99999");
        assert_eq!(human_size(100_000), "98K");
        assert_eq!(human_size(5 * 1024 * 1024 * 1024), "5.0G");
        assert_eq!(truncate_right("漢字漢字", 5), "漢字…");
        assert_eq!(truncate_left("漢字漢字", 5), "…漢字");
    }

    /// A scratch directory with a subdirectory and a few files of known sizes.
    fn fixture(tag: &str) -> PathBuf {
        let d = std::env::temp_dir().join(format!("lyst-browser-{tag}-{}", std::process::id()));
        let _ = fs::remove_dir_all(&d);
        fs::create_dir_all(d.join("Zeta")).unwrap();
        fs::write(d.join("c.rs"), vec![b'x'; 300]).unwrap();
        fs::write(d.join("A.md"), "1").unwrap();
        fs::write(d.join("b.txt"), vec![b'x'; 10]).unwrap();
        fs::write(d.join(".hidden"), "").unwrap();
        d
    }

    fn labels(b: &Browser) -> Vec<String> {
        b.entries.iter().map(Entry::label).collect()
    }

    #[test]
    fn sorting_and_hidden_files() {
        let dir = fixture("sort");
        let mut b = Browser::new(&dir, false, Prefs::new(false)).unwrap();
        assert_eq!(labels(&b), ["..", "Zeta/", "A.md", "b.txt", "c.rs"]);
        assert_eq!(b.sel, 1, "starts on the first real entry, not ..");

        let order = |b: &mut Browser, sort| {
            b.sort = sort;
            b.refresh();
            labels(b)
        };
        assert_eq!(order(&mut b, Sort::Ext), ["..", "Zeta/", "A.md", "c.rs", "b.txt"]);
        assert_eq!(order(&mut b, Sort::Size), ["..", "Zeta/", "c.rs", "b.txt", "A.md"]);

        b.sort = Sort::Name;
        b.show_hidden = true;
        b.refresh();
        assert_eq!(labels(&b), ["..", "Zeta/", ".hidden", "A.md", "b.txt", "c.rs"]);
        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn sort_cycles_through_all_modes() {
        let mut s = Sort::Name;
        let mut seen = vec![];
        for _ in 0..4 {
            seen.push(s.label());
            s = s.next();
        }
        assert!(s == Sort::Name);
        assert_eq!(seen, ["name", "extension", "size", "date"]);
    }

    #[test]
    fn refresh_keeps_selection_and_file_list_skips_dirs() {
        let dir = fixture("refresh");
        let mut b = Browser::new(&dir, false, Prefs::new(false)).unwrap();
        b.select_name("b.txt");
        fs::write(dir.join("a0.txt"), "").unwrap();
        b.refresh();
        assert_eq!(b.entries[b.sel].name, "b.txt");

        let (files, idx) = b.file_list();
        let names: Vec<_> = files.iter().map(|p| p.file_name().unwrap().to_string_lossy().into_owned()).collect();
        assert_eq!(names, ["A.md", "a0.txt", "b.txt", "c.rs"]);
        assert_eq!(idx, 2);

        b.select_path(&b.dir.join("c.rs"));
        assert_eq!(b.entries[b.sel].name, "c.rs");
        // A path in another directory is ignored.
        b.select_path(Path::new("/elsewhere/A.md"));
        assert_eq!(b.entries[b.sel].name, "c.rs");
        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn type_ahead_prefixes_and_cycling() {
        let dir = std::env::temp_dir().join(format!("lyst-browser-type-{}", std::process::id()));
        let _ = fs::remove_dir_all(&dir);
        fs::create_dir_all(&dir).unwrap();
        for n in ["bar", "baz", "Bob", "cat"] {
            fs::write(dir.join(n), "").unwrap();
        }
        let mut b = Browser::new(&dir, false, Prefs::new(false)).unwrap();
        let sel = |b: &Browser| b.entries[b.sel].name.clone();

        b.type_ahead('c');
        assert_eq!(sel(&b), "cat");
        // Repeating a letter cycles through names that start with it, ignoring case.
        b.typed.clear();
        b.type_ahead('b');
        assert_eq!(sel(&b), "bar");
        b.type_ahead('b');
        assert_eq!(sel(&b), "baz");
        b.type_ahead('B');
        assert_eq!(sel(&b), "Bob");
        b.type_ahead('b');
        assert_eq!(sel(&b), "bar", "wraps around");
        // Typing a longer prefix jumps straight to it.
        b.typed.clear();
        for c in "baz".chars() {
            b.type_ahead(c);
        }
        assert_eq!(sel(&b), "baz");
        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn unreadable_directory_is_an_error() {
        assert!(Browser::new(Path::new("/definitely/not/here"), false, Prefs::new(false)).is_err());
    }
}
