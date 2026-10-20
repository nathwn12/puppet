# Headless desktop (`desktop:"hidden"`)

Opt-in mode for the `do` tool. Plain `do` never touches it.

## Request

`do` with `desktop:"hidden"` plus `background` naming the program to
launch (`exe:notepad.exe` or a path). Window selectors (`hwnd:`, `pid:`,
`title:`, `class:`) are refused: there is no window yet to select.
`background` names a single program; it takes no arguments — `background_args`
supplies argv (array of strings) for the hidden launch only.

## What happens, in order

1. A private desktop is created and one dedicated thread moves onto it
   (`GetThreadDesktop` → `CreateDesktopW` → `SetThreadDesktop`). The
   server's own thread desktop never moves.
2. The program launches with its desktop pinned to the hidden one, with
   `background_args` as its argv (correctly quoted, so document paths
   with spaces survive as one argument; zero args keeps the historical
   bare-program command line byte-identical). Without
   `desktop:"hidden"`, `background_args` is ignored.
3. Its top-level window is resolved with `EnumDesktopWindows` — never the
   owner's `EnumWindows` — then the thread waits (bounded, 3 s) for a
   text-input control to exist before the caller drives, plus up to 4 s
   more for the window title to name the first argv entry's file stem
   when that entry is an existing file (the app may restore its session
   first and open the document async).
4. The burst drives the window through the same `PostMessage` path as
   plain `background` mode (best-effort; the target is never activated,
   focused, or raised). Live runs settle 1 s before evidence so posted
   messages are processed; `dry_run` posts nothing.
5. Evidence is read on the hidden-desktop thread: liveness, text
   readback, and a `PrintWindow` capture. The pre-drive readback is
   snapshotted alongside the per-control snapshot.
6. Driven input is reverted (pre-drive texts are restored), then the
   readback is taken again as `post_revert_text`, then the
   target is closed (`WM_CLOSE`, remainders terminated), the thread moves
   back, the desktop closes, and destruction is proven by reopening the
   name and requiring `NULL` (polled up to 2 s: a dying process can
   transiently pin its desktop past our own close).

## Evidence (`hidden_desktop` in the `do` JSON)

- Setup: `requested`, `desktop_name`, `desktop_created`,
  `set_thread_desktop`, `launched_pid`, `window_hwnd`, `window_class`,
  `window_title`, `window_pid`, `window_on_hidden_desktop`,
  `absent_from_owner_enum_windows` (the window is never on the owner's
  desktop).
- `window_alive`, `text_readback`: the joined text of every non-empty
  Edit-like descendant (recursive; mirrored views may repeat the same
  text).
- `pre_drive_text`, `post_revert_text`, `reverted`: the same readback
  before any driven input and after the revert, in the same teardown, plus
  whether they match. Same-fixture evidence — a cross-launch comparison
  would pass even with the revert removed, since every launch initializes
  the same content.
- `capture`: `ok`, `print_flags` (2 = `PW_RENDERFULLCONTENT` first,
  0 on retry), `w`, `h`, `min`, `max`, `uniform`, `bytes`, `png_b64`.
  On a successful capture the PNG also rides as a real MCP `image`
  content block next to the text JSON.
- `teardown`: `close_posted`, `process_exited`, `terminated`,
  `desktop_empty`, `set_thread_desktop_restored`, `close_desktop`,
  `open_after_close_null`, `proven` (all four of restored, closed,
  open-null, and desktop-empty).
- Owner untouched: `owner_cursor_delta` plus `cursor_before`/`cursor_after`,
  `owner_foreground_delta` plus `foreground_before`/`foreground_after`.

## Typing target

Keyboard messages posted to a top-level frame never reach its edit
control, so `type`/`key`/`hotkey` resolve the focused control when it is
text input (`GetGUIThreadInfo`), else the first visible + enabled
text-input descendant (recursive). The same resolution serves plain
`background` mode; mouse actions are unchanged (coordinate posts to the
window as before).

## Limits (do not overclaim)

- A private desktop is not a private profile: the launched app still runs
  as you, with your per-user persisted state. Observed: Win11 Notepad
  restores its saved tabs on a fresh hidden desktop and may keep a
  restored tab selected instead of the argv document, which makes a
  title-based proof nondeterministic. The deterministic test path is the
  dedicated fixture app below, which has no session state. A private
  desktop isolates windows and input — never the profile.
- Driven input is reverted before teardown so headless runs leave no
  typed text behind; only text content is restored, not caret position.
  Never edit owner data to clean up after a run.
- `PostMessage` delivery is best-effort: classic EDIT controls honor
  posted `WM_CHAR`/`WM_SETTEXT`, while some framework-hosted controls
  may ignore messages that did not come through real input. The
  per-action `posted` counts messages queued, not effects produced;
  `text_readback` is the proof they landed.
- Capture is `PrintWindow`: a pixel copy of the window, not a composited
  screen; some windows paint flat or refuse.
- No UAC prompts: elevation consent lives on the secure desktop and can
  never reach the hidden one, so the program must run unelevated without
  prompts; elevated targets are refused with the usual UIPI error.
- Single launched program, closed before return. The owner's cursor,
  foreground window, and focus are never touched; the proof fields above
  are the evidence, not an assertion.

## Tests

`tests/hidden_desktop.rs` (live, gated by `PUPPET_LIVE_DESKTOP=1`;
skips cleanly without it) launches the dedicated fixture app
(`src/bin/puppet_fixture_app`: one window titled from argv[1], one EDIT
control holding `baseline`, no I/O or persistence) on the hidden desktop
via `background` + `background_args: ["isolation-marker"]`, types a
unique marker, and asserts the window title proves argv delivery, the
readback holds baseline plus marker, `teardown.proven`, the MCP image
block, owner-desktop isolation, and — from the same fixture before
teardown — that `pre_drive_text` is the clean baseline, `post_revert_text`
equals it, `reverted` is true, and the typed readback differs from both.
`tests/clipboard.rs` (same gate) proves the
`clipboard_set`/`clipboard_get` round trip and that `dry_run` set
changes nothing, restoring the saved clipboard afterwards.
`tests/p3b_elements.rs` (schema and validation asserts always run; live
parts gated) proves element paging and the element wait against the
fixture app (`argv[2]` adds N named buttons after the baseline edit so a
251-control tree exercises real page boundaries; without it the window is
exactly the single-edit fixture).

## Elements continuation

`elements` accepts `max_depth` (1..32, default 8) bounding the UIA walk
(report back as `depth_limit`, with `depth_capped` telling whether the
cut-off hid controls) and `offset` (default 0) skipping that many leading
controls. The walk still caps at 200 entries; when the cap is hit and more
controls remain, `next_offset` carries offset + returned count, else null.
Both backends (UIA walk and Win32 fallback) page identically. The
screenshot `elements=true` block always uses the defaults (depth 8,
offset 0, cap 200).

## Element wait

`wait_for` `kind:"element"` polls the element model instead of pixels:
`window` is an optional window selector (same grammar as the `window`
tool; omitted means the foreground window, and no foreground window is
unsatisfied with a reason, not an error), `element` is a required
`name:`/`auto_id:`/`path:` selector (same resolution as
invoke/`set_value`), `state` is `appear` (default), `disappear`,
`enabled`, or `disabled`. `enabled`/`disabled` query UIA IsEnabled (or
the Win32 enabled flag under the fallback); the control need not be
invokable. A timeout is satisfied:false with ok:true like the other
kinds; a malformed selector, an ambiguous match, or an unresolvable
window is a per-action error. If the window disappears mid-wait,
`disappear` is satisfied and the other states keep polling to timeout.
Polling is cancellable like the other kinds. UIA is best-effort and the
walk is raw-view: offscreen and framework-hosted controls appear as the
provider exposes them.

## UIA patterns (P4c)

`invoke` accepts an optional `state` `"expand"|"collapse"` which drives
only the UIA ExpandCollapse pattern — never Invoke (a SplitButton
supports both, and Invoke could open a menu instead of collapsing). A
control without the pattern is a typed error, and the result reports the
post-action state in `expand_state`
(Collapsed|Expanded|PartiallyExpanded|LeafNode).
`set_value` falls back to UIA RangeValue for numeric controls (sliders):
numeric text is parsed as f64, range-checked against the control's
minimum/maximum, and read-only is honored; success reports the live value
in `value`. `elements` with `detail:true` adds each UIA entry's
TextPattern document text as `text`, capped at 4000 chars with a
`text_truncated` flag (entries without readable text omit both keys);
text never participates in the `fp` fingerprint.
`tests/p4c_patterns.rs` (schema assertions always run; live parts gated)
proves all three against the fixture app's `p4c` mode (tree + trackbar +
multiline edit).
