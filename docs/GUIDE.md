# puppet guide

Depth for the six tools: how a call is provisioned and locked, every option, every
argument and return shape, the `do` burst, and what the desktop refuses to give up.

For install, see [the README](../README.md).

## How it works

### Provisioning model

`puppet-mcp` on npm is a dependency-free Node launcher plus a pinned manifest
(`npm/engine-manifest.json`) — the published package holds **no server bytes**. On the
first serve run (no args) the launcher:

1. Refuses any platform except Windows x86_64 (override for diagnostics only:
   `PUPPET_MCP_ALLOW_NON_WINDOWS=1`).
2. Resolves the engine cache directory,
   `%LOCALAPPDATA%\puppet\engine\<serverVersion>-<bundleSha8>\`.
3. Downloads **only** `runtime.archive` from the tagged GitHub Release and verifies its
   size and sha256 **before and after** extraction, then verifies the extracted
   `puppet.exe` against the manifest hash.
4. Spawns the server with stdio passthrough. All launcher chatter goes to stderr; stdout
   belongs to the MCP channel.

Later runs reuse the cache and re-verify it every time. A version mismatch between the
launcher, the manifest and the cached engine leaf fails closed **before** any download or
spawn — a stale cache path (for example via an override) is refused, never run. A
mismatched hash writes nothing.

The pin is one release deep: the release workflow deletes older releases, so a pinned
version is only installable while its GitHub release still exists. `@latest` is the
supported install; a pin is a reproducibility aid, not a long-term address. There is no
fully offline fresh install — the first serve run still downloads.

### The target lock

Every acting tool (`screenshot`, `window`, `do`, `elements`) accepts one `target`
selector and resolves **exactly one** window for the call:

```json
{ "hwnd": 123456 }
{ "pid": 4242 }
{ "exe": "notepad.exe" }
{ "appId": "Microsoft.Windows.Notepad" }
{ "titleContains": "Untitled" }
{ "className": "Notepad" }
```

All present fields must match (AND). `exe` is case-insensitive with or without `.exe`,
`appId` is read from the window's AppUserModelID property store, `titleContains` is a
case-insensitive substring, `className` is a case-insensitive equality. A plain selector
string (`hwnd:1234`, `exe:notepad.exe`, `title:...`, `class:...`) works as sugar.

The window is resolved once, then **revalidated before every action**: `IsWindow`, owner
pid, process creation time (`GetProcessTimes`), image name, window class, AppUserModelID.
The creation time is what separates a reused PID from the real process — a recycled pid
that passes every other check is still `target_gone`. An ambiguous or vanished target is a
typed error (`ambiguous_target` with the candidate list, or `target_gone`) and the call
does nothing. There is **no fallback to the foreground window, ever**.

### Why it does not steal focus

With a target, acting verbs go UIA patterns first (`Invoke`, `Toggle`, `ExpandCollapse`,
`SelectionItem`, `ValuePattern.SetValue`, `ScrollPattern`), then `PostMessage` /
`SendMessage` to the locked hwnd with exact integer client coordinates.
`SendInput`, `SetCursorPos`, `SetForegroundWindow`, `BringWindowToTop` and `mouse_event`
are reachable only when the call mints the right permit:

| Verb | Needs |
|---|---|
| `invoke`, `toggle`, expand/collapse, `scroll` with `element` | nothing — pattern-driven |
| `type` with a resolvable element, `set_value` | `allow_foreground:true` (the UIA provider may focus the control it writes) |
| plain `type`, `key`, `hotkey` | `allow_foreground:true` (escalates to real `SendInput`) |
| `move`, `click` with `x`/`y`, `drag`, `scroll` with `x`/`y` | **both** `allow_foreground:true` and `allow_cursor:true` |
| `window` `action:"focus"` | the verb **and** `allow_foreground:true` |
| `window` `action:"maximize"` | `allow_foreground:true` (no documented no-activate maximize form) |
| `clipboard_set` | `allow_clipboard:true` |

Without the flags you get `foreground_required`, `cursor_move_required` or
`clipboard_write_required` **before anything is sent**, and the desktop is untouched.
`window` `move`/`resize`/`minimize`/`restore`/`topmost`/`close` use `SWP_NOACTIVATE`,
`SW_SHOWMINNOACTIVE`, `SW_SHOWNOACTIVATE` or `WM_CLOSE` and never activate or raise.

The guarantee is structural, not a promise: every forbidden call site lives in one module
(`src/gate.rs`) behind permit tokens only the explicit flags can mint, and
`tests/no_steal.rs` proves no other source file names those APIs. Every acting or capturing
call also records `GetForegroundWindow` and `GetCursorPos` before and after and returns
`foregroundUnchanged` / `cursorUnchanged` — that is telemetry, not the guarantee.

### Coordinates

Under a target, `x`/`y` are **client-area physical pixels** (`GetClientRect` space, origin
at the client top-left); the server is per-monitor DPI aware, so client pixels are device
pixels on every monitor. Without a target (the gated foreground path) `x`/`y` stay
virtual-desktop screen pixels. Window and element rects stay in screen pixels. Posted
message coordinates are signed 16-bit, so anything outside `-32768..32767` is refused with
`bad_coordinates` before a message is posted — never silently truncated to a wrong point.

### Caps

| Limit | Value |
|---|---|
| Screenshot edge | 8192 px (`bad_region` past it) |
| `wait_for` `stable` region edge | 4096 px |
| `elements` entries per page | 200 |
| Element `text` (detail) | 4000 chars, `text_truncated` flag |
| Clipboard text | 1 048 576 chars, typed failure past it |
| Tool calls in flight | 8 (`-32603 Server busy` past that) |
| `elements` walk depth | 1..32, default 8 |

Concurrency: 4 workers; only the mutating tools (`do`, `window`) serialize on a shared
input lock, so a `screenshot` can run while a `do` `wait` sleeps. Long sleeps never block
`ping`, `initialize`, `tools/list` or a cancellation. Past 8 calls in flight the next one
gets `-32603 Server busy` immediately — it is refused, not queued.

## Configuration

### Launcher flags (`puppet-mcp`)

| Flag | Default | Effect |
|---|---|---|
| *(no args)* | — | Provision the pinned server on the first run, then serve MCP over stdio. |
| `--doctor` | off | Print the launcher version, pinned manifest, resolved engine path, parsed engine version and whether they match; perform no fetch; exit 0. |
| `--print-manifest` | off | Alias for `--doctor`. |
| `--version` | off | Print `puppet-mcp <launcherVersion>`, exit 0. |
| `--help`, `-h` | off | Print usage, including the cache path template and every override name. |

### Server flags (`puppet.exe`)

| Flag | Default | Effect |
|---|---|---|
| *(no args)* | — | Serve MCP over stdio. This is what an MCP client uses. |
| `--version`, `-V` | off | Print `puppet <version> (<git sha>)` and exit 0 before any Win32 call or the stdio pump. |

### Environment variables

| Variable | Default | Effect |
|---|---|---|
| `PUPPET_ENGINE_CACHE` | `%LOCALAPPDATA%\puppet\engine\<serverVersion>-<bundleSha8>` | Absolute directory for the engine cache. A value that already names an engine leaf (`<semver>-<sha8>`) is used as-is, so a stale pin is refused by the version gate instead of being provisioned past. |
| `PUPPET_ENGINE_BUNDLE` | unset | Names a local zip to verify and extract instead of downloading. Pre-publish diagnostics only — size and sha256 are still checked. |
| `PUPPET_MCP_ALLOW_NON_WINDOWS` | unset | `1` bypasses the Windows-only **launcher** gate for diagnostics. The server still runs on Windows x64 only. |
| `LOCALAPPDATA` | set by Windows | Cache root. Unset means the cache directory cannot resolve and the launcher fails with a fixable message. |
| `PUPPET_LIVE_DESKTOP` | unset | `1` enables the live-gated integration tests (`hidden_desktop`, `clipboard`, and the live halves of the element tests). Development only; the suite skips them cleanly without it. |

Tool-level defaults live with each tool below.

## Every tool

All six are declared in `src/schemas.rs` with a real `outputSchema`; the tool descriptions
are the contract.

### `screenshot`

Capture for a vision model. Read-only: never activates, raises, restores or moves the
pointer.

| Argument | Default | Meaning |
|---|---|---|
| `target` | `null` | Selector object or string; wins over `window`. |
| `display` | `-1` | `-1` whole virtual desktop, `0..N` one monitor (out of range → `bad_display`). |
| `window` | `null` | Selector for a side-effect-free window capture; wins over `display`. |
| `region` | `null` | `[left,top,right,bottom]` in virtual-desktop px, applied after the window crop. |
| `max_px` | `1280` | Long-edge clamp, aspect-preserving, never upscales; `null` returns native pixels (max 8192). |
| `fmt` | `"jpeg"` | `png`, `jpeg` (`jpg` is an alias). |
| `quality` | `80` | JPEG quality 1..100; PNG ignores it. |
| `grid` | `false` | Overlay a light 128 px lattice. |
| `allow_crop_fallback` | `false` | Permit a foreground-only desktop crop to `dwm_rect` when `PrintWindow` refuses. |
| `elements` | `false` | Append a third content block with the element model. |

**Returns** `[image, coordinate_map_json]`, plus the element block when `elements=true`.
The map carries `source{x,y,w,h,kind}`, `scale`, `monitors`, `cursor`, `window`, `capture`,
`formula`, `foregroundUnchanged`, `cursorUnchanged`. Map image pixels back with:

```
screen_x = source.x + round((img_x + 0.5) * source.w / image.w)   // same for y
```

For window captures `source.x/y` equal the `dwm_rect` origin and `source.kind` is
`"window"`. Errors are `["error: <code>: <detail>"]` with
`bad_display|bad_region|bad_window|ambiguous_window|empty_crop|capture_failed|ambiguous_target|target_gone|window_minimized|capture_unavailable`.

```json
{ "target": { "titleContains": "Untitled - Notepad" }, "max_px": 1280 }
```

### `list_windows`

Read-only. No arguments; nothing is touched.

**Returns** `{ windows: [{ hwnd, pid, exe, title, class, rect, dwm_rect, client_rect,
minimized, maximized, foreground, cloaked, visible, dpi, elevated }] }`. `rect` is
`GetWindowRect` (compatibility, includes the invisible border); `dwm_rect` is
`DWMWA_EXTENDED_FRAME_BOUNDS` (the mapping truth, else `rect` with `dwm_fallback:true`);
`client_rect` is in screen space.

```json
{}
```

### `window`

| Argument | Default | Meaning |
|---|---|---|
| `action` | required | `focus`, `move`, `resize`, `minimize`, `maximize`, `restore`, `close`, `topmost`. |
| `target` | `null` | Selector; wins over `title`/`hwnd`. |
| `title` | `null` | Selector string. |
| `hwnd` | `null` | Window handle. |
| `x`, `y` | `null` | Required when `action:"move"`. |
| `width`, `height` | `null` | Required when `action:"resize"`. |
| `allow_foreground` | `false` | Required for `focus` and `maximize`. |

**Returns** `{ ok, action, hwnd, x, y, width, height, error, candidates, code,
foregroundUnchanged, cursorUnchanged, target_locked }`. An ambiguous match is `ok:false`
with the `candidates` list, not a guess.

```json
{ "action": "resize", "target": { "exe": "notepad.exe" }, "width": 1000, "height": 700 }
```

### `do`

Burst tool — one MCP round-trip runs an ordered batch. Full action list and ordering in
[the `do` burst](#the-do-burst).

| Argument | Default | Meaning |
|---|---|---|
| `actions` | required | Array of action objects, run in order. |
| `target` | `null` | Selector locking one window for the whole call. |
| `dry_run` | `false` | Validate and report; inject nothing. |
| `settle_ms` | `0` | Sleep between actions. |
| `focus_guard` | `false` | Restore the entry foreground window if it changed during the call. |
| `background` | `null` | Legacy window selector; alternative lock (`target` wins). |
| `background_args` | `null` | argv for a `desktop:"hidden"` launch only. |
| `desktop` | `null` | Only `"hidden"` is accepted. See [HEADLESS.md](HEADLESS.md). |
| `allow_foreground` | `false` | Permit `SendInput` keyboard/text escalation. |
| `allow_cursor` | `false` | Permit pointer movement (with `allow_foreground`). |
| `allow_clipboard` | `false` | Permit `clipboard_set`. |

**Returns** `{ results: [{ index, type, ok, ms, mode? }], total_ms, sendinput_calls,
sendinput_ms, events, focus_before, focus_after, foregroundUnchanged, cursorUnchanged,
target_locked?, target_pid?, target_exe? }`, plus `hidden_desktop` under
`desktop:"hidden"`.

```json
{
  "target": { "titleContains": "Untitled - Notepad" },
  "actions": [{ "type": "type", "text": "hello", "x": 40, "y": 60 }],
  "dry_run": true
}
```

### `cursor`

Read-only. No arguments.

**Returns** `{ x, y, screens: [[l,t,r,b], ...] }`.

```json
{}
```

### `elements`

Read-only structured UI tree: zero pixels, no input, no focus change.

| Argument | Default | Meaning |
|---|---|---|
| `target` | `null` | Selector; wins over `title`/`hwnd`. |
| `title` | `null` | Selector string. |
| `hwnd` | `null` | Window handle. |
| `detail` | `false` | Keep raw `win`/`hwnd` per entry and add TextPattern `text`. |
| `max_depth` | `8` | UIA walk depth 1..32. |
| `offset` | `0` | Skip that many leading controls for paging. |

One of `title`, `hwnd` or `target` is required.

**Returns** `{ backend, truncated, rect_space, elements, uia_available, uia_error,
fallback, empty, empty_reason, depth_capped, depth_limit, next_offset,
foregroundUnchanged, cursorUnchanged }`. Each entry is
`{ ref, role, name, auto_id, rect, invokable, path, fp }` (`detail:true` adds `win`,
`hwnd`, and `text` where readable).

- `ref` is `"<hwnd>.<path>"`, a per-enumeration correlator — **not** a durable handle.
  `invoke`/`set_value` re-enumerate and match `name:`/`auto_id:`/`path:`.
- `fp` is a stable identity fingerprint (FNV-1a over role/name/auto_id/invokable, 16 hex
  chars). Pass it as `expect_fp` to refuse a stale ref instead of acting on the wrong
  control.
- `next_offset` carries the next page when the 200-entry cap is hit and controls remain,
  else `null`. Both backends page identically.
- `backend` names what served the call: `"uia"` (native UIAutomation COM walk) or
  `"win32"` (`EnumChildWindows` fallback). An empty list is reported, never implied:
  `empty:true` with `empty_reason`, plus `uia_available` and `fallback`.

```json
{ "target": { "exe": "notepad.exe" }, "detail": false }
```

## The `do` burst

### Actions

| Type | Arguments | Delivery |
|---|---|---|
| `move` | `x`,`y` or `dx`,`dy` (±32767), `relative` | pointer; relative needs no desktop gate |
| `click` | `x`,`y` or `element`+`index`+`expect_fp`, `button` (left/right/middle), `count`, `hover` | UIA pattern first, else one posted down+up |
| `down` / `up` | `button` | posted button message |
| `drag` | `from`[2], `to`[2], `button` | posted down/move/up at exact points |
| `scroll` | `x`,`y` or `element`, `amount`, `axis` (`v`/`h`) | `WM_MOUSEWHEEL` at the point, or UIA `ScrollPattern` |
| `type` | `text`, `chunk_chars`, `chunk_ms` | UIA `ValuePattern.SetValue` with an element; else `SendInput` |
| `key` | `name`, `hold_ms` (0..60000), `repeat` (1..100), `gap_ms` | `SendInput` only |
| `hotkey` | `keys` (array or string) | `SendInput` only |
| `wait` | `ms` | sleeps, cancellably |
| `wait_for` | `kind` (`window_appear`/`window_disappear`/`pixel`/`stable`/`element`), `selector`, `x`,`y`,`color`,`tolerance`, `region`, `timeout_ms`, `poll_ms`, `window`, `element`, `state` | polls; cancellable |
| `focus_window` | `title`, `hwnd` | the only verb here that may focus; needs `allow_foreground` |
| `invoke` | `element` (`name:`/`auto_id:`/`path:`), `index`, `expect_fp`, `state` (`expand`/`collapse`) | UIA Invoke/SelectionItem/Toggle, or Win32 `BM_CLICK` |
| `set_value` | `element`, `text`, `expect_fp` | UIA `ValuePattern.SetValue`, RangeValue fallback for sliders |
| `clipboard_set` | `text` | `CF_UNICODETEXT`; needs `allow_clipboard` |
| `clipboard_get` | — | reads back `{ok,text}`; free |

Notes that keep the actions honest:

- `click` on an element reports `delivered:"uia"` (pattern, preferred) or
  `delivered:"posted"` plus `effect:"verified"|"pattern-returned"|"unchanged"|"unverified"`.
  A posted click whose readable control state did not change fails with
  `background_unavailable` instead of claiming success. A raw `x`/`y` click always reports
  `delivered:"posted"`, `effect:"unverified"`.
- `hover:true` is the only way to ask for a `WM_MOUSEMOVE`.
- `invoke` never falls back to a pixel click unless you pass `"fallback":"click"`; an
  ambiguous selector returns `ok:false` with a `candidates` list unless `index` is explicit
  (even `0`). Every result names its path in `via`
  (`uia-invoke|uia-select|uia-toggle|win32-click|pixel-click|none`) plus `backend` and
  `resolution` (`found|not-found|ambiguous|list-failed|stale|uipi_blocked`).
- `invoke` `state:"expand"|"collapse"` drives **only** UIA ExpandCollapse (a typed error
  without it — never Invoke, so a SplitButton cannot open a menu instead of collapsing) and
  reports `expand_state` (`Collapsed|Expanded|PartiallyExpanded|LeafNode`).
- `set_value` never echoes the text (it may be a password); it reports `value_len`. Without
  `ValuePattern`, a numeric RangeValue control accepts numeric text, parsed as f64,
  range-checked, read-only honored, and reports the live value in `value`.
- `type` with `chunk_chars` splits into one flush per chunk with `chunk_ms` between flushes
  (skipped under `dry_run`); absent `chunk_chars` keeps the single-coalesced behaviour
  exactly.
- `key` holds Shift when the layout (`VkKeyScanW` high byte) requires it, else a Unicode
  fallback like `type`. Per repeat: down, flush, hold, up, flush, cancellably.
- `wait_for` `kind:"element"` polls the element model: `window` optional (default
  foreground), `element` a required `name:`/`auto_id:`/`path:` selector, `state` `appear`
  (default)/`disappear`/`enabled`/`disabled` (UIA IsEnabled, or the Win32 enabled flag).
  A control need not be invokable to be waited on. If the window disappears mid-wait,
  `disappear` is satisfied and the other states keep polling to timeout; a malformed
  selector, an ambiguous match or an unresolvable window is a per-action error.
- `clipboard_set` overwrites the owner's global clipboard — a side effect outside the
  target window — so it is deny-by-default. `clipboard_get` results may hold secrets: treat
  them as sensitive and never log them beyond the caller.

### Ordering guarantees

1. **Array order is execution order.** Actions run sequentially; nothing is reordered or
   deduplicated.
2. **Coalescing.** Consecutive pure-input actions are coalesced into a **single**
   `SendInput` call (`sendinput_calls` reports how many were actually issued).
3. **Flush before a barrier.** `wait`, `focus_window` and `wait_for` flush pending input
   first, so a barrier never overtakes input queued behind it.
4. **Continue-always.** A per-action failure never aborts the burst: that action reports
   `ok:false` and the remaining actions still run. `ok:false` at the action level means a
   malformed action or a typed refusal — not a reason to skip the rest.
5. **A timeout is not an error.** `wait_for` that times out returns `ok:true` with
   `satisfied:false` (the flag, never an error), so the batch continues. `ok:false` is
   reserved for a malformed action or an unresolvable selector.
6. **Revalidation.** The target lock is revalidated before every action, not once per call.
7. **`dry_run` is inert.** It builds the exact `INPUT` array but calls `SendInput` **zero**
   times; `invoke` still resolves the element and reports
   `found`/`not-found`/`ambiguous` plus the path it would use, and `set_value` reports
   `value_len` without firing. Metadata is still reported.
8. **Elevation is refused, not switched.** Input aimed at a higher-integrity target is
   refused before injecting with `uipi_blocked`, naming the elevated exe/pid/hwnd and
   setting `blocked_by_uipi:true` on the action — under `dry_run` too. The mode is never
   silently changed and success is never pretended.

## Troubleshooting and limitations

### Typed errors and what to do

| Code | Meaning | Fix |
|---|---|---|
| `ambiguous_target` | Selector matched more than one window. | Use the returned candidate list; add `className`/`appId`/`hwnd`. |
| `target_gone` | The locked window vanished, or the pid was recycled (creation time differs). | Re-list with `list_windows` and re-select. |
| `bad_target` | Malformed selector. | Check the grammar in [the target lock](#the-target-lock). |
| `target_required` | The verb needs a lock. | Pass `target`. |
| `foreground_required` | The verb needs real input. | Pass `allow_foreground:true`, or use a UIA-pattern verb. |
| `cursor_move_required` | A pointer act passed `allow_foreground` but not `allow_cursor`. | Pass both — `SendInput` pointer movement is foreground input too. |
| `clipboard_write_required` | `clipboard_set` without permission. | Pass `allow_clipboard:true`. |
| `background_unavailable` | No background path exists for this action, or a posted click proved no effect. | Re-target by element, or accept the foreground path explicitly. |
| `bad_coordinates` | Posted coordinates outside the signed 16-bit range. | Stay within `-32768..32767`. |
| `window_minimized` | Target is minimized. | `window` `restore` first; it is never restored implicitly. |
| `capture_unavailable` | `PrintWindow` refused or returned a blank/black frame. | Try `allow_crop_fallback:true` on the foreground window, or `screenshot` without a target. |
| `stale_ref` | `expect_fp` did not match the re-walked control. | Take a fresh `elements` snapshot and retry. |
| `uipi_blocked` | Target runs at higher integrity. | Run the server at the same or higher integrity level. |

`-32603 Server busy` means more than 8 tool calls were in flight; the ninth is refused, not
queued.

### What UIA cannot reach

- **Elevated windows.** UIPI blocks both messages and UIA across integrity levels. Input is
  refused before injection (`uipi_blocked`), and `list_windows` reports `elevated` per
  window so you can see it coming.
- **UAC and other secure-desktop prompts.** Consent UI lives on the secure desktop; nothing
  here can reach it.
- **Minimized windows.** `screenshot` returns `window_minimized` rather than restoring.
- **Custom-drawn and framework-hosted surfaces.** UIA exposes what a provider chooses to
  expose: a control that paints its own surface without patterns may yield nothing, in
  which case the walk returns empty (`empty_reason`) or the `win32` fallback lists only
  HWND-based children. Anonymous layout nodes (no name, no `auto_id`, not invokable) are
  dropped.
- **Offscreen and virtualized items.** The walk is raw-view: offscreen items appear only as
  their provider exposes them, and controls deeper than `max_depth` are hidden with
  `depth_capped:true`.
- **Composited pixels.** Capture is `PrintWindow(PW_RENDERFULLCONTENT)` — a pixel copy of
  the window, not a composited screen; some windows paint flat or refuse. Windows.Graphics.
  Capture (WGC) is **deferred**: it needs a WinRT frame pool plus D3D11 in this stack and is
  untestable on a single-monitor box here, so `PrintWindow` covers the verifiable cases.
- **Keyboard through posted messages.** `WM_CHAR`/`WM_KEYDOWN` bypass the input queue,
  desync the target and are ignored by many controls, so plain `type`, `key` and `hotkey`
  are **never** posted — they escalate to `SendInput` behind `allow_foreground:true`. The
  one exception is a desktop the server itself created and owns (`desktop:"hidden"`), and
  only when the target window really lives on it: the permission is earned by desktop-name
  comparison, never granted by the argument alone.

### Posted input is best-effort

Background mode is **not** equivalent to real input. `PostMessage` input is not honoured by
every app, and a per-action posted count reports messages queued, not effects produced —
`text_readback` or a verified control state is the proof they landed. `move` has no
window-message equivalent and fails with `background_unavailable` instead of silently doing
nothing. When a background path does not exist, the answer is a typed error, never a silent
foreground switch.

### Known shape quirks

- `screenshot`'s `elements=true` block always uses the defaults (depth 8, offset 0, cap
  200) — use the `elements` tool for paging.
- `elements` `text` never participates in `fp`, so reading text cannot invalidate a ref.
- `wait_for` `kind:"element"` with no foreground window is `satisfied:false` with a reason,
  not an error.

## Further reading

- [Headless desktop](HEADLESS.md) — `do` with `desktop:"hidden"`: private desktop, evidence
  shape, limits.
- [Publishing](PUBLISHING.md) — owner-only release lane.
- [CHANGELOG](../CHANGELOG.md) — what changed, per version.
