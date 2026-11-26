# Sample files

Small files for trying lyst's table view by hand (`lyst samples/people.csv`).

| File | Shows |
| --- | --- |
| `people.csv` | CSV quoting (`"Paris, France"`, `""hi""`), an empty field, accented text, a field cut off with `…` |
| `scores.tsv` | Plain TSV |
| `variants.vcf` | `##` metadata as plain text, `#CHROM` header pinned as you scroll, 11 columns for ←/→ |
| `peaks.bed` | `track`/`browser` lines, a `#chrom` header |
| `semicolons.txt` | No known extension: press `D` and lyst guesses the `;` separator |

`gzip -k samples/variants.vcf` makes a compressed copy to try with a
`LESSOPEN`/`LYST_OPEN` preprocessor such as lesspipe.
