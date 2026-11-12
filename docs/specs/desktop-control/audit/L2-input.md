# Lens 2 — Input fidelity and focus semantics

Repo: `Q:\PROJECTS\PERSONAL\puppet`, HEAD `8eb1c2d` (`git log --oneline -20` checked).
Read-only; nothing built, launched, or modified.

## Answer first

Puppet posts `PostMessage`/`SendMessageTimeoutW` to a locked HWND or fires UIA patterns. There is
structurally no `SendInput`/`SetCursorPos`/`SetForegroundWindow`/`BringWindowToTop`/`mouse_event`
call site anywhere (`tests/no_steal.rs:30-96` static scan; `src/gate.rs:9-14`; `Burst::flush` is a
no-op by policy, `src/burst.rs:2188-2196`; `sendinput_calls` always 0, `docs/GUIDE.md:299-303`).
The no-steal half of the promise holds. The "it actually works" half is narrower than the docs
imply: raw `x/y` clicks report `ok:true` with `effect:"unverified"` even when the target ignored
every message (`src/burst.rs:2862-2869`), so **a successful click result is proof of delivery to
the queue, never proof the app acted**. Element clicks with readable state fail honestly when state
doesn't change (`src/burst.rs:3283-3293`); everything else is trust-me output. Treat every posted
click as guilty until verified by a screenshot diff or readback.

## 1. Message-based input reality

- Delivery paths, confirmed: `bg_execute` posts `WM_LBUTTONDOWN/UP` (etc.), `WM_MOUSEWHEEL/HWHEEL`,
  `WM_CHAR`, `WM_KEYDOWN/UP` + `WM_SYSKEYDOWN/UP` to one HWND (`src/burst.rs:836-1181`); UIA
  Invoke/Select/Toggle/ExpandCollapse/Scroll/RangeValue for element verbs (`src/uia.rs:877-886`,
  `1029-1040`, `1175-1184`); Win32 `BM_CLICK` via `SendMessageTimeoutW` (`src/burst.rs:2050-2073`).
  Best-effort is stated in-code (`BG_NOTE`, `src/burst.rs:30`) and in `docs/GUIDE.md:531-539`.
- **Clicks post to the locked top-level HWND with no child hit-test routing.** The only child
  resolution in the codebase is `bg_text_target` for *keyboard* (`src/burst.rs:751-784`), and it
  only recognizes `Edit`/`RichEdit` classes (`bg_is_text_control`, `src/burst.rs:698-708`,
  read-only controls skipped at `:713-726`). A posted button message to a frame whose real button
  is a child control goes nowhere the control reads. Symptom: `ok:true, delivered:"posted",
  effect:"unverified"` and nothing happens. Severity: high — the default success shape.
  Smallest change: route posted clicks through the same descendant walk (visible+enabled child
  whose rect contains the point, via `WindowFromPoint`/`ChildWindowFromPoint`), or downgrade raw
  clicks to `effect:"unverified"` + `ok:false` unless verified.
- Class-by-class verdict (code + Windows behavior; `PostMessage` bypasses the input queue, so
  anything reading raw/queued input sees nothing):
  - DirectInput/DirectX games, Unity/Unreal: read raw input, ignore window messages → silent
    no-op (`ok:true/unverified`). No code path addresses this; no real-input fallback exists.
  - Chromium/Electron renderers: expect a real hit-test + input pipeline; a message posted to the
    top-level frame is not routed to the renderer child → silent no-op. Same for WPF, Java/Swing,
    UWP/XAML islands, ConHost/Windows Terminal (consoles need real console input, not
    `WM_CHAR`), apps gating on `GetForegroundWindow()`/`GetAsyncKeyState()`.
  - What *does* work: classic Win32 `EDIT`/RichEdit controls (`WM_CHAR`/`WM_GETTEXT` path,
    `src/hidden.rs:798-818` proves the loop), buttons via `BM_CLICK`, UIA providers that implement
    Invoke/Scroll/ExpandCollapse.
- `WM_MOUSEMOVE` is never sent unless `hover:true` (`src/burst.rs:875-882`, `docs/GUIDE.md:408`).
  Hover-dependent controls (tooltips, menus, hot-tracking) silently don't react. Severity: medium.
- Drag is exactly three posted messages (down at `from`, one move to `to`, up at `to`;
  `src/burst.rs:915-956`, `docs/GUIDE.md:385`). No interpolated path, no capture, single-HWND only:
  no OLE cross-window drag-and-drop, no marquee-select timing. Symptom: drop target never receives
  `DragEnter` → silent no-op. Label out-of-scope.
- Double/multi-click (`count`, `src/burst.rs:858-893`): tight loop of down/up pairs with **no
  inter-click delay**. Apps using `GetDoubleClickTime` may see N single clicks, not a double-click.
  Symptom: selection/word-select doesn't happen; result still `ok:true`. Severity: medium.
  Smallest change: sleep `GetDoubleClickTime`-ish between pairs or document `count` as back-to-back.
- Elevation/UIPI is the one honest refusal: `uipi_refusal` pre-checks background, keyboard,
  scroll-element, and invoke targets and refuses with `uipi_blocked` + `blocked_by_uipi:true`, under
  `dry_run` too (`src/burst.rs:2089-2098`, `:2820-2834`, `:3595`, `:3800`, `:4020`;
  `list_windows.elevated`, `src/main.rs:1300-1309`). Secure desktop (UAC) unreachable
  (`docs/GUIDE.md:507`). Minimized windows return `window_minimized`, never restored.

## 2. Coordinate space

- Process is per-monitor-V2 DPI aware with a legacy fallback
  (`SetProcessDpiAwarenessContext(...PER_MONITOR_AWARE_V2)` then `SetProcessDpiAwareness`,
  `src/main.rs:1486-1488`). Target-locked `x/y` are client-area physical pixels; conversions are the
  OS's own `ScreenToClient`/`ClientToScreen` (`src/target.rs:611-651`), correct across mixed-DPI
  monitors. Screenshot side is consistent: `screen_x = source.x + round((img_x+0.5)*source.w/image.w)`
  (`src/main.rs:45`; `docs/GUIDE.md:190-192`).
- Posted coords are signed 16-bit; anything outside `-32768..32767` is refused `bad_coordinates`
  before posting (`msg_coords`, `src/burst.rs:507-518`; `bg_lparam`, `:500-502`). Scroll is the
  subtle one: `WM_MOUSEWHEEL` lParam is *screen* coords, converted via `client_point_to_screen`
  then re-checked (`src/burst.rs:957-1002`). Correct, verified in code.
- **Non-client areas are not addressable and not validated.** The target path uses client coords
  as-given (`src/burst.rs:804-810`) with no client-rect clamp, so title bar / resize borders
  (negative-y or beyond-`GetClientRect` points) post blindly into client message space; the window
  proc will misinterpret or ignore them. `WM_NCHITTEST` never runs on a posted message.
  Symptom: silent no-op or wrong-control activation. Severity: medium. Smallest change: clamp/validate
  against `GetClientRect` and refuse out-of-client points with the existing `bad_coordinates` shape.
- Window moves between resolve and click: the lock revalidates *identity* (hwnd/pid/creation
  time/image/class/appId, `src/target.rs:377-448`) but not *position*. Client-relative clicks are
  immune by construction; the legacy screen→client path converts at post time (`bg_client`,
  `src/burst.rs:520-547`); element centres convert at action time (`element_centre_client`,
  `:1951-1957`) but the rect was enumerated at action start — a window animating mid-burst can land
  off-target. Screenshots already detect mid-capture moves (`capture.moved_during_capture`,
  `docs/GUIDE.md:196-198`); input has no equivalent. Severity: low.

## 3. Keyboard

- `type` posts `WM_CHAR` per UTF-16 unit (`utf16_units`, `src/burst.rs:328-330`; loops at `:1029-1035`
  and `:3516-3530`) to the writable text control. Unicode-safe (surrogate pairs preserved), no
  pacing/chunk parameters, no per-message delay: a 1 MiB paste is one tight `PostMessage` loop
  (bounded only by the 300 s burst budget). No IME composition handling, no dead-key handling, no
  `AltGr` handling anywhere (grep for `IME|dead|AltGr|KEYEVENTF|composition` finds only comments and
  the HWND-noise-class list). Symptom on IME/dead-key layouts: composing characters arrive as
  isolated `WM_CHAR`s or nothing. Severity: medium; label IME/dead-key input unsupported.
- Layout dependence: single chars resolve via `VkKeyScanW` on the **server thread's** layout
  (`vk_of`, `src/burst.rs:386-404`); only the Shift high-bit is honored, Ctrl/Alt-required chars fall
  back to `WM_CHAR` (`:395-399`). So `key name:"?"` depends on the server's active layout, not the
  target app's — breaks on non-US layouts. The `WM_CHAR`/`type` path is layout-independent and
  should be preferred in docs. `KeyCode::Uni` posts `WM_CHAR` with lParam 1 (`:608-612`), fine.
- lParam construction for `key`/`hotkey` is careful: repeat 1, `MapVirtualKeyW` scan code, extended
  bit set (`is_extended_vk`, `:567-572`), Alt context bit + `WM_SYS*` variants, prev-state/transition
  bits on key-up (`posted_key_lparam`, `:579-592`; unit-pinned `:5122-5188`). Auto-repeat is *not*
  emulated — `repeat: 1..100` re-posts full down/up cycles with `gap_ms` between (`:3624-3698`); apps
  distinguishing held-repeat (repeat-count bits) see discrete presses.
- No cross-action modifier state exists: `down`/`up` are mouse-button verbs only; each `key`/`hotkey`
  wraps its own Shift. There is **no stuck-modifier path through cancel**: hold-cancel and
  budget-expiry both post the ups before returning (`:3642-3663`, `:3840-3865`). One residual hole:
  if an individual `PostMessage` of an *up* fails mid-release (`failed=true`, `:3867-3875`), the
  remaining ups are skipped — the queue refused the message so the app likely never saw the downs
  either, but the code does not retry/abort loudly. Severity: low.
- `key`/`hotkey` verify by default: lossless before/after capture + threshold-0 pixel diff,
  `effect:"changed"` with count+bbox or `"unchanged"` (`:3700-3755`, `:3891-3931`; `docs/GUIDE.md:424-431`).
  `verify:true` on an uncapturable target (message-only window) fails honestly with
  `delivered:"none"` (`:3611-3617`). `type` is never verified (`effect:"unverified"` always).

## 4. Timing and races

- Click-then-screenshot in one burst has **no implicit settle**: raw clicks report `unverified` and
  the next action runs immediately. Only the hidden-desktop path inserts a 1000 ms pre-readback
  pause (`src/burst.rs:4572-4574`); the ordinary target path relies on explicit `wait`/`wait_for` or
  `settle_ms`. `settle_ms` itself is **trailing-only** (one clamped sleep after all actions,
  `:4528-4534`, accounted at `:4746-4751`) despite `docs/GUIDE.md:290` saying "Sleep between
  actions". Symptom: agent reads a stale frame and concludes the click failed (or succeeded).
  Severity: medium — fix the doc line or implement inter-action pacing.
- `wait_for` kinds: `window_appear|window_disappear|pixel|stable|element` (`wait_for`,
  `src/burst.rs:1250-1683`). Timeout (max 300000 ms, `:1277-1284`) returns `ok:true,
  satisfied:false` — not an error; malformed/ambiguous/unresolvable is `ok:false`. Budget-clamped
  against the remaining burst budget (`:4445-4478`). `stable` defaults to the foreground window's
  monitor, not the virtual desktop (`default_stable_region`, `:129-146`). Element-wait supports
  `appear|disappear|enabled|disabled` via UIA `IsEnabled` or Win32 flag (`:1188-1213`).
- Serialization: only `do` and `window` hold the global `INPUT_LOCK`; `screenshot`/`list_windows`/
  `cursor`/`elements` run concurrently (`src/serve.rs:36`, `:979-989`). 4 workers, 8 in-flight cap,
  9th gets `-32603 Server busy` (`:23-29`). Within a burst: strict array order, continue-always,
  per-action revalidation, cancellable sleeps, 256-action / 300 s budget (`BURST_MAX_ACTIONS`,
  `BURST_BUDGET`, `src/burst.rs:34-48`). No interleaving *inside* a burst; concurrent `do` calls
  serialize on the lock (a hung holder is escaped via 20 ms `try_lock` polling + cancel,
  `src/serve.rs:42-55`).

## 5. Permission gates

- Refusals are comprehensive *because there is nothing to escalate to*: `allow_foreground:true`
  refuses every action (`src/burst.rs:2620-2637`), `delivery:"sendinput"` refuses per action
  (`:2668-2685`), `move`/cursor-less verbs refuse `no_cursor_policy`, `focus_window`/`window
  focus`/`window maximize` refuse `no_foreground_policy`, `clipboard_set` refuses
  `no_clipboard_policy` before touching any clipboard API (`:3933-3944`). `Gate::decide` matrix is
  unit-pinned (`src/gate.rs:359-660`); flags provably buy nothing (`:492-565`). Messages name the
  code + "Nothing was sent/written" (`:690-727`). Actionable: yes — each names the recovery
  (pass a locked target / use a posted verb).
- **Stale copy:** `INIT_INSTRUCTIONS` in `src/serve.rs:126` still advertises the old contract
  ("refused with foreground_required / cursor_move_required / clipboard_write_required unless the
  call passes allow_foreground:true / allow_cursor:true / allow_clipboard:true") — directly
  contradicted by the background-only policy every gate enforces. An agent reading `initialize`
  instructions will try flags that always refuse. Severity: medium. Smallest change: rewrite that
  sentence to "refused, no flag buys them."
- Read-path gate: **none, by design.** `clipboard_get` is `Delivery::None`, needs no target and no
  flag (`src/gate.rs:434-439`; `src/burst.rs:3946-3968`), reads up to 1 MiB (truncates past it,
  `:481-485`). It silently copies the owner's clipboard into the tool result. The only mitigation
  is prose ("treat them as sensitive and never log them", `docs/GUIDE.md:442-444`). Confirm this is
  the intended contract; if not, it is a silent exfiltration primitive, not an injection bug.

## 6. What cannot be done at all

Confirmed absent (no schema surface, no code path — grep over `src/schemas.rs` and `src/` finds
nothing): touch/pen/stylus input; real cross-window drag-and-drop (OLE `DragEnter`/`Drop` — the
`drag` verb is three posted messages to one HWND); window snap to zones (window actions are exactly
`focus|move|resize|minimize|maximize|restore|close|topmost`); monitor arrangement/DPI changes;
session-level acts (lock/logoff/switch desktop — the hidden desktop is create-drive-destroy only,
`src/hidden.rs`). All correctly out-of-scope; `move`/`maximize` refusals already say so. The gap is
documentation, not code: `docs/GUIDE.md` "What UIA cannot reach" (`:502-529`) should gain one line
each for touch/pen, cross-window DnD, and non-client clicks.

## Blunt lists

**Works for these app classes** (evidence in code): classic Win32 EDIT/RichEdit text entry via
`WM_CHAR` to the auto-resolved writable child (`src/burst.rs:698-784`, `:1027`, `:3512`); buttons
and controls exposing UIA Invoke/SelectionItem/Toggle/ExpandCollapse/Scroll/RangeValue
(`src/uia.rs`, `src/burst.rs:3226-3249`); Win32 button children via `BM_CLICK` (`:2005-2073`);
unelevated top-level windows for posted pointer messages the target's own proc dispatches; the
private hidden-desktop launch for headless runs (`src/hidden.rs`).

**Silently no-ops on these** (result says `ok:true`, nothing happened): DirectInput/DirectX games,
Unity/Unreal, anything polling raw input or `GetAsyncKeyState`; Chromium/Electron renderers, WPF,
Java/Swing, UWP/XAML, ConHost/Terminal for posted clicks/keys (message posted to the frame, child
never sees it); apps that require `WM_MOUSEMOVE`/hover first (no move sent without `hover:true`);
apps that check `GetForegroundWindow`; IME/dead-key/AltGr composition; cross-window drag-and-drop;
double-click-sensitive controls (back-to-back pairs, no `GetDoubleClickTime` gap); any elevated
target is the exception that fails loudly (`uipi_blocked`).

## Not checked

Windows version behaviors were asserted from documented Win32 semantics and the code's own comments,
not live probes (no build, no window created, per instructions); `git log` confirms HEAD `8eb1c2d`
but no test was executed; `cap.rs` resample/`dxgi.rs` capture paths and `deadline.rs` bounds were
not in lens scope and were not read beyond the cited lines.
