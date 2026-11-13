# puppet guide

Depth for the seven tools: how a call is provisioned and locked, every option, every
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

### Why it never steals focus, moves the pointer, or writes the clipboard

Background-only policy, absolute: the server may be driving a machine whose
owner is playing a fullscreen game, so puppet works purely in the background
or says it cannot. There is **no escalation path**: `SendInput` is never
called, `SetCursorPos` / `SetForegroundWindow` / `BringWindowToTop` /
`mouse_event` have zero call sites anywhere (pinned by a source scan in
`tests/no_steal.rs`), and the schemas keep `allow_foreground` /
`allow_cursor` / `allow_clipboard` only so old callers get a typed refusal
instead of an "unexpected keyword" error.

| Verb | Result |
|---|---|
| `invoke`, `toggle`, expand/collapse, `scroll` with `element`, `click`/`down`/`up`/`drag`/`scroll` with coords on a locked target | posted (`PostMessage`) or UIA pattern |
| plain `type`, `key`, `hotkey` on a locked target | posted `WM_CHAR` / `WM_KEYDOWN` / `WM_SYSKEYDOWN` (verified by default, see below) |
| `delivery:"sendinput"` or any call with `allow_foreground:true` | refused with `no_foreground_policy`, never acted on |
| `move`, or any click/drag/scroll that needs a real cursor | refused with `no_cursor_policy` |
| `focus_window`, `window` `action:"focus"`, `window` `action:"maximize"` | refused with `no_foreground_policy` (focusing IS taking the foreground; maximizing covers a monitor) |
| `clipboard_set` | refused with `no_clipboard_policy` (`clipboard_get` stays — reading cannot disturb) |
| `type` with an element, `set_value` | refused with `background_unavailable` naming the control (their UIA provider may take the foreground) |

Without a locked target, keyboard verbs refuse with `target_required` — the
foreground is never an option. Every refusal happens **before anything is
sent**, under `dry_run` too. `window` `move`/`resize`/`minimize`/`restore`/
`topmost`/`close` use `SWP_NOACTIVATE`, `SW_SHOWMINNOACTIVE`,
`SW_SHOWNOACTIVATE` or `WM_CLOSE` and never activate or raise.

The guarantee is structural, not a promise: there is no injection API to
reach, and `tests/no_steal.rs` proves the symbols appear nowhere. Every
acting or capturing call also records `GetForegroundWindow` and `GetCursorPos`
before and after and returns `foregroundUnchanged` / `cursorUnchanged` —
that is telemetry, not the guarantee.

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
| `PUPPET_LIVE_DESKTOP` | unset | `1` enables the live-gated integration tests (`hidden_desktop`, `clipboard`, `timeout_bound`, and the live halves of the element tests). Development only; the suite skips them cleanly without it. |
| `PUPPET_LIVE_VISIBLE` | unset | `1` also runs the one test that maximizes a real window (`src/winop.rs`); it is a separate opt-in so a headed box is never flashed. Development only. |
| `PUPPET_UIA_TIMEOUT_MS` | `5000` | Deadline for one bounded UIA operation (`elements` walk, `invoke`/`set_value` activation). Clamped to `50..120000`. Exceeding it returns `uia_timeout`. |
| `PUPPET_CAPTURE_TIMEOUT_MS` | `10000` | Deadline for one `PrintWindow` capture. Clamped to `50..120000`. Exceeding it returns `capture_timeout`. |

Test-only knobs, never needed to use the server: `PUPPET_TEST_BURST_BUDGET_MS` can
only shorten the `do` budget (clamped to 500 ms..300 s, `src/burst.rs`), and
`PUPPET_TEST_STALL_UIA_MS` / `PUPPET_TEST_STALL_CAPTURE_MS` simulate a stalled
provider once per process (`src/deadline.rs`). `PUPPET_GIT_SHA` / `PUPPET_GIT_DIRTY`
in `--version` output are injected by `build.rs` at compile time, not read at run time.

Tool-level defaults live with each tool below.

## Every tool

All seven are declared in `src/schemas.rs` with a real `outputSchema`; the tool descriptions
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
| `fmt` | `"png"` | `png`, `jpeg` (`jpg` is an alias). Lossless by default; JPEG is an explicit request. |
| `quality` | `80` | JPEG quality 1..100; PNG ignores it (a `quality` sent with `png` warns `quality_ignored`). |
| `grid` | `false` | Overlay a light 128 px lattice. |
| `allow_crop_fallback` | `false` | Permit a foreground-only desktop crop to `dwm_rect` when `PrintWindow` refuses. |
| `elements` | `false` | Append a third content block with the element model. |
| `out` | `null` | Absolute path: also write the exact returned bytes to disk (parent dirs created) and add `out:{path,bytes,fmt}` to the payload. |

**Returns** `[image, coordinate_map_json]`, plus the element block when `elements=true`.
The map carries `source{x,y,w,h,kind}`, `scale`, `monitors{count,origin,virtual,rects}`,
`cursor`, `window`, `capture`, `formula`, `foregroundUnchanged`, `cursorUnchanged`, and
`warnings` only when the server altered a request (`quality_clamped`, `quality_ignored`).
`capture.blank_check{max_channel,verdict,threshold}` is the blank-frame measurement:
full-frame only (`max channel < 8`), never partial or stale. Map
image pixels back with:

```
screen_x = source.x + round((img_x + 0.5) * source.w / image.w)   // same for y
```

`monitors.rects` is per monitor in the same `[l,t,r,b]` shape `cursor.screens` uses.
For window captures `source.x/y` equal the `dwm_rect` origin and `source.kind` is
`"window"`. If the window moves while it is being captured, the map reports the
POST-capture origin and `capture.moved_during_capture:true` (a pure translation is
recaptured once and the second result is returned) — a stale origin is never presented
as current. With `elements=true` plus `display`/`region`, the element block still
describes the FOREGROUND window (not the captured image), and the map names it in
`elements_window{hwnd,exe,title}`. That handle is sampled once at call start, so
the name and the block it describes always agree.

Errors are `["error: <code>: <detail>"]` with
`bad_display|bad_region|bad_window|ambiguous_window|empty_crop|capture_failed|ambiguous_target|target_gone|window_minimized|capture_unavailable|bad_out`.

```json
{ "target": { "titleContains": "Untitled - Notepad" }, "max_px": 1280 }
```

### `diff`

Pixel diff for "only the intended pixels changed": compare two captures and
report exactly which pixels moved. Read-only: no activation, no input. Errors
never raise.

| Argument | Default | Meaning |
|---|---|---|
| `base` | required | `{path:"<png>"}` (absolute .png path, decoded as-is) or a capture target (`{target\|window, display, region}` with the screenshot selector grammar; captured side-effect-free at native pixels). |
| `next` | required | Same shape as `base`. |
| `threshold` | `0` | Per-channel tolerance 0..255; only tolerance there is. |
| `max_px` | `1280` | Long-edge clamp for the returned diff image, aspect-preserving, never upscaling. |
| `out` | `null` | Absolute path: write the diff PNG to disk (parent dirs created). |
| `bbox` | `true` | Toggle the union box of changed pixels. |
| `mask` | `null` | `[[left,top,right,bottom],…]` in native operand pixels: masked pixels leave `changed`, `total`, `ratio` and `bbox`, and are counted in `masked_px`. Overlapping rects union; a rect outside the frame is `bad_region`. |

Both sides must land on the SAME pixel dimensions: a mismatch is a typed
`size_mismatch` naming both sizes, never a silent scale.

**Returns** `{changed,total,ratio,bbox:[x,y,w,h]|null,identical,base:{w,h},next:{w,h},threshold,masked_px,bbox_space,image?,scale?,base_source?,next_source?,diff_out?}`.
`changed`, `total` (the unmasked total), `ratio` and `bbox` are NATIVE operand pixels
(`bbox_space:"native"`); `image{w,h,format,quality}` and `scale` describe the returned
bytes and are absent when no image returns. A native box maps onto the image with
`img_x = floor(bbox.x * scale)`; a live operand's `base_source`/`next_source` is the
`{x,y,w,h,kind}` its capture reported (`screen_x = base_source.x + bbox.x`), and a
`{path}` operand reports `null` because it has no origin.
When anything changed, a second content block carries the diff image (changed
pixels solid red over a dimmed greyscale copy of `base`, masked pixels never red);
when identical there is no image block and `bbox` is `null`.

```json
{ "base": {"path": "C:/captures/before.png"}, "next": {"path": "C:/captures/after.png"} }
```

### `list_windows`

Read-only. No positional arguments; nothing is touched.

| Argument | Default | Meaning |
|---|---|---|
| `include_app_ids` | `false` | Add `appId` per window from the AppUserModelID property store (a string, or `null` when absent). Opt-in because that read is a per-window COM call. |

**Returns** `{ windows: [{ hwnd, pid, exe, title, class, rect, dwm_rect, client_rect,
minimized, maximized, foreground, cloaked, visible, dpi, elevated, appId? }] }`. `rect` is
`GetWindowRect` (compatibility, includes the invisible border); `dwm_rect` is
`DWMWA_EXTENDED_FRAME_BOUNDS` (the mapping truth, else `rect` with `dwm_fallback:true`);
`client_rect` is in screen space. `appId` appears only with `include_app_ids:true`.

```json
{ "include_app_ids": true }
```

### `window`

| Argument | Default | Meaning |
|---|---|---|
| `action` | required | `focus`, `move`, `resize`, `minimize`, `maximize`, `restore`, `close`, `topmost`. |
| `target` | `null` | Selector; wins over `title`/`hwnd`. |
| `title` | `null` | Selector string; the same target lock as `target` (`{titleContains}`), revalidated before the action. |
| `hwnd` | `null` | Window handle. |
| `x`, `y` | `null` | Required when `action:"move"`. |
| `width`, `height` | `null` | Required when `action:"resize"`. |
| `allow_foreground` | `false` | Retained for compatibility: passing it refuses the whole call with `no_foreground_policy`. |

`action:"focus"` and `action:"maximize"` are refused with
`no_foreground_policy` — focusing IS taking the foreground, and maximizing
covers the monitor (Win32 documents no no-activate form).

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
| `actions` | required | Array of action objects, run in order. At most 256 actions. |
| `target` | `null` | Selector locking one window for the whole call. |
| `dry_run` | `false` | Validate and report; inject nothing. |
| `settle_ms` | `0` | Sleep between actions. |
| `focus_guard` | `false` | Dead guard: restoring would take the foreground, so a changed foreground is reported with `focus_guard_error` and `focus_restored:false` instead of being restored. |
| `background` | `null` | Legacy window selector; alternative lock (`target` wins). |
| `background_args` | `null` | argv for a `desktop:"hidden"` launch only. |
| `desktop` | `null` | Only `"hidden"` is accepted. See [HEADLESS.md](HEADLESS.md). |
| `allow_foreground` | `false` | Retained for compatibility: passing it refuses every action with `no_foreground_policy`. |
| `allow_cursor` | `false` | Retained for compatibility: the pointer is never moved. |
| `allow_clipboard` | `false` | Retained for compatibility: `clipboard_set` is always refused. |

**Returns** `{ results: [{ index, type, ok, ms, mode?, delivered?, effect?, posted?, changed?, bbox?, code?, error? }], total_ms, sendinput_calls,
sendinput_ms, events, focus_before, focus_after, foregroundUnchanged, cursorUnchanged,
target_locked?, target_pid?, target_exe? }`, plus `hidden_desktop` under
`desktop:"hidden"`. `sendinput_calls` and `sendinput_ms` are always 0
(`SendInput` is never called); `events` counts posted messages.

A burst is bounded: at most **256 actions** and **300 s** of total wall-clock time.
Exceeding either is a typed `burst_budget_exceeded` result naming the limit and the
actual value, in the same per-result shape as a failed action; over the action cap
nothing runs, and once the time budget expires the remaining actions do not run. A
`wait` or `wait_for` that would outlive the remaining budget is refused/clamped and
reported the same way — the global input lock is never held past the budget.

```json
{
  "target": { "titleContains": "Untitled - Notepad" },
  "actions": [{ "type": "type", "text": "hello" }],
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
| `title` | `null` | Selector string; the same target lock as `target` (`{titleContains}`), revalidated before the action. |
| `hwnd` | `null` | Window handle. |
| `detail` | `false` | Keep raw `win`/`hwnd` per entry and add TextPattern `text`. |
| `max_depth` | `8` | UIA walk depth 1..32. |
| `offset` | `0` | Skip that many leading controls for paging. |

One of `title`, `hwnd` or `target` is required.

The walk is bounded: a provider that stops answering returns `uia_timeout` inside 5 s
(`PUPPET_UIA_TIMEOUT_MS`) instead of holding the worker. `elements` itself still answers —
the Win32 fallback fills the page, `backend` says `"win32"` and `uia_error` carries the
typed reason.

**Returns** `{ backend, truncated, rect_space, elements, uia_available, uia_error,
fallback, empty, empty_reason, depth_capped, depth_limit, next_offset,
foregroundUnchanged, cursorUnchanged }`. Each entry is
`{ ref, role, name, auto_id, rect, offscreen, invokable, path, fp }` (`detail:true` adds
`win`, `hwnd`, and `text` where readable).

- `offscreen:true` marks an entry whose `rect` has no area (`[0,0,0,0]`, or a zero/negative
  width or height): a control scrolled out of view, virtualized, or collapsed. The rect is
  kept for shape stability but must not be read as geometry — there is no pixel to aim at.
  `click` on such an element is refused with `offscreen_element` (scroll it into view, or
  use `invoke`/`set_value`, which act through the UIA pattern and need no geometry).
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
| `move` | `x`,`y` or `dx`,`dy` | always refused with `no_cursor_policy` |
| `click` | `x`,`y` or `element`+`index`+`expect_fp`, `button` (left/right/middle), `count`, `hover` | UIA pattern first on an element, else one posted down+up; needs a locked target or `no_cursor_policy` |
| `down` / `up` | `button` | posted button message on a locked target, else `no_cursor_policy` |
| `drag` | `from`[2], `to`[2], `button` | posted down/move/up at exact points on a locked target, else `no_cursor_policy` |
| `scroll` | `x`,`y` or `element`, `amount`, `axis` (`v`/`h`) | `WM_MOUSEWHEEL` at the point on a locked target, or UIA `ScrollPattern` |
| `type` | `text` | `WM_CHAR` posted to the locked target's writable text control (`delivered:"posted"`, `effect:"unverified"`) |
| `key` | `name`, `delivery` (`auto`\|`posted`\|`sendinput`, default `auto`), `hold_ms` (default 80), `repeat` (1..100), `gap_ms`, `verify` (default `true`) | `WM_KEYDOWN`/`WM_SYSKEYDOWN` posted to the locked target with a correct lParam (repeat 1, scan code, extended bit, key-up bits); verified by default with a before/after pixel diff |
| `hotkey` | `keys` (array or string), `delivery`, `hold_ms`, `verify` | pressed in order, held, released in reverse; same verify contract as `key` |
| `wait` | `ms` | sleeps, cancellably |
| `wait_for` | `kind` (`window_appear`/`window_disappear`/`pixel`/`stable`/`element`), `selector`, `x`,`y`,`color`,`tolerance`, `region`, `timeout_ms`, `poll_ms`, `window`, `element`, `state` | polls; cancellable |
| `focus_window` | `title`, `hwnd` | always refused with `no_foreground_policy` |
| `invoke` | `element` (`name:`/`auto_id:`/`path:`), `index`, `expect_fp`, `state` (`expand`/`collapse`) | UIA Invoke/SelectionItem/Toggle, or Win32 `BM_CLICK` |
| `set_value` | `element`, `text`, `expect_fp` | always refused with `background_unavailable` naming the element |
| `clipboard_set` | `text` | always refused with `no_clipboard_policy` |
| `clipboard_get` | — | reads back `{ok,text}`; free |

Notes that keep the actions honest:

- `click` on an element reports `delivered:"uia"` (pattern, preferred) or
  `delivered:"posted"` plus `effect:"verified"|"pattern-returned"|"unchanged"|"unverified"`.
  A posted click whose readable control state did not change fails with
  `background_unavailable` instead of claiming success. A raw `x`/`y` click always reports
  `delivered:"posted"`, `effect:"unverified"`. An element with no on-screen rectangle is
  refused **before** any input with `offscreen_element`: its "centre" would be the target's
  client origin, so nothing is posted and the message names the fix (scroll it into view,
  or use `invoke`).
- `hover:true` is the only way to ask for a `WM_MOUSEMOVE`.
- `invoke` never falls back to a pixel click unless you pass `"fallback":"click"`; an
  ambiguous selector returns `ok:false` with a `candidates` list unless `index` is explicit
  (even `0`). Every result names its path in `via`
  (`uia-invoke|uia-select|uia-toggle|win32-click|pixel-click|none`) plus `backend` and
  `resolution` (`found|not-found|ambiguous|list-failed|stale|uipi_blocked`).
- `invoke` `state:"expand"|"collapse"` drives **only** UIA ExpandCollapse (a typed error
  without it — never Invoke, so a SplitButton cannot open a menu instead of collapsing) and
  reports `expand_state` (`Collapsed|Expanded|PartiallyExpanded|LeafNode`).
- `set_value` and `type`-on-an-element are refused with
  `background_unavailable` naming the control, before any provider call. The
  UIA provider behind them may take the foreground, and no flag buys that.
- `type` posts `WM_CHAR` per UTF-16 unit to the locked target's writable
  text control (a read-only control is skipped: typing there would be a
  silent no-op). Delivery is reported (`delivered:"posted"`,
  `effect:"unverified"`), never implied.
- Posted `key`/`hotkey` are verified by default: the target is captured
  before and after (native pixels, lossless) and diffed per channel at
  threshold 0. `effect:"changed"` carries the exact `changed` count and the
  union `bbox:[x,y,w,h]`; `effect:"unchanged"` carries `changed:0` and
  `bbox:null` — a result, not an error. Pass `verify:false` for
  `effect:"unverified"`. An 80 ms hold sits between down and up by default.
  A target that cannot be captured (e.g. a message-only window) makes
  `verify:true` fail honestly with `delivered:"none"`.
- `key` holds Shift when the layout (`VkKeyScanW` high byte) requires it, and
  sends `WM_SYSKEYDOWN`/`WM_SYSKEYUP` when Alt is held, with a correct
  lParam (repeat 1, `MapVirtualKeyW` scan code, extended bit, context code,
  previous-state + transition bits on key-up).
- `wait_for` `kind:"element"` polls the element model: `window` optional (default
  foreground), `element` a required `name:`/`auto_id:`/`path:` selector, `state` `appear`
  (default)/`disappear`/`enabled`/`disabled` (UIA IsEnabled, or the Win32 enabled flag).
  A control need not be invokable to be waited on. If the window disappears mid-wait,
  `disappear` is satisfied and the other states keep polling to timeout; a malformed
  selector, an ambiguous match or an unresolvable window is a per-action error.
- `clipboard_set` is refused with `no_clipboard_policy` before any clipboard
  API — a refused call touches nothing. `clipboard_get` results may hold
  secrets: treat them as sensitive and never log them beyond the caller.

### Ordering guarantees

1. **Array order is execution order.** Actions run sequentially; nothing is reordered or
   deduplicated.
2. **Posted, never coalesced through input.** Keyboard and pointer verbs post
   one message group per action (`posted` counts them); there is no input
   queue and no `SendInput` call (`sendinput_calls` is always 0).
3. **No barriers to flush.** `wait`, `focus_window` (refused) and `wait_for`
   poll or refuse on their own; nothing queues input behind them.
4. **Continue-always.** A per-action failure never aborts the burst: that action reports
   `ok:false` and the remaining actions still run. `ok:false` at the action level means a
   malformed action or a typed refusal — not a reason to skip the rest.
   The call-level verdict is separate: the payload carries top-level `ok` (true only when
   **every** action succeeded) and `failed` (how many did not). The MCP envelope reports
   `isError:true` only when **no** action succeeded — a burst where the target vanished
   before the first action is a real failure, a burst with one bad action among good ones
   is not.
5. **A timeout is not an error.** `wait_for` that times out returns `ok:true` with
   `satisfied:false` (the flag, never an error), so the batch continues. `ok:false` is
   reserved for a malformed action or an unresolvable selector.
6. **Revalidation.** The target lock is revalidated before every action, not once per call.
7. **`dry_run` is inert.** It validates and reports exactly what the live
   call would do — including the policy refusals — but posts nothing.
   `invoke` still resolves the element and reports
   `found`/`not-found`/`ambiguous` plus the path it would use.
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
| `no_foreground_policy` | The verb would take the foreground (`delivery:"sendinput"`, `allow_foreground:true`, `focus_window`, `window` `focus`/`maximize`). | Nothing buys the foreground: use a locked target with a posted verb. |
| `no_cursor_policy` | The verb would move the system pointer (`move`, cursor-less click/drag/scroll). | Pass a locked target for an exact posted message. |
| `no_clipboard_policy` | `clipboard_set` would overwrite the owner's shared clipboard. | Nothing buys a write; `clipboard_get` stays. |
| `background_unavailable` | No background path exists for this action (e.g. `type`/ `set_value` on an element, whose provider may focus), or a posted click proved no effect. | Re-target, or use a posted verb. |
| `bad_coordinates` | Posted coordinates outside the signed 16-bit range. | Stay within `-32768..32767`. |
| `window_minimized` | Target is minimized. | `window` `restore` first; it is never restored implicitly. |
| `capture_unavailable` | `PrintWindow` refused or returned a blank/black frame. | Try `allow_crop_fallback:true` on the foreground window, or `screenshot` without a target. |
| `stale_ref` | `expect_fp` did not match the re-walked control. | Take a fresh `elements` snapshot and retry. |
| `uipi_blocked` | Target runs at higher integrity. | Run the server at the same or higher integrity level. |
| `burst_budget_exceeded` | A `do` burst exceeded 256 actions or the 300 s total budget. | Split the batch; the result names the limit and the actual value. |
| `uia_timeout` | A UIA call (element walk, `invoke`, `set_value`) did not return within 5 s: the target's UI thread is not responding. | Retry once the app is responsive, or capture the desktop instead. `elements` still answers from the Win32 fallback and reports the reason in `uia_error`; an abandoned activation's effect is unverified. |
| `capture_timeout` | A `PrintWindow` capture did not return within 10 s: the target's UI thread is not responding. | Retry, or capture the desktop instead. |

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
- **Posted keyboard is the route.** `WM_CHAR`/`WM_KEYDOWN`/`WM_SYSKEYDOWN`
  go straight to the locked target (never the foreground): plain `type` to
  its writable text control, `key`/`hotkey` with a correct lParam and an 80 ms
  default hold, verified by default with a before/after pixel diff. `delivery:"sendinput"`
  is refused outright. The private desktop the server itself created and owns
  (`desktop:"hidden"`) keeps the same posted delivery for the program living
  on it: the permission is earned by desktop-name comparison, never granted
  by the argument alone.

### Posted input is best-effort

Background mode is **not** equivalent to real input. `PostMessage` input is not honoured by
every app, and a per-action posted count reports messages queued, not effects produced —
verification (a before/after pixel diff on keys, `text_readback` or a verified control
state) is the proof they landed. `move` needs a real cursor and fails with
`no_cursor_policy`. When no background path exists, the answer is a typed error, never a
silent foreground switch. A `size_mismatch` diff names both sizes rather than scaling
silently.

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
