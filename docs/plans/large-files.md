# Plan: lazy random access for huge and compressed files

Status: **deferred** (planned November 2026, not started).

## Problem

A 5 GB `calls.vcf.gz` that decompresses to 40 GB can't be viewed well
today:

- lyst has no decompression of its own. A `.gz` opens through a
  `LESSOPEN`/`LYST_OPEN` preprocessor (e.g. `lesspipe.sh`), and lyst reads
  all of the preprocessor's output into memory before showing anything.
  That means 40 GB of RAM and a wait for the whole file.
- Without a preprocessor, the file opens as gzip bytes in hex view.
- Even uncompressed files keep one 8-byte entry per line in the line index.
  For 100 million lines that's about 800 MB, although the file itself is
  memory-mapped.

## Goal

A 5 GB `.vcf.gz` opens instantly, scrolls and jumps anywhere in a few tens
of MB of memory, and keeps table view, search, filter and sort. Whole-file
operations stream through the file in the background instead of loading it.

## Where lyst stands

- `Document` (`src/doc.rs`) holds the whole file as one contiguous slice,
  memory-mapped or owned, plus `line_starts`, the start of every line.
- Whole-file users of `doc.bytes()`:
  - forward search (`App::find`, a regex over all the data)
  - hex view (`draw_hex`)
  - binary and ANSI detection (`looks_binary`, `has_ansi`)
  - the `=` info page (line-ending sample)
  - `cat` mode (`main.rs`)
  - `Source::load`'s check that preprocessor output equals the original
- About 25 call sites use `doc.line(i)`, which returns a slice borrowed from
  that buffer: in `app.rs`, `table.rs`, `rows.rs`, `highlight.rs` and
  `search.rs`.
- Syntax highlighting (`DocHighlighter::spans`) parses from line 0 in
  strides, so jumping to the end of a huge file would parse everything.
- Background work with a progress display already exists for the browser's
  disk usage (`src/usage.rs`, polled from `tick`), which is a pattern to
  reuse.

## Phase 1: read the file in pieces (no new formats)

This phase changes how files are stored, not what's supported. On its own,
it makes huge uncompressed files fast and light.

1. **Hide the storage behind `Document`.** Storage becomes a byte range
   read on demand, with backends for memory-mapped files, in-memory data
   (stdin and preprocessor output), and later compressed files.
   - `line(i)` returns `Cow<[u8]>`: borrowed when memory-mapped, copied
     otherwise.
   - The whole-file users above switch to `read(range)`, `head(n)`, or
     streaming (`copy_to(writer)` for `cat` mode).
2. **Sparse line index.** Record every 1,024th line start instead of every
   line. Line *i* is found by scanning forward from the nearest record with
   `memchr`, at most 1,023 lines. That's about 800 KB for 100 million lines.
3. **Background indexing.** The first screen appears as soon as its lines
   are indexed.
   - The status bar shows "Line 12 of ≥ 3,400,000…" until counting
     finishes.
   - Jumps past the indexed part (`G`, `L` with a large number, `90%`) show
     progress and wait for it; Esc cancels.
   - `View.top_line` must cope with a line count that's still growing.
4. **Streaming search.** Search runs in line-aligned chunks on a background
   thread, with progress and Esc to cancel. Matches can't span lines, which
   the one-line prompt already implies; the help text should say so.
5. **Highlighting limit.** Turn syntax highlighting off above roughly
   50 MB, with a status note. Table view doesn't use it.

Tests: the existing suite passes unchanged, plus `Document` tests for:

- lines crossing an index record
- CRLF line endings
- no trailing newline
- empty files
- line-for-line equality with a full index

## Phase 2: bgzip (BGZF)

Almost every real `.vcf.gz` is bgzip-compressed, because tabix and
bcftools require it.

1. **Detection by content, not name.** A bgzip file is gzip
   (`1f 8b 08`, FLG with FEXTRA) with a `BC` extra subfield in each block
   header. Such files skip the preprocessor, which would decompress all of
   them into memory.
2. **Block map.** Each header's `BSIZE` gives the block's compressed size.
   The block's last 4 bytes (`ISIZE`) give its uncompressed size, at most
   64 KB. Scanning only those gives each block's offset in both the
   compressed file and the decompressed text, without decompressing.
   - A 5 GB file has roughly 300,000 blocks, about 5 MB of map, scanned in
     a few seconds in the background.
   - If `file.gz.gzi` (from `bgzip -i`) exists, read it instead: a count,
     then pairs of little-endian u64 (compressed offset, uncompressed
     offset).
   - The empty 28-byte end-of-file block must be handled.
3. **Blocks decompressed on demand.** An LRU cache of about 256 blocks
   (16 MB). The phase 1 line index runs over the decompressed text, using
   blocks decompressed in parallel on all cores. Expect roughly
   10–30 seconds to count every line of 40 GB on a laptop, with the screen
   usable meanwhile.
4. **Dependency.** `flate2` with the pure-Rust `zlib-rs` backend.
5. **Small extras:**
   - `cat` mode decompresses as it streams, like `zcat`
   - the `=` page shows "BGZF, N blocks, 5.1 GB → 40.3 GB"
   - follow mode stays unavailable for compressed files.

Tests: a small bgzip writer in the test code (`bgzip` isn't assumed to be
installed), covering:

- lines and CRLF pairs split across block boundaries
- the end-of-file block
- a `.gzi` index
- line-for-line equality with an in-memory document

Also benchmark a generated multi-GB VCF for time to first screen and peak
memory (`/usr/bin/time -l`), and confirm the estimates above before putting
numbers in the README.

## Phase 3: filter and sort at scale

- **`&` filter.** Stream through the file in the background with progress
  and Esc. Keep only the matching lines (`rows.rs` builds a new document of
  them), so memory follows the result size. Stop with a message above about
  1 GB of matches.
- **Sort.** It has to hold every row's key and position. Cap it at around
  5 million rows, and past that refuse with a hint to filter first.

## Phase 4 (optional): plain gzip

Plain gzip has no blocks, so it can't be read from the middle. Options,
simplest first:

- **(a) Today's behavior.** Preprocessor output goes into memory. Fine for
  small files; keep it as the default.
- **(b) Temporary file.** Decompress to a temporary file and memory-map it,
  after checking free disk space and deleting it on exit.
- **(c) zran-style resume points.** One background pass that saves a resume
  point (bit offset plus a 32 KB window) every ~16 MB of output, about
  80 MB of memory for 40 GB. This needs low-level inflate state that
  `flate2` doesn't expose, so it's the riskiest item; treat it as research.

## Risks

- **Phase 1 touches the core everywhere:** the `line()` call sites, and a
  view position that has to cope with a growing line count. Keeping phase 1
  free of other changes contains that.
- **The event loop needs a general pattern for background jobs** with
  progress and cancel.
- **Block counts, decompression speed and memory figures are estimates**
  that assume typical bgzip blocks and an SSD.

## Order

Phase 1, then 2, then 3, with a commit after each. Each phase is useful on
its own:

- after phase 1, huge plain files are fast
- after phase 2, `.vcf.gz` files are
- after phase 3, filtering and sorting scale.
