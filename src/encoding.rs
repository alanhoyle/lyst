//! Turning raw line bytes into screen cells.

use crossterm::style::Color;
use std::sync::OnceLock;
use unicode_width::UnicodeWidthChar;

#[derive(Clone, Copy, Debug, PartialEq, Eq, clap::ValueEnum)]
pub enum Encoding {
    /// UTF-8; invalid bytes shown as <XX>.
    Utf8,
    /// IBM PC code page 437, the original DOS character set.
    Cp437,
    /// ISO-8859-1.
    Latin1,
    /// Strip the high bit (WordStar-style files).
    Ascii7,
}

impl Encoding {
    pub fn next(self) -> Self {
        match self {
            Encoding::Utf8 => Encoding::Cp437,
            Encoding::Cp437 => Encoding::Latin1,
            Encoding::Latin1 => Encoding::Ascii7,
            Encoding::Ascii7 => Encoding::Utf8,
        }
    }

    pub fn label(self) -> &'static str {
        match self {
            Encoding::Utf8 => "UTF-8",
            Encoding::Cp437 => "CP437",
            Encoding::Latin1 => "Latin-1",
            Encoding::Ascii7 => "7-bit",
        }
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum CellKind {
    Text,
    /// Rendered stand-in for a control or undecodable byte.
    Special,
}

/// Text attributes set by ANSI SGR escape sequences (`ESC [ ... m`).
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct AnsiStyle {
    pub fg: Option<Color>,
    pub bg: Option<Color>,
    pub bold: bool,
    pub dim: bool,
    pub italic: bool,
    pub underline: bool,
    pub reverse: bool,
}

impl AnsiStyle {
    fn apply_sgr(&mut self, params: &[u8]) {
        let nums: Vec<u16> = params
            .split(|&b| b == b';' || b == b':')
            .map(|p| std::str::from_utf8(p).ok().and_then(|s| s.parse().ok()).unwrap_or(0))
            .collect();
        let mut i = 0;
        while i < nums.len() {
            let n = nums[i];
            match n {
                0 => *self = AnsiStyle::default(),
                1 => self.bold = true,
                2 => self.dim = true,
                3 => self.italic = true,
                4 => self.underline = true,
                7 => self.reverse = true,
                22 => (self.bold, self.dim) = (false, false),
                23 => self.italic = false,
                24 => self.underline = false,
                27 => self.reverse = false,
                30..=37 => self.fg = Some(Color::AnsiValue((n - 30) as u8)),
                39 => self.fg = None,
                40..=47 => self.bg = Some(Color::AnsiValue((n - 40) as u8)),
                49 => self.bg = None,
                90..=97 => self.fg = Some(Color::AnsiValue((n - 90 + 8) as u8)),
                100..=107 => self.bg = Some(Color::AnsiValue((n - 100 + 8) as u8)),
                38 | 48 => {
                    let color = match nums.get(i + 1) {
                        Some(5) => {
                            let c = nums.get(i + 2).map(|&v| Color::AnsiValue(v as u8));
                            i += 2;
                            c
                        }
                        Some(2) => {
                            let c = match nums.get(i + 2..i + 5) {
                                Some(&[r, g, b]) => Some(Color::Rgb { r: r as u8, g: g as u8, b: b as u8 }),
                                _ => None,
                            };
                            i += 4;
                            c
                        }
                        _ => None,
                    };
                    if n == 38 {
                        self.fg = color.or(self.fg);
                    } else {
                        self.bg = color.or(self.bg);
                    }
                }
                _ => {}
            }
            i += 1;
        }
    }
}

/// If `s` starts with a recognised escape sequence, returns its length and,
/// for SGR sequences, the parameter bytes.
fn escape_len(s: &[u8]) -> Option<(usize, Option<&[u8]>)> {
    match s.get(1)? {
        b'[' => {
            let mut i = 2;
            while s.get(i).is_some_and(|b| (0x30..=0x3f).contains(b)) {
                i += 1;
            }
            let params_end = i;
            while s.get(i).is_some_and(|b| (0x20..=0x2f).contains(b)) {
                i += 1;
            }
            let fin = *s.get(i)?;
            if !(0x40..=0x7e).contains(&fin) {
                return None;
            }
            let sgr = (fin == b'm' && params_end == i).then(|| &s[2..params_end]);
            Some((i + 1, sgr))
        }
        // OSC (e.g. hyperlinks), terminated by BEL or ESC \.
        b']' => {
            let mut i = 2;
            while i < s.len() {
                match s[i] {
                    0x07 => return Some((i + 1, None)),
                    0x1b if s.get(i + 1) == Some(&b'\\') => return Some((i + 2, None)),
                    _ => i += 1,
                }
            }
            None
        }
        _ => None,
    }
}

#[derive(Clone, Copy, Debug)]
pub struct Cell {
    pub ch: char,
    pub width: u8,
    pub kind: CellKind,
    /// Offset within the line of the byte this cell came from.
    pub byte: usize,
    pub ansi: AnsiStyle,
}

const CP437_LOW: &str = " ☺☻♥♦♣♠•◘○◙♂♀♪♫☼►◄↕‼¶§▬↨↑↓→←∟↔▲▼";
const CP437_HIGH: &str = concat!(
    "ÇüéâäàåçêëèïîìÄÅ",
    "ÉæÆôöòûùÿÖÜ¢£¥₧ƒ",
    "áíóúñÑªº¿⌐¬½¼¡«»",
    "░▒▓│┤╡╢╖╕╣║╗╝╜╛┐",
    "└┴┬├─┼╞╟╚╔╩╦╠═╬╧",
    "╨╤╥╙╘╒╓╫╪┘┌█▄▌▐▀",
    "αßΓπΣσµτΦΘΩδ∞φε∩",
    "≡±≥≤⌠⌡÷≈°∙·√ⁿ²■\u{a0}",
);

/// The CP437 glyph for byte `b`.
pub fn cp437(b: u8) -> char {
    static TABLE: OnceLock<[char; 256]> = OnceLock::new();
    TABLE.get_or_init(|| {
        let mut t = [' '; 256];
        for (i, c) in CP437_LOW.chars().enumerate() {
            t[i] = c;
        }
        for (i, slot) in t.iter_mut().enumerate().take(0x7f).skip(0x20) {
            *slot = i as u8 as char;
        }
        t[0x7f] = '⌂';
        for (i, c) in CP437_HIGH.chars().enumerate() {
            t[0x80 + i] = c;
        }
        t
    })[b as usize]
}

struct Layout {
    cells: Vec<Cell>,
    col: usize,
    tab: usize,
    max_cols: usize,
    style: AnsiStyle,
}

impl Layout {
    fn full(&self) -> bool {
        self.col >= self.max_cols
    }

    fn push(&mut self, ch: char, width: usize, kind: CellKind, byte: usize) {
        self.cells.push(Cell { ch, width: width as u8, kind, byte, ansi: self.style });
        self.col += width;
    }

    fn push_str(&mut self, s: &str, kind: CellKind, byte: usize) {
        for ch in s.chars() {
            self.push(ch, 1, kind, byte);
        }
    }

    fn tab(&mut self, byte: usize) {
        let n = self.tab - self.col % self.tab;
        for _ in 0..n {
            self.push(' ', 1, CellKind::Text, byte);
        }
    }

    fn hex(&mut self, b: u8, byte: usize) {
        self.push_str(&format!("<{b:02X}>"), CellKind::Special, byte);
    }

    /// 7-bit ASCII byte, with control characters in caret notation.
    fn ascii(&mut self, b: u8, byte: usize) {
        match b {
            b'\t' => self.tab(byte),
            0x00..=0x1f | 0x7f => {
                self.push('^', 1, CellKind::Special, byte);
                self.push((b ^ 0x40) as char, 1, CellKind::Special, byte);
            }
            _ => self.push(b as char, 1, CellKind::Text, byte),
        }
    }

    fn unicode(&mut self, c: char, byte: usize) {
        if c.is_ascii() {
            return self.ascii(c as u8, byte);
        }
        if c.is_control() {
            return self.hex((c as u32) as u8, byte);
        }
        match c.width() {
            Some(w) if w > 0 => self.push(c, w, CellKind::Text, byte),
            // Zero-width characters (combining marks, joiners) are dropped so
            // column accounting stays exact.
            _ => {}
        }
    }

    /// Lays out `bytes`, which start at offset `base` in the line.
    fn bytes(&mut self, bytes: &[u8], base: usize, enc: Encoding) {
        match enc {
            Encoding::Utf8 => {
                let mut off = base;
                for chunk in bytes.utf8_chunks() {
                    for (i, c) in chunk.valid().char_indices() {
                        self.unicode(c, off + i);
                        if self.full() {
                            return;
                        }
                    }
                    off += chunk.valid().len();
                    for &b in chunk.invalid() {
                        self.hex(b, off);
                        off += 1;
                        if self.full() {
                            return;
                        }
                    }
                }
            }
            Encoding::Cp437 | Encoding::Latin1 | Encoding::Ascii7 => {
                for (j, &b) in bytes.iter().enumerate() {
                    let i = base + j;
                    match enc {
                        Encoding::Cp437 if b == b'\t' => self.tab(i),
                        Encoding::Cp437 => self.push(cp437(b), 1, CellKind::Text, i),
                        Encoding::Latin1 if b < 0x80 => self.ascii(b, i),
                        Encoding::Latin1 if b < 0xa0 => self.hex(b, i),
                        Encoding::Latin1 => self.push(b as char, 1, CellKind::Text, i),
                        _ => self.ascii(b & 0x7f, i),
                    }
                    if self.full() {
                        return;
                    }
                }
            }
        }
    }
}

/// Lays out `line` into cells, stopping once `max_cols` columns are filled.
/// With `ansi`, escape sequences are consumed and SGR colors applied to cells.
pub fn layout(line: &[u8], enc: Encoding, tab: usize, max_cols: usize, ansi: bool) -> Vec<Cell> {
    let mut l = Layout {
        cells: Vec::with_capacity(line.len().min(max_cols)),
        col: 0,
        tab: tab.max(1),
        max_cols,
        style: AnsiStyle::default(),
    };
    if !ansi {
        l.bytes(line, 0, enc);
        return l.cells;
    }
    let mut pos = 0;
    while pos < line.len() && !l.full() {
        let esc = memchr::memchr(0x1b, &line[pos..]).map_or(line.len(), |i| pos + i);
        l.bytes(&line[pos..esc], pos, enc);
        if esc >= line.len() || l.full() {
            break;
        }
        match escape_len(&line[esc..]) {
            Some((n, sgr)) => {
                if let Some(p) = sgr {
                    l.style.apply_sgr(p);
                }
                pos = esc + n;
            }
            None => {
                l.bytes(&line[esc..esc + 1], esc, enc);
                pos = esc + 1;
            }
        }
    }
    l.cells
}

/// Splits laid-out cells into rows of at most `width` columns.
pub fn wrap_rows(cells: &[Cell], width: usize) -> Vec<(usize, usize)> {
    let width = width.max(1);
    let mut rows = Vec::new();
    let (mut start, mut col) = (0, 0);
    for (i, c) in cells.iter().enumerate() {
        let w = c.width as usize;
        if col + w > width && i > start {
            rows.push((start, i));
            start = i;
            col = 0;
        }
        col += w;
    }
    rows.push((start, cells.len()));
    rows
}

/// Glyph for a byte in the hex view's character column.
pub fn hex_glyph(b: u8, enc: Encoding) -> char {
    match enc {
        Encoding::Cp437 if b != 0 => cp437(b),
        Encoding::Ascii7 if (0x20..0x7f).contains(&(b & 0x7f)) => (b & 0x7f) as char,
        _ if (0x20..0x7f).contains(&b) => b as char,
        _ => '.',
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn text(cells: &[Cell]) -> String {
        cells.iter().map(|c| c.ch).collect()
    }

    #[test]
    fn cp437_table_sizes() {
        assert_eq!(CP437_LOW.chars().count(), 32);
        assert_eq!(CP437_HIGH.chars().count(), 128);
        assert_eq!(cp437(0xC9), '╔');
        assert_eq!(cp437(0x01), '☺');
        assert_eq!(cp437(b'A'), 'A');
    }

    #[test]
    fn tabs_expand_to_stops() {
        let c = layout(b"a\tb", Encoding::Utf8, 4, usize::MAX, false);
        assert_eq!(text(&c), "a   b");
        assert_eq!(c[4].byte, 2);
    }

    #[test]
    fn controls_and_invalid_bytes() {
        assert_eq!(text(&layout(b"\x01x\xff", Encoding::Utf8, 8, usize::MAX, false)), "^Ax<FF>");
        assert_eq!(text(&layout(b"\xc1", Encoding::Ascii7, 8, usize::MAX, false)), "A");
        assert_eq!(text(&layout(b"\xe9", Encoding::Latin1, 8, usize::MAX, false)), "é");
    }

    #[test]
    fn utf8_offsets_and_wide_chars() {
        let c = layout("é漢x".as_bytes(), Encoding::Utf8, 8, usize::MAX, false);
        assert_eq!(c.iter().map(|c| c.byte).collect::<Vec<_>>(), vec![0, 2, 5]);
        assert_eq!(c[1].width, 2);
    }

    #[test]
    fn layout_stops_at_max_cols() {
        assert_eq!(layout(b"abcdef", Encoding::Utf8, 8, 3, false).len(), 3);
    }

    #[test]
    fn ansi_colors_are_consumed_and_applied() {
        let c = layout(b"a\x1b[1;31mb\x1b[0mc\x1b]8;;http://x\x07d", Encoding::Utf8, 8, usize::MAX, true);
        assert_eq!(text(&c), "abcd");
        assert_eq!(c[1].ansi.fg, Some(Color::AnsiValue(1)));
        assert!(c[1].ansi.bold);
        assert_eq!(c[2].ansi, AnsiStyle::default());
        assert_eq!(c[2].byte, 13);
        let c = layout(b"\x1b[38;5;208mx\x1b[48;2;1;2;3my", Encoding::Utf8, 8, usize::MAX, true);
        assert_eq!(c[0].ansi.fg, Some(Color::AnsiValue(208)));
        assert_eq!(c[1].ansi.bg, Some(Color::Rgb { r: 1, g: 2, b: 3 }));
        // Without ANSI mode, ESC is shown as a control character.
        assert_eq!(text(&layout(b"\x1b[0m", Encoding::Utf8, 8, usize::MAX, false)), "^[[0m");
    }

    #[test]
    fn wrapping() {
        let c = layout(b"abcdefg", Encoding::Utf8, 8, usize::MAX, false);
        assert_eq!(wrap_rows(&c, 3), vec![(0, 3), (3, 6), (6, 7)]);
        assert_eq!(wrap_rows(&[], 3), vec![(0, 0)]);
    }

    #[test]
    fn wide_chars_never_split_across_rows() {
        let c = layout("a漢b".as_bytes(), Encoding::Utf8, 8, usize::MAX, false);
        assert_eq!(wrap_rows(&c, 2), vec![(0, 1), (1, 2), (2, 3)]);
        // Width 0 is treated as 1 rather than looping forever.
        assert_eq!(wrap_rows(&c, 0).len(), 3);
    }

    #[test]
    fn encoding_cycle_and_labels() {
        let mut e = Encoding::Utf8;
        let mut seen = vec![];
        for _ in 0..4 {
            seen.push(e.label());
            e = e.next();
        }
        assert_eq!(e, Encoding::Utf8);
        assert_eq!(seen, ["UTF-8", "CP437", "Latin-1", "7-bit"]);
    }

    #[test]
    fn single_byte_encodings() {
        let t = |b: &[u8], e| text(&layout(b, e, 4, usize::MAX, false));
        // CP437 shows control bytes as glyphs but still expands tabs.
        assert_eq!(t(b"\x01\t\xb0", Encoding::Cp437), "☺   ░");
        assert_eq!(cp437(0x7f), '⌂');
        // Latin-1's C1 range has no glyphs.
        assert_eq!(t(b"\x85\xa9", Encoding::Latin1), "<85>©");
        // 7-bit turns high-bit controls into caret notation.
        assert_eq!(t(b"\x8d", Encoding::Ascii7), "^M");
    }

    #[test]
    fn zero_width_and_c1_unicode() {
        // A combining accent is dropped; U+0085 (NEL) is shown in hex.
        assert_eq!(text(&layout("e\u{301}\u{85}".as_bytes(), Encoding::Utf8, 8, usize::MAX, false)), "e<85>");
    }

    #[test]
    fn sgr_resets_and_bright_colors() {
        let mut s = AnsiStyle::default();
        s.apply_sgr(b"1;2;3;4;7;91;102");
        assert!(s.bold && s.dim && s.italic && s.underline && s.reverse);
        assert_eq!((s.fg, s.bg), (Some(Color::AnsiValue(9)), Some(Color::AnsiValue(10))));
        s.apply_sgr(b"22;23;24;27;39;49");
        assert_eq!(s, AnsiStyle::default());
        // Empty parameters mean reset, like ESC[m.
        s.bold = true;
        s.apply_sgr(b"");
        assert_eq!(s, AnsiStyle::default());
        // A truncated extended color leaves the previous color alone.
        s.apply_sgr(b"31;38;2;1");
        assert_eq!(s.fg, Some(Color::AnsiValue(1)));
        // Colon-separated forms are accepted too.
        s.apply_sgr(b"38:5:42");
        assert_eq!(s.fg, Some(Color::AnsiValue(42)));
    }

    #[test]
    fn unfinished_and_non_sgr_escapes() {
        // Cursor movement is swallowed without changing the style.
        let c = layout(b"a\x1b[2Kb", Encoding::Utf8, 8, usize::MAX, true);
        assert_eq!(text(&c), "ab");
        assert_eq!(c[1].ansi, AnsiStyle::default());
        // An escape cut off at the end of the line is shown raw.
        assert_eq!(text(&layout(b"a\x1b[31", Encoding::Utf8, 8, usize::MAX, true)), "a^[[31");
        // An unterminated OSC is shown raw as well.
        assert_eq!(text(&layout(b"\x1b]8;;x", Encoding::Utf8, 8, usize::MAX, true)), "^[]8;;x");
        // OSC terminated by ESC \ is consumed.
        assert_eq!(text(&layout(b"\x1b]0;t\x1b\\z", Encoding::Utf8, 8, usize::MAX, true)), "z");
    }

    #[test]
    fn hex_glyphs() {
        assert_eq!(hex_glyph(b'A', Encoding::Utf8), 'A');
        assert_eq!(hex_glyph(0x00, Encoding::Utf8), '.');
        assert_eq!(hex_glyph(0xe9, Encoding::Utf8), '.');
        assert_eq!(hex_glyph(0x00, Encoding::Cp437), '.');
        assert_eq!(hex_glyph(0x03, Encoding::Cp437), '♥');
        assert_eq!(hex_glyph(0xc1, Encoding::Ascii7), 'A');
        assert_eq!(hex_glyph(0x8a, Encoding::Ascii7), '.');
    }
}
