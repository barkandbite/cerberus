#!/usr/bin/env bash
# parity.sh — the rendering-parity yardstick (RENDERING_PARITY_PLAN.md, W0).
#
# For each page in docs/parity-corpus.txt (or a single page passed on the CLI):
#   1. mirror it locally  (curl: HTML + same-origin assets; curl trusts the
#      environment proxy CA, whereas Chrome cannot tunnel the policy proxy —
#      hence the local mirror),
#   2. serve it on 127.0.0.1 (in the no-proxy list, so no proxy for either
#      browser),
#   3. render it in headless Chromium  → <name>-chrome.png  (REFERENCE),
#   4. render it in Cerberus           → <name>-cerb.png,
#   5. score the difference with `cerberus-app diff` (Cerberus draws a 36px
#      toolbar Chrome's page screenshot lacks, so we crop it before diffing).
#
# Output PNGs and a parity.csv (name,rmse,mismatch_pct) land in --out-dir.
#
# Usage:
#   scripts/parity.sh [--out-dir DIR] [--corpus FILE]
#   scripts/parity.sh --url <URL> --name <NAME> [--width W] [--height H]
#
# Environment overrides (with sensible sandbox defaults):
#   CHROME  path to a headless Chromium  (default: the pinned pw-browsers build)
#   CERB    path to the cerberus-app binary (default: target/release/cerberus-app)
set -euo pipefail

REPO="$(cd "$(dirname "$0")/.." && pwd)"
CHROME="${CHROME:-/opt/pw-browsers/chromium-1194/chrome-linux/chrome}"
CERB="${CERB:-$REPO/target/release/cerberus-app}"
TOOLBAR_PX=36 # Cerberus's chrome; cropped off the Cerberus image before diffing.

OUT_DIR="${TMPDIR:-/tmp}/cerberus-parity"
CORPUS="$REPO/docs/parity-corpus.txt"
ONE_URL=""; ONE_NAME=""; ONE_W=""; ONE_H=""
while [ $# -gt 0 ]; do
  case "$1" in
    --out-dir) OUT_DIR="$2"; shift 2 ;;
    --corpus)  CORPUS="$2"; shift 2 ;;
    --url)     ONE_URL="$2"; shift 2 ;;
    --name)    ONE_NAME="$2"; shift 2 ;;
    --width)   ONE_W="$2"; shift 2 ;;
    --height)  ONE_H="$2"; shift 2 ;;
    *) echo "parity.sh: unknown arg $1" >&2; exit 2 ;;
  esac
done

if [ ! -x "$CERB" ]; then
  echo "parity.sh: cerberus-app not found at $CERB (build it: cargo build --release -p cerberus-app)" >&2
  exit 1
fi
mkdir -p "$OUT_DIR"
CSV="$OUT_DIR/parity.csv"
echo "name,rmse,mismatch_pct,compared_px" > "$CSV"

# score_one <name> <url> <width> <height>
score_one() {
  local name="$1" url="$2" w="${3:-1200}" h="${4:-1000}"
  local mir="$OUT_DIR/mirror-$name"
  rm -rf "$mir"; mkdir -p "$mir"
  local origin; origin="$(echo "$url" | grep -oE '^https?://[^/]+')"
  if ! curl -fsSL -A "Mozilla/5.0" "$url" -o "$mir/index.html"; then
    echo "parity.sh: could not fetch $url — skipping $name" >&2
    return 0
  fi
  # Mirror same-origin relative assets referenced in the HTML (best-effort). The
  # `|| true` keeps a no-match `grep` (a page with no assets, e.g. example.com)
  # from tripping `pipefail` and aborting the run.
  { grep -oE '(src|href)="[^"]+"|url\(([^)]+)\)' "$mir/index.html" \
    | grep -oE '[^"(]*\.(png|jpg|jpeg|gif|svg|webp|css|js)' \
    | grep -vE '^https?:|^//|^data:' | sort -u || true; } | while read -r p; do
      p="${p#/}"; mkdir -p "$mir/$(dirname "$p")" 2>/dev/null || true
      curl -fsSL -A "Mozilla/5.0" "$origin/$p" -o "$mir/$p" 2>/dev/null || true
    done

  local port; port=$(( (RANDOM % 2000) + 8300 ))
  ( cd "$mir" && python3 -m http.server "$port" --bind 127.0.0.1 >/dev/null 2>&1 ) &
  local srv=$!
  trap 'kill "$srv" 2>/dev/null || true' RETURN
  # Wait for the server to accept connections.
  local local_url="http://127.0.0.1:$port/index.html" i
  for i in 1 2 3 4 5 6 7 8 9 10; do
    [ "$(curl -s -o /dev/null -w '%{http_code}' "$local_url")" = "200" ] && break
    sleep 0.3
  done

  local chrome_png="$OUT_DIR/$name-chrome.png" cerb_png="$OUT_DIR/$name-cerb.png"
  timeout 90 "$CHROME" --headless=new --no-sandbox --disable-gpu --hide-scrollbars \
    --disable-background-networking --no-first-run --force-color-profile=srgb \
    --force-device-scale-factor=1 --window-size="$w,$h" \
    --screenshot="$chrome_png" "$local_url" >/dev/null 2>&1 || true
  timeout 90 "$CERB" render --url "$local_url" --out "$cerb_png" --width "$w" --height "$h" \
    >/dev/null 2>&1 || true

  if [ ! -f "$chrome_png" ] || [ ! -f "$cerb_png" ]; then
    echo "parity.sh: a render failed for $name — skipping score" >&2
    return 0
  fi
  # Score, echoing the human-readable report and appending a CSV row.
  local report
  report="$("$CERB" diff --ref "$chrome_png" --cerb "$cerb_png" --crop-top "$TOOLBAR_PX")"
  echo "$report"
  local rmse pct px
  rmse="$(echo "$report" | sed -nE 's/.*RMSE \(0=match\)  : ([0-9.]+).*/\1/p')"
  pct="$(echo "$report" | sed -nE 's/.*\(([0-9.]+)% over tolerance.*/\1/p')"
  px="$(echo "$report" | sed -nE 's/.*\(([0-9]+) px\)/\1/p' | head -1)"
  echo "$name,${rmse:-NA},${pct:-NA},${px:-NA}" >> "$CSV"
}

if [ -n "$ONE_URL" ]; then
  score_one "${ONE_NAME:-page}" "$ONE_URL" "$ONE_W" "$ONE_H"
else
  [ -f "$CORPUS" ] || { echo "parity.sh: no corpus at $CORPUS" >&2; exit 1; }
  while read -r name url w h _rest; do
    case "$name" in ''|'#'*) continue ;; esac
    score_one "$name" "$url" "${w:-1200}" "${h:-1000}"
  done < "$CORPUS"
fi

echo ""
echo "parity summary (lower RMSE = closer to Chrome):"
column -t -s, "$CSV" 2>/dev/null || cat "$CSV"
echo "artifacts in $OUT_DIR"
