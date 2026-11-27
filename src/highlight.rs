//! Syntax highlighting with syntect, using bat's syntax and theme collection.

use crate::doc::Document;
use crossterm::style::Color;
use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::OnceLock;
use syntect::highlighting::{FontStyle, HighlightIterator, HighlightState, Highlighter, Theme, ThemeSet};
use syntect::parsing::{ParseState, ScopeStack, SyntaxDefinition, SyntaxReference, SyntaxSet};
use two_face::theme::EmbeddedLazyThemeSet;

/// Lines per parser checkpoint.
const STRIDE: usize = 256;
/// How far back (in chunks) to look for an exact parser state before starting
/// fresh. Starting fresh is usually right, since most syntaxes resynchronize
/// at line starts; long block comments or strings may be off until scrolled through.
const MAX_CATCHUP: usize = 16;
/// Lines longer than this (e.g. minified files) are shown unhighlighted.
const MAX_LINE: usize = 16 * 1024;
const MAX_CACHED_CHUNKS: usize = 256;

/// Where user additions live: `$LYST_CONFIG_DIR`, else `$XDG_CONFIG_HOME/lyst`,
/// else `~/.config/lyst`. Syntaxes go in `syntaxes/`, themes in `themes/`.
pub fn config_dir() -> Option<PathBuf> {
    let var = |k| std::env::var_os(k).filter(|v| !v.is_empty()).map(PathBuf::from);
    var("LYST_CONFIG_DIR")
        .or_else(|| var("XDG_CONFIG_HOME").map(|d| d.join("lyst")))
        .or_else(|| crate::paths::home().map(|h| h.join(".config").join("lyst")))
}

/// Themes loaded from `.tmTheme` files in the config directory.
struct UserThemes {
    /// (name from the file stem, file, theme)
    themes: Vec<(String, PathBuf, &'static Theme)>,
    errors: Vec<String>,
}

impl UserThemes {
    fn load(dir: Option<&Path>) -> Self {
        let mut out = UserThemes { themes: Vec::new(), errors: Vec::new() };
        let Some(dir) = dir else { return out };
        // Themes may sit in themes/ or next to the syntaxes that use them.
        let files = files_with_ext(&dir.join("themes"), "tmTheme")
            .into_iter()
            .chain(files_with_ext(&dir.join("syntaxes"), "tmTheme"));
        for path in files {
            match ThemeSet::get_theme(&path) {
                // Leaked deliberately: themes live for the whole run.
                Ok(t) => out.themes.push((stem(&path), path, Box::leak(Box::new(t)))),
                Err(e) => out.errors.push(format!("{}: {e}", path.display())),
            }
        }
        out
    }

    fn find(&self, name: &str) -> Option<&'static Theme> {
        self.themes.iter().find(|(n, ..)| n.eq_ignore_ascii_case(name)).map(|t| t.2)
    }
}

fn user_themes() -> &'static UserThemes {
    static U: OnceLock<UserThemes> = OnceLock::new();
    U.get_or_init(|| UserThemes::load(config_dir().as_deref()))
}

/// The syntax set: bat's collection plus any user `.sublime-syntax` files.
struct Assets {
    syntaxes: SyntaxSet,
    /// Syntax name -> theme requested by its `.sublime-settings` `color_scheme`.
    syntax_themes: HashMap<String, &'static Theme>,
    /// (syntax name, file) for each user syntax.
    user_syntaxes: Vec<(String, PathBuf)>,
    errors: Vec<String>,
}

impl Assets {
    fn load(dir: Option<&Path>, themes: &UserThemes) -> Self {
        let base = two_face::syntax::extra_newlines();
        let files = dir.map(|d| files_with_ext(&d.join("syntaxes"), "sublime-syntax")).unwrap_or_default();
        let mut a = Assets { syntaxes: SyntaxSet::new(), syntax_themes: HashMap::new(), user_syntaxes: Vec::new(), errors: Vec::new() };
        if files.is_empty() {
            a.syntaxes = base;
            return a;
        }
        // Added after the built-ins, so user syntaxes win when both claim an extension.
        let mut builder = base.into_builder();
        let mut wanted = Vec::new();
        for path in files {
            let def = std::fs::read_to_string(&path)
                .map_err(|e| e.to_string())
                .and_then(|text| SyntaxDefinition::load_from_str(&text, true, Some(&stem(&path))).map_err(|e| e.to_string()));
            match def {
                Ok(mut def) => {
                    // Some syntaxes (biosyntax among them) write extensions as ".fa".
                    for e in &mut def.file_extensions {
                        if let Some(bare) = e.strip_prefix('.') {
                            *e = bare.to_string();
                        }
                    }
                    if let Some(scheme) = color_scheme(&path.with_extension("sublime-settings")) {
                        wanted.push((def.name.clone(), scheme));
                    }
                    a.user_syntaxes.push((def.name.clone(), path));
                    builder.add(def);
                }
                Err(e) => a.errors.push(format!("{}: {e}", path.display())),
            }
        }
        a.syntaxes = builder.build();
        for (syntax, scheme) in wanted {
            match themes.find(&scheme) {
                Some(t) => {
                    a.syntax_themes.insert(syntax, t);
                }
                None => a.errors.push(format!("{syntax}: color scheme '{scheme}' not found")),
            }
        }
        a
    }
}

fn assets() -> &'static Assets {
    static A: OnceLock<Assets> = OnceLock::new();
    A.get_or_init(|| Assets::load(config_dir().as_deref(), user_themes()))
}

fn syntaxes() -> &'static SyntaxSet {
    &assets().syntaxes
}

fn themes() -> &'static EmbeddedLazyThemeSet {
    static T: OnceLock<EmbeddedLazyThemeSet> = OnceLock::new();
    T.get_or_init(two_face::theme::extra)
}

/// Reads `"color_scheme": "Packages/X/Name.tmTheme"` from a Sublime settings
/// file (JSON with comments) and returns `Name`.
fn color_scheme(settings: &Path) -> Option<String> {
    let text = std::fs::read_to_string(settings).ok()?;
    text.lines()
        .map(str::trim_start)
        .filter(|l| !l.starts_with("//"))
        .find_map(|l| l.strip_prefix("\"color_scheme\""))
        .and_then(|rest| rest.split('"').nth(1))
        .map(|v| stem(Path::new(v)))
}

fn stem(p: &Path) -> String {
    p.file_stem().map(|s| s.to_string_lossy().into_owned()).unwrap_or_default()
}

/// Files under `dir` (recursively) with extension `ext`, sorted.
fn files_with_ext(dir: &Path, ext: &str) -> Vec<PathBuf> {
    fn walk(dir: &Path, ext: &str, depth: usize, out: &mut Vec<PathBuf>) {
        let Ok(rd) = std::fs::read_dir(dir) else { return };
        for e in rd.flatten() {
            let p = e.path();
            if p.is_dir() && depth < 8 {
                walk(&p, ext, depth + 1, out);
            } else if p.extension().is_some_and(|x| x.eq_ignore_ascii_case(ext)) {
                out.push(p);
            }
        }
    }
    let mut out = Vec::new();
    walk(dir, ext, 0, &mut out);
    out.sort();
    out
}

/// Loads the syntax definitions so the first highlighted file opens quickly.
pub fn preload() {
    syntaxes();
}

pub fn theme_names() -> Vec<String> {
    let mut names: Vec<String> = EmbeddedLazyThemeSet::theme_names().iter().map(|t| t.as_name().to_string()).collect();
    names.extend(user_themes().themes.iter().map(|(n, p, _)| format!("{n}  ({})", p.display())));
    names
}

/// Human-readable list of syntaxes, user additions, and loading problems.
pub fn describe_syntaxes() -> String {
    let a = assets();
    let mut out = String::new();
    let mut all: Vec<&SyntaxReference> = a.syntaxes.syntaxes().iter().collect();
    all.sort_by_key(|s| s.name.to_lowercase());
    for s in all {
        let user = a.user_syntaxes.iter().find(|(n, _)| *n == s.name);
        let mark = if user.is_some() { "*" } else { " " };
        out.push_str(&format!("{mark} {:<28} {}\n", s.name, s.file_extensions.join(", ")));
    }
    match config_dir() {
        Some(d) => out.push_str(&format!(
            "\n* = loaded from {} ({} syntaxes, {} with their own theme)\n",
            d.join("syntaxes").display(),
            a.user_syntaxes.len(),
            a.syntax_themes.len()
        )),
        None => out.push_str("\n(no config directory: set HOME or LYST_CONFIG_DIR)\n"),
    }
    for e in user_themes().errors.iter().chain(&a.errors) {
        out.push_str(&format!("warning: {e}\n"));
    }
    out
}

#[derive(Clone)]
pub struct HlConfig {
    pub enabled: bool,
    theme: &'static Theme,
    /// Chosen with --theme, so it overrides themes that syntaxes ask for.
    explicit_theme: bool,
    force: Option<&'static SyntaxReference>,
    truecolor: bool,
}

impl HlConfig {
    pub fn new(enabled: bool, theme: Option<&str>, syntax: Option<&str>) -> Result<Self, String> {
        let name = theme.unwrap_or("ansi");
        let embedded = || {
            let t = EmbeddedLazyThemeSet::theme_names().iter().copied().find(|t| t.as_name() == name)?;
            Some(themes().get(t))
        };
        let Some(theme_ref) = user_themes().find(name).or_else(embedded) else {
            return Err(format!("unknown theme '{name}' (see --list-themes)"));
        };
        let force = match syntax {
            Some(s) => Some(
                syntaxes()
                    .find_syntax_by_token(s)
                    .or_else(|| syntaxes().syntaxes().iter().find(|x| x.name.eq_ignore_ascii_case(s)))
                    .ok_or_else(|| format!("unknown syntax '{s}' (see --list-syntaxes)"))?,
            ),
            None => None,
        };
        let truecolor = std::env::var("COLORTERM").is_ok_and(|v| v.contains("truecolor") || v.contains("24bit"));
        Ok(HlConfig { enabled, theme: theme_ref, explicit_theme: theme.is_some(), force, truecolor })
    }
}

/// A styled byte range within one line.
#[derive(Clone, Copy, Debug)]
pub struct Span {
    pub start: usize,
    pub end: usize,
    /// `None` means the terminal's default color.
    pub fg: Option<Color>,
    pub bg: Option<Color>,
    pub bold: bool,
    pub italic: bool,
    pub underline: bool,
}

/// Finds the span covering `byte`, if any. `spans` must be sorted.
pub fn span_at(spans: &[Span], byte: usize) -> Option<&Span> {
    let i = spans.partition_point(|s| s.end <= byte);
    spans.get(i).filter(|s| s.start <= byte)
}

type State = (ParseState, HighlightState);

/// Per-document highlighting state: parser checkpoints and cached spans.
pub struct DocHighlighter {
    /// The set `syntax` belongs to; parsing needs it to resolve contexts.
    set: &'static SyntaxSet,
    syntax: &'static SyntaxReference,
    theme: &'static Theme,
    truecolor: bool,
    /// Parser state at the start of each chunk of `STRIDE` lines.
    checkpoints: HashMap<usize, State>,
    chunks: HashMap<usize, Vec<Vec<Span>>>,
}

impl DocHighlighter {
    /// Picks a syntax from the file name, then the first line (shebangs etc.).
    pub fn detect(doc: &Document, cfg: &HlConfig) -> Option<Self> {
        Self::detect_in(doc, cfg, assets())
    }

    fn detect_in(doc: &Document, cfg: &HlConfig, assets: &'static Assets) -> Option<Self> {
        let ss = &assets.syntaxes;
        let by_name = || {
            let mut name = Path::new(&doc.name).file_name()?.to_str()?;
            // A preprocessor may have decompressed `foo.rs.gz`: look at `foo.rs`.
            if doc.filter.is_some() {
                for ext in [".gz", ".bz2", ".xz", ".zst", ".lz", ".lzma", ".Z"] {
                    name = name.strip_suffix(ext).unwrap_or(name);
                }
            }
            let ext = Path::new(name).extension().and_then(|e| e.to_str());
            ss.find_syntax_by_extension(name).or_else(|| ss.find_syntax_by_extension(ext?))
        };
        let by_first_line = || ss.find_syntax_by_first_line(std::str::from_utf8(doc.line(0).1).ok()?);
        let syntax = cfg.force.or_else(by_name).or_else(by_first_line)?;
        if syntax.name == "Plain Text" {
            return None;
        }
        // Like Sublime, a syntax can bring its own color scheme.
        let theme = match assets.syntax_themes.get(&syntax.name) {
            Some(t) if !cfg.explicit_theme => t,
            _ => cfg.theme,
        };
        let mut h = DocHighlighter {
            set: ss,
            syntax,
            theme,
            truecolor: cfg.truecolor,
            checkpoints: HashMap::new(),
            chunks: HashMap::new(),
        };
        h.reset();
        Some(h)
    }

    pub fn name(&self) -> &str {
        &self.syntax.name
    }

    /// Forgets cached results (e.g. after the file grew).
    pub fn reset(&mut self) {
        self.chunks.clear();
        self.checkpoints.clear();
        self.checkpoints.insert(0, self.fresh());
    }

    fn fresh(&self) -> State {
        let hl = Highlighter::new(self.theme);
        (ParseState::new(self.syntax), HighlightState::new(&hl, ScopeStack::new()))
    }

    pub fn spans(&mut self, doc: &Document, line: usize) -> Vec<Span> {
        let chunk = line / STRIDE;
        if !self.chunks.contains_key(&chunk) {
            self.compute(doc, chunk);
        }
        self.chunks.get(&chunk).and_then(|c| c.get(line % STRIDE)).cloned().unwrap_or_default()
    }

    fn compute(&mut self, doc: &Document, chunk: usize) {
        if self.chunks.len() >= MAX_CACHED_CHUNKS {
            self.chunks.clear();
        }
        let start = (chunk.saturating_sub(MAX_CATCHUP)..=chunk).rev().find(|c| self.checkpoints.contains_key(c));
        let (mut c, (mut ps, mut hs)) = match start {
            Some(c) => (c, self.checkpoints[&c].clone()),
            None => (chunk, self.fresh()),
        };
        let hl = Highlighter::new(self.theme);
        while c <= chunk {
            let lines = c * STRIDE..((c + 1) * STRIDE).min(doc.line_count());
            let spans: Vec<Vec<Span>> = lines.map(|l| self.line(&mut ps, &mut hs, &hl, doc.line(l).1)).collect();
            self.chunks.insert(c, spans);
            self.checkpoints.insert(c + 1, (ps.clone(), hs.clone()));
            c += 1;
        }
    }

    fn line(&self, ps: &mut ParseState, hs: &mut HighlightState, hl: &Highlighter, bytes: &[u8]) -> Vec<Span> {
        if bytes.len() > MAX_LINE {
            return Vec::new();
        }
        let (text, valid) = match std::str::from_utf8(bytes) {
            Ok(s) => (format!("{s}\n"), true),
            Err(_) => (format!("{}\n", String::from_utf8_lossy(bytes)), false),
        };
        let Ok(ops) = ps.parse_line(&text, self.set) else {
            return Vec::new();
        };
        // Text the theme doesn't single out keeps the terminal's own colors, so
        // dark-background themes stay readable on light terminals. (These are
        // syntect's fallbacks when a theme sets no defaults.)
        let default_fg = self.theme.settings.foreground.unwrap_or(syntect::highlighting::Color::BLACK);
        let default_bg = self.theme.settings.background.unwrap_or(syntect::highlighting::Color::WHITE);
        let mut spans = Vec::new();
        let mut pos = 0;
        for (style, piece) in HighlightIterator::new(hs, &ops, &text, hl) {
            // Lossy-decoded lines have shifted offsets: advance state, show plain.
            if valid {
                let end = (pos + piece.len()).min(bytes.len());
                if end > pos {
                    spans.push(Span {
                        start: pos,
                        end,
                        fg: (style.foreground != default_fg).then(|| self.color(style.foreground)).flatten(),
                        bg: (style.background != default_bg).then(|| self.color(style.background)).flatten(),
                        bold: style.font_style.contains(FontStyle::BOLD),
                        italic: style.font_style.contains(FontStyle::ITALIC),
                        underline: style.font_style.contains(FontStyle::UNDERLINE),
                    });
                }
            }
            pos += piece.len();
        }
        spans
    }

    /// bat's convention: alpha 0 encodes an ANSI palette index in the red
    /// channel, alpha 1 means "terminal default"; anything else is RGB.
    fn color(&self, c: syntect::highlighting::Color) -> Option<Color> {
        match c.a {
            0 => Some(Color::AnsiValue(c.r)),
            1 => None,
            _ if self.truecolor => Some(Color::Rgb { r: c.r, g: c.g, b: c.b }),
            _ => Some(Color::AnsiValue(rgb_to_256(c.r, c.g, c.b))),
        }
    }
}

/// Nearest xterm-256 palette entry.
fn rgb_to_256(r: u8, g: u8, b: u8) -> u8 {
    if r == g && g == b {
        return match r {
            0..8 => 16,
            249.. => 231,
            _ => 232 + ((r as u16 - 8) * 24 / 241) as u8,
        };
    }
    let q = |v: u8| if v < 48 { 0 } else if v < 115 { 1 } else { (v - 35) / 40 };
    16 + 36 * q(r) + 6 * q(g) + q(b)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn cfg() -> HlConfig {
        HlConfig::new(true, None, None).unwrap()
    }

    #[test]
    fn detects_by_extension_name_and_shebang() {
        let d = Document::from_bytes("x/main.rs", b"fn main() {}\n".to_vec());
        assert_eq!(DocHighlighter::detect(&d, &cfg()).unwrap().name(), "Rust");
        let d = Document::from_bytes("Makefile", b"all:\n".to_vec());
        assert_eq!(DocHighlighter::detect(&d, &cfg()).unwrap().name(), "Makefile");
        let d = Document::from_bytes("script", b"#!/usr/bin/env python3\nprint(1)\n".to_vec());
        assert_eq!(DocHighlighter::detect(&d, &cfg()).unwrap().name(), "Python");
        let d = Document::from_bytes("lib.py.gz", b"x = 1\n".to_vec()).filtered(Some("lesspipe".into()));
        assert_eq!(DocHighlighter::detect(&d, &cfg()).unwrap().name(), "Python");
        let d = Document::from_bytes("notes", b"hello\n".to_vec());
        assert!(DocHighlighter::detect(&d, &cfg()).is_none());
    }

    #[test]
    fn spans_cover_line_and_state_carries_across_lines() {
        let src = "/* start\nstill comment */ let x = 1;\n";
        let d = Document::from_bytes("a.rs", src.as_bytes().to_vec());
        let mut h = DocHighlighter::detect(&d, &cfg()).unwrap();
        let spans = h.spans(&d, 1);
        assert_eq!(spans.first().unwrap().start, 0);
        assert_eq!(spans.last().unwrap().end, d.line(1).1.len());
        // "still" is inside the block comment, "let" is a keyword: different styles.
        let comment = span_at(&spans, 0).unwrap().fg;
        let keyword = span_at(&spans, 17).unwrap().fg;
        assert_ne!(comment, keyword);
    }

    #[test]
    fn bad_names_are_reported() {
        assert!(HlConfig::new(true, Some("nope"), None).is_err());
        assert!(HlConfig::new(true, None, Some("nope")).is_err());
        assert!(HlConfig::new(true, Some("ansi"), Some("rust")).is_ok());
    }

    #[test]
    fn user_syntax_with_its_own_theme() {
        let dir = std::env::temp_dir().join(format!("lyst-cfg-{}", std::process::id()));
        let syn = dir.join("syntaxes").join("bio");
        std::fs::create_dir_all(&syn).unwrap();
        std::fs::write(
            syn.join("dna.sublime-syntax"),
            "%YAML 1.2\n---\nname: dna\nfile_extensions: [.dna]\nscope: text.dna\ncontexts:\n  main:\n    - match: '^>.*'\n      scope: header\n    - match: '[Aa]++'\n      scope: ntA\n",
        )
        .unwrap();
        std::fs::write(
            syn.join("dna.sublime-settings"),
            "// comment\n{\n    // \"color_scheme\": \"wrong.tmTheme\",\n    \"color_scheme\": \"Packages/bio/biotheme.tmTheme\",\n}\n",
        )
        .unwrap();
        std::fs::write(
            syn.join("biotheme.tmTheme"),
            r#"<?xml version="1.0" encoding="UTF-8"?>
<plist version="1.0"><dict><key>name</key><string>bio</string><key>settings</key><array>
<dict><key>settings</key><dict><key>foreground</key><string>#F8F8F2</string><key>background</key><string>#272822</string></dict></dict>
<dict><key>scope</key><string>ntA</string><key>settings</key><dict><key>foreground</key><string>#000000</string><key>background</key><string>#47FF19</string></dict></dict>
</array></dict></plist>"#,
        )
        .unwrap();

        let themes: &'static UserThemes = Box::leak(Box::new(UserThemes::load(Some(&dir))));
        let assets: &'static Assets = Box::leak(Box::new(Assets::load(Some(&dir), themes)));
        assert!(assets.errors.is_empty(), "{:?}", assets.errors);
        assert!(assets.syntax_themes.contains_key("dna"));

        let doc = Document::from_bytes("x.dna", b">seq1\nCAAAG\n".to_vec());
        let mut h = DocHighlighter::detect_in(&doc, &cfg(), assets).unwrap();
        assert_eq!(h.name(), "dna");
        let spans = h.spans(&doc, 1);
        let a = span_at(&spans, 1).unwrap();
        assert!(a.bg.is_some(), "nucleotide gets the theme's background");
        // Unscoped text ("C") keeps the terminal defaults, not the theme's dark base.
        let c = span_at(&spans, 0).unwrap();
        assert_eq!((c.fg, c.bg), (None, None));
        let _ = std::fs::remove_dir_all(&dir);
    }
}
