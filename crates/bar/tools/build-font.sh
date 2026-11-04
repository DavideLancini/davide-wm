#!/usr/bin/env bash
# Builds the subsetted font embedded in the binary.
#
# The bar needs ~190 glyphs out of the font's ~3500: ASCII, the Latin-1 accents
# Italian needs, a few symbols, and one icon per widget. Subsetting takes the
# file down to a few tens of KB, which is what makes embedding it reasonable.
#
# The source is Noto Sans Mono ExtraCondensed: 12.0 px per cell at 24 px
# against FiraCode's 14.8, i.e. 19% narrower for the same nominal size, which
# is what buys back horizontal room on a dense bar.
#
# Requires: pyftsubset (python-fonttools). Run after changing the icon set;
# the result is committed, so a normal build needs no font tooling.
set -euo pipefail

SRC="${1:-/usr/share/fonts/noto/NotoSansMono-ExtraCondensed.ttf}"
OUT="$(dirname "$0")/../assets/topbar-font.ttf"

[ -r "$SRC" ] || { echo "source font not found: $SRC" >&2; exit 1; }
mkdir -p "$(dirname "$OUT")"

# Text glyphs: printable ASCII plus the accented letters and symbols that show
# up in workspace names, clipboard previews and units.
TEXT='U+0020-007E,U+00A0,U+00B0,U+00B5,U+00C0-00FF,U+2013,U+2014,U+2018,U+2019,U+201C,U+201D,U+2026'

# Widget icons are drawn by draw-icons.py, sized to this font's cell. They used
# to be grafted from FiraCode Nerd Font, but those glyphs are authored for a
# wider cell: scaled to fit they either overflowed or shrank away from the
# text. Keep the list in tools/draw-icons.py in sync with src/icons.rs; the
# tests cross-check both.

pyftsubset "$SRC" \
  --unicodes="$TEXT" \
  --layout-features='' \
  --no-hinting \
  --desubroutinize \
  --drop-tables+=DSIG,GSUB,GPOS,MATH,BASE,JSTF,LTSH,PCLT,VDMX,hdmx,cvt,fpgm,prep \
  --name-IDs='' \
  --output-file="$OUT"

# Draw the icons straight into the cell.
python3 "$(dirname "$0")/draw-icons.py" "$OUT" "$OUT.tmp"
mv "$OUT.tmp" "$OUT"

printf 'text   : %s (%s)\n' "$SRC" "$(du -h "$SRC" | cut -f1)"
printf 'icons  : drawn by tools/draw-icons.py\n'
printf 'subset : %s (%s)\n' "$OUT" "$(du -h "$OUT" | cut -f1)"
