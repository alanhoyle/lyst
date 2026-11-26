# lyst

A modern, Rust take on Vernon Buerg's classic DOS file viewer **LIST**: a fast,
read-only pager with single-key commands, a status line, hex view, and search.

``` shell
cargo install --path .
lyst                 # browse the current directory
lyst ~/src           # browse a directory
lyst file.txt other.log
some-command | lyst
```

## Features

- File browser like LIST's opening screen: `..`, directories, then files with
  size and date. Arrows/PgUp/PgDn/Home/End to move, Enter to view, ← or
  Backspace to go up, type letters to jump (repeat a letter to cycle),
  `F2` sort (name/extension/size/date), `F3` dotfiles, `~` home.
  `F4` switches between one line per file (size and date) and a compact
  multi-column view (start in it with `--wide`); in columns, arrows move
  through the grid and Backspace goes up a directory.
  From the viewer, `X`/Esc returns to the list, `[`/`]` step through the
  directory's files, `O` opens the browser at the current file, `Q` quits
- Syntax highlighting for ~200 languages (syntect with bat's syntax and theme
  collection, pure Rust, no external tools). Detected from the file name,
  extension, or shebang line; `S` toggles it. The default `ansi` theme uses
  your terminal's palette; `--theme NAME` picks another (`--list-themes`), and
  `--syntax rust` forces a language; add your own Sublime syntaxes and themes
  (see below)
- Input preprocessors, compatible with less's `LESSOPEN`/`LESSCLOSE`
  (`LYST_OPEN`/`LYST_CLOSE` take precedence; `--no-preprocess` disables).
  Supports `|cmd %s`, `||cmd %s`, the `|-` stdin variants, and the
  replacement-file form. Works with `lesspipe` to view archives, PDFs,
  compressed files, etc. The preprocessor's name appears in the status bar
- ANSI color escapes rendered as colors when present (like `less -R`),
  e.g. `git log --color | lyst`; `C` toggles, `--no-ansi` disables
- Opens huge files instantly (memory-mapped, line index built with `memchr`)
- Single-key LIST-style commands: `F` find, `A` again, `L` go to line, `W` wrap,
  `H` hex, `X` exit; vi-style `j`/`k`/`g`/`G`/`/`/`n`/`N` also work
- Literal or regex search (`R`), smart case, match highlighting, wraps around
- Hex dump mode, chosen automatically for binary files
- Encodings: UTF-8, **CP437** (real DOS box-drawing and smiley glyphs), Latin-1,
  and 7-bit (strips the high bit, for old WordStar files). Cycle with `E`
- Line numbers (`#`), tab width (`--tabs`), horizontal scroll
- Follow mode (`T` or `-f`) like `tail -f`
- Bookmarks (`m` + letter to set, `'` + letter to jump)
- Multiple files (`[` / `]`), each keeping its own position
- Mouse: wheel scrolls (Shift+wheel or sideways swipe scrolls horizontally);
  in the browser, click selects, double-click opens, right-click goes up.
  `F9` (or `--no-mouse`) hands the mouse back to the terminal for selecting text
- `--classic` for white-on-blue DOS colors
- Acts like `cat` when stdout isn't a terminal

## Extra syntaxes and themes

`lyst` reads Sublime Text `.sublime-syntax` files from
`~/.config/lyst/syntaxes/` (or `$XDG_CONFIG_HOME/lyst/syntaxes/`, or
`$LYST_CONFIG_DIR/syntaxes/`), including subfolders. User syntaxes take
priority over built-in ones for the same extension. `.tmTheme` files in
`themes/` (or next to the syntaxes) can be chosen with `--theme NAME`.

As in Sublime, a syntax can bring its own color scheme: if `foo.sublime-syntax`
has a `foo.sublime-settings` beside it naming a `"color_scheme"`, files in that
syntax use that theme unless you pass `--theme`. Text the theme doesn't single
out keeps your terminal's colors.

For example, [bioSyntax](https://github.com/bioSyntax/bioSyntax-sublime)
(FASTA, FASTQ, SAM, VCF, BED, GTF, PDB, ...):

```
git clone https://github.com/bioSyntax/bioSyntax-sublime ~/.config/lyst/syntaxes/biosyntax
lyst --list-syntaxes     # user syntaxes are marked *, problems are listed
```

Press `F1` or `?` in the viewer for the full key list, or run `lyst --help` for
the command-line options.
