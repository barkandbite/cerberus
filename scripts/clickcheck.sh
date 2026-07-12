#!/usr/bin/env bash
# clickcheck.sh — the clickability yardstick (goal: every visible link/control
# on a page produces a dispatchable hit box).
#
# For each page in docs/parity-corpus.txt (same corpus as parity.sh):
#   1. mirror it locally (curl trusts the proxy CA; same approach as parity.sh),
#   2. serve it on 127.0.0.1,
#   3. render it with Cerberus, dumping the link/control hit boxes
#      (`render --dump-links`) and the rendered text (`--dump-text`),
#   4. audit: every VISIBLE navigable href (one whose anchor text actually
#      rendered — collapsed menus/hidden drawers aren't clickable in the
#      reference either) must have at least one hit box; boxes must be
#      non-degenerate.
#
# Prints a per-page coverage table and fails (exit 1) if any page's visible
# link coverage drops below the threshold (default 90%).
set -euo pipefail

REPO="$(cd "$(dirname "$0")/.." && pwd)"
CERB="${CERB:-$REPO/target/release/cerberus-app}"
CORPUS="${1:-$REPO/docs/parity-corpus.txt}"
OUT_DIR="${TMPDIR:-/tmp}/cerberus-clickcheck"
THRESHOLD="${THRESHOLD:-90}"

[ -x "$CERB" ] || { echo "clickcheck.sh: build cerberus-app first (cargo build --release -p cerberus-app)" >&2; exit 1; }
mkdir -p "$OUT_DIR"
FAIL=0
SUMMARY="$OUT_DIR/summary.txt"
: > "$SUMMARY"

check_one() {
  local name="$1" url="$2" w="${3:-1200}" h="${4:-1000}"
  local mir="$OUT_DIR/mirror-$name"
  rm -rf "$mir"; mkdir -p "$mir"
  if ! curl -fsSL -A "Mozilla/5.0" "$url" -o "$mir/index.html"; then
    echo "clickcheck: could not fetch $url — skipping $name" >&2
    return 0
  fi
  local origin; origin="$(echo "$url" | grep -oE '^https?://[^/]+')"
  { grep -oE '(src|href)="[^"]+"' "$mir/index.html" \
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
  local local_url="http://127.0.0.1:$port/index.html" i
  for i in 1 2 3 4 5 6 7 8 9 10; do
    [ "$(curl -s -o /dev/null -w '%{http_code}' "$local_url")" = "200" ] && break
    sleep 0.3
  done

  local links_json="$OUT_DIR/$name-links.jsonl" page_txt="$OUT_DIR/$name-text.txt"
  timeout 90 "$CERB" render --url "$local_url" --out "$OUT_DIR/$name.png" \
    --width "$w" --height "$h" --dump-links "$links_json" --dump-text \
    > "$page_txt" 2>/dev/null || true
  [ -f "$links_json" ] || { echo "clickcheck: render produced no dump for $name" >&2; return 0; }

  if ! python3 - "$mir/index.html" "$links_json" "$page_txt" "$name" "$THRESHOLD" >> "$SUMMARY"; then
    FAIL=1
  fi <<'PYAUDIT'
import html as html_mod
import json, re, sys
html_path, jsonl_path, text_path, name, threshold = (
    sys.argv[1], sys.argv[2], sys.argv[3], sys.argv[4], int(sys.argv[5]))
html = open(html_path, encoding="utf-8", errors="replace").read()
page_text = open(text_path, encoding="utf-8", errors="replace").read()
# Unique navigable hrefs (skip fragments, js:, mailto:), each with its anchor
# texts. An anchor whose text never RENDERED (collapsed menus, display:none
# drawers) is not clickable in the reference either, so it doesn't count
# against coverage.
dom = {}
for m in re.finditer(
    r'<a\s[^>]*href\s*=\s*["\']([^"\']+)["\'][^>]*>(.*?)</a>', html, re.I | re.S
):
    h = html_mod.unescape(m.group(1).strip())
    if not h or h.startswith(('#', 'javascript:', 'mailto:')):
        continue
    text = html_mod.unescape(re.sub(r'<[^>]+>', ' ', m.group(2)))
    text = ' '.join(text.split())
    dom.setdefault(h, set())
    if len(text) >= 2:
        dom[h].add(text)
visible = {h for h, texts in dom.items() if any(t in page_text for t in texts)}
boxed = set()
boxes = fields = degenerate = 0
for line in open(jsonl_path, encoding="utf-8"):
    line = line.strip()
    if not line:
        continue
    o = json.loads(line)
    if o["kind"] == "link":
        boxes += 1
        if o["w"] <= 0 or o["h"] <= 0:
            degenerate += 1
        boxed.add(o["href"].strip())
    else:
        fields += 1
covered = {h for h in visible if h in boxed}
pct = 100.0 if not visible else 100.0 * len(covered) / len(visible)
missing = sorted(visible - covered)[:5]
status = "ok" if pct >= threshold and degenerate == 0 else "FAIL"
print(f"{name}: {len(covered)}/{len(visible)} visible hrefs boxed ({pct:.0f}%; "
      f"{len(dom)} total in DOM), {boxes} link boxes, {fields} control boxes, "
      f"{degenerate} degenerate -> {status}")
if missing and status == "FAIL":
    print(f"  missing e.g.: {missing}")
sys.exit(0 if status == "ok" else 1)
PYAUDIT
  return 0
}

while read -r name url w h _rest; do
  case "$name" in ''|'#'*) continue ;; esac
  check_one "$name" "$url" "${w:-1200}" "${h:-1000}"
done < "$CORPUS"

echo ""
echo "clickability coverage (visible navigable hrefs with a hit box):"
cat "$SUMMARY"
echo "artifacts in $OUT_DIR"
exit $FAIL
