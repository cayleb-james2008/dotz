#!/usr/bin/env bash
# dotz — Windows cross-build (Linux host, no Visual Studio, no sudo).
#
# Why this script exists
# ----------------------
# dotz's Windows link previously failed with 20 undefined C++ STL symbols
# (__std_find_trivial_8, __std_search_1, ...) referenced by the pinned
# `ort`/ONNX Runtime prebuilt. Root cause, established 2026-09-18:
#
#   * cargo-xwin's bundled CRT is OLDER than the MSVC C++ STL that Microsoft's
#     prebuilt onnxruntime.lib was compiled against, so the 20 internal
#     `__std_*` helper symbols were not defined by any library on the link line.
#   * xwin 0.19/0.23 would silently reuse an already-populated cache directory,
#     so asking for a newer CRT had no effect until the cache was fetched clean.
#
# The fix: fetch CRT 14.44.17.14 into a FRESH xwin cache. That CRT's libcpmt.lib
# DOES define the missing symbols (verified: 4 hits for __std_find_trivial_8).
#
# Extra gotcha: cargo-xwin extracts the Windows SDK libs in lowercase on a
# case-sensitive Linux filesystem, but MSVC asks for `PathCch.lib` and
# `DirectML.lib`. Symlinks are created below.
#
# Verified result: dotz.exe builds and RUNS on a real Windows 10 Enterprise
# LTSC VM (RESULT=RUNNING_AFTER_15s). See dotz/OVERHAUL-NOTES.md.
#
# Usage:  bash scripts/windows-build.sh [extra cargo args]
set -euo pipefail

HERE="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
CACHE="${XWIN_CACHE_DIR:-/tmp/xwin-crt-14.44}"
CRT_VERSION="14.44.17.14"
TARGET="x86_64-pc-windows-msvc"

# rustc >= 1.89 is required by current cargo-xwin; 1.90 is installed here.
RUSTUP_TOOLCHAIN="${RUSTUP_TOOLCHAIN:-1.90.0}"

echo "== dotz Windows build =="
echo "  cache:   $CACHE"
echo "  CRT:     $CRT_VERSION"
echo "  toolchain: $RUSTUP_TOOLCHAIN"

# 1. Clean cache on first run so the newer CRT is actually downloaded.
if [ ! -f "$CACHE/xwin/DONE" ]; then
  echo "  fetching CRT/SDK into a clean cache (first run only)…"
  rm -rf "$CACHE"; mkdir -p "$CACHE"
else
  echo "  cache present (delete $CACHE to force a re-fetch)"
fi

# 2. First pass: fetch the CRT/SDK into the cache (build may fail on the
#    lowercase-named SDK import libs; that is expected and fixed in step 3).
cd "$HERE/src-tauri"
set +e
XWIN_CACHE_DIR="$CACHE" rustup run "$RUSTUP_TOOLCHAIN" cargo xwin build \
  --release --target "$TARGET" --xwin-crt-version "$CRT_VERSION" "$@"
set -e

# 3. Case-correct the SDK import libs the MSVC linker asks for by name.
#    cargo-xwin extracts them lowercase; the MSVC link line asks for
#    `PathCch.lib` / `DirectML.lib`, which fails on case-sensitive Linux.
LIBDIR="$CACHE/xwin/sdk/lib/um/x86_64"
for pair in "pathcch.lib:PathCch.lib" "directml.lib:DirectML.lib"; do
  src="${pair%%:*}"; dst="${pair##*:}"
  [ -f "$LIBDIR/$src" ] && ln -sf "$src" "$LIBDIR/$dst"
done

# 4. Final link.
XWIN_CACHE_DIR="$CACHE" rustup run "$RUSTUP_TOOLCHAIN" cargo xwin build \
  --release --target "$TARGET" --xwin-crt-version "$CRT_VERSION" "$@"

echo
echo "  artifact: $HERE/target/$TARGET/release/dotz.exe"
echo "  NOTE: dotz links the DYNAMIC C++ runtime (ONNX Runtime requires /MD),"
echo "        so ship these next to dotz.exe (in the installer's resources):"
echo "          msvcp140.dll msvcp140_1.dll vcruntime140.dll vcruntime140_1.dll"
echo "        They come from the MSVC 14.44 redistributable."
