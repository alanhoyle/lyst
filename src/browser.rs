//! Directory browser shown when lyst is started without a file.

use crate::app::{Painter, Style, Theme};
use crate::goto::DirPrompt;
use crate::usage::Usage;
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
    /// The name prefix being typed after `/`, while jumping.
    jump: Option<String>,
    /// Set when the user asks to edit the selected file; the caller runs
    /// the editor.
    pub want_edit: Option<PathBuf>,
    /// A binary file the user was warned about; editing it again goes ahead.
    edit_warned: Option<PathBuf>,
    /// The "go to directory" prompt, while open.
    goto: Option<DirPrompt>,
    /// Disk usage being counted (or counted) for the selected entry.
    usage: Option<Usage>,
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
            jump: None,
            usage: None,
            goto: None,
            want_edit: None,
            edit_warned: None,
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

    /// Selects the first entry, starting at the selection (or just after it,
    /// for `next`) and wrapping around, whose name starts with the jump
    /// prefix, ignoring case. Returns false if nothing matches.
    fn jump_to(&mut self, next: bool) -> bool {
        let Some(prefix) = self.jump.as_ref().map(|p| p.to_lowercase()) else { return false };
        let n = self.entries.len();
        let skip = usize::from(next);
        let found = (skip..n + skip)
            .map(|d| (self.sel + d) % n)
            .find(|&i| self.entries[i].name != ".." && self.entries[i].name.to_lowercase().starts_with(&prefix));
        if let Some(i) = found {
            self.sel = i;
        }
        found.is_some()
    }

    /// Handles a key while jumping; `None` means the jump ended and the key
    /// should be handled normally.
    fn jump_key(&mut self, k: KeyEvent) -> Option<Action> {
        let ctrl = k.modifiers.contains(KeyModifiers::CONTROL);
        let prefix = self.jump.as_mut()?;
        match k.code {
            KeyCode::Esc => self.jump = None,
            KeyCode::Backspace => {
                prefix.pop();
                self.jump_to(false);
            }
            KeyCode::Char('/') => {
                self.jump_to(true);
            }
            KeyCode::Char(c) if !ctrl => {
                prefix.push(c);
                self.jump_to(false);
            }
            _ => {
                self.jump = None;
                return None;
            }
        }
        Some(Action::None)
    }

    pub fn on_key(&mut self, k: KeyEvent) -> Action {
        self.message = None;
        let warned = self.edit_warned.take();
        if self.goto.is_some() {
            self.goto_key(k);
            return Action::None;
        }
        if let Some(action) = self.jump_key(k) {
            return action;
        }
        let ctrl = k.modifiers.contains(KeyModifiers::CONTROL);
        let page = self.rows().saturating_sub(1).max(1);
        let last = self.entries.len().saturating_sub(1);
        let (_, cols) = self.grid();
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
            KeyCode::Char('~') => {
                if let Some(home) = std::env::var_os("HOME") {
                    self.chdir(home.into(), None);
                }
            }
            KeyCode::Char('O') => self.open_external(),
            KeyCode::Char('/') if !ctrl => self.jump = Some(String::new()),
            KeyCode::Char('q' | 'Q') if !ctrl => return Action::Quit,
            KeyCode::Char('w' | 'W') if !ctrl => self.open_web(),
            KeyCode::Char('e' | 'E' | 'v' | 'V') if !ctrl => self.request_edit(warned),
            KeyCode::Char('j' | 'J') if !ctrl => self.goto = Some(DirPrompt::new(&self.dir, self.show_hidden)),
            KeyCode::Char('u' | 'U') if !ctrl => {
                if let Some(e) = self.entries.get(self.sel) {
                    self.usage = Some(Usage::start(&e.path));
                }
            }
            _ => {}
        }
        Action::None
    }

    /// Asks the caller to run the editor on the selected file, warning first
    /// (and needing a second press) if it doesn't look like text.
    fn request_edit(&mut self, warned: Option<PathBuf>) {
        let Some(e) = self.entries.get(self.sel) else { return };
        if e.is_dir {
            return self.error(format!("{} is a directory; pick a file to edit", e.label()));
        }
        let (path, label) = (e.path.clone(), e.label());
        match crate::external::looks_binary(&path) {
            Err(err) => self.error(format!("Can't edit {label}: {err}")),
            Ok(true) if warned.as_ref() != Some(&path) => {
                self.error(format!("{label} doesn't look like a text file; press E again to edit it anyway"));
                self.edit_warned = Some(path);
            }
            Ok(_) => self.want_edit = Some(path),
        }
    }

    /// After the editor exits: rereads the directory, keeping the selection.
    pub fn edited(&mut self, path: &Path, result: io::Result<std::process::ExitStatus>) {
        self.refresh();
        self.select_path(path);
        let editor = crate::external::editor();
        match result {
            Err(e) => self.error(format!("Can't run editor \"{editor}\": {e} (set $VISUAL or $EDITOR)")),
            Ok(s) if !s.success() => self.error(format!("Editor \"{editor}\" exited with {s}")),
            Ok(_) => {}
        }
    }

    /// Handles a key in the "go to directory" prompt.
    fn goto_key(&mut self, k: KeyEvent) {
        let ctrl = k.modifiers.contains(KeyModifiers::CONTROL);
        let Some(p) = &mut self.goto else { return };
        match k.code {
            KeyCode::Esc => self.goto = None,
            KeyCode::Char('c' | 'g') if ctrl => self.goto = None,
            KeyCode::Enter => {
                let empty = p.input.is_empty();
                let target = p.target();
                self.goto = None;
                if !empty {
                    self.go_to(target);
                }
            }
            KeyCode::Tab => p.complete(),
            KeyCode::BackTab | KeyCode::Up => p.step(-1),
            KeyCode::Down => p.step(1),
            KeyCode::Backspace => p.backspace(),
            KeyCode::Char('w') if ctrl => p.delete_word(),
            KeyCode::Char('u') if ctrl => p.clear(),
            KeyCode::Char(c) if !ctrl => p.push(c),
            _ => {}
        }
    }

    /// Shows a directory, or a file's directory with the file selected.
    fn go_to(&mut self, target: PathBuf) {
        let path = match fs::canonicalize(&target) {
            Ok(p) => p,
            Err(e) => return self.error(format!("{}: {e}", target.display())),
        };
        if path.is_dir() {
            self.chdir(path, None);
        } else if let Some(dir) = path.parent() {
            let name = path.file_name().map(|n| n.to_string_lossy().into_owned());
            self.chdir(dir.to_path_buf(), name);
        }
    }

    /// Stops showing (and counting) disk usage once another entry is selected.
    fn drop_stale_usage(&mut self) {
        let selected = self.entries.get(self.sel).map(|e| &e.path);
        if self.usage.as_ref().is_some_and(|u| Some(&u.path) != selected) {
            self.usage = None;
        }
    }

    /// Called when idle; true if the screen needs redrawing for disk usage
    /// progress or its final total.
    pub fn tick(&mut self) -> bool {
        let Some(u) = &mut self.usage else { return false };
        if u.reported {
            return false;
        }
        u.reported = u.totals().done;
        true
    }

    /// The bottom-bar text for the disk usage count.
    fn usage_line(&self, u: &Usage) -> String {
        let t = u.totals();
        let name = self.entries.get(self.sel).map(|e| e.label()).unwrap_or_default();
        let mut s = format!(" {name}  {} on disk", human_size(t.disk));
        if t.dirs > 0 || t.files != 1 || !t.done {
            let files = if t.files == 1 { "file" } else { "files" };
            s += &format!(" ({} in {} {files}", human_size(t.bytes), t.files);
            if t.dirs > 0 {
                let dirs = if t.dirs == 1 { "subdirectory" } else { "subdirectories" };
                s += &format!(", {} {dirs}", t.dirs);
            }
            s += ")";
        } else {
            s += &format!(" ({} bytes)", t.bytes);
        }
        if t.errors > 0 {
            s += &format!(", {} unreadable", t.errors);
        }
        if !t.done {
            s += " …";
        }
        s
    }

    /// Opens the selected file or directory in the default web browser.
    fn open_web(&mut self) {
        let Some(e) = self.entries.get(self.sel) else { return };
        let (path, label) = (e.path.clone(), e.label());
        match crate::external::open_in_browser(&path) {
            Ok(()) => self.flash(format!("Opening {label} in the web browser")),
            Err(err) => self.error(format!("Can't open {label} in a web browser: {err}")),
        }
    }

    /// Opens the selected file or directory in the OS's default application.
    fn open_external(&mut self) {
        let Some(e) = self.entries.get(self.sel) else { return };
        let (path, label) = (e.path.clone(), e.label());
        match crate::external::open(&path) {
            Ok(()) => self.flash(format!("Opening {label} in its default application")),
            Err(err) => self.error(format!("Can't open {label}: {err}")),
        }
    }

    /// Handles a mouse event; `None` means it was ignored.
    pub fn on_mouse(&mut self, m: MouseEvent) -> Option<Action> {
        let (colw, cols) = self.grid();
        let last = self.entries.len().saturating_sub(1);
        if self.goto.is_some() && matches!(m.kind, MouseEventKind::Down(_)) {
            self.goto = None;
            return Some(Action::None);
        }
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
                self.jump = None;
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
        self.jump = None;
        Some(Action::None)
    }

    fn cycle_sort(&mut self) {
        self.sort = self.sort.next();
        self.refresh();
    }

    pub fn draw(&mut self, out: &mut impl Write) -> io::Result<()> {
        self.drop_stale_usage();
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
        if let Some(g) = &self.goto {
            self.draw_suggestions(out, g)?;
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

    /// Lists the prompt's suggestions just above the bottom bar, scrolled to
    /// keep the highlighted one in view.
    fn draw_suggestions(&self, out: &mut impl Write, g: &DirPrompt) -> io::Result<()> {
        let room = self.rows().min(10);
        if g.matches.is_empty() || room == 0 {
            return Ok(());
        }
        let shown = g.matches.len().min(room);
        let start = g.sel.map_or(0, |i| (i + 1).saturating_sub(shown));
        let more = g.matches.len() - shown;
        let first_y = self.height.saturating_sub(1 + shown);
        for (k, i) in (start..start + shown).enumerate() {
            let m = &g.matches[i];
            // Just the last component; the input shows the rest.
            let name = m.trim_end_matches('/').rsplit('/').next().unwrap_or(m);
            let style = if g.sel == Some(i) { self.theme.select } else { self.theme.status };
            let mut p = Painter::new(self.width);
            p.text(style, &format!("  {name}/"));
            if k == 0 && start > 0 {
                p.text(self.theme.status, &format!("   ↑ {start} more"));
            }
            if k + 1 == shown && more > start {
                p.text(self.theme.status, &format!("   ↓ {} more", more - start));
            }
            p.finish(out, first_y + k, self.theme.status)?;
        }
        Ok(())
    }

    fn draw_bottom(&self, out: &mut impl Write) -> io::Result<()> {
        let y = self.height.saturating_sub(1);
        let mut p = Painter::new(self.width);
        if let Some(g) = &self.goto {
            let label = " Go to directory: ";
            let hint = if g.input.is_empty() { "   Tab completes  ~ home  Esc cancels" } else { "" };
            // Keep the end of a long path in view.
            let room = self.width.saturating_sub(label.width() + 1);
            let shown = truncate_left(&g.input, room);
            p.text(Style { bold: true, ..self.theme.text }, label);
            p.text(self.theme.text, &shown);
            p.text(self.theme.hint, hint);
            let cx = (label.width() + shown.width()).min(self.width.saturating_sub(1));
            p.finish(out, y, self.theme.text)?;
            return queue!(out, cursor::MoveTo(cx as u16, y as u16), cursor::Show);
        }
        match &self.message {
            Some((m, err)) => p.text(if *err { self.theme.error } else { self.theme.text }, m),
            None if let Some(prefix) = &self.jump => {
                let matched = self.entries.get(self.sel).is_some_and(|e| {
                    e.name != ".." && e.name.to_lowercase().starts_with(&prefix.to_lowercase())
                });
                p.text(self.theme.text, &format!(" Jump: {prefix}"));
                if !matched && !prefix.is_empty() {
                    p.text(self.theme.error, "  (no match)");
                }
                p.text(self.theme.hint, "   / Next  Enter View  Esc Cancel");
            }
            None if let Some(u) = &self.usage => p.text(self.theme.text, &self.usage_line(u)),
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
                " Enter View  E Edit  O Open  W Web  ← Up  / Jump  J Go to  U Usage  F2 Sort  F3 Hidden  F4 Cols  Esc Exit",
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
    fn slash_jumps_by_prefix_and_cycles() {
        let dir = std::env::temp_dir().join(format!("lyst-browser-jump-{}", std::process::id()));
        let _ = fs::remove_dir_all(&dir);
        fs::create_dir_all(&dir).unwrap();
        for n in ["bar", "baz", "Bob", "cat"] {
            fs::write(dir.join(n), "").unwrap();
        }
        let mut b = Browser::new(&dir, false, Prefs::new(false)).unwrap();
        let sel = |b: &Browser| b.entries[b.sel].name.clone();
        let key = |c| KeyEvent::new(KeyCode::Char(c), KeyModifiers::NONE);
        let code = |c| KeyEvent::new(c, KeyModifiers::NONE);
        let start = sel(&b);

        // Letters without `/` don't move the selection.
        b.on_key(key('c'));
        assert_eq!(sel(&b), start);
        assert!(b.jump.is_none());

        b.on_key(key('/'));
        b.on_key(key('c'));
        assert_eq!(sel(&b), "cat");
        // Backspace widens the prefix; matching ignores case.
        b.on_key(code(KeyCode::Backspace));
        b.on_key(key('B'));
        assert_eq!(sel(&b), "bar", "wraps around to the first match");
        b.on_key(key('a'));
        b.on_key(key('z'));
        assert_eq!(sel(&b), "baz");
        // `/` again moves to the next match of the prefix.
        b.on_key(code(KeyCode::Backspace));
        b.on_key(code(KeyCode::Backspace));
        b.on_key(key('/'));
        assert_eq!(sel(&b), "Bob");
        b.on_key(key('/'));
        assert_eq!(sel(&b), "bar", "wraps around");
        // No match leaves the selection where it was.
        b.on_key(key('q'));
        assert_eq!(sel(&b), "bar");
        // Esc ends the jump; the selection stays.
        b.on_key(code(KeyCode::Esc));
        assert!(b.jump.is_none());
        assert_eq!(sel(&b), "bar");
        // An arrow ends the jump and moves as usual.
        b.on_key(key('/'));
        b.on_key(code(KeyCode::Down));
        assert!(b.jump.is_none());
        assert_eq!(sel(&b), "baz");
        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn j_goes_to_a_typed_directory() {
        let dir = std::env::temp_dir().join(format!("lyst-browser-goto-{}", std::process::id()));
        let _ = fs::remove_dir_all(&dir);
        fs::create_dir_all(dir.join("alpha/beta")).unwrap();
        fs::write(dir.join("alpha/beta/file.txt"), "").unwrap();
        let mut b = Browser::new(&dir, false, Prefs::new(false)).unwrap();
        b.resize(80, 12);
        let key = |c| KeyEvent::new(KeyCode::Char(c), KeyModifiers::NONE);
        let code = |c| KeyEvent::new(c, KeyModifiers::NONE);

        b.on_key(key('j'));
        for c in "al".chars() {
            b.on_key(key(c));
        }
        b.on_key(code(KeyCode::Tab));
        b.on_key(code(KeyCode::Tab));
        assert_eq!(b.goto.as_ref().unwrap().input, "alpha/beta/");
        let mut screen = Vec::new();
        b.draw(&mut screen).unwrap();
        let screen = String::from_utf8_lossy(&screen);
        assert!(screen.contains("Go to directory: ") && screen.contains("alpha/beta/"), "{screen}");
        b.on_key(code(KeyCode::Enter));
        assert!(b.goto.is_none());
        assert_eq!(b.dir, fs::canonicalize(dir.join("alpha/beta")).unwrap());

        // A file goes to its directory with the file selected; `q` is typed.
        b.on_key(key('j'));
        for c in "../../alpha/beta/file.txt".chars() {
            b.on_key(key(c));
        }
        b.on_key(code(KeyCode::Enter));
        assert_eq!(b.entries[b.sel].name, "file.txt");

        // Missing paths are an error; Esc cancels without moving.
        let here = b.dir.clone();
        b.on_key(key('j'));
        b.on_key(key('q'));
        b.on_key(code(KeyCode::Enter));
        assert!(b.message.as_ref().is_some_and(|(_, err)| *err));
        b.on_key(key('j'));
        b.on_key(key('x'));
        b.on_key(code(KeyCode::Esc));
        assert!(b.goto.is_none());
        assert_eq!(b.dir, here);
        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn e_edits_files_and_warns_about_binary_ones() {
        let dir = std::env::temp_dir().join(format!("lyst-browser-edit-{}", std::process::id()));
        let _ = fs::remove_dir_all(&dir);
        fs::create_dir_all(dir.join("adir")).unwrap();
        fs::write(dir.join("b.bin"), b"\0\x01\x02").unwrap();
        fs::write(dir.join("c.txt"), "text\n").unwrap();
        let mut b = Browser::new(&dir, false, Prefs::new(false)).unwrap();
        let key = |c| KeyEvent::new(KeyCode::Char(c), KeyModifiers::NONE);
        let down = KeyEvent::new(KeyCode::Down, KeyModifiers::NONE);

        assert_eq!(b.entries[b.sel].name, "adir");
        b.on_key(key('e'));
        assert!(b.want_edit.is_none());
        assert!(b.message.as_ref().is_some_and(|(m, err)| *err && m.contains("directory")));

        b.on_key(down);
        b.on_key(key('e'));
        assert!(b.want_edit.is_none(), "warned first");
        assert!(b.message.as_ref().is_some_and(|(m, _)| m.contains("doesn't look like a text file")));
        b.on_key(key('E'));
        assert_eq!(b.want_edit.take(), Some(b.entries[b.sel].path.clone()));

        b.on_key(down);
        b.on_key(key('v'));
        let path = b.want_edit.take().unwrap();
        assert!(path.ends_with("c.txt"));
        b.edited(&path, Ok(std::process::ExitStatus::default()));
        assert_eq!(b.entries[b.sel].name, "c.txt");
        assert!(b.message.is_none());
        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn q_quits_unless_jumping() {
        let mut b = Browser::new(Path::new("samples"), false, Prefs::new(false)).unwrap();
        let key = |c| KeyEvent::new(KeyCode::Char(c), KeyModifiers::NONE);
        b.on_key(key('/'));
        assert!(matches!(b.on_key(key('q')), Action::None), "typed as part of a name");
        b.on_key(KeyEvent::new(KeyCode::Esc, KeyModifiers::NONE));
        assert!(matches!(b.on_key(key('q')), Action::Quit));
        assert!(matches!(b.on_key(key('Q')), Action::Quit));
    }

    #[test]
    fn u_counts_disk_usage_of_the_selection() {
        let dir = std::env::temp_dir().join(format!("lyst-browser-du-{}", std::process::id()));
        let _ = fs::remove_dir_all(&dir);
        fs::create_dir_all(dir.join("sub/deeper")).unwrap();
        fs::write(dir.join("sub/a"), vec![b'x'; 3000]).unwrap();
        fs::write(dir.join("sub/deeper/b"), vec![b'x'; 2000]).unwrap();
        fs::write(dir.join("z"), "").unwrap();
        let mut b = Browser::new(&dir, false, Prefs::new(false)).unwrap();
        b.resize(100, 10);
        assert_eq!(b.entries[b.sel].name, "sub");
        b.on_key(KeyEvent::new(KeyCode::Char('u'), KeyModifiers::NONE));
        let deadline = Instant::now() + Duration::from_secs(10);
        while b.tick() {
            assert!(Instant::now() < deadline);
            std::thread::sleep(Duration::from_millis(5));
        }
        let line = b.usage_line(b.usage.as_ref().unwrap());
        assert!(line.contains("sub/") && line.contains("in 2 files, 1 subdirectory"), "{line}");
        assert!(!line.contains('…'), "finished: {line}");

        // Moving to another entry drops the count from the bottom bar.
        b.on_key(KeyEvent::new(KeyCode::Down, KeyModifiers::NONE));
        b.draw(&mut Vec::new()).unwrap();
        assert!(b.usage.is_none());
        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn enter_while_jumping_opens_and_capital_o_is_typed() {
        let dir = std::env::temp_dir().join(format!("lyst-browser-o-{}", std::process::id()));
        let _ = fs::remove_dir_all(&dir);
        fs::create_dir_all(&dir).unwrap();
        for n in ["fa", "foo"] {
            fs::write(dir.join(n), "").unwrap();
        }
        let mut b = Browser::new(&dir, false, Prefs::new(false)).unwrap();
        let key = |c| KeyEvent::new(KeyCode::Char(c), KeyModifiers::NONE);
        for c in "/fO".chars() {
            b.on_key(key(c));
        }
        assert_eq!(b.entries[b.sel].name, "foo");
        assert!(b.message.is_none(), "nothing was opened: {:?}", b.message);
        assert!(matches!(b.on_key(KeyEvent::new(KeyCode::Enter, KeyModifiers::NONE)), Action::Open));
        assert!(b.jump.is_none());
        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn unreadable_directory_is_an_error() {
        assert!(Browser::new(Path::new("/definitely/not/here"), false, Prefs::new(false)).is_err());
    }
}
