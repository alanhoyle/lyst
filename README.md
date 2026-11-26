# lyst

A modern, Rust tool inspired by Vernon Buerg's classic DOS file viewer **LIST**: a fast,
read-only pager with single-key commands, a status line, hex view, and search.

![lyst browsing the sample files: a CSV and a VCF in table view, a search, then Rust syntax highlighting and the hex view](demo/lyst.gif)

*Recorded with [VHS](https://github.com/charmbracelet/vhs) from
[demo/demo.tape](demo/demo.tape); rerun `vhs demo/demo.tape` after a release
build to update it.*

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
  Backspace to go up, `/` then the start of a name to jump to it (`/` again
  for the next match, Enter to open, Esc to stop), `J` to type a directory
  to go to, with suggestions as you type and Tab completion (Tab again or
  ↑/↓ step through them; `~` and relative paths work, and a file's path
  opens its directory with the file selected), `U` disk usage of the
  selected file or directory (counted in the background, like `du`),
  `F2` sort (name/extension/size/date), `F3` dotfiles, `~` home,
  `O` (capital) opens the selection in its default application, `Q` or Esc
  quits.
  `F4` switches between one line per file (size and date) and a compact
  multi-column view (start in it with `--wide`); in columns, arrows move
  through the grid and Backspace goes up a directory.
  From the viewer, `X`/Esc returns to the list, `[`/`]` step through the
  directory's files, `B` opens the browser at the current file, `Q` quits
- `O` opens the file you're viewing in the OS's default application
  (`open` on macOS, `xdg-open` on Linux, `start` on Windows)
- `V` edits the file you're viewing (`E` or `V` in the browser) in
  `$VISUAL` or `$EDITOR`, falling back to `vi` (Notepad on Windows), then
  reloads it at the same line. The setting can include arguments, like
  `code --wait`. Files that don't look like text get a warning first; press
  the key again to edit anyway
- `W` (capital; `w` or `W` in the browser) opens the file in your default
  web browser, or `$BROWSER` if set: handy for HTML, SVG, PDFs and images
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
- Table view for delimited text, on automatically for `.csv`, `.tsv`, and the
  tab-separated bioinformatics formats VCF, BED (and bedGraph/narrowPeak),
  GTF/GFF and SAM, including compressed ones like `calls.vcf.gz` opened through
  a preprocessor. `D` toggles it for any file, guessing the separator (tab,
  comma, semicolon or `|`). Columns line up and get alternating colors (`S`
  toggles them), ←/→ move a column at a time, and the header row stays pinned
  at the top as you scroll. Metadata lines (`##` in VCF, `@` in SAM,
  `track`/`browser` in BED) are shown as plain text, and the `#CHROM` or
  `#chrom` line is used as the header. Fields wider than 40 columns are cut
  off with `…`. CSV quoting is handled within a line, but a quoted field
  containing a line break ends the row there
- ANSI color escapes rendered as colors when present (like `less -R`),
  e.g. `git log --color | lyst`; `C` toggles, `--no-ansi` disables
- Opens huge files instantly (memory-mapped, line index built with `memchr`)
- Single-key LIST-style commands: `F` find, `A` again, `L` go to line, `w` wrap,
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
