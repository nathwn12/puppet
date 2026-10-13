# Changelog

All notable changes to this project will be documented in this file.

The format is based on [Keep a Changelog](https://keepachangelog.com/en/1.0.0/),
and this project adheres to [Semantic Versioning](https://semver.org/spec/v2.0.0.html).

## [Unreleased]

### Added

- `do` parameter `desktop: "hidden"` (opt-in, default null): launches the
  program named by `background` (`exe:notepad.exe` or a path) on a private
  desktop, resolves its window with `EnumDesktopWindows` (never the owner's
  `EnumWindows`), drives it through the existing `background` PostMessage
  path, captures it with `PrintWindow(PW_RENDERFULLCONTENT)`, closes that
  process (and any process owning a window on that private desktop), and
  destroys the desktop. Teardown moves the thread off the desktop before
  `CloseDesktop` and proves it gone with a NULL `OpenDesktopW`. The owner's
  desktop, cursor, foreground window, and focus are never touched; omitting
  the parameter leaves plain `do` byte-identical.

## [4.3.0] - 2026-10-12

### Added

- `do` action `set_value`: writes text via UIA `ValuePattern.SetValue`
  on the resolved control (`name:`/`auto_id:`/`path:` selector), with no
  click, no coordinates, and no focus change. It fails closed with typed
  errors (element not found, no ValuePattern, read-only, re-resolve
  failure), reports the target and the value length, and never echoes
  the text value back.

## [4.2.1] - 2026-10-12

### Fixed

- Removed the `ref:` invoke selector: a `ref` token pins only window +
  path and cannot promise element identity, so invoking by `ref:` could
  silently retarget activation to a replacement control when the UI
  changed between enumeration and invoke. Invoke accepts `name:`,
  `auto_id:`, and `path:`; `ref` stays on element entries as a
  per-enumeration correlation identifier, not a durable handle.
- Corrected the `elements` tool description: the compact default entry
  omits `win` (and `hwnd` on the Win32 fallback), which `detail=true`
  retains; fixed the `hwmd` typo to `hwnd`.

## [4.2.0] - 2026-10-12

### Added

- Standalone `elements` tool: zero-pixel structured UI enumeration for a
  window (title/hwmd selector plus `detail`), returning the same
  top-level object the screenshot `elements` block carries as
  structured JSON (per entry, the compact default shape omits `win`,
  and `hwnd` on the Win32 fallback, which `detail=true` retains).
- Per-enumeration `ref` token (`"<hwnd>.<path>"`) on every element from
  both the UIA and Win32 backends, for correlating entries between
  listings; it is not an invoke selector and not an identity guarantee.

## [4.1.0] - 2026-10-11

### Added

- Native Rust port of the Python incumbent — no Python runtime or venv at
  runtime; single static Windows binary.
- 5-tool stdio surface over JSON-RPC 2.0 (`screenshot`, `list_windows`,
  `window`, `do`, `cursor`), reporting
  `serverInfo { name: puppet, version: 4.1.0 }`.
- DXGI desktop-capture path alongside the GDI fallback.

### Measured

- Parity against the Python incumbent: 8/8 core, 60/60 do-parity, 9/9, 18/18
  invoke resolution (measured on the author's machine).
- Head-to-head bench against a 10-server field: ranked 2/10.
