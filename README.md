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
  size, date and (on Unix) permissions, under a header that totals the
  files' sizes. `h` shows all its keys.
  - Arrows/PgUp/PgDn/Home/End move, Enter views, ← or Backspace goes up,
    `~` goes home, `Q` or Esc quits
  - `/` then the start of a name jumps to it (`/` again for the next match)
  - `F` finds files and directories by name anywhere below the current
    directory, searching in the background as you type; every word typed
    must appear in the path
  - `J` prompts for a directory to go to, with suggestions and Tab
    completion; left empty, it lists recently visited directories, which are
    remembered between runs (in `~/.local/state/lyst/dirs`, the only file
    lyst writes). `-` goes back to the previous directory
  - Space tags files; Enter then views just the tagged ones, with `[`/`]`
    stepping through them. Esc clears the tags
  - `U` totals the disk usage of the selection in the background, like `du`;
    `Y` copies its full path
  - `F2` sorts by name, extension, size or date, `F3` shows dotfiles, and
    `F4` switches to a compact multi-column view (start in it with `--wide`)
  - From the viewer, `X`/Esc returns to the list, `[`/`]` step through the
    directory's files, and `B` opens the browser at the current file
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
  toggles them), ←/→ select a column, scrolling to it as needed, and the header row stays pinned
  at the top as you scroll. Metadata lines (`##` in VCF, `@` in SAM,
  `track`/`browser` in BED) are shown as plain text, and the `#CHROM` or
  `#chrom` line is used as the header. Fields wider than 40 columns are cut
  off with `…`. CSV quoting is handled within a line, but a quoted field
  containing a line break ends the row there. The first column stays in
  view as you scroll right (`P` toggles). `<` and `>` sort the rows by the
  selected column (←/→ move the selection, shown highlighted in the header),
  descending or ascending: numbers by value, and
  text naturally, so `chr2` comes before `chr10`
- `&` shows only the lines matching a pattern (`&!pattern`: lines that
  don't match), like `less`; `&` then Enter shows all lines again. Line
  numbers stay those of the file, and in a table the header stays
- `y` copies the current line (the one with the search match, else the top
  line), `Y` the file's path, using the terminal's clipboard (OSC 52), so
  it works over SSH too
- `=` shows file information: path, size, modification time, permissions,
  owner, line count, line endings, type, and the MIME type recognized from
  the file's contents (with [`infer`](https://crates.io/crates/infer))
- ANSI color escapes rendered as colors when present (like `less -R`),
  e.g. `git log --color | lyst`; `C` toggles, `--no-ansi` disables
- Opens huge files instantly (memory-mapped, line index built with `memchr`)
- Single-key LIST-style commands: `F` find, `A` again, `L` go to line, `w` wrap,
  `H` hex, `X` exit, `h` help; vi-style `j`/`k`/`g`/`G`/`/`/`n`/`N` also work
- Literal or regex search (`R`), smart case, match highlighting, wraps around
- Zip, tar and tar.gz archives show their table of contents, like
  `unzip -l` or `tar -tvf`, without extracting anything: a table of mode,
  owner, size, compressed size (zip), date and name, so `<`/`>` sort it and
  `&` filters it. Archives are recognized by their contents, not their
  names, and take priority over a preprocessor. A large `.tar.gz` shows
  progress while it's read, and Esc cancels
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

Press `h`, `F1` or `?` in the viewer or browser for the full key list, or run `lyst --help` for
the command-line options.
