//! Directory browser shown when lyst is started without a file.

use crate::app::{Painter, Style, Theme};
use crate::findfiles::Walk;
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
    /// Unix permission bits; None elsewhere.
    mode: Option<u32>,
    /// Unix owner and group ids; None elsewhere.
    owner: Option<(u32, u32)>,
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

pub const HELP: &str = "\
lyst file browser

MOVING AROUND
  Up / Down, PgUp / PgDn    move the selection
  Home / End                first / last entry
  Enter or Right            open a directory, or view a file
  Left or Backspace         up to the parent directory
  ~                         your home directory
  -                         back to the previous directory (again: return)
  / name                    jump to the entry starting with name;
                            / again: next match, Enter opens, Esc stops
  J                         type a directory to go to, with Tab completion;
                            empty, it lists recent directories (kept
                            between runs), Up/Down or Tab to pick one
  F                         find files and directories by name anywhere
                            below here: words typed must all appear in the
                            path; Up/Down pick, Enter opens, Esc returns

FILES
  Space                     tag the file (then Enter views just the tagged
                            files, [ and ] stepping through them); Esc
                            clears the tags
  E or V                    edit the file in $VISUAL or $EDITOR
  O                         open in its default application
  W                         open in the default web browser
  U                         disk usage of the file or directory
  Y                         copy the full path to the clipboard
  #                         a shell in this directory ($SHELL); exit it to
                            come back

DISPLAY
  F2                        sort by name, extension, size or date
  F3                        show or hide dotfiles
  F4                        one line per file, or columns
  F5 or Ctrl-R              reread the directory

OTHER
  h, F1 or ?                this help
  F9                        mouse on/off
  Q or Esc                  quit
";

/// Most results a find lists.
const FIND_SHOWN: usize = 2000;

/// The recursive find, while active.
struct Find {
    walk: Walk,
    input: String,
    /// Matches in all, of which `entries` shows up to `FIND_SHOWN`.
    total: usize,
    /// Paths found when the results were last updated.
    seen: usize,
    walking: bool,
    /// The directory listing to return to: entries, selection, scroll.
    saved: (Vec<Entry>, usize, usize),
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
    /// Set when the user asks for a shell here; the caller runs it.
    pub want_shell: Option<PathBuf>,
    /// A binary file the user was warned about; editing it again goes ahead.
    edit_warned: Option<PathBuf>,
    /// The "go to directory" prompt, while open.
    goto: Option<DirPrompt>,
    /// Disk usage being counted (or counted) for the selected entry.
    usage: Option<Usage>,
    /// Recently visited directories, most recent first.
    history: Vec<PathBuf>,
    /// Tagged files, in the order they were tagged.
    tags: Vec<PathBuf>,
    find: Option<Find>,
    /// The help page's scroll position, while shown.
    help: Option<usize>,
    /// Text to put on the clipboard with the next frame (OSC 52).
    clipboard: Option<String>,
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
            dir: crate::paths::canonicalize(dir)?,
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
            want_shell: None,
            edit_warned: None,
            history: Vec::new(),
            tags: Vec::new(),
            find: None,
            help: None,
            clipboard: None,
            last_click: None,
            message: None,
            theme: Theme::new(classic),
            width: w as usize,
            height: h as usize,
        };
        b.load()?;
        b.history = crate::history::record(&[], &b.dir);
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
                mode: unix_mode(&meta),
                owner: unix_owner(&meta),
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
                    mode: fs::metadata(parent).ok().and_then(|m| unix_mode(&m)),
                    owner: fs::metadata(parent).ok().and_then(|m| unix_owner(&m)),
                },
            );
        }
        self.entries = entries;
        self.sel = self.sel.min(self.entries.len().saturating_sub(1));
        Ok(())
    }

    /// Re-reads the directory, keeping the selection on the same name.
    pub fn refresh(&mut self) {
        if self.find.is_some() {
            return self.update_find(true);
        }
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
        if self.find.is_some() {
            if let Some(i) = self.entries.iter().position(|e| e.path == path) {
                self.sel = i;
            }
            return;
        }
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

    /// The files to view, plus the index of the selected one: the tagged
    /// files if any, else all plain files in the listing.
    pub fn file_list(&self) -> (Vec<PathBuf>, usize) {
        if !self.tags.is_empty() {
            let sel = self.entries.get(self.sel).map(|e| &e.path);
            let idx = self.tags.iter().position(|t| Some(t) == sel).unwrap_or(0);
            return (self.tags.clone(), idx);
        }
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
        self.history = crate::history::record(&self.history, &self.dir);
    }

    /// `-`: the most recent other directory that still exists.
    fn go_back(&mut self) {
        let Some(prev) = self.history.iter().find(|d| **d != self.dir && d.is_dir()).cloned() else {
            return self.error("No previous directory");
        };
        let here = self.dir.clone();
        self.chdir(prev, None);
        // Coming back to a directory selects the one just left, if inside it.
        if here.parent() == Some(self.dir.as_path()) {
            self.select_path(&here);
        }
    }

    /// Recent directories for the go-to prompt, as typed.
    fn recent(&self) -> Vec<String> {
        self.history.iter().filter(|d| **d != self.dir).take(50).map(|d| crate::history::display(d)).collect()
    }

    // --------------------------------------------------------------- tags

    fn toggle_tag(&mut self) {
        let Some(e) = self.entries.get(self.sel) else { return };
        if e.is_dir {
            return self.error("Only files can be tagged");
        }
        let p = e.path.clone();
        if let Some(i) = self.tags.iter().position(|t| *t == p) {
            self.tags.remove(i);
        } else {
            self.tags.push(p);
        }
        self.sel = (self.sel + 1).min(self.entries.len().saturating_sub(1));
        match self.tags.len() {
            0 => self.flash("No files tagged"),
            n => self.flash(format!("{n} tagged: Enter views them, Esc clears")),
        }
    }

    // --------------------------------------------------------------- find

    fn start_find(&mut self) {
        let saved = (std::mem::take(&mut self.entries), self.sel, self.top);
        let walk = Walk::start(&self.dir, self.show_hidden);
        self.find = Some(Find { walk, input: String::new(), total: 0, seen: 0, walking: true, saved });
        self.sel = 0;
        self.top = 0;
        self.update_find(false);
    }

    /// Lists the paths matching the find input. `keep` keeps the same path
    /// selected (new results arrived); otherwise the first is selected.
    fn update_find(&mut self, keep: bool) {
        let Some(f) = &mut self.find else { return };
        let (hits, total) = f.walk.matching(&f.input, FIND_SHOWN);
        f.total = total;
        f.seen = f.walk.count();
        f.walking = !f.walk.done();
        let root = f.walk.root.clone();
        let selected = self.entries.get(self.sel).map(|e| e.path.clone()).filter(|_| keep);
        self.entries = hits
            .into_iter()
            .map(|rel| {
                let name = rel.trim_end_matches('/').to_string();
                let path = root.join(&name);
                let meta = fs::symlink_metadata(&path).ok();
                Entry {
                    is_dir: rel.ends_with('/'),
                    is_link: meta.as_ref().is_some_and(|m| m.file_type().is_symlink()),
                    size: meta.as_ref().map_or(0, |m| m.len()),
                    mode: meta.as_ref().and_then(unix_mode),
                    owner: meta.as_ref().and_then(unix_owner),
                    modified: meta.and_then(|m| m.modified().ok()),
                    name,
                    path,
                }
            })
            .collect();
        self.sel = selected.and_then(|p| self.entries.iter().position(|e| e.path == p)).unwrap_or(0);
        if !keep {
            self.top = 0;
        }
    }

    /// Leaves the find, back to the directory listing (`restore`) or not
    /// (a result was opened elsewhere).
    fn end_find(&mut self, restore: bool) {
        if let Some(f) = self.find.take()
            && restore
        {
            (self.entries, self.sel, self.top) = f.saved;
        }
    }

    /// Handles a key while finding; `None` lets the normal handling move
    /// the selection.
    fn find_key(&mut self, k: KeyEvent) -> Option<Action> {
        let ctrl = k.modifiers.contains(KeyModifiers::CONTROL);
        let f = self.find.as_mut()?;
        match k.code {
            KeyCode::Esc => self.end_find(true),
            KeyCode::Char('c') if ctrl => self.end_find(true),
            KeyCode::Enter => return Some(self.activate()),
            KeyCode::Backspace => {
                f.input.pop();
                self.update_find(false);
            }
            KeyCode::Char('u') if ctrl => {
                f.input.clear();
                self.update_find(false);
            }
            KeyCode::Char(c) if !ctrl => {
                f.input.push(c);
                self.update_find(false);
            }
            KeyCode::Left | KeyCode::Right => {}
            _ => return None,
        }
        Some(Action::None)
    }

    // --------------------------------------------------------------- help

    fn help_key(&mut self, k: KeyEvent) {
        let Some(scroll) = self.help else { return };
        let max = HELP.lines().count().saturating_sub(self.rows());
        let page = self.rows().saturating_sub(1).max(1);
        self.help = match k.code {
            KeyCode::Up | KeyCode::Char('k') => Some(scroll.saturating_sub(1)),
            KeyCode::Down | KeyCode::Char('j') => Some((scroll + 1).min(max)),
            KeyCode::PageUp | KeyCode::Char('b') => Some(scroll.saturating_sub(page)),
            KeyCode::PageDown | KeyCode::Char(' ') => Some((scroll + page).min(max)),
            _ => None,
        };
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
            self.end_find(false);
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
        if self.help.is_some() {
            self.help_key(k);
            return Action::None;
        }
        if self.goto.is_some() {
            self.goto_key(k);
            return Action::None;
        }
        if let Some(action) = self.find_key(k) {
            return action;
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
            KeyCode::Esc if !self.tags.is_empty() => {
                self.tags.clear();
                self.flash("Tags cleared");
            }
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
            KeyCode::Left | KeyCode::Backspace if self.find.is_none() => self.parent(),
            KeyCode::F(4) => self.wide = !self.wide,
            KeyCode::F(2) => self.cycle_sort(),
            KeyCode::F(3) => {
                self.show_hidden = !self.show_hidden;
                self.refresh();
            }
            KeyCode::F(5) => self.refresh(),
            KeyCode::Char('~') => {
                if let Some(home) = crate::paths::home() {
                    self.chdir(home, None);
                }
            }
            KeyCode::Char('#') => self.want_shell = Some(self.dir.clone()),
            KeyCode::Char('O') => self.open_external(),
            KeyCode::Char('/') if !ctrl => self.jump = Some(String::new()),
            KeyCode::Char('q' | 'Q') if !ctrl => return Action::Quit,
            KeyCode::Char('w' | 'W') if !ctrl => self.open_web(),
            KeyCode::Char('e' | 'E' | 'v' | 'V') if !ctrl => self.request_edit(warned),
            KeyCode::Char('j' | 'J') if !ctrl => self.goto = Some(DirPrompt::new(&self.dir, self.show_hidden, self.recent())),
            KeyCode::Char('-') => self.go_back(),
            KeyCode::Char(' ') => self.toggle_tag(),
            KeyCode::Char('f' | 'F') if !ctrl => self.start_find(),
            KeyCode::Char('y' | 'Y') if !ctrl => {
                if let Some(e) = self.entries.get(self.sel) {
                    let p = e.path.display().to_string();
                    self.flash(format!("Copied {p}"));
                    self.clipboard = Some(p);
                }
            }
            KeyCode::F(1) | KeyCode::Char('h' | 'H' | '?') => self.help = Some(0),
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

    /// After the shell exits: rereads the directory (it may have changed),
    /// keeping the selection.
    pub fn shell_done(&mut self, result: io::Result<std::process::ExitStatus>) {
        let selected = self.entries.get(self.sel).map(|e| e.path.clone());
        self.refresh();
        if let Some(p) = selected {
            self.select_path(&p);
        }
        if let Err(e) = result {
            self.error(format!("Can't run shell \"{}\": {e} (set $SHELL)", crate::external::shell_program()));
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
        let path = match crate::paths::canonicalize(&target) {
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
        if let Some(f) = &self.find
            && f.walking
            && (f.walk.count() != f.seen || f.walk.done())
        {
            self.update_find(true);
            return true;
        }
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
        let mut s = format!(" {} on disk", human_size(t.disk));
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
        if let Some(scroll) = self.help {
            let max = HELP.lines().count().saturating_sub(self.rows());
            self.help = match m.kind {
                MouseEventKind::ScrollDown => Some((scroll + 3).min(max)),
                MouseEventKind::ScrollUp => Some(scroll.saturating_sub(3)),
                MouseEventKind::Down(_) => None,
                _ => return None,
            };
            return Some(Action::None);
        }
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
        if let Some(text) = self.clipboard.take() {
            queue!(out, crossterm::style::Print(crate::external::osc52(&text)))?;
        }

        // Status bar: directory on the left, counts and sort on the right.
        let dirs = self.entries.iter().filter(|e| e.is_dir && e.name != "..").count();
        let files = self.entries.iter().filter(|e| !e.is_dir).count();
        let bytes: u64 = self.entries.iter().filter(|e| !e.is_dir).map(|e| e.size).sum();
        let total = if bytes < 100_000 { format!("{bytes} bytes") } else { human_size(bytes) };
        let tagged = if self.tags.is_empty() { String::new() } else { format!("{} tagged  ", self.tags.len()) };
        let right = match &self.find {
            Some(f) => {
                let more = if f.total > self.entries.len() { format!(" (first {})", self.entries.len()) } else { String::new() };
                let busy = if f.walking { format!("  searching… {} paths", f.seen) } else { String::new() };
                format!("{tagged}{} matches{more}{busy} ", f.total)
            }
            None => format!(
                "{tagged}{dirs} dirs  {files} files  {total}  Sort: {}{} ",
                self.sort.label(),
                if self.show_hidden { "  +hidden" } else { "" }
            ),
        };
        let where_ = match &self.find {
            Some(_) => format!("Find under {}", self.dir.display()),
            None => self.dir.display().to_string(),
        };
        let left = format!(" {}", truncate_left(&where_, self.width.saturating_sub(right.width() + 2)));
        let mut p = Painter::new(self.width);
        let bold = crate::app::Style { bold: true, ..self.theme.status };
        p.text(bold, &left);
        p.text(bold, &" ".repeat(self.width.saturating_sub(left.width() + right.width())));
        p.text(self.theme.status, &right);
        p.finish(out, 0, self.theme.status)?;

        if let Some(scroll) = self.help {
            for (y, i) in (scroll..scroll + self.rows()).enumerate() {
                let mut p = Painter::new(self.width);
                if let Some(l) = HELP.lines().nth(i) {
                    let heading = l.chars().next().is_some_and(|c| c.is_ascii_uppercase());
                    p.text(if heading { Style { bold: true, ..self.theme.text } } else { self.theme.text }, l);
                }
                p.finish(out, y + 1, self.theme.text)?;
            }
        } else if self.wide {
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
                if self.tags.contains(&e.path) {
                    p.text(self.theme.hit, "•");
                    p.text(style, &truncate_right(&label, cell.saturating_sub(1)));
                    p.text(style, &" ".repeat(cell.saturating_sub(label.width().min(cell.saturating_sub(1)) + 1)));
                    p.text(self.theme.text, "  ");
                    continue;
                }
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
        // Permissions only on Unix, and only when names keep a fair width.
        let perms = cfg!(unix) && self.width >= 64;
        let meta_w = if perms { 37 } else { 26 }; // "  12.3M  2026-11-25 21:16  rwxr-xr-x"
        let name_w = self.width.saturating_sub(meta_w + 2).max(8);
        for y in 0..rows {
            let mut p = Painter::new(self.width);
            let i = self.top + y;
            let fill = if i == self.sel { self.theme.select } else { self.theme.text };
            if let Some(e) = self.entries.get(i) {
                let style = self.entry_style(i, e);
                let name = truncate_right(&e.label(), name_w);
                if self.tags.contains(&e.path) {
                    p.text(Style { bold: true, ..self.theme.hit }, "•");
                } else {
                    p.text(style, " ");
                }
                p.text(style, &name);
                p.text(style, &" ".repeat(name_w.saturating_sub(name.width())));
                let size = if e.is_dir { "<DIR>".to_string() } else { human_size(e.size) };
                let date = e.modified.map(format_time).unwrap_or_default();
                let meta_style = if i == self.sel { self.theme.select } else { self.theme.gutter };
                p.text(meta_style, &format!(" {size:>7}  {date:<16}"));
                if perms {
                    let mode = e.mode.map(crate::usage::perms).unwrap_or_default();
                    p.text(meta_style, &format!("  {mode:<9}"));
                }
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
            // Recent directories in full; otherwise just the last
            // component, since the input shows the rest.
            let name = if g.showing_recent { m.trim_end_matches('/') } else { m.trim_end_matches('/').rsplit('/').next().unwrap_or(m) };
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
        if self.help.is_some() {
            p.text(self.theme.hint, " Up/Down/PgUp/PgDn scroll · any other key returns");
            return p.finish(out, y, self.theme.text);
        }
        if let Some(f) = &self.find {
            let label = " Find: ";
            let room = self.width.saturating_sub(label.width() + 1);
            let shown = truncate_left(&f.input, room);
            p.text(Style { bold: true, ..self.theme.text }, label);
            p.text(self.theme.text, &shown);
            let hint = match &self.message {
                Some((m, _)) => format!("   {m}"),
                None if f.input.is_empty() => "   words to look for in paths  ↑/↓ pick  Enter opens  Esc returns".into(),
                None if f.total == 0 && !f.walking => "   (no matches)".into(),
                None => String::new(),
            };
            p.text(self.theme.hint, &hint);
            let cx = (label.width() + shown.width()).min(self.width.saturating_sub(1));
            p.finish(out, y, self.theme.text)?;
            return queue!(out, cursor::MoveTo(cx as u16, y as u16), cursor::Show);
        }
        if let Some(g) = &self.goto {
            let label = " Go to directory: ";
            let hint = match (g.input.is_empty(), g.showing_recent) {
                (true, true) => "   ↑/↓ recent directories  Tab completes  Esc cancels",
                (true, false) => "   Tab completes  ~ home  Esc cancels",
                _ => "",
            };
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
                // Columns hide sizes, dates, permissions and owners, so show them
                // for the selection.
                let mut info = String::new();
                if let Some(e) = self.entries.get(self.sel) {
                    let size = if e.is_dir { "<DIR>".to_string() } else { format!("{} bytes", e.size) };
                    let date = e.modified.map(format_time).unwrap_or_default();
                    let mode = e.mode.map(|m| format!("  {}", crate::usage::mode_string(m))).unwrap_or_default();
                    let owner = owner_text(e.owner);
                    let owner = if owner.is_empty() { owner } else { format!("  {owner}") };
                    info = format!(" {size}  {date}{mode}{owner}");
                    p.text(self.theme.text, &info);
                }
                // The details come first; the key hints shrink to fit after them.
                let room = self.width.saturating_sub(info.width());
                let hint = ["   h Help  F4 Details  Q Quit", "   h Help"].into_iter().find(|h| h.width() <= room);
                p.text(self.theme.hint, hint.unwrap_or(""));
            }
            None => p.text(
                self.theme.hint,
                " h Help  Enter View  / Jump  J Go to  F Find  - Back  Space Tag  E Edit  U Usage  Q Quit",
            ),
        }
        p.finish(out, y, self.theme.text)
    }
}

#[cfg(unix)]
fn unix_mode(meta: &fs::Metadata) -> Option<u32> {
    use std::os::unix::fs::PermissionsExt;
    Some(meta.permissions().mode())
}

#[cfg(not(unix))]
fn unix_mode(_: &fs::Metadata) -> Option<u32> {
    None
}

#[cfg(unix)]
fn unix_owner(meta: &fs::Metadata) -> Option<(u32, u32)> {
    use std::os::unix::fs::MetadataExt;
    Some((meta.uid(), meta.gid()))
}

#[cfg(not(unix))]
fn unix_owner(_: &fs::Metadata) -> Option<(u32, u32)> {
    None
}

/// `owner:group` for the status line; empty where there's no such thing.
fn owner_text(owner: Option<(u32, u32)>) -> String {
    #[cfg(unix)]
    if let Some((uid, gid)) = owner {
        return crate::owners::owner_group(uid, gid);
    }
    let _ = owner;
    String::new()
}

pub(crate) fn human_size(n: u64) -> String {
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
        assert_eq!(b.dir, crate::paths::canonicalize(&dir.join("alpha/beta")).unwrap());

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

    fn press(b: &mut Browser, s: &str) {
        for c in s.chars() {
            b.on_key(KeyEvent::new(KeyCode::Char(c), KeyModifiers::NONE));
        }
    }

    fn code(b: &mut Browser, c: KeyCode) -> Action {
        b.on_key(KeyEvent::new(c, KeyModifiers::NONE))
    }

    #[test]
    fn f_finds_files_below_and_esc_returns() {
        let dir = std::env::temp_dir().join(format!("lyst-browser-find-{}", std::process::id()));
        let _ = fs::remove_dir_all(&dir);
        fs::create_dir_all(dir.join("a/b")).unwrap();
        fs::write(dir.join("a/b/notes.md"), "x").unwrap();
        fs::write(dir.join("a/other.md"), "x").unwrap();
        fs::write(dir.join("top.txt"), "x").unwrap();
        let mut b = Browser::new(&dir, false, Prefs::new(false)).unwrap();
        let before = labels(&b);

        press(&mut b, "f");
        let deadline = Instant::now() + Duration::from_secs(10);
        while b.find.as_ref().unwrap().walking {
            assert!(Instant::now() < deadline);
            std::thread::sleep(Duration::from_millis(5));
            b.tick();
        }
        assert_eq!(b.entries.len(), 5, "everything, while nothing is typed: {:?}", labels(&b));
        press(&mut b, "B NOTE");
        let sep = std::path::MAIN_SEPARATOR;
        assert_eq!(labels(&b), [format!("a{sep}b{sep}notes.md")]);
        // Enter opens it; the results stay for when the viewer closes.
        assert!(matches!(code(&mut b, KeyCode::Enter), Action::Open));
        let (files, idx) = b.file_list();
        assert_eq!(files[idx], crate::paths::canonicalize(&dir.join("a/b/notes.md")).unwrap());
        b.refresh();
        assert_eq!(b.entries.len(), 1);

        // Esc goes back to the listing as it was.
        code(&mut b, KeyCode::Esc);
        assert!(b.find.is_none());
        assert_eq!(labels(&b), before);

        // Entering a found directory leaves the find there.
        press(&mut b, "f");
        while b.find.as_ref().unwrap().walking {
            std::thread::sleep(Duration::from_millis(5));
            b.tick();
        }
        press(&mut b, "a");
        b.sel = b.entries.iter().position(|e| e.name == "a").unwrap();
        code(&mut b, KeyCode::Enter);
        assert!(b.find.is_none());
        assert!(b.dir.ends_with("a"));
        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn space_tags_files_to_view_together() {
        let dir = fixture("tags");
        let mut b = Browser::new(&dir, false, Prefs::new(false)).unwrap();
        let names = |b: &Browser, v: &[PathBuf]| -> Vec<String> {
            let _ = b;
            v.iter().map(|p| p.file_name().unwrap().to_string_lossy().into_owned()).collect()
        };
        // Directories can't be tagged.
        b.sel = b.entries.iter().position(|e| e.is_dir && e.name != "..").unwrap();
        press(&mut b, " ");
        assert!(b.tags.is_empty() && b.message.as_ref().is_some_and(|(_, e)| *e));
        let files: Vec<usize> = (0..b.entries.len()).filter(|&i| !b.entries[i].is_dir).collect();
        b.sel = files[2];
        press(&mut b, " ");
        b.sel = files[0];
        press(&mut b, " ");
        assert_eq!(b.sel, files[0] + 1, "moves down after tagging");
        let (list, idx) = b.file_list();
        assert_eq!(list.len(), 2);
        assert_eq!(names(&b, &list), [b.entries[files[2]].name.clone(), b.entries[files[0]].name.clone()]);
        assert_eq!(idx, 0, "the selection isn't tagged: start at the first tagged file");
        b.sel = files[0];
        assert_eq!(b.file_list().1, 1);
        // Untag, then Esc clears the rest before it quits.
        press(&mut b, " ");
        assert_eq!(b.tags.len(), 1);
        assert!(matches!(code(&mut b, KeyCode::Esc), Action::None));
        assert!(b.tags.is_empty());
        assert!(matches!(code(&mut b, KeyCode::Esc), Action::Quit));
        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn minus_goes_back_and_j_lists_recent_directories() {
        let dir = std::env::temp_dir().join(format!("lyst-browser-back-{}", std::process::id()));
        let _ = fs::remove_dir_all(&dir);
        fs::create_dir_all(dir.join("one/two")).unwrap();
        let dir = crate::paths::canonicalize(&dir).unwrap();
        let mut b = Browser::new(&dir, false, Prefs::new(false)).unwrap();
        press(&mut b, "-");
        assert!(b.message.as_ref().is_some_and(|(m, _)| m.contains("No previous")));
        b.chdir(dir.join("one/two"), None);
        press(&mut b, "-");
        assert_eq!(b.dir, dir);
        press(&mut b, "-");
        assert_eq!(b.dir, dir.join("one/two"), "again returns");
        // The empty prompt offers the other directories, most recent first.
        press(&mut b, "j");
        let g = b.goto.as_ref().unwrap();
        assert!(g.showing_recent);
        assert_eq!(g.matches[0], crate::history::display(&dir));
        code(&mut b, KeyCode::Down);
        code(&mut b, KeyCode::Enter);
        assert_eq!(b.dir, dir);
        let _ = fs::remove_dir_all(&dir);
    }

    /// The screen as plain text, escape sequences removed.
    fn screen(b: &mut Browser) -> String {
        let mut out = Vec::new();
        b.draw(&mut out).unwrap();
        let raw = String::from_utf8_lossy(&out);
        let re = regex::Regex::new(r"\x1b\[[0-9;?]*[A-Za-z]").unwrap();
        re.replace_all(&raw, "").into_owned()
    }

    #[test]
    fn header_totals_file_sizes_and_unix_shows_permissions() {
        let dir = std::env::temp_dir().join(format!("lyst-browser-perms-{}", std::process::id()));
        let _ = fs::remove_dir_all(&dir);
        fs::create_dir_all(dir.join("sub")).unwrap();
        fs::write(dir.join("a.txt"), vec![b'x'; 600]).unwrap();
        fs::write(dir.join("b.txt"), vec![b'y'; 400]).unwrap();
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            fs::set_permissions(dir.join("a.txt"), fs::Permissions::from_mode(0o640)).unwrap();
        }
        let mut b = Browser::new(&dir, false, Prefs::new(false)).unwrap();
        b.resize(80, 8);
        let s = screen(&mut b);
        assert!(s.contains("1 dirs  2 files  1000 bytes"), "{s}");
        #[cfg(unix)]
        assert!(s.contains("rw-r-----"), "{s}");
        #[cfg(not(unix))]
        assert!(!s.contains("rw-"), "{s}");
        // Narrow screens leave the column out to keep names readable.
        b.resize(60, 8);
        assert!(!screen(&mut b).contains("rw-r-----"));

        // The wide view puts it on the bottom line for the selection.
        b.resize(80, 8);
        b.wide = true;
        b.select_name("a.txt");
        let s = screen(&mut b);
        #[cfg(unix)]
        {
            use std::os::unix::fs::MetadataExt;
            let m = fs::metadata(dir.join("a.txt")).unwrap();
            let owner = crate::owners::owner_group(m.uid(), m.gid());
            assert!(s.contains(&format!(" 600 bytes  {}", format_time(m.modified().unwrap()))) && s.contains(&format!("rw-r----- (640)  {owner}")), "{s}");
        }
        #[cfg(not(unix))]
        assert!(s.contains(" 600 bytes  ") && !s.contains("(640)"), "{s}");
        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn hash_asks_for_a_shell_here() {
        let mut b = Browser::new(Path::new("samples"), false, Prefs::new(false)).unwrap();
        press(&mut b, "#");
        assert_eq!(b.want_shell.take(), Some(b.dir.clone()));
        b.shell_done(Ok(std::process::ExitStatus::default()));
        assert!(b.message.is_none());
    }

    #[test]
    fn h_shows_help_and_y_copies_the_path() {
        let mut b = Browser::new(Path::new("samples"), false, Prefs::new(false)).unwrap();
        b.resize(80, 20);
        press(&mut b, "h");
        let mut out = Vec::new();
        b.draw(&mut out).unwrap();
        assert!(String::from_utf8_lossy(&out).contains("lyst file browser"));
        // The wheel scrolls it (not past the end); a click closes it.
        let mouse = |kind| MouseEvent { kind, column: 0, row: 0, modifiers: KeyModifiers::NONE };
        b.resize(80, 10);
        b.on_mouse(mouse(MouseEventKind::ScrollDown));
        assert_eq!(b.help, Some(3));
        for _ in 0..50 {
            b.on_mouse(mouse(MouseEventKind::ScrollDown));
        }
        assert_eq!(b.help, Some(HELP.lines().count() - 8));
        b.on_mouse(mouse(MouseEventKind::ScrollUp));
        assert_eq!(b.help, Some(HELP.lines().count() - 11));
        b.on_mouse(mouse(MouseEventKind::Down(MouseButton::Left)));
        assert!(b.help.is_none());
        b.resize(80, 20);
        press(&mut b, "h");
        press(&mut b, "x");
        assert!(b.help.is_none(), "any other key closes it");
        press(&mut b, "y");
        let path = b.entries[b.sel].path.display().to_string();
        let mut out = Vec::new();
        b.draw(&mut out).unwrap();
        assert!(String::from_utf8_lossy(&out).contains(&crate::external::osc52(&path)));
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
        assert!(line.contains(" on disk (5000 in 2 files, 1 subdirectory)"), "{line}");
        assert!(!line.contains("sub/"), "the selection is already highlighted: {line}");
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
