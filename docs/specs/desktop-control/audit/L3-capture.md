# Lens 3 — Is the screenshot true? (puppet capture fidelity)

Repo `Q:\PROJECTS\PERSONAL\puppet`, HEAD `8eb1c2d`. Read-only audit; no builds, no captures.
Method: `src/cap.rs` (grab/encode/diff), `src/shot.rs` (window path), `src/screen.rs`
(desktop path + mapping), `src/dxgi.rs` (fast path), `src/uia.rs` (element rects),
`src/main.rs` (FORMULA, DPI, rects), `src/serve.rs` (defaults), `docs/GUIDE.md`.
`git log --oneline -20` confirms HEAD above.

## Answer first

Puppet's capture chain is unusually honest about its failure modes (typed refusals
instead of garbage pixels), and the coordinate formula checks out. The places an
agent's visual verdict goes wrong anyway, ranked:

1. **`diff` bbox is native pixels; the diff *image* is clamped — with no scale to
   reconcile them.** Counts/bbox are computed full-res, then the viz is
   long-edge-clamped (`max_px` default 1280) for the wire, and the payload carries
   no viz dimensions or scale. Overlaying `bbox` on the returned image misplaces
   the box whenever native > clamp. **Lie by omission — HIGH.**
2. **Default screenshots are lossy JPEG (q80).** Any agent-side pixel reasoning on
   default captures, or a `diff` fed by saved default captures, inherits
   compression noise. The live-`diff` path itself is safe (forces native PNG).
   **Gap — MEDIUM-HIGH.**
3. **No freeze facility; threshold-0 `diff` counts everything.** Caret blink,
   taskbar clock, animations, toasts all flip `identical` to false; there is no
   anti-aliasing tolerance by design. False-positive "changed" verdicts are the
   default outcome on live UI. **Gap — MEDIUM-HIGH.**
4. **The cursor is never in the pixels** (neither backend composites it), though a
   `cursor{x,y}` position is reported alongside. An agent looking *for* the cursor
   in the image will never find it. **Gap — MEDIUM** (also a determinism win).
5. **Black-frame detector catches only full-black.** Partial DRM/video black or a
   stale-but-plausible frame passes as truth. **Gap — MEDIUM.**
6. Narrower: alpha dropped in `diff` decode (transparency-only change is
   "identical"); window-path element walk can postdate the pixels with no
   element-side moved flag; DXGI serves a cached frame on timeout with no
   staleness flag; HDR/wide-gamut untagged; banker's rounding vs `Math.round`.
   **Gaps — LOW.**

## 1. Capture API and blind spots

- Window path = `PrintWindow`, trying flags `[2, 0]` (`PW_RENDERFULLCONTENT` then
  plain) — `src/shot.rs:109-118` (`render_frame`), `:181-192`. No
  `BitBlt`-from-screen for windows (except opt-in fallback), no DWM thumbnails,
  no Windows.Graphics.Capture — Mode B explicitly deferred,
  `src/main.rs:44` (`MODE_B_NOTE`).
- Desktop/region path = DXGI Desktop Duplication fast path per full output, else
  GDI `BitBlt` from the screen DC — `src/screen.rs:617-686`, `src/cap.rs:399-413`,
  `src/dxgi.rs:1-31`. Multi-monitor virtual span (`display=-1`, >1 monitor) goes
  **straight to GDI** with a `note` — `src/screen.rs:616`, `:630-634`. Region grabs
  go straight to GDI (no full-frame grab) — `src/screen.rs:617-623`.
- Refusals (honest, typed — these are the opposite of lies): minimized
  `window_minimized` (`shot.rs:147-149`); cloaked (`:150-153`);
  `WDA_EXCLUDEFROMCAPTURE` (`:173-177`); `PrintWindow`-refused background window →
  `capture_unavailable`, no occlusion-prone silent crop unless
  `allow_crop_fallback:true` **and** foreground (`shot.rs:339-399`); zero-size
  (`:169-171`); `MAX_GRAB_EDGE` 8192 (`screen.rs:19`, `:625-629`).
- GPU-composited content: full-black/full-blank frames are rejected by the
  detector (`max(R,G,B) < 8` over the frame, `cap.rs:489-497`) — Chromium/Electron
  black-window failures surface as errors, good. But the detector is all-or-
  nothing: a **partially** black frame (DRM video rect, half-painted window,
  letterboxed player) has `hi >= 8` and passes as a true capture. A stale-but-
  non-identical frame (compositor re-presenting an old buffer after content
  changed) is undetectable by the identical-signature retry (`shot.rs:211-237`,
  one 100 ms retry only when the sig *equals* the previous capture of that hwnd).
  **Gap — MEDIUM.** Fix the claim: document detector as "full-frame blank only,
  not partial/stale."
- DXGI serves the **cached** frame when `AcquireNextFrame(0)` times out
  (`dxgi.rs:415-437`). Benign when the screen is truly static (cache == current),
  resolution change handled (`check_size`, cache dropped `:424-431`), but the
  pixels can predate the call with no staleness flag and a fresh-looking `ms`.
  **Gap — LOW.**

## 2. Coordinates and scaling

- Formula `src/main.rs:45`:
  `screen_x = source.x + round((img_x + 0.5) * source.w / image.w)` (same for y);
  user docs `docs/GUIDE.md:190-192`. Verified correct under the exclusive-edge
  convention: first pixel maps to `source.x`, last pixel to `source.x+source.w`
  (one-past-end). `round` is Python half-even (`py_round_half_even`,
  `main.rs:62-78`); an agent using `Math.round` (half-up) disagrees only on exact
  `.5` with odd floor — sub-pixel rarity. **Verified, not a bug — LOW.**
- All rects are physical device pixels: process sets
  `DPI_AWARENESS_CONTEXT_PER_MONITOR_AWARE_V2` (`main.rs:1486-1487`), so
  `DWMWA_EXTENDED_FRAME_BOUNDS` (`main.rs:1075-1096`), `GetWindowRect`, and
  `EnumDisplayMonitors` (`main.rs:915-926`) agree in the same space as the
  `BitBlt`/`GetDIBits` bitmap. Mixed-DPI multimonitor is therefore consistent by
  construction (each monitor's physical rect tiles the virtual desktop).
- Negative-offset monitors: `i32` throughout; `virtual_rect()` uses
  `SM_XVIRTUALSCREEN/SM_YVIRTUALSCREEN` (`main.rs:959-967`); region intersect
  math is sign-safe (`screen.rs:583-592`, overflow-guarded `:558-582`).
  **Verified — no lie.**
- Straddling windows: single `dwm_rect`, `PrintWindow` renders the whole window
  regardless of monitor — no seam. Virtual-span multi-DPI via one GDI `BitBlt`
  is contiguous physical pixels — fine.
- `scale = iw/sw` (`cap.rs:1172`); exactly `1.0` when `max_px:null` or image below
  the clamp (never upscales, `cap.rs:1158-1171`); `iw/ih` via half-even
  (`:1161-1162`) and `scale` is what the formula consumes. `screen.rs:743-747`
  refuses non-uniform scale. **Verified — no drift.**
- Resize kernel caveat (one line): downscales use a Pillow-faithful bilinear port
  (`cap.rs:524-531`); edge pixels blend, so formula-mapped clicks within ~1 px of
  a high-contrast edge can land on the blended neighbor. Inherent to any
  downscaled screenshot, not a code bug.

## 3. Colour and format truth

- No colour management anywhere: `GetDIBits` `BI_RGB` 32-bit → alpha dropped,
  RGB kept (`cap.rs:337-373`); PNG encoded untagged `Rgb8` (`cap.rs:861-870`);
  DXGI path demands `B8G8R8A8_UNORM` and anything else (e.g. HDR FP16/scRGB)
  falls back to GDI (`dxgi.rs:215-217`, `screen.rs:672-683`, labelled
  `backend:"gdi"`). On an HDR/P3 display the bytes are the SDR-composited
  framebuffer, silently gamut-clamped and untagged. **Gap — LOW** on a standard
  SDR box; fix the claim with one sentence in GUIDE.
- Default `fmt` is **`jpeg` q80** (`serve.rs:456-463`); PNG only when `out` is set
  without `fmt`. JPEG quality clamped 1–100 (`cap.rs:1154-1155`). Consequences:
  (a) two default captures of identical pixels are **not** byte-identical;
  (b) an agent saving default screenshots and feeding them to `diff` (or its own
  comparator) gets compression-noise positives. The `diff` **live-capture** path
  is immune — it forces `fmt png`, `max_px None`, `grid false`
  (`diff.rs:48-60`, `:100-113`). **Gap — MEDIUM-HIGH.** Smallest honest fix:
  document "default captures are lossy; pass `fmt:png, max_px:null` for any pixel
  comparison" next to the `max_px` row (`GUIDE.md:177-179`).
- PNG round-trip is byte-exact (pinned by test `cap.rs:1232-1242`).
- `scale != 1` whenever the long edge exceeds `max_px` (default 1280,
  `serve.rs:437-453`); `scale` is consumed by the formula, so clicks still land.
  `max_px:null` returns native pixels (capped 8192, `cap.rs:1132-1143`).

## 4. `diff` semantics

- Rule: per-pixel max per-channel abs delta `> threshold` counts; union bbox;
  `ratio = changed/total`; dimensions must match or typed `size_mismatch` naming
  both (`cap.rs:990-1050`, `diff.rs:179-206`). Ports gladius `png_diff`
  (`cap.rs:885-893`).
- The gladius-class bug (silent under-count passing as "no change") is **not
  present in that form** — there is no AA-subtraction logic at all; the comment
  says so explicitly (`cap.rs:892-893`: "There is no anti-aliasing rule here").
  The failure mode here is the **mirror**: with default `threshold 0`
  (`serve.rs:507`), sub-pixel AA/text-rendering shimmer always *counts*.
  `threshold > 0` then swallows real sub-threshold drift by design — a caller
  raising `threshold` to quiet AA noise can hide a real 1–2 LSB change. That is
  the documented contract ("threshold is the only tolerance", `GUIDE.md:221`),
  so: **gap, not lie — MEDIUM-HIGH in combination with §5.**
- No region masking: `DiffArgs` is exactly
  `{base, next, threshold, max_px, out, bbox}` (`diff.rs:20-27`). A caller cannot
  exclude the clock/caret rect. **Gap — MEDIUM** (fix: behaviour or claim).
- **bbox/viz mismatch (the real lie):** counts and `bbox` are computed on native
  pixels, but the returned viz image is `finalize(viz, a.max_px /* default 1280
  */, png)` (`diff.rs:281-288`) — clamped for the wire while `bbox` stays
  native, and the payload carries **no viz dimensions or scale**
  (`diff.rs:208-234`: `{changed,total,ratio,bbox,identical,base:{w,h},next:{w,h},
  threshold,diff_out?}`). Whenever native long-edge > 1280, an agent drawing
  `bbox` onto the shown image misplaces it by up to `native/1280`. The viz's own
  red pixels are drawn pre-clamp then resampled (visible but bled), so the image
  and the numbers disagree about *where*. **Lie by omission — HIGH.** Smallest
  honest fix: add `image:{w,h}` (post-clamp) to the diff payload, or compute
  `bbox` in viz space; either is a few lines in `dispatch_diff`.
- Alpha is dropped on decode (RGBA→RGB, gray+alpha→gray, `cap.rs:925-969`); a
  **transparency-only change diffs `identical:true`**. Palette/16-bit PNGs are a
  typed refusal (honest, `cap.rs:970-974`). **Gap — LOW-MEDIUM.**
- Identical case emits no image block (documented, `GUIDE.md:231-232`), `bbox`
  `null` unless `bbox:true` (default true, `diff.rs:213-224`, `serve.rs:526-539`).
  Honest.

## 5. Dynamic content

- Cursor: **never composited into pixels by either backend.** No
  `DrawIcon`/`GetCursorInfo` anywhere in `src/` (grep: zero hits); GDI `BitBlt`
  from the screen DC does not draw the hardware cursor; DXGI duplication
  "typically" excludes the pointer shape by design (`dxgi.rs:28-31`, position
  still from `GetCursorPos`). `cursor{x,y}` in the map (`screen.rs:364-367`,
  `:775-778`) is a *position report*, not an image annotation. No doc claims the
  cursor is visible — **gap, not lie — MEDIUM** for agents that look for it; it
  is also why cursor motion can't cause diff noise (good).
- Everything else **is** baked in with no freeze: blinking carets, text-cursor,
  animations, video, toasts, tooltips, hover states, taskbar clock. No
  `DwmFlush`/vblank wait, no settle parameter on `screenshot` (grep: `settle_ms`
  is burst-only; screenshot path has none), no `deterministic`/`freeze` mode.
  The only stability aid is the window-path STALE retry (identical-sig →
  one 100 ms re-grab, flag `stale`, `shot.rs:211-237`) — it detects a *frozen*
  frame, it does not freeze a *live* screen. Two back-to-back desktop captures
  differ whenever the clock ticks. `foregroundUnchanged`/`cursorUnchanged`
  (`screen.rs:412-419`, `:796-803`) testify only that the *owner* didn't
  interfere, not that content is stable — an agent could misread them as a
  determinism guarantee. **Gap — MEDIUM-HIGH.** Smallest honest fix: one GUIDE
  line — "consecutive captures are not guaranteed identical; mask or threshold
  volatile regions; `foregroundUnchanged` is about input, not pixels."

## 6. `elements:true` coordinate agreement

- Both planes claim screen space and mean it: `rect_space:"screen"`
  (`shot.rs:663`); UIA `CurrentBoundingRectangle` (physical screen px,
  `uia.rs:266-286`, `[l,t,w,h]`); Win32 fallback `GetWindowRect` (physical under
  PerMonitorV2, `shot.rs:453-461`). Schema text says element rects "compose with
  the same coordinate_map formula" (`schemas.rs:8`, TOOL_SCREENSHOT) — shapes
  differ (`[l,t,w,h]` vs `source{x,y,w,h}`) but the mapping is the same origin +
  scale math. **Agreement verified for a stationary window.**
- TOCTOU: the element walk runs **after** the capture with only the target-lock
  revalidated (`screen.rs:433-439`) — no geometry re-check. If the window moves
  between `PrintWindow` and the UIA walk, `moved_during_capture` describes the
  *image* only; the element rects silently describe the new position and
  misalign with the pixels. Narrow race, flagged honest on the image side.
  **Gap — LOW.**
- Desktop/region + `elements:true` names the window the block describes via
  `elements_window{hwnd,exe,title}` sampled **once** at call start
  (`screen.rs:506-512`, `:813-835`, regression-tested `:1061-1129`) — the one
  place pixels and model knowingly diverge (image = desktop, block = foreground
  window), and it is explicit. Window path has no `elements_window` (it is the
  target by construction). **Honest — no finding.**

## Not checked

- Live behavior (no captures built or run): actual `PrintWindow` output on
  GPU-composited/occluded/layered windows; real mixed-DPI and HDR rigs; DXGI
  session behavior under RDP/Citrix/session-0; measured `BitBlt` vs DXGI pixel
  equality across backends (same BGRA→RGB order in both paths — `cap.rs:367-372`
  vs `dxgi.rs:183-186 — suggests equality but unexecuted).
- `list_windows` rects and `cursor.screens` vs screenshot `monitors.rects`
  consistency (same `EnumDisplayMonitors` source, not traced call-by-call).
- Whether `grid:true` + `out:png` bytes disturb downstream `diff` users (grid is
  forced off on diff live-captures; saved grid files are caller error).
- Timing/`ms` accuracy, deadline/`capture_timeout` values (`deadline.rs`
  untouched).

## Adjacent (one line each)

- `allow_crop_fallback` desktop crop reports `moved_during_capture` but never
  recaptures (best-effort by design, `shot.rs:351-364`) — documented in the note;
  fine.
- JPEG `quality` is accepted-and-ignored for PNG (`screen.rs:322-328`) —
  documented; fine.
