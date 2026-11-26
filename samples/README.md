# Sample files

Small files for trying lyst's table view by hand (`lyst samples/people.csv`).

| File | Shows |
| --- | --- |
| `people.csv` | CSV quoting (`"Paris, France"`, `""hi""`), an empty field, accented text, a field cut off with `…` |
| `scores.tsv` | Plain TSV |
| `variants.vcf` | `##` metadata as plain text, `#CHROM` header pinned as you scroll, 11 columns for ←/→ |
| `peaks.bed` | `track`/`browser` lines, a `#chrom` header |
| `semicolons.txt` | No known extension: press `D` and lyst guesses the `;` separator |
| `inventory.py` | Python syntax highlighting: shebang, docstrings, decorators, f-strings, type hints |
| `peaks.rs` | Rust syntax highlighting: lifetimes, enums, `match`, closures, doc comments, a test module |

The two source files are real programs that work on the data files here:
`python3 samples/inventory.py` reads `semicolons.txt`, and
`rustc samples/peaks.rs && ./peaks samples/peaks.bed` merges the BED peaks.
Press `S` to toggle highlighting, or try `--theme` with a name from
`lyst --list-themes`.

`gzip -k samples/variants.vcf` makes a compressed copy to try with a
`LESSOPEN`/`LYST_OPEN` preprocessor such as lesspipe.
