#!/usr/bin/env bash
# Build the Windows binary from Linux and smoke-test it without a Windows box.
#
# Two Windows-specific things this catches that a Linux `cargo test` cannot:
#   1. The PE *subsystem* — a GUI build must be IMAGE_SUBSYSTEM_WINDOWS_GUI (2)
#      so double-clicking the .exe does not pop a console window; a debug build
#      stays console (3) so `cargo run` prints during development.
#   2. That the real Windows binary actually runs — the headless `render` path
#      (parse -> style -> layout -> paint -> QuickJS) is driven under Wine and
#      must exit 0 and write a PPM.
#
# Requirements (see .cargo/config.toml):
#   rustup target add x86_64-pc-windows-gnu
#   apt-get install gcc-mingw-w64-x86-64 wine64
#
# Usage: scripts/win-test.sh [--release]
set -euo pipefail

PROFILE_DIR=debug
CARGO_PROFILE=()
if [[ "${1:-}" == "--release" ]]; then
  PROFILE_DIR=release
  CARGO_PROFILE=(--release)
fi

TARGET=x86_64-pc-windows-gnu
ROOT="$(cd "$(dirname "$0")/.." && pwd)"
EXE="$ROOT/target/$TARGET/$PROFILE_DIR/cerberus-app.exe"

echo "== cross-building cerberus-app.exe ($PROFILE_DIR) =="
cargo build "${CARGO_PROFILE[@]}" --target "$TARGET" -p cerberus-app --bin cerberus-app

echo "== PE subsystem =="
SUB=$(llvm-readobj --file-headers "$EXE" 2>/dev/null | sed -n 's/.*Subsystem: IMAGE_SUBSYSTEM_WINDOWS_\([A-Z]*\).*/\1/p')
echo "subsystem: $SUB"
if [[ "$PROFILE_DIR" == "release" && "$SUB" != "GUI" ]]; then
  echo "FAIL: release build must be GUI subsystem (no console box), got $SUB" >&2
  exit 1
fi
if [[ "$PROFILE_DIR" == "debug" && "$SUB" != "CUI" ]]; then
  echo "FAIL: debug build must stay console subsystem, got $SUB" >&2
  exit 1
fi

if command -v wine64 >/dev/null 2>&1 || [[ -x /usr/lib/wine/wine64 ]]; then
  WINE=$(command -v wine64 || echo /usr/lib/wine/wine64)
  export WINEPREFIX="${WINEPREFIX:-$ROOT/target/wineprefix}"
  export WINEDEBUG=-all
  TMP="$(mktemp -d)"
  printf '<html><body><h1 style="color:#369">win</h1><p>The quick brown fox.</p></body></html>' > "$TMP/win.html"
  echo "== running render under Wine =="
  ( cd "$TMP" && "$WINE" "$EXE" render --input win.html --out win.ppm ) 2>&1 | grep -v '^wine:' | sed -n '1,4p;/wrote/p'
  [[ -s "$TMP/win.ppm" ]] && echo "OK: Windows binary rendered a page" || { echo "FAIL: no PPM written" >&2; exit 1; }
  rm -rf "$TMP"
else
  echo "(wine not installed; skipped execution test)"
fi
echo "== win-test passed =="
