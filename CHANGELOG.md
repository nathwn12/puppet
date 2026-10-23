# Changelog

All notable changes to this project will be documented in this file.

The format is based on [Keep a Changelog](https://keepachangelog.com/en/1.0.0/),
and this project adheres to [Semantic Versioning](https://semver.org/spec/v2.0.0.html).

## [5.0.0] - 2026-10-23

### Changed (breaking)

- **Background text now requires `allow_foreground:true`.** `type` with an
  element and `set_value` write through UIA `ValuePattern.SetValue`, whose
  provider may focus the control it writes. They are no longer
  background-safe: without the flag they return `foreground_required`
  before any provider call. `invoke`/`toggle`/`expand`/`collapse`/`scroll`
  patterns stay background-safe.
- **Visible `maximize` requires `allow_foreground:true`.** A maximized
  window covers its monitor and Win32 documents no no-activate form, so the
  default returns `foreground_required` and the window is untouched. This
  is a gate, not a focus-safety claim: passing the flag makes the verb an
  explicit opt-in whose activation is measured and reported, never
  silent.
- **Posted keyboard removed from the default path.** `WM_CHAR`/`WM_KEYDOWN`
  bypass the input queue, desync the target and are ignored by many
  controls, so plain `type`/`key`/`hotkey` are never posted: they need
  `allow_foreground:true` and escalate to real `SendInput`. Posted keyboard
  survives only on a desktop the server created *and* that the target
  window actually lives on — `desktop:"hidden"` is earned by desktop-name
  comparison, never granted by the argument alone.
- Typed `foreground_required` / `cursor_move_required` refusals replace
  silent fallbacks: the foreground family is refused *before* anything is
  sent, and pointer acts need both `allow_cursor:true` and
  `allow_foreground:true`.

### Added

- **Target lock.** Every acting verb takes
  `target={hwnd?,pid?,exe?,appId?,titleContains?,className?}` (or a selector
  string) that resolves ONE window for the call and is revalidated before
  every action — `hwnd`, `pid` with process start time, image name and
  class, plus `appId`. Ambiguous or vanished targets are typed
  (`ambiguous_target` / `target_gone`); there is no foreground fallback.
- **No-steal default input path.** Under a lock, clicks/keys go UIA-pattern
  first, then `PostMessage`/`SendMessage` with exact client coordinates —
  no `SendInput`, no cursor movement, no activation.
- **Non-activating capture.** `screenshot` can capture a window by
  selector without activating it, reporting
  `foregroundUnchanged`/`cursorUnchanged`.
- **Posted-click honesty.** Every click reports what it proved: `delivered`
  (`uia` | `posted`) and `effect`
  (`verified` | `pattern-returned` | `unchanged` | `unverified`). A posted
  element click whose readable control state did not change fails with
  `background_unavailable`; a raw x/y click always reports
  `delivered:"posted"`, `effect:"unverified"`.

## [4.5.5] - 2026-10-23

### Fixed

- Release gate: the pre-publish cleanroom installed the tarball and booted
  it, but the launcher fetches `runtime.archive` from the GitHub Release —
  an asset the `publish` job creates *after* `needs: [build]`. Any new
  version therefore 404'd at the gate, which is what turned the 4.5.4 tag
  red. The launcher gained `PUPPET_ENGINE_BUNDLE`, a diagnostics-only
  override that reads a local zip instead of downloading, and
  `scripts/cleanroom-check.mjs` gained `--bundle` to point it at the
  archive built in the same run. The size and sha256 checks are unchanged
  and still run on the local bytes, and `--registry` mode scrubs the
  override, so the gate keeps its teeth. The gate is not weakened: it is
  fed the real artifact.

### Added

- `release.yml` accepts `workflow_dispatch` with `version` and `dry_run`
  (default `true`), so the whole build + pre-publish gate can be rehearsed
  under a `v<version>` tag before one exists. A dry run skips exactly the
  irreversible steps — release creation, npm publish, outgoing-release
  deletion — and the public-registry gate that depends on them.

No engine behaviour change.

## [4.5.4] - 2026-10-23

### Changed

- Version-only rebuild of unchanged engine source, cut to exercise the
  release gate. The only change since `v4.5.3` is CI: releases are now
  gated on a fresh-profile install and boot. No behaviour change, no
  dependency change.

## [4.5.3] - 2026-10-21

### Fixed

- fix: npm package shipped a manifest with stale engine hashes; packaged manifest now verified against the built assets before publish.

## [4.5.2] - 2026-10-21

### Added

- Launcher `--version` prints `puppet-mcp <version>` and exits 0
  (previously exited 2 with usage); `--doctor` also reports the launcher
  version, the resolved engine path, the engine version parsed from the
  cache directory name, and whether they match.
- Fail closed on version skew: before downloading or spawning, the
  launcher compares its `package.json` version against the version in the
  engine cache path it is about to run and refuses with the re-provision
  command instead of silently running a stale engine.
- Global installs refused by design: `preinstall`/`postinstall` guards
  reject `npm i -g` so a global copy can never shadow a pinned `npx` run.
  Install with `npx puppet-mcp@4.5.2`.

## [4.5.1] - 2026-10-20

### Fixed

- Docs: one-release policy — use `npx puppet-mcp@latest`; a pinned version is only installable while its GitHub release exists.

## [4.4.1] - 2026-10-18

### Fixed

- Docs-only patch: the README rewritten to the house skeleton with the
  npx install front door (`npx -y puppet-mcp@latest`); no behaviour
  changes.

## [4.4.0] - 2026-10-18

### Added

- `instructions` in the `initialize` result: a usage contract for a
  client/model that has never seen this server — the six tools in one line
  each, the coordinate rule with the image-to-screen formula, batch +
  `dry_run`, and read-only-first on the owner's real desktop.

### Fixed

- Typed stale-element-ref refusal: every `elements` entry carries `fp`, a
  stable identity fingerprint, and `invoke`/`set_value` accept `expect_fp`
  to refuse a stale ref instead of acting on the wrong control.
- UIPI/elevation refusal: every `list_windows` entry carries `elevated` so
  a caller can tell an elevated target apart from an unresponsive one, and
  UIPI-blocked input is refused with a typed error instead of silently
  doing nothing.
- Test fix: the foreground assertions are environment-independent (new
  `src/fixture.rs`), so the suite passes the same on a bare runner as on
  the owner's desktop.

## [4.5.0] - 2026-10-20

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
- Clipboard: `do` `clipboard_set` writes CF_UNICODETEXT (max 1_048_576
  chars; over-cap sets fail typed before touching the clipboard) and
  `clipboard_get` reads it back truncated to 1_048_576 chars, NUL-checked
  against the GlobalSize bound.
- Elements paging: `elements` accepts `max_depth` (1..32, default 8) and
  `offset` (default 0); when the 200-entry cap is hit and more controls
  remain, `next_offset` names the next page, else null.
- `wait_for` `kind:"element"`: polls for a control (`window` selector
  optional, `element` name:|auto_id:|path:, `state`
  appear|disappear|enabled|disabled) with the same timeout semantics as
  the pixel and window waits; a timeout reports `satisfied:false`, never
  an error.
- UIA actuation: `do` `invoke` accepts `state` `"expand"|"collapse"`
  driving only the UIA ExpandCollapse pattern (typed error without it);
  `do` `set_value` falls back to UIA RangeValue for sliders (numeric
  text, range-checked, read-only honored); `elements` with `detail:true`
  adds TextPattern `text` (capped at 4000 chars, `text_truncated` flag).
- Structured output: tool results carry typed JSON content blocks
  (coordinate map, element lists, per-action results); dual-era MCP
  protocol versions negotiated at `initialize`.
- Input: `do` `key` holds Shift when the layout requires it; DXGI
  desktop-capture fast path with GDI fallback; `scroll` supports the
  horizontal wheel; `move` accepts relative `dx`/`dy`; `key` accepts
  `hold_ms`/`repeat`/`gap_ms`; `scroll` accepts `element` driving UIA
  ScrollPattern instead of the wheel.
- Gates and caps: toolchain pinned to 1.85.0 (rustfmt + clippy), CI with
  fmt/clippy/gitleaks/build/test; memory caps (`stable` regions 4096px,
  grabs 8192px, shared overflow-safe region validator); `initialize`
  `instructions` now enumerate all 16 `do` action types.

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
