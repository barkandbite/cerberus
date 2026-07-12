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
ONE_URL=""; ONE_NAME=""; ONE_W=""; ONE_H=""; HIDE=""; ENGINE=""
while [ $# -gt 0 ]; do
  case "$1" in
    --out-dir) OUT_DIR="$2"; shift 2 ;;
    --corpus)  CORPUS="$2"; shift 2 ;;
    --url)     ONE_URL="$2"; shift 2 ;;
    --name)    ONE_NAME="$2"; shift 2 ;;
    --width)   ONE_W="$2"; shift 2 ;;
    --height)  ONE_H="$2"; shift 2 ;;
    # Layout engine to A/B (block|taffy) during the layout migration.
    --engine)  ENGINE="$2"; shift 2 ;;
    # CSS selector(s) hidden (display:none) in BOTH browsers before diffing, for
    # a clean geometric comparison of a page whose JS would hide them (e.g. a
    # fundraising banner Chrome's JS removes but our mirror keeps) — plan §9.
    --hide)    HIDE="$2"; shift 2 ;;
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
  # Inject a hide rule for the requested selector(s) into <head>, so both
  # browsers render without them (plan §9 clean-geometry comparison).
  if [ -n "$HIDE" ]; then
    local hidecss="<style>$HIDE{display:none!important}</style>"
    # Insert right before </head> (first occurrence).
    python3 - "$mir/index.html" "$hidecss" <<'PYHIDE'
import sys
path, css = sys.argv[1], sys.argv[2]
html = open(path, encoding="utf-8", errors="replace").read()
html = html.replace("</head>", css + "</head>", 1)
open(path, "w", encoding="utf-8").write(html)
PYHIDE
  fi
  # Self-contain the mirror: drop cross-origin scripts/styles/frames (the
  # sandbox can't reach them — Chrome otherwise stalls on their loads until the
  # kill timeout). Both browsers then render the SAME self-contained bytes,
  # which is the yardstick's contract.
  python3 - "$mir/index.html" "$origin" <<'PYSTRIP'
import re, sys
path, origin = sys.argv[1], sys.argv[2]
html = open(path, encoding="utf-8", errors="replace").read()
# Absolute SAME-origin asset URLs become relative, so the mirror step below
# fetches them and both browsers load the styled page (mozilla.org's CSS links
# are absolute; without this the page compared unstyled).
host = origin.split('//', 1)[1]
html = re.sub(r'(src|href)\s*=\s*(["\'])(?:https?:)?//' + re.escape(host) + r'/', r'\1=\2/', html, flags=re.I)
html = re.sub(r'<script\s[^>]*src\s*=\s*["\'](?:https?:)?//[^"\']*["\'][^>]*>\s*</script>', '', html, flags=re.I)
html = re.sub(r'<link\s[^>]*href\s*=\s*["\'](?:https?:)?//[^"\']*["\'][^>]*>', '', html, flags=re.I)
html = re.sub(r'<iframe\s[^>]*src\s*=\s*["\'](?:https?:)?//[^"\']*["\'][^>]*>\s*</iframe>', '', html, flags=re.I)
# Cross-origin images (CDNs) hang Chrome's load event in the sandbox; drop the
# elements so both browsers lay out the same imageless document.
html = re.sub(r'<img\s[^>]*src\s*=\s*["\'](?:https?:)?//[^"\']*["\'][^>]*>', '', html, flags=re.I)
open(path, "w", encoding="utf-8").write(html)
PYSTRIP

  # Mirror same-origin relative assets referenced in the HTML (best-effort). The
  # `|| true` keeps a no-match `grep` (a page with no assets, e.g. example.com)
  # from tripping `pipefail` and aborting the run.
  { grep -oE '(src|href)="[^"]+"|url\(([^)]+)\)' "$mir/index.html" \
    | grep -oE '[^"(]*\.(png|jpg|jpeg|gif|svg|webp|css|js)' \
    | grep -vE '^https?:|^//|^data:' | sort -u || true; } | while read -r p; do
      p="${p#/}"; mkdir -p "$mir/$(dirname "$p")" 2>/dev/null || true
      curl -fsSL -A "Mozilla/5.0" "$origin/$p" -o "$mir/$p" 2>/dev/null || true
    done

  # 8300-9799: stays clear of Chrome's unsafe-port list (10080 = ERR_UNSAFE_PORT).
  local port; port=$(( (RANDOM % 1500) + 8300 ))
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
  local engine_arg=""
  [ -n "$ENGINE" ] && engine_arg="--engine $ENGINE"
  timeout 90 "$CERB" render --url "$local_url" --out "$cerb_png" --width "$w" --height "$h" \
    $engine_arg >/dev/null 2>&1 || true

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
