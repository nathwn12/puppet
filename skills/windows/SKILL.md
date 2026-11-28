---
name: windows
description: Target and drive Windows windows without stealing focus, moving the pointer, or touching the clipboard; screenshot cropping and scaling rules; cleanup expectations.
---

# Windows targeting and background-safe driving

## Targeting a window

- Every acting or capturing tool takes one `target` selector that resolves exactly one window: `{hwnd,pid,exe,appId,titleContains,className}` (all present fields AND). Sugar strings `hwnd:1234`, `exe:notepad.exe`, `title:...`, `class:...` also work.
- The window is resolved once, then revalidated before every action (`IsWindow`, owner pid, process creation time, image name, class, AppUserModelID). A recycled pid that passes every other check is still `target_gone`.
- An ambiguous or vanished target is a typed error (`ambiguous_target` with candidates, or `target_gone`) and the call does nothing.
- There is no fallback to the foreground window, ever. Without a target, keyboard verbs refuse with `target_required`.

## Never steal focus, move the pointer, or touch the clipboard

- Background-only policy is absolute: `SendInput`, `SetCursorPos`, `SetForegroundWindow`, `BringWindowToTop`, `mouse_event` have zero call sites (pinned by `tests/no_steal.rs`).
- `allow_foreground`, `allow_cursor`, `allow_clipboard` are accepted by the schema only to return a typed refusal (`no_foreground_policy`, `no_cursor_policy`, `no_clipboard_policy`); they never enable anything.
- `focus_window`, `window action:"focus"`, `window action:"maximize"` are refused (`no_foreground_policy`). `window move|resize|minimize|restore|topmost|close` use `SWP_NOACTIVATE`/`SW_SHOWMINNOACTIVE`/`WM_CLOSE`.
- `clipboard_set` is refused (`no_clipboard_policy`); `clipboard_get` stays (reading cannot disturb). `type`/`set_value` with an element are refused (`background_unavailable`).
- Every acting/capturing call records `GetForegroundWindow` and `GetCursorPos` before/after and reports `foregroundUnchanged`/`cursorUnchanged`.

## Screenshots: crop and scale

- `screenshot` captures the whole virtual desktop (`display=-1`), one monitor, a window (`window=<selector>` or `target={...}`), or a region; it never activates the window.
- Coordinates under a target are client-area physical pixels; without a target they are virtual-desktop screen pixels. Window/element rects are always screen pixels.
- Image-to-screen formula: `screen_x = source.x + round((img_x + 0.5) * source.w / image.w)` (same for y). The map carries `source{x,y,w,h,kind}`, `scale`, monitors, cursor, window, capture, formula.
- `max_px` (default 1280) clamps the long edge, aspect-preserving, never upscales; `null` returns native (max 8192). `region` is `[left,top,right,bottom]` virtual-desktop px applied after the window crop.
- `allow_crop_fallback` (default false) permits a foreground-only desktop crop to `dwm_rect` when `PrintWindow` refuses.

## Cleanup expectations

- `do` with `desktop:"hidden"` runs on a private desktop and reverts driven text before teardown; never edit owner data to clean up after a run.
- Headless runs leave no typed text behind (only text content is restored, not caret position); the target is closed and the desktop destroyed and proven.
- Prefer `dry_run=true` to validate policy without posting anything. Errors are typed; there is no escalation path.
## Tools and coordinates (summary)

- Tools: `screenshot`, `diff`, `list_windows`, `window`, `do`, `cursor`, `elements`. `list_windows` is read-only and never changes focus.
- Coordinates: under a target, `x`/`y` are client-area physical pixels; without a target they are virtual-desktop screen pixels. Rects from `elements` and `list_windows` are always screen pixels.
- `do` runs an ordered `actions[]` batch against one target via UIA patterns first, then `PostMessage`/`SendMessage`; per-action `ok`, never aborts the rest. `dry_run=true` validates exactly like the live call (every policy refusal included) but posts nothing.
- `elements` is read-only structured UI for a window (zero pixels, no focus change); entries carry `ref`, `fp`, `rect`, `offscreen`, `invokable`.

## Error codes you will see

`no_foreground_policy`, `no_cursor_policy`, `no_clipboard_policy`, `target_required`, `ambiguous_target`, `target_gone`, `background_unavailable`, plus capture/input codes (`capture_failed`, `empty_crop`, `bad_coordinates`, etc.).
