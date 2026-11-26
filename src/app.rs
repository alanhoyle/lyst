//! Viewer state, key handling and rendering.

use crate::doc::Document;
use crate::encoding::{self, AnsiStyle, Cell, CellKind, Encoding};
use crate::highlight::{self, DocHighlighter, HlConfig, Span};
use crate::search::{Match, Search};
use crate::table::{Format, RowKind, TCell, Table};
use crossterm::event::{KeyCode, KeyEvent, KeyModifiers, MouseButton, MouseEvent, MouseEventKind};
use crossterm::style::{Attribute, Color, Print, SetAttribute, SetBackgroundColor, SetForegroundColor};
use crossterm::{cursor, queue, terminal};
use std::collections::HashMap;
use std::io::{self, Read, Write};
use std::path::PathBuf;
use unicode_width::UnicodeWidthStr;

pub const HELP: &str = "\
lyst - a modern take on Vernon Buerg's LIST

MOVING AROUND
  Up / Down, j / k, Enter   scroll one line
  PgUp / PgDn, b / Space    scroll one page
  Home / End, g / G         top / bottom of file
  Left / Right              scroll sideways 8 columns (Shift: 40)
  L or :                    go to line  (123, 50%, or 0x1F00 offset)

SEARCHING
  F or /                    find forward
  \\                         find backward
  A or n                    find again, same direction
  N                         find again, opposite direction
  R                         toggle regex vs literal search
  In the prompt: Up/Down recall history, Ctrl-R toggles regex.
  Searches ignore case unless the pattern contains a capital letter.

DISPLAY
  w                         toggle line wrap
  H                         toggle hex dump
  E                         cycle encoding: UTF-8, CP437, Latin-1, 7-bit
  #                         toggle line numbers
  S                         toggle syntax highlighting
  C                         toggle ANSI color escapes (auto-detected)
  T                         follow mode: keep reading as the file grows
  D                         table view for CSV/TSV (on by default for .csv,
                            .tsv, .vcf, .bed, .gtf, .gff, .sam); in it,
                            Left/Right move by column and S toggles colors

MARKS AND FILES
  m <letter>                set a mark at the top line
  ' <letter>                jump to a mark
  [ / ]                     previous / next file
  O                         open this file in its default application
  W                         open this file in the default web browser
  V                         edit this file in $VISUAL or $EDITOR, then reload
  B                         open the file browser at this file's directory

MOUSE
  Wheel                     scroll (sideways swipe, or Shift/Option/Ctrl+wheel:
                            horizontal, when wrap is off)
  F9                        turn mouse support off/on; off lets the
                            terminal select text for copying
  In the file browser: click selects, double-click opens, right-click
  goes up a directory, the wheel moves the selection.

OTHER
  F1 or ?                   this help
  Ctrl-L                    redraw the screen
  X or Esc                  close (back to the file browser, if open)
  Q                         quit
";

#[derive(Clone, Copy, PartialEq, Eq)]
pub(crate) struct Style {
    pub fg: Color,
    pub bg: Color,
    pub bold: bool,
    pub italic: bool,
    pub underline: bool,
    pub dim: bool,
    pub reverse: bool,
}

pub(crate) const fn st(fg: Color, bg: Color) -> Style {
    Style { fg, bg, bold: false, italic: false, underline: false, dim: false, reverse: false }
}

pub(crate) struct Theme {
    pub text: Style,
    pub special: Style,
    pub gutter: Style,
    pub status: Style,
    pub hint: Style,
    pub error: Style,
    pub hit: Style,
    pub current: Style,
    pub select: Style,
    pub dir: Style,
    /// Text colors cycled across table columns.
    pub columns: [Color; 6],
}

impl Theme {
    pub fn new(classic: bool) -> Self {
        if classic { Self::classic() } else { Self::modern() }
    }

    fn modern() -> Self {
        Theme {
            text: st(Color::Reset, Color::Reset),
            special: st(Color::DarkCyan, Color::Reset),
            gutter: st(Color::DarkGrey, Color::Reset),
            status: st(Color::Black, Color::Cyan),
            hint: st(Color::DarkGrey, Color::Reset),
            error: Style { bold: true, ..st(Color::Red, Color::Reset) },
            hit: st(Color::Black, Color::DarkYellow),
            current: st(Color::Black, Color::Yellow),
            select: st(Color::Black, Color::Grey),
            dir: Style { bold: true, ..st(Color::Cyan, Color::Reset) },
            columns: [Color::Reset, Color::DarkCyan, Color::DarkGreen, Color::DarkYellow, Color::DarkMagenta, Color::Blue],
        }
    }

    /// White on blue, like the DOS original.
    fn classic() -> Self {
        Theme {
            text: st(Color::Grey, Color::DarkBlue),
            special: st(Color::Yellow, Color::DarkBlue),
            gutter: st(Color::Cyan, Color::DarkBlue),
            status: st(Color::Black, Color::DarkCyan),
            hint: st(Color::Black, Color::DarkCyan),
            error: Style { bold: true, ..st(Color::White, Color::DarkRed) },
            hit: st(Color::Black, Color::Grey),
            current: st(Color::Black, Color::Yellow),
            select: st(Color::DarkBlue, Color::Grey),
            dir: Style { bold: true, ..st(Color::White, Color::DarkBlue) },
            columns: [Color::Grey, Color::Yellow, Color::Cyan, Color::Green, Color::Magenta, Color::White],
        }
    }
}

#[derive(Clone)]
pub struct Options {
    pub wrap: bool,
    pub hex: bool,
    pub numbers: bool,
    pub tab: usize,
    pub encoding: Encoding,
    pub follow: bool,
    pub classic: bool,
    pub regex: bool,
    pub highlight: HlConfig,
    /// Interpret ANSI color escapes when a file contains them.
    pub ansi: bool,
}

pub enum Source {
    Stdin,
    Path(PathBuf),
}

impl Source {
    pub fn load(&self) -> io::Result<Document> {
        match self {
            Source::Stdin => {
                let mut v = Vec::new();
                io::stdin().lock().read_to_end(&mut v)?;
                let (v, filter) = crate::preprocess::stdin(v)?;
                Ok(Document::from_bytes("<stdin>", v).filtered(filter))
            }
            Source::Path(p) if crate::table::reads_directly(&p.to_string_lossy()) => Document::open(p),
            Source::Path(p) => match crate::preprocess::file(p)? {
                Some((bytes, filter)) => {
                    // Preprocessors such as lesspipe pass plain text through
                    // unchanged, or only add colors (lesspipe runs bat when
                    // LESS has -R). Use the original file then, so it stays
                    // memory-mapped and keeps follow mode and our own syntax
                    // highlighting, which S can toggle.
                    if let Ok(orig) = Document::open(p)
                        && (orig.bytes() == bytes.as_slice()
                            || (memchr::memchr(0x1b, &bytes).is_some()
                                && encoding::strip_ansi(&bytes) == orig.bytes()))
                    {
                        return Ok(orig);
                    }
                    Ok(Document::from_bytes(p.display().to_string(), bytes).filtered(Some(filter)))
                }
                None => Document::open(p),
            },
        }
    }
}

/// Per-file scroll position.
#[derive(Default)]
struct View {
    top_line: usize,
    /// Wrapped row within `top_line` that is at the top of the screen.
    top_sub: usize,
    hscroll: usize,
    hex: bool,
    hex_top: usize,
    cur_match: Option<Match>,
    marks: HashMap<char, usize>,
    /// Last line drawn on the previous frame.
    bottom_line: usize,
    /// Syntax highlighter, if a syntax was recognized for this file.
    hl: Option<DocHighlighter>,
    /// Interpret ANSI escape sequences as colors.
    ansi: bool,
    /// Show delimited text as aligned columns.
    table: Option<Table>,
}

impl View {
    fn new(doc: &Document, opts: &Options) -> Self {
        let hex = opts.hex || doc.looks_binary();
        let ansi = opts.ansi && doc.has_ansi();
        // A preprocessor may have laid the table out already (lesspipe runs
        // csvlook on .csv.gz); only add our own if the separator survived.
        let named = Format::for_name(&doc.name)
            .filter(|f| doc.filter.is_none() || Format::sniff(doc).is_some_and(|s| s.same_separator(f)));
        let table = match named {
            Some(f) if !hex => Some(Table::new(doc, f, &|b: &[u8], max| {
                encoding::layout(b, opts.encoding, opts.tab, max, ansi)
            })),
            _ => None,
        };
        View { hex, hl: DocHighlighter::detect(doc, &opts.highlight), ansi, table, ..View::default() }
    }
}

struct Slot {
    source: Source,
    parked: Option<(Document, View)>,
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum Dir {
    Forward,
    Backward,
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum PromptKind {
    Find(Dir),
    Goto,
}

enum Mode {
    Normal,
    Prompt { kind: PromptKind, input: String, hist: Option<usize> },
    Help { scroll: usize },
    Mark { set: bool },
}

pub struct App {
    slots: Vec<Slot>,
    cur: usize,
    doc: Document,
    view: View,
    opts: Options,
    theme: Theme,
    search: Option<Search>,
    last_dir: Dir,
    history: Vec<String>,
    mode: Mode,
    message: Option<(String, bool)>,
    width: usize,
    height: usize,
    /// line index -> wrapped row count, for the current width/encoding.
    wrap_cache: HashMap<usize, usize>,
    pub quit: bool,
    /// Quit the whole program, not just return to the file browser.
    pub quit_all: bool,
    /// Set when the user asks for the file browser: (directory, file to select).
    pub want_browser: Option<(PathBuf, Option<String>)>,
    /// Set when the user asks to edit this file; the caller runs the editor.
    pub want_edit: Option<PathBuf>,
    /// A binary file the user was warned about; editing it again goes ahead.
    edit_warned: Option<PathBuf>,
    /// Launched from the file browser, so exiting returns there.
    pub from_browser: bool,
    pub full_redraw: bool,
}

impl App {
    pub fn new(sources: Vec<Source>, first: usize, doc: Document, opts: Options) -> Self {
        let (w, h) = terminal::size().unwrap_or((80, 25));
        let theme = Theme::new(opts.classic);
        let slots: Vec<Slot> = sources.into_iter().map(|source| Slot { source, parked: None }).collect();
        let mut app = App {
            view: View::default(),
            slots,
            cur: first,
            doc,
            opts,
            theme,
            search: None,
            last_dir: Dir::Forward,
            history: Vec::new(),
            mode: Mode::Normal,
            message: None,
            width: w as usize,
            height: h as usize,
            wrap_cache: HashMap::new(),
            quit: false,
            quit_all: false,
            want_browser: None,
            want_edit: None,
            edit_warned: None,
            from_browser: false,
            full_redraw: false,
        };
        app.view = app.fresh_view();
        if app.view.hex && !app.opts.hex {
            app.flash("Binary file: showing hex (H to toggle)");
        }
        if app.opts.follow {
            app.go_end();
        }
        app
    }

    fn fresh_view(&self) -> View {
        View::new(&self.doc, &self.opts)
    }

    pub fn options(&self) -> &Options {
        &self.opts
    }

    /// Path of the file being viewed, if it is a real file.
    pub fn current_path(&self) -> Option<&std::path::Path> {
        match &self.slots[self.cur].source {
            Source::Path(p) => Some(p),
            Source::Stdin => None,
        }
    }

    fn open_browser(&mut self) {
        let (dir, name) = match self.current_path() {
            Some(p) => {
                let p = std::fs::canonicalize(p).unwrap_or_else(|_| p.to_path_buf());
                let name = p.file_name().map(|n| n.to_string_lossy().into_owned());
                (p.parent().map(|d| d.to_path_buf()).unwrap_or_else(|| ".".into()), name)
            }
            None => (".".into(), None),
        };
        self.want_browser = Some((dir, name));
        self.quit = true;
    }

    /// Opens the current file in the OS's default application.
    fn open_external(&mut self) {
        let Some(p) = self.current_path().map(std::path::Path::to_path_buf) else {
            return self.error("Standard input has no file to open in another application");
        };
        let name = p.file_name().map_or_else(|| p.display().to_string(), |n| n.to_string_lossy().into_owned());
        match crate::external::open(&p) {
            Ok(()) => self.flash(format!("Opening {name} in its default application")),
            Err(e) => self.error(format!("Can't open {name}: {e}")),
        }
    }

    /// Opens this file in the default web browser (or `$BROWSER`).
    fn open_web(&mut self) {
        let Some(p) = self.current_path().map(std::path::Path::to_path_buf) else {
            return self.error("Standard input has no file to open in a web browser");
        };
        let name = p.file_name().map_or_else(|| p.display().to_string(), |n| n.to_string_lossy().into_owned());
        match crate::external::open_in_browser(&p) {
            Ok(()) => self.flash(format!("Opening {name} in the web browser")),
            Err(e) => self.error(format!("Can't open {name} in a web browser: {e}")),
        }
    }

    /// Asks the caller to run the editor on this file, warning first (and
    /// needing a second press) if it doesn't look like text.
    fn request_edit(&mut self, warned: Option<PathBuf>) {
        let Some(p) = self.current_path().map(std::path::Path::to_path_buf) else {
            return self.error("Standard input has no file to edit");
        };
        let name = p.file_name().map_or_else(|| p.display().to_string(), |n| n.to_string_lossy().into_owned());
        match crate::external::looks_binary(&p) {
            Err(e) => self.error(format!("Can't edit {name}: {e}")),
            Ok(true) if warned.as_ref() != Some(&p) => {
                self.error(format!("{name} doesn't look like a text file; press V again to edit it anyway"));
                self.edit_warned = Some(p);
            }
            Ok(_) => self.want_edit = Some(p),
        }
    }

    /// After the editor exits: reloads the file, keeping the place.
    pub fn edited(&mut self, result: std::io::Result<std::process::ExitStatus>) {
        let editor = crate::external::editor();
        match result {
            Err(e) => return self.error(format!("Can't run editor \"{editor}\": {e} (set $VISUAL or $EDITOR)")),
            Ok(s) if !s.success() => self.error(format!("Editor \"{editor}\" exited with {s}")),
            Ok(_) => {}
        }
        self.full_redraw = true;
        let line = self.view.top_line + 1;
        match self.slots[self.cur].source.load() {
            Ok(doc) => {
                self.view = View::new(&doc, &self.opts);
                self.doc = doc;
                self.invalidate_layout();
                self.goto_line(line);
                if self.message.is_none() {
                    self.flash(format!("Reloaded {}", self.doc.name));
                }
            }
            Err(e) => self.error(format!("{}: {e}", self.doc.name)),
        }
    }

    /// Switches between table and plain text view.
    fn toggle_table(&mut self) {
        self.view.hscroll = 0;
        if self.view.table.take().is_some() {
            self.flash("Text view");
            return self.normalize();
        }
        let Some(format) = Format::for_name(&self.doc.name).or_else(|| Format::sniff(&self.doc)) else {
            return self.error("No columns found (looked for tab, comma, semicolon and | separators)");
        };
        self.view.hex = false;
        let t = Table::new(&self.doc, format, &|b, m| self.layout(b, m));
        self.flash(format!("Table view: {}", t.format.label));
        self.view.table = Some(t);
        self.normalize();
    }

    /// Scrolls the table one column left or right.
    fn column_step(&mut self, right: bool) {
        let Some(t) = &self.view.table else { return };
        let starts = t.column_starts();
        let h = self.view.hscroll;
        self.view.hscroll = if right {
            starts.iter().copied().find(|&x| x > h).unwrap_or(h)
        } else {
            starts.iter().copied().rev().find(|&x| x < h).unwrap_or(0)
        };
    }

    pub fn set_search(&mut self, pattern: &str) {
        match Search::new(pattern, self.opts.regex) {
            Ok(s) => {
                self.history.push(pattern.to_string());
                self.search = Some(s);
                self.find(Dir::Forward, false);
            }
            Err(e) => self.error(e),
        }
    }

    pub fn goto_line(&mut self, n: usize) {
        self.view.top_line = n.saturating_sub(1).min(self.doc.line_count() - 1);
        self.view.top_sub = 0;
    }

    // ---------------------------------------------------------------- geometry

    fn text_rows(&self) -> usize {
        self.height.saturating_sub(2).max(1)
    }

    fn gutter_width(&self) -> usize {
        if self.opts.numbers && !self.view.hex {
            self.doc.line_count().to_string().len() + 1
        } else {
            0
        }
    }

    fn text_width(&self) -> usize {
        self.width.saturating_sub(self.gutter_width()).max(1)
    }

    fn hex_rows(&self) -> usize {
        self.doc.len().div_ceil(16).max(1)
    }

    /// Whether long lines wrap. Never in table view, which scrolls by column.
    fn wrap(&self) -> bool {
        self.opts.wrap && self.view.table.is_none()
    }

    /// The table view's header is pinned to the top row once scrolled past.
    fn sticky_header(&self) -> Option<usize> {
        self.view.table.as_ref()?.header.filter(|&h| h < self.view.top_line)
    }

    fn layout(&self, line: &[u8], max_cols: usize) -> Vec<Cell> {
        encoding::layout(line, self.opts.encoding, self.opts.tab, max_cols, self.view.ansi)
    }

    fn row_count(&mut self, i: usize) -> usize {
        if let Some(&n) = self.wrap_cache.get(&i) {
            return n;
        }
        let cells = self.layout(self.doc.line(i).1, usize::MAX);
        let n = encoding::wrap_rows(&cells, self.text_width()).len();
        if self.wrap_cache.len() > 50_000 {
            self.wrap_cache.clear();
        }
        self.wrap_cache.insert(i, n);
        n
    }

    /// Topmost position that still fills the screen to the end of the file.
    fn end_position(&mut self) -> (usize, usize) {
        let rows = self.text_rows();
        if !self.wrap() {
            let count = self.doc.line_count();
            // Leave room for a pinned header so the last row stays visible.
            let pinned = self.view.table.as_ref().is_some_and(|t| t.header.is_some()) && count > rows;
            return (count.saturating_sub(rows - usize::from(pinned && rows > 1)), 0);
        }
        let mut need = rows;
        let mut line = self.doc.line_count() - 1;
        loop {
            let n = self.row_count(line);
            if n >= need {
                return (line, n - need);
            }
            need -= n;
            if line == 0 {
                return (0, 0);
            }
            line -= 1;
        }
    }

    /// Keeps the view position valid after any change.
    fn normalize(&mut self) {
        if self.view.hex {
            let max = self.hex_rows().saturating_sub(self.text_rows());
            self.view.hex_top = self.view.hex_top.min(max);
            return;
        }
        let last = self.doc.line_count() - 1;
        self.view.top_line = self.view.top_line.min(last);
        if !self.wrap() {
            self.view.top_sub = 0;
        } else {
            let rows = self.row_count(self.view.top_line);
            self.view.top_sub = self.view.top_sub.min(rows - 1);
        }
        let end = self.end_position();
        if (self.view.top_line, self.view.top_sub) > end {
            (self.view.top_line, self.view.top_sub) = end;
        }
    }

    fn invalidate_layout(&mut self) {
        self.wrap_cache.clear();
    }

    pub fn resize(&mut self, w: u16, h: u16) {
        self.width = w as usize;
        self.height = h as usize;
        self.invalidate_layout();
    }

    /// Byte range currently on screen.
    fn visible_range(&self) -> (usize, usize) {
        if self.view.hex {
            let start = self.view.hex_top * 16;
            let end = (start + self.text_rows() * 16).min(self.doc.len());
            (start, end)
        } else {
            let bottom = self.view.bottom_line.max(self.view.top_line);
            (self.doc.line_start(self.view.top_line), self.doc.line_end(bottom))
        }
    }

    fn top_offset(&self) -> usize {
        self.visible_range().0
    }

    fn jump_to_offset(&mut self, off: usize, context: usize) {
        if self.view.hex {
            self.view.hex_top = (off / 16).saturating_sub(context);
        } else {
            self.view.top_line = self.doc.line_of_offset(off).saturating_sub(context);
            self.view.top_sub = 0;
        }
    }

    // --------------------------------------------------------------- scrolling

    fn scroll_down(&mut self, mut n: usize) {
        if self.view.hex {
            self.view.hex_top += n;
        } else if !self.wrap() {
            self.view.top_line += n;
        } else {
            while n > 0 {
                let rows = self.row_count(self.view.top_line);
                let avail = rows - 1 - self.view.top_sub.min(rows - 1);
                if avail >= n {
                    self.view.top_sub += n;
                    break;
                }
                if self.view.top_line + 1 >= self.doc.line_count() {
                    break;
                }
                n -= avail + 1;
                self.view.top_line += 1;
                self.view.top_sub = 0;
            }
        }
        self.normalize();
    }

    fn scroll_up(&mut self, mut n: usize) {
        if self.view.hex {
            self.view.hex_top = self.view.hex_top.saturating_sub(n);
        } else if !self.wrap() {
            self.view.top_line = self.view.top_line.saturating_sub(n);
        } else {
            while n > 0 {
                if self.view.top_sub >= n {
                    self.view.top_sub -= n;
                    break;
                }
                n -= self.view.top_sub;
                self.view.top_sub = 0;
                if self.view.top_line == 0 {
                    break;
                }
                self.view.top_line -= 1;
                self.view.top_sub = self.row_count(self.view.top_line) - 1;
                n -= 1;
            }
        }
    }

    fn go_top(&mut self) {
        self.view.top_line = 0;
        self.view.top_sub = 0;
        self.view.hex_top = 0;
    }

    fn go_end(&mut self) {
        self.view.hex_top = usize::MAX;
        self.view.top_line = usize::MAX;
        self.normalize();
        if !self.view.hex {
            (self.view.top_line, self.view.top_sub) = self.end_position();
        }
    }

    // --------------------------------------------------------------- messages

    pub(crate) fn flash(&mut self, s: impl Into<String>) {
        self.message = Some((s.into(), false));
    }

    pub(crate) fn error(&mut self, s: impl Into<String>) {
        self.message = Some((s.into(), true));
    }

    // ------------------------------------------------------------------ files

    fn switch_file(&mut self, idx: usize) {
        if idx == self.cur || idx >= self.slots.len() {
            return;
        }
        let (doc, view) = match self.slots[idx].parked.take() {
            Some(p) => p,
            None => match self.slots[idx].source.load() {
                Ok(d) => {
                    let v = View::new(&d, &self.opts);
                    (d, v)
                }
                Err(e) => {
                    let name = match &self.slots[idx].source {
                        Source::Stdin => "<stdin>".to_string(),
                        Source::Path(p) => p.display().to_string(),
                    };
                    self.error(format!("{name}: {e}"));
                    return;
                }
            },
        };
        let old_doc = std::mem::replace(&mut self.doc, doc);
        let old_view = std::mem::replace(&mut self.view, view);
        self.slots[self.cur].parked = Some((old_doc, old_view));
        self.cur = idx;
        self.invalidate_layout();
        self.flash(format!("File {} of {}: {}", idx + 1, self.slots.len(), self.doc.name));
    }

    /// Called periodically; reloads the file in follow mode.
    pub fn tick(&mut self) -> bool {
        if !self.opts.follow {
            return false;
        }
        match self.doc.reload() {
            Ok(false) => false,
            Ok(true) => {
                self.invalidate_layout();
                if let Some(h) = &mut self.view.hl {
                    h.reset();
                }
                self.go_end();
                true
            }
            Err(e) => {
                self.opts.follow = false;
                self.error(format!("Follow stopped: {e}"));
                true
            }
        }
    }

    // ----------------------------------------------------------------- search

    fn find(&mut self, dir: Dir, again: bool) {
        let Some(search) = &self.search else {
            self.error("No previous search (press F to find)");
            return;
        };
        self.last_dir = dir;
        let (vis_start, vis_end) = self.visible_range();
        let data = self.doc.bytes();
        let origin = match self.view.cur_match {
            Some((s, e)) if again && s >= vis_start && s < vis_end.max(vis_start + 1) => match dir {
                Dir::Forward if e > s => e,
                Dir::Forward => s + 1,
                Dir::Backward => s,
            },
            _ => match dir {
                Dir::Forward => vis_start,
                Dir::Backward => vis_end,
            },
        };
        let mut wrapped = false;
        let found = match dir {
            Dir::Forward => search.forward(data, origin).or_else(|| {
                wrapped = true;
                search.forward(data, 0)
            }),
            Dir::Backward => search.backward(&self.doc, origin).or_else(|| {
                wrapped = true;
                search.backward(&self.doc, data.len())
            }),
        };
        let Some(m) = found else {
            let p = search.pattern.clone();
            self.error(format!("Not found: {p}"));
            return;
        };
        if wrapped {
            self.flash(match dir {
                Dir::Forward => "Search wrapped to top",
                Dir::Backward => "Search wrapped to bottom",
            });
        }
        self.view.cur_match = Some(m);
        self.reveal(m.0);
    }

    /// Scrolls so that byte `off` is on screen.
    fn reveal(&mut self, off: usize) {
        let ctx = (self.text_rows() / 4).min(3);
        if self.view.hex {
            let row = off / 16;
            if row < self.view.hex_top || row >= self.view.hex_top + self.text_rows() {
                self.jump_to_offset(off, ctx);
            }
            self.normalize();
            return;
        }
        let line = self.doc.line_of_offset(off);
        if line < self.view.top_line || line > self.view.bottom_line || (line == self.view.top_line && self.view.top_sub > 0) {
            self.jump_to_offset(off, ctx);
        }
        if !self.wrap() {
            let (start, bytes) = self.doc.line(line);
            let rel = off - start;
            let cells: Vec<Cell> = match &self.view.table {
                Some(t) if t.kind(line, bytes) != RowKind::Meta => {
                    t.row_cells(bytes, &|b, m| self.layout(b, m)).into_iter().map(|c| c.cell).collect()
                }
                _ => self.layout(bytes, usize::MAX),
            };
            let col: usize = cells.iter().take_while(|c| c.byte < rel).map(|c| c.width as usize).sum();
            let w = self.text_width();
            if col < self.view.hscroll || col + 1 > self.view.hscroll + w {
                self.view.hscroll = if col < w * 2 / 3 { 0 } else { col - w / 3 };
            }
        }
        self.normalize();
    }

    fn goto(&mut self, input: &str) {
        let s = input.trim();
        let hex = s.strip_prefix("0x").or_else(|| s.strip_prefix("0X"));
        if let Some(h) = hex {
            match usize::from_str_radix(h, 16) {
                Ok(off) => self.jump_to_offset(off.min(self.doc.len()), 0),
                Err(_) => return self.error(format!("Bad offset: {s}")),
            }
        } else if let Some(p) = s.strip_suffix('%') {
            match p.trim().parse::<f64>() {
                Ok(p) => {
                    let p = p.clamp(0.0, 100.0) / 100.0;
                    if self.view.hex {
                        self.view.hex_top = ((self.hex_rows() as f64) * p) as usize;
                    } else {
                        self.goto_line(((self.doc.line_count() as f64) * p) as usize + 1);
                    }
                }
                Err(_) => return self.error(format!("Bad percentage: {s}")),
            }
        } else {
            match s.parse::<usize>() {
                Ok(n) if self.view.hex => self.jump_to_offset(n.min(self.doc.len()), 0),
                Ok(n) => self.goto_line(n),
                Err(_) => return self.error(format!("Not a line number: {s}")),
            }
        }
        self.normalize();
    }

    // ------------------------------------------------------------------- keys

    pub fn on_key(&mut self, k: KeyEvent) {
        self.message = None;
        let warned = self.edit_warned.take();
        if matches!(k.code, KeyCode::Char('v' | 'V')) && matches!(self.mode, Mode::Normal) {
            return self.request_edit(warned);
        }
        match std::mem::replace(&mut self.mode, Mode::Normal) {
            Mode::Normal => self.key_normal(k),
            Mode::Help { scroll } => self.key_help(k, scroll),
            Mode::Mark { set } => self.key_mark(k, set),
            Mode::Prompt { kind, input, hist } => self.key_prompt(k, kind, input, hist),
        }
    }

    fn key_normal(&mut self, k: KeyEvent) {
        let ctrl = k.modifiers.contains(KeyModifiers::CONTROL);
        let shift = k.modifiers.contains(KeyModifiers::SHIFT);
        let page = self.text_rows().saturating_sub(1).max(1);
        if ctrl {
            match k.code {
                KeyCode::Char('c') => {
                    self.quit = true;
                    self.quit_all = true;
                }
                KeyCode::Char('l') => self.full_redraw = true,
                KeyCode::Char('f') => self.scroll_down(page),
                KeyCode::Char('b') => self.scroll_up(page),
                KeyCode::Char('d') => self.scroll_down(page / 2),
                KeyCode::Char('u') => self.scroll_up(page / 2),
                KeyCode::Home => self.go_top(),
                KeyCode::End => self.go_end(),
                KeyCode::Left => self.view.hscroll = 0,
                KeyCode::Right => self.view.hscroll += 40,
                _ => {}
            }
            return;
        }
        match k.code {
            KeyCode::Esc | KeyCode::Char('x' | 'X') => self.quit = true,
            KeyCode::Char('q' | 'Q') => {
                self.quit = true;
                self.quit_all = true;
            }
            KeyCode::Char('o' | 'O') => self.open_external(),
            KeyCode::Char('B') => self.open_browser(),
            KeyCode::Up | KeyCode::Char('k') => self.scroll_up(1),
            KeyCode::Down | KeyCode::Enter | KeyCode::Char('j') => self.scroll_down(1),
            KeyCode::PageUp | KeyCode::Char('b') => self.scroll_up(page),
            KeyCode::PageDown | KeyCode::Char(' ') => self.scroll_down(page),
            KeyCode::Home | KeyCode::Char('g') => self.go_top(),
            KeyCode::End | KeyCode::Char('G') => self.go_end(),
            KeyCode::Left if self.view.table.is_some() && !self.view.hex => self.column_step(false),
            KeyCode::Right if self.view.table.is_some() && !self.view.hex => self.column_step(true),
            KeyCode::Left if !self.wrap() && !self.view.hex => {
                self.view.hscroll = self.view.hscroll.saturating_sub(if shift { 40 } else { 8 })
            }
            KeyCode::Right if !self.wrap() && !self.view.hex => self.view.hscroll += if shift { 40 } else { 8 },
            KeyCode::Char('f' | 'F' | '/') => self.prompt(PromptKind::Find(Dir::Forward)),
            KeyCode::Char('\\') => self.prompt(PromptKind::Find(Dir::Backward)),
            KeyCode::Char('a' | 'A' | 'n') => self.find(self.last_dir, true),
            KeyCode::Char('N') => self.find(
                match self.last_dir {
                    Dir::Forward => Dir::Backward,
                    Dir::Backward => Dir::Forward,
                },
                true,
            ),
            KeyCode::Char('l' | 'L' | ':') => self.prompt(PromptKind::Goto),
            KeyCode::Char('W') => self.open_web(),
            KeyCode::Char('w') if self.view.table.is_some() && !self.view.hex => {
                self.flash("Table view doesn't wrap: ←/→ move by column (D for text view)")
            }
            KeyCode::Char('d' | 'D') => self.toggle_table(),
            KeyCode::Char('s' | 'S') if self.view.table.is_some() && !self.view.hex => {
                let t = self.view.table.as_mut().expect("table view");
                t.colors = !t.colors;
                let on = t.colors;
                self.flash(if on { "Column colors on" } else { "Column colors off" });
            }
            KeyCode::Char('w') => {
                self.opts.wrap = !self.opts.wrap;
                self.view.top_sub = 0;
                self.view.hscroll = 0;
                self.normalize();
                self.flash(if self.opts.wrap { "Wrap on" } else { "Wrap off" });
            }
            KeyCode::Char('h' | 'H') => self.toggle_hex(),
            KeyCode::Char('e' | 'E') => {
                self.opts.encoding = self.opts.encoding.next();
                self.invalidate_layout();
                self.normalize();
                self.flash(format!("Encoding: {}", self.opts.encoding.label()));
            }
            KeyCode::Char('#') => {
                self.opts.numbers = !self.opts.numbers;
                self.invalidate_layout();
                self.normalize();
            }
            KeyCode::Char('t' | 'T') => self.toggle_follow(),
            KeyCode::Char('c' | 'C') => {
                self.view.ansi = !self.view.ansi;
                self.invalidate_layout();
                self.normalize();
                self.flash(if self.view.ansi { "ANSI colors on" } else { "ANSI colors off: escapes shown raw" });
            }
            KeyCode::Char('s' | 'S') => {
                self.opts.highlight.enabled = !self.opts.highlight.enabled;
                let msg = match (&self.view.hl, self.opts.highlight.enabled) {
                    (_, false) => "Syntax highlighting off".to_string(),
                    (_, true) if self.view.ansi => {
                        "Syntax highlighting on, but this file's own ANSI colors are shown (C turns them off)".to_string()
                    }
                    (Some(h), true) => format!("Syntax highlighting on ({})", h.name()),
                    (None, true) => "Syntax highlighting on (no syntax recognized for this file)".to_string(),
                };
                self.flash(msg);
            }
            KeyCode::Char('r' | 'R') => {
                self.opts.regex = !self.opts.regex;
                self.flash(if self.opts.regex { "Regex search" } else { "Literal search" });
            }
            KeyCode::Char('m') => {
                self.mode = Mode::Mark { set: true };
                self.flash("Set mark: press a letter");
            }
            KeyCode::Char('\'') => {
                self.mode = Mode::Mark { set: false };
                self.flash("Jump to mark: press a letter");
            }
            KeyCode::Char('[') if self.cur > 0 => self.switch_file(self.cur - 1),
            KeyCode::Char(']') => self.switch_file(self.cur + 1),
            KeyCode::Char('[') => {}
            KeyCode::F(1) | KeyCode::Char('?') => self.mode = Mode::Help { scroll: 0 },
            _ => {}
        }
    }

    /// Handles a mouse event; returns false if it was ignored (no redraw needed).
    pub fn on_mouse(&mut self, m: MouseEvent) -> bool {
        // Terminals differ in which modifier they pass through with the wheel,
        // so any of them turns vertical wheel motion sideways.
        let sideways = m.modifiers.intersects(KeyModifiers::SHIFT | KeyModifiers::ALT | KeyModifiers::CONTROL);
        if let Mode::Help { scroll } = self.mode {
            let max = HELP.lines().count().saturating_sub(self.text_rows());
            self.mode = match m.kind {
                MouseEventKind::ScrollDown => Mode::Help { scroll: (scroll + 3).min(max) },
                MouseEventKind::ScrollUp => Mode::Help { scroll: scroll.saturating_sub(3) },
                MouseEventKind::Down(_) => Mode::Normal,
                _ => return false,
            };
            return true;
        }
        if !matches!(self.mode, Mode::Normal) {
            return false;
        }
        match m.kind {
            MouseEventKind::ScrollDown if sideways => self.hscroll_by(8),
            MouseEventKind::ScrollUp if sideways => self.hscroll_by(-8),
            MouseEventKind::ScrollRight => self.hscroll_by(8),
            MouseEventKind::ScrollLeft => self.hscroll_by(-8),
            MouseEventKind::ScrollDown => self.scroll_down(3),
            MouseEventKind::ScrollUp => self.scroll_up(3),
            MouseEventKind::Down(MouseButton::Left) => self.message = None,
            _ => return false,
        }
        true
    }

    fn hscroll_by(&mut self, delta: isize) {
        if !self.wrap() && !self.view.hex {
            self.view.hscroll = self.view.hscroll.saturating_add_signed(delta);
        }
    }

    fn toggle_hex(&mut self) {
        if self.view.hex {
            self.view.hex = false;
            self.view.top_line = self.doc.line_of_offset(self.view.hex_top * 16);
            self.view.top_sub = 0;
        } else {
            self.view.hex_top = self.doc.line_start(self.view.top_line) / 16;
            self.view.hex = true;
        }
        self.invalidate_layout();
        self.normalize();
    }

    fn toggle_follow(&mut self) {
        if matches!(self.slots[self.cur].source, Source::Stdin) {
            return self.error("Can't follow standard input");
        }
        if self.doc.filter.is_some() {
            return self.error("Can't follow preprocessed input");
        }
        self.opts.follow = !self.opts.follow;
        if self.opts.follow {
            self.tick();
            self.go_end();
            self.flash("Following file (T to stop)");
        } else {
            self.flash("Follow off");
        }
    }

    fn key_help(&mut self, k: KeyEvent, scroll: usize) {
        let max = HELP.lines().count().saturating_sub(self.text_rows());
        let page = self.text_rows().saturating_sub(1).max(1);
        let scroll = match k.code {
            KeyCode::Up | KeyCode::Char('k') => scroll.saturating_sub(1),
            KeyCode::Down | KeyCode::Char('j') => scroll + 1,
            KeyCode::PageUp | KeyCode::Char('b') => scroll.saturating_sub(page),
            KeyCode::PageDown | KeyCode::Char(' ') => scroll + page,
            _ => return,
        };
        self.mode = Mode::Help { scroll: scroll.min(max) };
    }

    fn key_mark(&mut self, k: KeyEvent, set: bool) {
        let KeyCode::Char(c) = k.code else { return };
        if !c.is_ascii_alphanumeric() {
            return;
        }
        if set {
            let off = self.top_offset();
            self.view.marks.insert(c, off);
            self.flash(format!("Mark '{c}' set"));
        } else if let Some(&off) = self.view.marks.get(&c) {
            self.jump_to_offset(off, 0);
            self.normalize();
        } else {
            self.error(format!("Mark '{c}' not set"));
        }
    }

    fn prompt(&mut self, kind: PromptKind) {
        self.mode = Mode::Prompt { kind, input: String::new(), hist: None };
    }

    fn key_prompt(&mut self, k: KeyEvent, kind: PromptKind, mut input: String, mut hist: Option<usize>) {
        let ctrl = k.modifiers.contains(KeyModifiers::CONTROL);
        match k.code {
            KeyCode::Esc => return,
            KeyCode::Char('c') if ctrl => return,
            KeyCode::Enter => return self.submit(kind, input),
            KeyCode::Char('u') if ctrl => input.clear(),
            KeyCode::Char('r') if ctrl => self.opts.regex = !self.opts.regex,
            KeyCode::Char(c) if !ctrl => input.push(c),
            KeyCode::Backspace => {
                input.pop();
            }
            KeyCode::Up if matches!(kind, PromptKind::Find(_)) && !self.history.is_empty() => {
                let i = hist.map_or(self.history.len() - 1, |i| i.saturating_sub(1));
                hist = Some(i);
                input = self.history[i].clone();
            }
            KeyCode::Down if hist.is_some() => {
                let i = hist.unwrap() + 1;
                if i < self.history.len() {
                    hist = Some(i);
                    input = self.history[i].clone();
                } else {
                    hist = None;
                    input.clear();
                }
            }
            _ => {}
        }
        self.mode = Mode::Prompt { kind, input, hist };
    }

    fn submit(&mut self, kind: PromptKind, input: String) {
        match kind {
            PromptKind::Goto if input.trim().is_empty() => {}
            PromptKind::Goto => self.goto(&input),
            PromptKind::Find(dir) => {
                let pattern = if input.is_empty() {
                    match self.history.last() {
                        Some(p) => p.clone(),
                        None => return,
                    }
                } else {
                    input
                };
                match Search::new(&pattern, self.opts.regex) {
                    Ok(s) => {
                        self.history.retain(|h| *h != pattern);
                        self.history.push(pattern);
                        self.search = Some(s);
                        self.view.cur_match = None;
                        self.find(dir, false);
                    }
                    Err(e) => self.error(format!("Bad pattern: {e}")),
                }
            }
        }
    }

    // ---------------------------------------------------------------- drawing

    pub fn draw(&mut self, out: &mut impl Write) -> io::Result<()> {
        queue!(out, terminal::BeginSynchronizedUpdate, cursor::Hide)?;
        if self.full_redraw {
            queue!(out, terminal::Clear(terminal::ClearType::All))?;
            self.full_redraw = false;
        }
        match self.mode {
            Mode::Help { scroll } => self.draw_help(out, scroll)?,
            _ if self.view.hex => self.draw_hex(out)?,
            _ if self.view.table.is_some() => {
                self.normalize();
                self.draw_table(out)?;
            }
            _ => {
                self.normalize();
                self.draw_text(out)?;
            }
        }
        self.draw_status(out)?;
        self.draw_bottom(out)?;
        queue!(out, terminal::EndSynchronizedUpdate)?;
        out.flush()
    }

    fn row_matches(&self, hay: &[u8], base: usize) -> Vec<(usize, usize, bool)> {
        let Some(s) = &self.search else { return Vec::new() };
        s.matches_in(hay)
            .into_iter()
            .map(|(a, b)| (a, b, self.view.cur_match == Some((base + a, base + b))))
            .collect()
    }

    fn syntax_style(&self, spans: &[Span], byte: usize) -> Style {
        match highlight::span_at(spans, byte) {
            Some(s) => Style {
                fg: s.fg.unwrap_or(self.theme.text.fg),
                bg: s.bg.unwrap_or(self.theme.text.bg),
                bold: s.bold,
                italic: s.italic,
                underline: s.underline,
                ..self.theme.text
            },
            None => self.theme.text,
        }
    }

    fn style_at(&self, matches: &[(usize, usize, bool)], byte: usize, base: Style) -> Style {
        match matches.iter().find(|&&(a, b, _)| byte >= a && byte < b) {
            Some(&(_, _, true)) => self.theme.current,
            Some(_) => self.theme.hit,
            None => base,
        }
    }

    fn draw_text(&mut self, out: &mut impl Write) -> io::Result<()> {
        let rows = self.text_rows();
        let gw = self.gutter_width();
        let tw = self.text_width();
        let hscroll = if self.wrap() { 0 } else { self.view.hscroll };
        let mut y = 0;
        let mut line = self.view.top_line;
        let mut bottom = line;
        while y < rows {
            if line >= self.doc.line_count() {
                let mut p = Painter::new(self.width);
                p.text(self.theme.gutter, "~");
                p.finish(out, y + 1, self.theme.text)?;
                y += 1;
                continue;
            }
            let (start, bytes) = self.doc.line(line);
            let max = if self.wrap() { usize::MAX } else { hscroll + tw };
            let cells = self.layout(bytes, max);
            let matches = self.row_matches(bytes, start);
            let spans: Vec<Span> = match &mut self.view.hl {
                Some(h) if self.opts.highlight.enabled && !self.view.ansi => h.spans(&self.doc, line),
                _ => Vec::new(),
            };
            let ranges = if self.wrap() { encoding::wrap_rows(&cells, tw) } else { vec![(0, cells.len())] };
            let skip = if line == self.view.top_line { self.view.top_sub } else { 0 };
            for (ri, &(a, b)) in ranges.iter().enumerate().skip(skip) {
                if y >= rows {
                    break;
                }
                let mut p = Painter::new(self.width);
                if gw > 0 {
                    let g = if ri == 0 { format!("{:>w$} ", line + 1, w = gw - 1) } else { " ".repeat(gw) };
                    p.text(self.theme.gutter, &g);
                }
                let mut col = 0;
                for c in &cells[a..b] {
                    let w = c.width as usize;
                    let cstart = col;
                    col += w;
                    if col <= hscroll {
                        continue;
                    }
                    let base = if c.kind == CellKind::Special {
                        self.theme.special
                    } else {
                        apply_ansi(self.syntax_style(&spans, c.byte), &c.ansi)
                    };
                    let style = self.style_at(&matches, c.byte, base);
                    if cstart < hscroll {
                        p.text(style, &" ".repeat(col - hscroll));
                    } else if !p.put(style, c.ch, w) {
                        break;
                    }
                }
                p.finish(out, y + 1, self.theme.text)?;
                y += 1;
            }
            bottom = line;
            line += 1;
        }
        self.view.bottom_line = bottom;
        Ok(())
    }

    fn draw_table(&mut self, out: &mut impl Write) -> io::Result<()> {
        let sticky = self.sticky_header();
        let Some(mut t) = self.view.table.take() else { return Ok(()) };
        let rows = self.text_rows();
        let count = self.doc.line_count();
        let top = self.view.top_line;
        let body = rows - usize::from(sticky.is_some() && rows > 1);
        // Widen columns for everything about to be shown, so this frame lines up.
        {
            let lay = |b: &[u8], m| self.layout(b, m);
            for i in sticky.into_iter().chain(top..(top + body).min(count)) {
                let bytes = self.doc.line(i).1;
                if t.kind(i, bytes) != RowKind::Meta {
                    t.measure(bytes, &lay);
                }
            }
        }
        let mut y = 0;
        if let Some(h) = sticky.filter(|_| rows > 1) {
            self.draw_table_row(out, &t, h, y)?;
            y += 1;
        }
        let mut line = top;
        while y < rows {
            if line >= count {
                let mut p = Painter::new(self.width);
                p.text(self.theme.gutter, "~");
                p.finish(out, y + 1, self.theme.text)?;
            } else {
                self.draw_table_row(out, &t, line, y)?;
                self.view.bottom_line = line;
                line += 1;
            }
            y += 1;
        }
        self.view.bottom_line = self.view.bottom_line.clamp(top, line.saturating_sub(1).max(top));
        self.view.table = Some(t);
        Ok(())
    }

    fn draw_table_row(&self, out: &mut impl Write, t: &Table, line: usize, y: usize) -> io::Result<()> {
        let gw = self.gutter_width();
        let hscroll = self.view.hscroll;
        let (start, bytes) = self.doc.line(line);
        let matches = self.row_matches(bytes, start);
        let kind = t.kind(line, bytes);
        let cells: Vec<TCell> = if kind == RowKind::Meta {
            let cells = self.layout(bytes, hscroll + self.text_width());
            cells.into_iter().map(|cell| TCell { cell, col: None }).collect()
        } else {
            t.row_cells(bytes, &|b, m| self.layout(b, m))
        };
        let mut p = Painter::new(self.width);
        if gw > 0 {
            p.text(self.theme.gutter, &format!("{:>w$} ", line + 1, w = gw - 1));
        }
        let mut col = 0;
        for tc in &cells {
            let c = &tc.cell;
            let w = c.width as usize;
            let cstart = col;
            col += w;
            if col <= hscroll {
                continue;
            }
            let base = match (kind, tc.col) {
                (RowKind::Meta, _) => self.theme.hint,
                _ if c.kind == CellKind::Special => self.theme.special,
                (_, None) => self.theme.gutter,
                (_, Some(k)) => {
                    let mut s = self.theme.text;
                    if t.colors {
                        s.fg = self.theme.columns[k % self.theme.columns.len()];
                    }
                    if kind == RowKind::Header {
                        s.bold = true;
                        s.underline = true;
                    }
                    apply_ansi(s, &c.ansi)
                }
            };
            let style = if tc.col.is_some() || kind == RowKind::Meta { self.style_at(&matches, c.byte, base) } else { base };
            if cstart < hscroll {
                p.text(style, &" ".repeat(col - hscroll));
            } else if !p.put(style, c.ch, w) {
                break;
            }
        }
        p.finish(out, y + 1, self.theme.text)
    }

    fn draw_hex(&mut self, out: &mut impl Write) -> io::Result<()> {
        self.normalize();
        let data = self.doc.bytes();
        let digits = if data.len() > 0xFFFF_FFFF { 12 } else { 8 };
        let (vs, ve) = self.visible_range();
        let matches = self.row_matches(&data[vs..ve], vs);
        let enc = self.opts.encoding;
        for y in 0..self.text_rows() {
            let off = (self.view.hex_top + y) * 16;
            let mut p = Painter::new(self.width);
            if off < data.len() || (off == 0 && y == 0) {
                p.text(self.theme.gutter, &format!("{off:0digits$X}  "));
                let row = &data[off..(off + 16).min(data.len())];
                for i in 0..16 {
                    let s = if i < row.len() {
                        self.style_at(&matches, off + i - vs, self.theme.text)
                    } else {
                        self.theme.text
                    };
                    let txt = row.get(i).map_or("  ".to_string(), |b| format!("{b:02x}"));
                    p.text(s, &txt);
                    p.text(self.theme.text, if i == 7 { "  " } else { " " });
                }
                p.text(self.theme.gutter, " │");
                for (i, &b) in row.iter().enumerate() {
                    let base = if hex_printable(b, enc) { self.theme.text } else { self.theme.special };
                    p.put(self.style_at(&matches, off + i - vs, base), encoding::hex_glyph(b, enc), 1);
                }
            }
            p.finish(out, y + 1, self.theme.text)?;
        }
        Ok(())
    }

    fn draw_help(&self, out: &mut impl Write, scroll: usize) -> io::Result<()> {
        let lines: Vec<&str> = HELP.lines().collect();
        for y in 0..self.text_rows() {
            let mut p = Painter::new(self.width);
            if let Some(l) = lines.get(scroll + y) {
                let style = if l.chars().next().is_some_and(|c| c.is_ascii_uppercase()) {
                    Style { bold: true, ..self.theme.text }
                } else {
                    self.theme.text
                };
                p.text(style, l);
            }
            p.finish(out, y + 1, self.theme.text)?;
        }
        Ok(())
    }

    fn draw_status(&self, out: &mut impl Write) -> io::Result<()> {
        let mut flags = vec![self.opts.encoding.label()];
        if self.view.hex {
            flags.push("HEX");
        } else if self.wrap() {
            flags.push("WRAP");
        }
        if self.opts.regex {
            flags.push("REGEX");
        }
        if let Some(t) = self.view.table.as_ref().filter(|_| !self.view.hex) {
            flags.insert(0, t.format.label);
        } else if let Some(h) = &self.view.hl
            && self.opts.highlight.enabled
            && !self.view.hex
            && !self.view.ansi
        {
            flags.insert(0, h.name());
        }
        if self.view.ansi && !self.view.hex {
            flags.push("ANSI");
        }
        if let Some(f) = &self.doc.filter {
            flags.insert(0, f);
        }
        if self.opts.follow {
            flags.push("FOLLOW");
        }
        let right = if matches!(self.mode, Mode::Help { .. }) {
            "Help ".to_string()
        } else if self.view.hex {
            let (_, end) = self.visible_range();
            let pct = if self.doc.len() == 0 { 100 } else { end * 100 / self.doc.len() };
            format!("Offset 0x{:X} of 0x{:X}  {pct:>3}%  {} ", self.view.hex_top * 16, self.doc.len(), flags.join(" "))
        } else {
            let count = self.doc.line_count();
            let pct = (self.view.bottom_line + 1) * 100 / count;
            let col = match &self.view.table {
                Some(t) => {
                    let starts = t.column_starts();
                    let k = starts.iter().filter(|&&x| x <= self.view.hscroll).count().max(1);
                    format!("Column {k} of {}  ", starts.len())
                }
                None if self.wrap() => String::new(),
                None => format!("Col {}  ", self.view.hscroll + 1),
            };
            format!("Line {} of {count}  {col}{pct:>3}%  {} ", self.view.top_line + 1, flags.join(" "))
        };
        let mut left = format!(" {}", self.doc.name);
        if self.slots.len() > 1 {
            left.push_str(&format!("  [{}/{}]", self.cur + 1, self.slots.len()));
        }
        let rw = right.width();
        let avail = self.width.saturating_sub(rw + 1);
        if left.width() > avail {
            // Keep the end of long paths, which is the informative part.
            let chars: Vec<char> = left.chars().collect();
            let keep = avail.saturating_sub(2);
            left = format!(" …{}", chars[chars.len().saturating_sub(keep)..].iter().collect::<String>());
        }
        let mut p = Painter::new(self.width);
        let s = Style { bold: true, ..self.theme.status };
        p.text(s, &left);
        let pad = self.width.saturating_sub(left.width() + rw);
        p.text(s, &" ".repeat(pad));
        p.text(self.theme.status, &right);
        p.finish(out, 0, self.theme.status)
    }

    fn draw_bottom(&self, out: &mut impl Write) -> io::Result<()> {
        let y = self.height.saturating_sub(1);
        let mut p = Painter::new(self.width);
        if let Mode::Prompt { kind, input, .. } = &self.mode {
            let label = match kind {
                PromptKind::Find(Dir::Forward) => "Find",
                PromptKind::Find(Dir::Backward) => "Find backward",
                PromptKind::Goto if self.view.hex => "Go to offset (dec, 0xHEX, N%)",
                PromptKind::Goto => "Go to line (N, N%, 0xOFFSET)",
            };
            let regex = if self.opts.regex && matches!(kind, PromptKind::Find(_)) { " [regex]" } else { "" };
            let label = format!("{label}{regex}: ");
            let room = self.width.saturating_sub(label.width() + 1);
            let shown: String = {
                let chars: Vec<char> = input.chars().collect();
                let mut start = chars.len();
                let mut w = 0;
                while start > 0 && w < room {
                    start -= 1;
                    w += unicode_width::UnicodeWidthChar::width(chars[start]).unwrap_or(0);
                }
                chars[start..].iter().collect()
            };
            p.text(Style { bold: true, ..self.theme.text }, &label);
            p.text(self.theme.text, &shown);
            let cx = (label.width() + shown.width()).min(self.width.saturating_sub(1));
            p.finish(out, y, self.theme.text)?;
            return queue!(out, cursor::MoveTo(cx as u16, y as u16), cursor::Show);
        }
        match &self.message {
            Some((m, true)) => p.text(self.theme.error, m),
            Some((m, false)) => p.text(self.theme.text, m),
            None if matches!(self.mode, Mode::Help { .. }) => {
                p.text(self.theme.hint, " Up/Down/PgUp/PgDn scroll · any other key returns")
            }
            None if self.view.table.is_some() && !self.view.hex => p.text(
                self.theme.hint,
                " F1 Help  F Find  L Line  ←/→ Column  S Colors  D Text view  O Open  X Exit",
            ),
            None => p.text(
                self.theme.hint,
                if self.from_browser {
                    " F1 Help  F Find  A Again  L Line  W Wrap  H Hex  O Open  X Back  Q Quit"
                } else {
                    " F1 Help  F Find  A Again  L Line  W Wrap  H Hex  O Open  B Browse  X Exit"
                },
            ),
        }
        p.finish(out, y, self.theme.text)
    }
}

fn apply_ansi(base: Style, a: &AnsiStyle) -> Style {
    if *a == AnsiStyle::default() {
        return base;
    }
    Style {
        fg: a.fg.unwrap_or(base.fg),
        bg: a.bg.unwrap_or(base.bg),
        bold: base.bold || a.bold,
        italic: base.italic || a.italic,
        underline: base.underline || a.underline,
        dim: base.dim || a.dim,
        reverse: base.reverse || a.reverse,
    }
}

fn hex_printable(b: u8, enc: Encoding) -> bool {
    match enc {
        Encoding::Cp437 => b != 0,
        Encoding::Ascii7 => (0x20..0x7f).contains(&(b & 0x7f)),
        _ => (0x20..0x7f).contains(&b),
    }
}

/// Accumulates one screen row of styled text, clipped to the screen width.
pub(crate) struct Painter {
    segs: Vec<(Style, String)>,
    col: usize,
    width: usize,
}

impl Painter {
    pub fn new(width: usize) -> Self {
        Painter { segs: Vec::new(), col: 0, width }
    }

    /// Adds one character of display width `w`; false if it doesn't fit.
    pub fn put(&mut self, style: Style, ch: char, w: usize) -> bool {
        if self.col + w > self.width {
            return false;
        }
        match self.segs.last_mut() {
            Some((s, text)) if *s == style => text.push(ch),
            _ => self.segs.push((style, ch.to_string())),
        }
        self.col += w;
        true
    }

    pub fn text(&mut self, style: Style, s: &str) {
        for ch in s.chars() {
            let w = unicode_width::UnicodeWidthChar::width(ch).unwrap_or(0);
            if w > 0 && !self.put(style, ch, w) {
                break;
            }
        }
    }

    pub fn finish(mut self, out: &mut impl Write, y: usize, fill: Style) -> io::Result<()> {
        let pad = self.width.saturating_sub(self.col);
        if pad > 0 {
            self.segs.push((fill, " ".repeat(pad)));
        }
        queue!(out, cursor::MoveTo(0, y as u16))?;
        for (s, text) in &self.segs {
            queue!(out, SetAttribute(Attribute::Reset))?;
            for (on, attr) in [
                (s.bold, Attribute::Bold),
                (s.dim, Attribute::Dim),
                (s.italic, Attribute::Italic),
                (s.underline, Attribute::Underlined),
                (s.reverse, Attribute::Reverse),
            ] {
                if on {
                    queue!(out, SetAttribute(attr))?;
                }
            }
            queue!(
                out,
                SetForegroundColor(s.fg),
                SetBackgroundColor(s.bg),
                Print(text)
            )?;
        }
        queue!(out, SetAttribute(Attribute::Reset))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn opts() -> Options {
        Options {
            wrap: false,
            hex: false,
            numbers: false,
            tab: 8,
            encoding: Encoding::Utf8,
            follow: false,
            classic: false,
            regex: false,
            highlight: HlConfig::new(false, None, None).unwrap(),
            ansi: true,
        }
    }

    fn app(bytes: &[u8], opts: Options) -> App {
        let doc = Document::from_bytes("t", bytes.to_vec());
        let mut a = App::new(vec![Source::Stdin], 0, doc, opts);
        a.resize(80, 25);
        a
    }

    fn numbered(n: usize) -> Vec<u8> {
        (1..=n).map(|i| format!("line {i}\n")).collect::<String>().into_bytes()
    }

    #[test]
    fn v_asks_to_edit_and_warns_about_binary_files() {
        let mut a = app(b"piped\n", opts());
        a.on_key(key('v'));
        assert!(matches!(&a.message, Some((m, true)) if m.contains("Standard input")));
        assert!(a.want_edit.is_none());

        let dir = std::env::temp_dir().join(format!("lyst-app-edit-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let text = dir.join("notes.txt");
        let bin = dir.join("blob.bin");
        std::fs::write(&text, numbered(100)).unwrap();
        std::fs::write(&bin, b"\x7fELF\0\0\x01").unwrap();
        let open = |p: &std::path::Path| {
            let src = Source::Path(p.to_path_buf());
            let doc = src.load().unwrap();
            let mut a = App::new(vec![src], 0, doc, opts());
            a.resize(80, 25);
            a
        };

        let mut a = open(&text);
        a.goto_line(40);
        a.on_key(key('v'));
        assert_eq!(a.want_edit.take().as_deref(), Some(text.as_path()));
        // The "editor" changes the file; the viewer reloads it in place.
        std::fs::write(&text, format!("new first line\n{}", String::from_utf8(numbered(100)).unwrap())).unwrap();
        a.edited(Ok(std::process::ExitStatus::default()));
        assert_eq!(a.doc.line_count(), 101);
        assert_eq!(a.view.top_line, 39, "same line number");
        assert!(matches!(&a.message, Some((m, false)) if m.contains("Reloaded")));

        let mut a = open(&bin);
        a.on_key(key('v'));
        assert!(a.want_edit.is_none());
        assert!(matches!(&a.message, Some((m, true)) if m.contains("doesn't look like a text file")));
        // Pressing it again edits anyway; any other key in between resets.
        a.on_key(key('V'));
        assert_eq!(a.want_edit.take().as_deref(), Some(bin.as_path()));
        a.on_key(key('v'));
        a.on_key(key('j'));
        a.on_key(key('v'));
        assert!(a.want_edit.is_none());

        // 'v' typed into a prompt is just text.
        let mut a = open(&text);
        a.on_key(key('f'));
        a.on_key(key('v'));
        assert!(a.want_edit.is_none());
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn binary_files_open_in_hex_with_a_notice() {
        let a = app(b"\x7fELF\0\0\x01", opts());
        assert!(a.view.hex);
        assert!(matches!(&a.message, Some((m, false)) if m.contains("Binary")));
        let a = app(b"text\n", opts());
        assert!(!a.view.hex && a.message.is_none());
    }

    #[test]
    fn goto_line_is_one_based_and_clamped() {
        let mut a = app(&numbered(10), opts());
        a.goto_line(0);
        assert_eq!(a.view.top_line, 0);
        a.goto_line(3);
        assert_eq!(a.view.top_line, 2);
        a.goto_line(999);
        assert_eq!(a.view.top_line, 9);
    }

    #[test]
    fn startup_search_finds_and_records_history() {
        let mut a = app(b"alpha\nbeta\ngamma\n", opts());
        a.set_search("gam");
        assert_eq!(a.view.cur_match, Some((11, 14)));
        assert_eq!(a.history, ["gam"]);
    }

    #[test]
    fn invalid_regex_reports_an_error() {
        let mut o = opts();
        o.regex = true;
        let mut a = app(b"x\n", o);
        a.set_search("(");
        assert!(a.search.is_none());
        assert!(matches!(a.message, Some((_, true))));
    }

    #[test]
    fn follow_mode_starts_at_the_end() {
        let mut o = opts();
        o.follow = true;
        let a = app(&numbered(1000), o);
        assert!(a.view.top_line > 900, "top_line = {}", a.view.top_line);
    }

    fn key(c: char) -> KeyEvent {
        KeyEvent::new(KeyCode::Char(c), KeyModifiers::NONE)
    }

    #[test]
    fn open_externally_needs_a_real_file() {
        let mut a = app(b"piped\n", opts());
        a.on_key(key('o'));
        assert!(matches!(&a.message, Some((m, true)) if m.contains("Standard input")));
        assert!(!a.quit);
    }

    #[test]
    fn capital_b_opens_the_browser() {
        let mut a = app(b"x\n", opts());
        a.on_key(key('B'));
        assert!(a.quit && !a.quit_all);
        assert!(a.want_browser.is_some());
        // Lowercase b still pages up.
        let mut a = app(b"x\n", opts());
        a.on_key(key('b'));
        assert!(!a.quit && a.want_browser.is_none());
    }

    /// Draws a frame and returns the screen as text, one string per row.
    fn screen(a: &mut App) -> Vec<String> {
        let mut out = Vec::new();
        a.draw(&mut out).unwrap();
        let s = String::from_utf8(out).unwrap();
        let mut rows = vec![String::new(); a.height];
        let mut y = 0;
        let mut chars = s.chars().peekable();
        while let Some(c) = chars.next() {
            if c != '\x1b' {
                if let Some(r) = rows.get_mut(y) {
                    r.push(c);
                }
                continue;
            }
            // CSI: parameters, then a final letter. MoveTo is `row;colH`, 1-based.
            if chars.next_if_eq(&'[').is_some() {
                let mut params = String::new();
                while let Some(&p) = chars.peek() {
                    chars.next();
                    if p.is_ascii_alphabetic() {
                        if p == 'H' {
                            y = params.split(';').next().and_then(|r| r.parse::<usize>().ok()).unwrap_or(1) - 1;
                        }
                        break;
                    }
                    params.push(p);
                }
            }
        }
        rows.iter().map(|r| r.trim_end().to_string()).collect()
    }

    fn named(name: &str, bytes: &[u8]) -> App {
        let mut a = App::new(vec![Source::Stdin], 0, Document::from_bytes(name, bytes.to_vec()), opts());
        a.resize(60, 8);
        a
    }

    #[test]
    fn csv_files_open_as_aligned_tables() {
        let mut a = named("people.csv", b"name,age,city\nann,3,\"Paris, FR\"\nbartholomew,41,Oslo\n");
        let s = screen(&mut a);
        assert_eq!(s[1], "name        │ age │ city");
        assert_eq!(s[2], "ann         │ 3   │ Paris, FR");
        assert_eq!(s[3], "bartholomew │ 41  │ Oslo");
        assert!(s[0].contains("CSV"), "status: {}", s[0]);
        assert!(s[7].contains("D Text view"), "bottom: {}", s[7]);
    }

    #[test]
    fn vcf_metadata_is_plain_and_header_sticks_when_scrolled() {
        let mut v = b"##fileformat=VCFv4.2\n##source=test\n#CHROM\tPOS\tREF\n".to_vec();
        for i in 1..=20 {
            v.extend(format!("chr1\t{}\tA\n", i * 100).bytes());
        }
        let mut a = named("calls.vcf", &v);
        let s = screen(&mut a);
        assert_eq!(s[1], "##fileformat=VCFv4.2");
        assert_eq!(s[3], "#CHROM │ POS  │ REF");
        a.goto_line(10);
        let s = screen(&mut a);
        assert_eq!(s[1], "#CHROM │ POS  │ REF", "header pinned");
        assert_eq!(s[2], "chr1   │ 700  │ A");
        // At the end, the last row is still visible below the pinned header.
        a.on_key(KeyEvent::new(KeyCode::End, KeyModifiers::NONE));
        let s = screen(&mut a);
        assert_eq!(s[6], "chr1   │ 2000 │ A");
    }

    #[test]
    fn preformatted_preprocessor_output_is_left_alone() {
        let csvlook = b"| name | age |\n| ---- | --- |\n| Ann  |  34 |\n| Dana, Jr |  52 |\n";
        let doc = Document::from_bytes("people.csv.gz", csvlook.to_vec()).filtered(Some("lesspipe.sh".into()));
        let a = App::new(vec![Source::Stdin], 0, doc, opts());
        assert!(a.view.table.is_none());
        // Plain decompressed CSV still gets the table view.
        let doc = Document::from_bytes("people.csv.gz", b"name,age\nAnn,34\n".to_vec()).filtered(Some("lesspipe.sh".into()));
        let a = App::new(vec![Source::Stdin], 0, doc, opts());
        assert_eq!(a.view.table.as_ref().map(|t| t.format.label), Some("CSV"));
    }

    #[test]
    fn d_toggles_table_view_and_sniffs_unknown_files() {
        let mut a = named("<stdin>", b"a\tb\n1\t2\n");
        assert!(a.view.table.is_none());
        a.on_key(key('d'));
        assert_eq!(a.view.table.as_ref().map(|t| t.format.label), Some("TSV"));
        assert_eq!(screen(&mut a)[1], "a │ b");
        a.on_key(key('D'));
        assert!(a.view.table.is_none());
        assert_eq!(screen(&mut a)[1], "a       b", "back to tab-expanded text");

        let mut a = named("notes.txt", b"no separators here\n");
        a.on_key(key('d'));
        assert!(a.view.table.is_none());
        assert!(matches!(&a.message, Some((m, true)) if m.contains("No columns")));
    }

    #[test]
    fn arrows_move_by_column_and_search_reveals_the_column() {
        let mut a = named("w.tsv", b"aaaaaaaaaa\tbbbbbbbbbb\tcccccccccc\tdddddddddd\tneedle\n");
        let right = KeyEvent::new(KeyCode::Right, KeyModifiers::NONE);
        let left = KeyEvent::new(KeyCode::Left, KeyModifiers::NONE);
        assert!(screen(&mut a)[0].contains("Column 1 of 5"));
        a.on_key(right);
        assert_eq!(a.view.hscroll, 13);
        let s = screen(&mut a);
        assert!(s[1].starts_with("bbbbbbbbbb │"));
        assert!(s[0].contains("Column 2 of 5"), "status: {}", s[0]);
        a.on_key(right);
        a.on_key(left);
        assert_eq!(a.view.hscroll, 13);
        a.on_key(left);
        assert_eq!(a.view.hscroll, 0);
        a.set_search("needle");
        assert!(screen(&mut a)[1].contains("needle"));
    }

    #[test]
    fn s_toggles_column_colors_and_w_is_refused_in_table_view() {
        let mut a = named("x.csv", b"a,b\n");
        a.on_key(key('s'));
        assert!(!a.view.table.as_ref().unwrap().colors);
        assert!(a.opts.highlight.enabled == opts().highlight.enabled, "syntax setting untouched");
        a.on_key(key('w'));
        assert!(!a.opts.wrap);
        assert!(matches!(&a.message, Some((m, false)) if m.contains("doesn't wrap")));
    }

    #[test]
    fn draw_renders_file_text() {
        let mut a = app(b"hello from lyst\n", opts());
        let mut out = Vec::new();
        a.draw(&mut out).unwrap();
        let s = String::from_utf8_lossy(&out);
        assert!(s.contains("hello from lyst"));
    }
}
