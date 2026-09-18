# dotz — Windows cross-build

dotz is Windows-native (Tauri 2, NSIS target) and its Windows build now works from Linux,
with **no Visual Studio and no sudo**, and the resulting `dotz.exe` has been **run on a real
Windows 10 VM** (see `OVERHAUL-NOTES.md`).

## Quick start

```bash
bash scripts/windows-build.sh
# -> src-tauri/target/x86_64-pc-windows-msvc/release/dotz.exe
```

Requires `cargo-xwin` (install with `cargo install --locked cargo-xwin`, needs rustc >= 1.89;
this host uses toolchain `1.90.0`) and the `x86_64-pc-windows-msvc` rustup target.

## Why the script is not a one-liner

dotz is the only one of the five apps that links a **C++** dependency: the pinned
`ort` / ONNX Runtime prebuilt. Two things have to be right, and both were found empirically
on 2026-09-18:

1. **CRT version.** The 20 symbols the ONNX prebuilt references are internal MSVC C++ STL
   helpers (`__std_find_trivial_8`, `__std_search_1`, `__std_last_of_trivial_pos_1`, …).
   cargo-xwin's *default* CRT does not define them and the link fails with
   "undefined symbol". MSVC **14.44**'s `libcpmt.lib` **does** define them, so the script
   passes `--xwin-crt-version 14.44.17.14`.
   Gotcha: xwin silently reuses an already-populated cache, so asking for a newer CRT does
   nothing until the cache is fetched clean — the script uses a dedicated cache directory
   (`/tmp/xwin-crt-14.44`) and fetches it fresh.

2. **Case-sensitive SDK lib names.** cargo-xwin extracts the Windows SDK import libraries in
   lowercase on Linux, but the MSVC link line asks for `PathCch.lib` and `DirectML.lib`.
   The script symlinks the correctly-cased names, then links.

## Runtime dependency (important for packaging)

Unlike the other four apps (which the overhaul configured to link the C runtime **statically**
via a Windows-scoped `.cargo/config.toml`), dotz **cannot** use `+crt-static`: ONNX Runtime's
prebuilt is compiled `/MD` (dynamic) and the linker rejects a static-crate build as a
`RuntimeLibrary` mismatch. So dotz keeps the **dynamic** C++ runtime and its installer must
ship these four DLLs next to `dotz.exe`:

```
msvcp140.dll  msvcp140_1.dll  vcruntime140.dll  vcruntime140_1.dll
```

They come from the Microsoft Visual C++ 2015–2022 redistributable (MSVC 14.44 line).
Verified working on a clean Windows 10 Enterprise LTSC install: without them the app exits
immediately with "VCRUNTIME140.dll was not found"; with them, `dotz.exe` launches and stays
running (`RESULT=RUNNING_AFTER_15s`) — `_verify/windows-vm/` holds the receipt.
