#!/usr/bin/env bash
# Builds the subsetted font embedded in the binary.
#
# The launcher, the clipboard picker and the notification cards need Latin
# text and punctuation — no
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

# Latin-1 and Latin Extended-A, typographic punctuation, euro, arrows: what
# shows up in application names, clipboard entries and notifications.
TEXT='U+0020-007E,U+00A0-017F,U+2010-2027,U+2030-203A,U+20AC,U+2122,U+2190-2195,U+2212'

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
