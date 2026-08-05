#!/usr/bin/env bash
# Drive the *actual* Windows GUI (cerberus-app.exe run) under Wine on a headless
# Xvfb display and save a PNG screenshot of the browser window. This is how a
# Linux box / CI sandbox can eyeball the Windows GUI without a Windows machine.
#
# Scope + caveat: Wine's software GDI path and font stack are close to but not
# byte-identical with real Windows, so this is for layout / "does it render"
# / gross-regression checks, NOT pixel-parity judgements against Chrome-on-Windows.
#
# Requirements:
#   rustup target add x86_64-pc-windows-gnu
#   apt-get install gcc-mingw-w64-x86-64 wine64 xvfb imagemagick x11-utils
#
# Usage: scripts/win-gui-shot.sh [OUT_PNG] [URL]
#   OUT_PNG  where to write the screenshot (default: target/win-gui.png)
#   URL      page to open (default: the built-in cerberus:home)
set -u

ROOT="$(cd "$(dirname "$0")/.." && pwd)"
OUT="${1:-$ROOT/target/win-gui.png}"
URL="${2:-}"
TARGET=x86_64-pc-windows-gnu
EXE="$ROOT/target/$TARGET/release/cerberus-app.exe"
DISP=:99
WORK="$(mktemp -d)"

WINE=""
command -v wine64 >/dev/null 2>&1 && WINE=$(command -v wine64)
[ -z "$WINE" ] && [ -x /usr/lib/wine/wine64 ] && WINE=/usr/lib/wine/wine64
for need in Xvfb import xwininfo "$WINE"; do
  if [ -z "$need" ] || { ! command -v "$need" >/dev/null 2>&1 && [ ! -x "$need" ]; }; then
    echo "skip: missing dependency ($need) — see the header for apt/rustup setup" >&2
    exit 3
  fi
done

export WINEPREFIX="${WINEPREFIX:-$ROOT/target/wineprefix}"
export WINEDEBUG=-all

echo "== cross-building release cerberus-app.exe =="
cargo build --release --target "$TARGET" -p cerberus-app --bin cerberus-app || exit 1

XPID=""; WPID=""
cleanup() {
  # Kill only the exact processes we started (each is its own session leader via
  # setsid, so kill the whole group), never a broad pkill that could hit the
  # caller's shell. Then reap so no zombie/half-dead child leaks a signal upward.
  [ -n "$WPID" ] && kill -TERM -- "-$WPID" 2>/dev/null
  [ -n "$XPID" ] && kill -TERM -- "-$XPID" 2>/dev/null
  wait "$WPID" "$XPID" 2>/dev/null
  rm -rf "$WORK"
}
trap cleanup EXIT

# Pick a display whose lock/socket is free, clearing a stale one a killed prior
# run left behind (reusing a lingering display is what made this flaky).
DISP=""
for n in 99 98 97 96 95 94; do
  if pgrep -f "Xvfb :$n\b" >/dev/null 2>&1; then continue; fi   # in use, skip
  rm -f "/tmp/.X${n}-lock" "/tmp/.X11-unix/X${n}" 2>/dev/null   # clear if stale
  DISP=":$n"; break
done
[ -z "$DISP" ] && { echo "FAIL: no free X display" >&2; exit 1; }

setsid Xvfb "$DISP" -screen 0 1280x900x24 -nolisten tcp >"$WORK/xvfb.log" 2>&1 &
XPID=$!
export DISPLAY=$DISP
# Wait for the X server to accept connections instead of a fixed sleep.
for _ in $(seq 1 20); do xdpyinfo -display "$DISP" >/dev/null 2>&1 && break; sleep 0.5; done

# One-shot boot of the prefix if it is brand new (quietly).
[ -d "$WINEPREFIX" ] || "$WINE" wineboot --init >/dev/null 2>&1

ARGS=(run)
[ -n "$URL" ] && ARGS=(run "$URL")
echo "== launching GUI under Wine (virtual desktop) =="
# explorer /desktop hosts the app in one reliably-mapped X window.
setsid "$WINE" explorer /desktop=cerb,1200x820 "$EXE" "${ARGS[@]}" >"$WORK/gui.log" 2>&1 &
WPID=$!

# Wait (up to ~60s) for the cerberus-app.exe window to reach a real size.
GEO=""
for _ in $(seq 1 20); do
  sleep 3
  GEO=$(xwininfo -root -tree 2>/dev/null \
        | grep -i 'cerberus-app.exe' \
        | grep -oE '[0-9]{3,}x[0-9]{2,}\+[0-9-]+\+[0-9-]+' \
        | sort -t x -k1 -rn | head -1)
  [ -n "$GEO" ] && break
done

if [ -z "$GEO" ]; then
  echo "FAIL: cerberus-app.exe window never mapped" >&2
  echo "-- gui.log --" >&2; grep -viE '^wine:|fixme' "$WORK/gui.log" | tail -15 >&2
  exit 1
fi

# GEO is WxH+relX+relY; the desktop is at 0,0 so rel == absolute here.
WH=${GEO%%+*}; REST=${GEO#*+}; X=${REST%%+*}; Y=${REST#*+}
[ "$X" -lt 0 ] && X=0; [ "$Y" -lt 0 ] && Y=0
echo "== window mapped: $GEO -> crop ${WH}+${X}+${Y} =="

# Adaptive settle: a heavy page (e.g. cnn.com is ~11s to style under Wine) is
# still on its "Loading…" screen after a fixed short sleep. Poll the window
# until two consecutive frames are near-identical (load + first paint done),
# capped at ~40s so a genuinely stuck page still returns. Cheap pages converge
# in the first couple of iterations.
shoot() { import -window root "$WORK/root.png" 2>/dev/null &&
  convert "$WORK/root.png" -crop "${WH}+${X}+${Y}" +repage "$1" 2>/dev/null; }
sleep 2
shoot "$WORK/prev.png" || { echo "FAIL: import" >&2; exit 1; }
for _ in $(seq 1 20); do
  sleep 2
  shoot "$OUT" || { echo "FAIL: import" >&2; exit 1; }
  # Per-pixel mean absolute difference between consecutive frames.
  D=$(convert "$WORK/prev.png" "$OUT" -compose difference -composite \
        -colorspace Gray -format "%[fx:mean]" info: 2>/dev/null)
  # Converged when frames differ by < 0.1% average.
  awk -v d="$D" 'BEGIN{ exit (d+0 < 0.001) ? 0 : 1 }' && break
  cp "$OUT" "$WORK/prev.png"
done

MEAN=$(convert "$OUT" -format "%[fx:mean]" info: 2>/dev/null)
echo "screenshot: $OUT ($WH, mean=$MEAN)"
# A fully black/blank frame means the window did not actually paint.
awk -v m="$MEAN" 'BEGIN{ exit (m+0 > 0.02) ? 0 : 1 }' \
  || { echo "FAIL: frame is blank (mean=$MEAN)" >&2; exit 1; }
echo "== win-gui-shot OK =="
# Tear down our servers now and exit with a deterministic 0 (the trap would run
# anyway, but doing it explicitly keeps the exit code out of teardown's hands).
cleanup; trap - EXIT; exit 0
