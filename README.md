<p align="center">
  <img src="assets/icon.svg" width="120" height="120" alt="puppet icon" />
</p>
<h1 align="center">puppet</h1>
<p align="center"><strong>Give your coding agent eyes and hands for your Windows desktop.</strong></p>

## Install

```powershell
npx -y puppet-mcp@latest --doctor
```

You need Node 22.14 or newer on Windows 10/11 x64 first. The first serve run (no args) provisions the pinned server binary from the tagged GitHub Release, sha256-verified. `--doctor` only prints the manifest with the resolved cache path and exits 0 without downloading or provisioning anything.

### Or let your AI install it

```text
Install the npm package puppet-mcp from nathwn12/puppet into my MCP client config, served with npx puppet-mcp@latest over stdio.
Then verify with npx puppet-mcp@latest --doctor and npm view puppet-mcp version and show me both outputs.
```

### Wire it yourself

Wire the server into your MCP client config:

```json
{
  "mcpServers": {
    "puppet": {
      "command": "npx",
      "args": [
        "puppet-mcp@latest"
      ]
    }
  }
}
```

### Overrides

Use `npx puppet-mcp@latest` for installs. A pinned version is only installable while its GitHub release still exists; the one-release policy deletes older releases, so pinned versions are not a supported long-term install.

Build from source when you want the working tree instead of the Release binary:

```text
git clone https://github.com/nathwn12/puppet
cargo build --release --locked
```

Redirect the cache with `PUPPET_ENGINE_CACHE`, or inspect the pin with no network access:

```powershell
npx -y puppet-mcp@latest --print-manifest
```

The names above come from `puppet-mcp --help`. There is no fully offline fresh install: the first serve run still downloads the pinned binary, so treat these as pins and overrides, not as a no-network setup.

## Use it

List what is open with the `list_windows` tool. Read-only, changes nothing:

```json
{}
```

You get back each visible top-level window with `hwnd`, `pid`, `exe`, `title`, `class`, screen `rect`, `dpi`, and whether the process runs `elevated`. See the screen with the `screenshot` tool:

```json
{
  "max_px": 1280
}
```

You get back an image frame plus a coordinate map that turns image pixels back into screen pixels. Send input with the `do` tool, previewed first:

```json
{
  "target": { "titleContains": "Untitled - Notepad" },
  "actions": [{ "type": "type", "text": "hello", "x": 40, "y": 60 }],
  "dry_run": true
}
```

You get back per-action results with `index`, `type`, `ok`, and `ms`, plus `foregroundUnchanged` and `cursorUnchanged`. Remove `dry_run` to deliver the input for real — to that window, in the background, without touching the pointer, the keyboard or the focus.

## Background-safe by default

Every acting tool (`screenshot`, `window`, `do`, `elements`) accepts one `target` selector and resolves exactly one window for the call:

```json
{ "hwnd": 123456 }
{ "pid": 4242 }
{ "exe": "notepad.exe" }
{ "appId": "Microsoft.Windows.Notepad" }
{ "titleContains": "Untitled" }
{ "className": "Notepad" }
```

All present fields must match (AND). `exe` is case-insensitive with or without `.exe`, `appId` is read from the window's AppUserModelID property store, `titleContains` is a case-insensitive substring, `className` is a case-insensitive equality. A plain selector string (`hwnd:1234`, `exe:notepad.exe`, `title:...`) works as sugar.

- **Target-locked.** The window is resolved once, then revalidated before every action (`IsWindow`, owner pid, process creation time via `GetProcessTimes`, image name, window class, AppUserModelID). The creation time is what separates a reused PID from the real process: a recycled pid that passes every other check is still `target_gone`. An ambiguous or vanished target is a typed error — `ambiguous_target` (with the candidate list) or `target_gone` — and the call does nothing. There is **no fallback to the foreground window**, ever.
- **No-steal input.** With a target, `do` goes UIA patterns first (`Invoke`, `Toggle`, `Expand`, `SelectionItem`, `ValuePattern.SetValue`, `ScrollPattern`), then `PostMessage`/`SendMessage` to the locked hwnd with exact integer client coordinates. `SendInput`, `SetCursorPos`, `SetForegroundWindow`, `BringWindowToTop` and `mouse_event` are only reachable when the call passes `allow_foreground: true` (input) and, for a pointer act, `allow_cursor: true` as well — a pointer act delivered by `SendInput` is foreground input too, so `move` / `click x/y` / `drag` / `scroll x/y` need **both** flags. Without them you get `foreground_required` / `cursor_move_required` before anything is sent, and the desktop is untouched. `window` `action:"focus"` is the one explicit focus verb and it is double-explicit: the verb **and** `allow_foreground: true` are required. `window` `action:"maximize"` also needs `allow_foreground: true`: a maximized window covers its monitor and neither `ShowWindowAsync(SW_MAXIMIZE)` nor `SetWindowPlacement(SW_SHOWMAXIMIZED)` documents a no-activate form, so the default returns `foreground_required` and touches nothing. `key`/`hotkey`/plain `type` never post window messages; they need `allow_foreground: true` (SendInput).
- **Exact and instant.** `click` is one button-down + one button-up at the resolved point (element centre or exact client `x`/`y`); `hover:true` is the only way to ask for a `WM_MOUSEMOVE`. `drag` is down/move/up messages at exact points. `scroll` is `WM_MOUSEWHEEL` at the point. No interpolation, no animation, no pointer journey.
- **Text without keys — but as an explicit opt-in.** `type` with a resolvable element and `set_value` write through UIA `ValuePattern.SetValue`, whose provider may **focus** the control it writes (measured on the fixture EDIT). They therefore require `allow_foreground: true` and return `foreground_required` before any provider call otherwise. `invoke`/toggle/expand/collapse/scroll patterns stay background-safe and need no flag. Keyboard input is **never posted**: `WM_CHAR`/`WM_KEYDOWN` bypass the input queue, desync the target and are ignored by many controls (Microsoft 2025 guidance and the pywinauto tracker both document it), so a plain `type`, `key` or `hotkey` returns `foreground_required` unless the call passes `allow_foreground: true`, which escalates to real `SendInput`. The one exception is a desktop the server itself created and owns (`desktop:"hidden"`) — and only when the target window really lives on it; the permission is earned, never granted by the argument alone.
- **Posted clicks are honest.** A posted `WM_LBUTTONDOWN/UP` is delivery, not proof. `click` on an element reports `delivered:"uia"` (UIA pattern, preferred) or `delivered:"posted"` plus `effect:"verified"`/`"pattern-returned"`/`"unchanged"`/`"unverified"`; when a readable control state did not change after a posted click the action fails with `background_unavailable` instead of claiming success. A raw `x`/`y` click always reports `delivered:"posted"`, `effect:"unverified"`.
- **Capture without activation.** `screenshot` with a target resolves it and captures with `PrintWindow(PW_RENDERFULLCONTENT)` — no activation, no restore, no focus change. A minimized window returns `window_minimized` (never restored); a refused or blank/black frame returns `capture_unavailable` with the reason (never a foreground crop unless `allow_crop_fallback:true` on the foreground window). WGC (Windows.Graphics.Capture) is **deferred**: it needs a WinRT frame pool + D3D11 in this stack and is untestable on a single-monitor box here; PrintWindow covers the verifiable cases.
- **Coordinates.** Under `target`, `x`/`y` are **client-area physical pixels** (`GetClientRect` space, origin at the client top-left); the server is per-monitor DPI aware, so client pixels are device pixels on every monitor. Without a target (the gated foreground path) `x`/`y` stay virtual-desktop screen pixels. Window and element rects stay in screen pixels. Posted message coordinates are signed 16-bit, so anything outside `-32768..32767` is refused with `bad_coordinates` before a message is posted — never silently truncated to a wrong point.
- **Telemetry.** Every acting/capturing call records `GetForegroundWindow` and `GetCursorPos` before and after and returns `foregroundUnchanged` / `cursorUnchanged`. That is telemetry, not the guarantee: the guarantee is that the forbidden call sites live in one module (`src/gate.rs`) behind permit tokens that only the explicit flags can mint, and `tests/no_steal.rs` proves no other source file names those APIs.

Typed codes: `ambiguous_target`, `target_gone`, `bad_target`, `foreground_required`, `cursor_move_required`, `background_unavailable`, `target_required`, `bad_coordinates`, `window_minimized`, `capture_unavailable`.

## What you get

- Six tools to see and drive the desktop: `screenshot`, `list_windows`, `window`, `do`, `cursor`, and `elements` (src/schemas.rs).
- Background-safe, target-locked control: one `target` selector per call, revalidated before every action, no pointer or keyboard injection into your session, no focus stealing, no fallback to the foreground window.
- Coordinates you can trust: every screenshot carries the map that turns image pixels back into screen pixels.
- A small local footprint: one native binary over stdio; the launcher provisions it on the first serve run (no args) and re-verifies the cache each time.
- Input that never asks first — but never touches you either: with a `target`, `do` delivers by UIA pattern or posted window message; foreground input and pointer moves are refused unless you pass `allow_foreground:true` / `allow_cursor:true`, and input aimed at an elevated window is refused with a typed error instead of failing silently.
- A clipboard: `do` `clipboard_set` writes CF_UNICODETEXT (max 1_048_576 chars; over-cap sets fail typed before touching the clipboard) and `clipboard_get` reads it back truncated to 1_048_576 chars; treat `get` results as sensitive (they may be secrets) and never log them beyond the caller.
- Memory caps: `wait_for` `stable` regions refuse edges over 4096px; screenshot grabs refuse edges over 8192px; at most 8 tool calls in flight (`-32603 Server busy` past that).
- Element listing with pages: `elements` accepts `max_depth` (1..32, default 8) and `offset` (default 0); when the 200-entry cap is hit and more controls remain, `next_offset` names the next page, else null.
- Element waits: `do` `wait_for` `kind:"element"` polls for a control (`window` selector optional, `element` name:|auto_id:|path:, `state` appear|disappear|enabled|disabled) with the same timeout semantics as the pixel and window waits.
- Expand/collapse: `do` `invoke` accepts optional `state` `"expand"|"collapse"` driving only the UIA ExpandCollapse pattern (typed error without it; never Invoke), reporting `expand_state` after the action.
- Numeric set: `do` `set_value` falls back to UIA RangeValue for sliders (numeric text, range-checked, read-only honored), reporting the live value in `value`.
- Element text: `elements` with `detail:true` adds TextPattern `text` (capped at 4000 chars, `text_truncated` flag); text never participates in `fp`.
- Relative mouse: `do` `move` accepts `dx`/`dy` (signed deltas, |d|<=32767, no desktop gate) as an alternative to absolute `x`/`y`; never both.
- Held keys: `do` `key` accepts `hold_ms` (0..60000) and `repeat` (1..100, `gap_ms` between repeats) with cancellable sleeps; dry_run assembles every event without sleeping.
- Element scroll: `do` `scroll` accepts `element` (name:|auto_id:|path:) driving UIA ScrollPattern with `axis` v|h instead of the wheel (typed error without the pattern); without it, the plain wheel.
- Provenance: `puppet.py` names in `src` comments are the retired Python prototype this Rust server keeps wire- and behavior-parity with.
- A headless mode: `do` with `desktop:"hidden"` launches one program on a private hidden desktop, drives it with best-effort posted input, captures it, and proves the teardown — see [docs/HEADLESS.md](docs/HEADLESS.md).

## Configure it

You run with no args to serve, or you pass one diagnostic flag:

- `(no args)` — provision the pinned server on the first serve run, then serve MCP over stdio. This is the default.
- `--doctor` — print the pinned manifest and the resolved cache path, then exit 0 without downloading or provisioning anything. Default off with no network access on this path.
- `--print-manifest` — same output as `--doctor`: print the pinned manifest and the resolved cache path, then exit 0 without downloading or provisioning anything. Default off with no network access on this path.
- `--help, -h` — print usage. Default off.

You override two paths with env vars:

- `PUPPET_ENGINE_CACHE` — absolute directory for the engine cache. Default: the engine cache lives under the OS local-app-data directory; `--help` prints the cache path template; the engine fills in the version and bundle when it provisions.
- `PUPPET_MCP_ALLOW_NON_WINDOWS=1` — bypass the Windows-only gate for diagnostics. Default unset, so other platforms are refused. The server still runs on Windows x64 only.

Use `npx puppet-mcp@latest` for installs. A pinned version is only installable while its GitHub release still exists; the one-release policy deletes older releases, so pinned versions are not a supported long-term install.

## Requirements

- Rust 1.85.0 (`rust-toolchain.toml` pins channel 1.85.0 with rustfmt + clippy; `Cargo.toml` rust-version 1.85).
- Node 22.14 or newer (package engines, checked with `node --version`).
- Windows x64. Other platforms are refused unless you set the diagnostics override named in `--help`.
- The first serve run (no args) downloads the pinned server binary from the tagged GitHub Release, sha256-verified before and after extraction (docs/PUBLISHING.md).

## Links

- Repo: https://github.com/nathwn12/puppet
- Package: https://www.npmjs.com/package/puppet-mcp
- Releases: https://github.com/nathwn12/puppet/releases
- Issues: https://github.com/nathwn12/puppet/issues
- Docs: [publishing](docs/PUBLISHING.md), [headless desktop](docs/HEADLESS.md)
- License: MIT, see [LICENSE](LICENSE)

<!-- nathwn12 house footer: identical in every repo. Do not edit. -->
<p align="center">
  <a href="https://github.com/nathwn12"><img src="assets/signature.svg" width="96" alt="nathwn12" /></a>
</p>
<p align="center">
  <sub>Built by <a href="https://github.com/nathwn12">nathwn12</a> · <a href="https://github.com/nathwn12?tab=repositories">All projects</a></sub>
</p>
