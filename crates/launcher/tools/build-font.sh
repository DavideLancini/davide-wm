#!/usr/bin/env bash
# Builds the subsetted font embedded in the binary.
#
# The launcher only needs ASCII plus the Latin-1 accents Italian needs — no
# icon set (unlike topbar, the launcher draws no widgets). Subsetting takes the file
# down to a few tens of KB, which is what makes embedding it reasonable.
#
# Source: Noto Sans Mono ExtraCondensed, same choice as topbar (see its
# tools/build-font.sh) — narrower cell than the plain weight, which buys back
# horizontal room for search text and application names.
#
# Requires: pyftsubset (python-fonttools). The result is committed, so a
# normal build needs no font tooling.
set -euo pipefail

SRC="${1:-/usr/share/fonts/noto/NotoSansMono-ExtraCondensed.ttf}"
OUT="$(dirname "$0")/../assets/launcher-font.ttf"

[ -r "$SRC" ] || { echo "source font not found: $SRC" >&2; exit 1; }
mkdir -p "$(dirname "$OUT")"

# Printable ASCII plus the accented letters and symbols that show up in
# application names and search queries.
TEXT='U+0020-007E,U+00A0,U+00B0,U+00B5,U+00C0-00FF,U+2013,U+2014,U+2018,U+2019,U+201C,U+201D,U+2026'

pyftsubset "$SRC" \
  --unicodes="$TEXT" \
  --layout-features='' \
  --no-hinting \
  --desubroutinize \
  --drop-tables+=DSIG,GSUB,GPOS,MATH,BASE,JSTF,LTSH,PCLT,VDMX,hdmx,cvt,fpgm,prep \
  --name-IDs='' \
  --output-file="$OUT"

printf 'source : %s (%s)\n' "$SRC" "$(du -h "$SRC" | cut -f1)"
printf 'subset : %s (%s)\n' "$OUT" "$(du -h "$OUT" | cut -f1)"
