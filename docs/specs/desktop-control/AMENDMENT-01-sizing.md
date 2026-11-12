# Amendment 01 — sizing: what an image reply says about its own pixels

Sharpens five clauses of [SPEC.md](SPEC.md). It adds no workstream, reopens no frozen
decision, and edits nothing: SPEC.md keeps the hash recorded in [FREEZE.md](FREEZE.md), and
where this file and SPEC.md disagree, SPEC.md wins and this file is the defect.

| | |
|---|---|
| Amends | W3.1, W3.2 — the image replies only; W3.3, W3.4 and W3.5 are untouched |
| Leaves alone | D1–D7, I1–I6, every item outside W3, [PLAN.md](PLAN.md)'s sequence |
| Changes | two operand origins and one image key on `diff`, one warning channel, two pins |
| Effort | **1.5 engineer-days**, inside W3's existing 4.75 |
| Supersedes | nothing |

| Item | What it settles | Amends | Effort |
|---|---|---|---|
| A1 | A diff's `bbox` can be placed on the returned image **and** on the screen, from the payload. | W3.1 | 0.5 d |
| A2 | A clamp that alters what the caller asked for announces itself; silence is a defect. | W3.1, W3.2 | 0.5 d |
| A3 | The 1280 default is correct and is **not** raised. `max_px:null` is the native path. | W3.2 | 0 d |
| A4 | Detail comes from a native region capture; no upscale path is ever added. | W3.2 | 0.25 d |
| A5 | Every image-bearing reply reports `fmt`/`quality` — already D4's, cross-referenced. | W3.1, W3.2 | 0.25 d |

## The state this amendment starts from

The `screenshot` half is already right, and nothing below touches it:

- **`screenshot` is honest.** It returns `source{x,y,w,h,kind}`, `scale` (`iw/sw`, bilinear,
  aspect-preserved, never upscaling) and the image-to-screen formula (`src/screen.rs:314-337`,
  `:359`, `:398`; `src/cap.rs:1158-1172`). `max_px:null` captures native at `scale:1.0`;
  the default is 1280; 8192 is a typed refusal, not a clamp (`src/cap.rs:1132-1143`).
- **`diff` is not.** Its visualisation is clamped by the same 1280 default
  (`src/serve.rs:508-509` → `src/diff.rs:260,285`) while `bbox` is computed on the
  un-resized operand, and the payload returns neither the applied `scale` nor the returned
  image's dimensions — `finalize` already produces both and both are discarded
  (`src/diff.rs:208-234`; the tuple is `(data, iw, ih, scale, fmt, quality)`,
  `src/cap.rs:1122`). A box drawn on the shrunken visualisation lands in the wrong place,
  and nothing in the reply says so.

That single gap is the whole reason this file exists. Everything else here is a decision
recorded so it is not re-litigated as a defect.

---

## A1 A diff places its box on the image and on the screen

- **Problem** A `diff` reply returns a clamped visualisation and a native-space `bbox` with
  no scale, no image dimensions and no operand origin — so a caller can neither draw the box
  on the image it received nor map it to the screen coordinates every acting verb consumes.
- **Amends** W3.1 (which adds `scale`, `image:{w,h,format}` and `bbox_space:"native"` but
  gives no rule that survives a file operand and no screen mapping at all).
- **Today** `src/diff.rs:208-234` emits
  `{ok,changed,total,ratio,bbox,identical,base,next,threshold,diff_out?}`; `bbox` comes from
  `pixel_diff` on the operand (`src/cap.rs:1002-1047`); the visualisation passes
  `a.max_px` into `finalize` (`src/diff.rs:260,285`) and drops the returned `iw/ih/scale`.
  Operands are forced native and lossless (`src/diff.rs:48-60,100-112`), so "native" and
  "operand-image pixels" are the same grid and always were.
- **Contract** The reply carries the three facts `screenshot` already carries, plus the
  origin `screenshot` gets for free and `diff` currently throws away:
  - `image:{w,h,format}` — the returned bytes. Absent when `identical:true` and no image
    block is returned, exactly as today.
  - `scale` — `image.w / base.w`. Uniform by construction (`src/cap.rs:1172`), `1.0` when
    `max_px:null`.
  - `bbox_space:"native"` — `bbox` is `[x,y,w,h]` in the operand image's own pixel grid:
    device pixels when the operand was a live capture, file pixels for `{path:…}`.
  - `base_source` / `next_source` — `{x,y,w,h,kind}` for a live operand (the `source` object
    the operand's own capture reply already carries, `src/screen.rs:331-337`, discarded today
    at `src/diff.rs:136-148`), `null` for a `{path}` operand.

  Mapping rules, stated in the description so they are not folklore:
  - to the returned image: `img_x = floor(bbox.x * scale)` (same for `y`, `w`, `h`);
  - to the screen, live operands: `screen_x = base_source.x + bbox.x` — the operand is
    device pixels at `scale 1`, so the origin adds with no resample in between;
  - to the screen, `{path}` operands: impossible by construction; `base_source:null` says so
    rather than implying an origin.
- **Fields** Added on `diff`: `image:{w,h,format}`, `scale`, `bbox_space`, `base_source`,
  `next_source`. Unchanged: `changed`, `total`, `ratio`, `bbox`, `identical`, `base`,
  `next`, `threshold`, `diff_out`. Removed: none. No field is renamed, and
  `bbox_space`'s only value stays `"native"` — this item defines it, it does not re-spell it.
- **Acceptance** Two tests in `tests/envelope_truth.rs` (or a new `tests/sizing_truth.rs`):
  (a) a 2000×2000 synthetic PNG pair differing in one 4×4 block returns `scale < 1`,
  `image.w == round(2000 * scale)`, and the mapped read at
  `floor(bbox.x * scale), floor(bbox.y * scale)` is red — the assertion that a naive box
  misses; (b) the same pair as live window operands returns
  `base_source.kind == "window"` and `base_source.x + bbox[0]` equal to the pixel the
  fixture painted. **Fails today:** `scale`, `image` and `*_source` are absent from the
  payload, so (a) indexes on a missing key and (b) cannot compute a screen coordinate.
- **Effort** 0.5 d on top of W3.1's 1 d — the capture reply already carries the source, and
  `finalize` already returns the rest.

## A2 A clamp that bites says so

- **Problem** `quality` is clamped 1..100 inside `finalize` (`src/cap.rs:1154-1155`) with no
  field recording that the request was altered, and `quality` supplied with `fmt:"png"` is
  accepted and then has no effect at all. The reply reports the value it applied, which is
  only detectable by a caller that remembers what it asked for.
- **Amends** W3.2 (its `Contract` says "keeps `quality` clamped 1..100 (ignored for PNG, as
  today)" — a stated clamp with no stated report).
- **Today** `src/serve.rs:464` reads `quality` with no range check (`req_int`,
  `src/main.rs:450`); `src/cap.rs:1154-1155` clamps; `src/screen.rs:321-328` reports the
  applied `image.quality`; `src/cap.rs:1176-1180` ignores quality entirely for PNG. A call
  with `quality:500` returns `ok:true`, `image.quality:100`, and no mention of the clamp.
- **Contract** Every reply that carries an image gains `warnings`, absent when empty, an
  array of `{code, field, requested, applied}`:
  - `code:"quality_clamped"` — `quality` outside 1..100; `requested` the caller's value,
    `applied` the clamped one.
  - `code:"quality_ignored"` — `quality` supplied with `fmt:"png"`, where it had no effect;
    `requested` the caller's value, `applied:null`.

  A clamp that shrinks the returned image is **not** a warning: `max_px` is honoured exactly,
  and `scale` + `image{w,h}` announce the resize on every reply that has them (W3.1, W3.2,
  A1). Emitting a warning on the default capture path would train a caller to ignore the
  channel, so the channel is reserved for requests that were altered. Any future clamp that
  changes what was asked for appends a code here; a new clamp with no code is a defect. A
  new hard ceiling that refuses instead of clamping (`max_px > 8192`, `src/cap.rs:1138-1139`)
  needs nothing, because the refusal names the value.
- **Fields** Added on `screenshot` and `diff`: `warnings:[{code,field,requested,applied}]`,
  absent when empty. Unchanged: `image.quality`.
- **Acceptance** `tests/schema_truth.rs` / `tests/envelope_truth.rs`: `quality:500` returns
  `warnings[0].code == "quality_clamped"` with `requested:500, applied:100`;
  `fmt:"png", quality:50` returns `quality_ignored`; a default call carries no `warnings`
  key at all. **Fails today:** no `warnings` key exists on any reply, so the clamp is
  silent and the PNG case is a silent no-op.
- **Effort** 0.5 d.

## A3 The 1280 default stands

- **Problem** The natural next "fix" after A1 is to raise the default to system resolution so
  the model "sees native". That would spend tokens for no information and would undo the one
  thing the current default gets right.
- **Amends** W3.2, as a decision, so no later contributor reopens it as a defect.
- **Today** `max_px` defaults to 1280 for both tools (`src/serve.rs:437-438`, `:508-509`) and
  `null` means native (`src/cap.rs:1132-1143,1157,1172`). Both are already the intended
  behaviour; nothing is broken.
- **Contract** The default stays **1280**, and is not raised, not made configurable, and not
  driven by display size. `max_px:null` remains the explicit native path, and a native
  capture stays the caller's decision, taken per call, because only the caller knows whether
  it is locating a control or reading 9-pixel text.

  The arithmetic behind the decision, so it is auditable rather than taste: on a 2560×1440
  display the default returns 1280×720 — 921 600 px, 1 196 model tokens at the documented
  `ceil(w/28) × ceil(h/28)` rule. `max_px:null` returns 3 686 400 px, which the same client
  resizes to about 1456×819 — 1 560 tokens. So 4× the pixels on the wire buys ~1.14× the
  long edge after the client's own downscale, for ~1.30× the tokens. The default is inside
  one rounding step of the client's budget; the difference is paid for in context, not in
  information.
- **Fields** None.
- **Acceptance** `tests/schema_truth.rs` pins `max_px.default == 1280` on both `screenshot`
  and `diff`, and asserts `max_px:null` yields `scale == 1.0` with `image.w == source.w`.
  This is a **guard, not a red test**: it passes today and fails on the change this clause
  forbids. Unlike A1, A2 and A5 there is no broken behaviour to make fail, and inventing one
  would be theatre.
- **Effort** 0 d. The two assertions ride the test file W3.2 already touches.

## A4 Detail is region capture, never upscaling

- **Problem** The cheapest-looking way to satisfy "I need more detail here" is to upscale a
  preview. Upscaling adds bytes and pixels but no information, and it makes `scale` a lie in
  the other direction.
- **Amends** W3.2's `Contract` (whose "never upscaling" today lives only in schema prose,
  `src/schemas.rs:8`), by making it a tested property and naming the supported path.
- **Today** Region capture already exists and composes with native: `region` is parsed and
  validated (`src/screen.rs:120-160`, `:554-580`), intersected with the capture scope and
  grabbed at the crop rect, with `source` reporting the intersected crop
  (`src/screen.rs:246-273`). `finalize` resizes only when `max(long edge) > max_px`
  (`src/cap.rs:1158-1170`), so `max_px:null` returns the crop at `scale:1.0`. The `diff`
  operands are already native and lossless (`src/diff.rs:48-60,100-112`). The only resample
  toward larger dimensions anywhere in the tree is the internal 64×64 blank-detector
  thumbnail (`src/cap.rs:475`), which never produces returned bytes.
- **Contract** Detail is requested as `region:[l,t,r,b]` plus `max_px:null` (a tight window
  or region), never by asking for a bigger preview. No upscale path is added: `finalize`
  keeps its `>` guard, and no `max_px` value larger than the capture can enlarge anything.
  The `screenshot` description states this in one clause next to the existing zoom guidance
  (`src/schemas.rs:8`), so the recommended route is the only one an agent reads.
- **Fields** None. This item is a property and a sentence.
- **Acceptance** A characterisation test in `tests/envelope_truth.rs`:
  `screenshot{region:[…], max_px:null, fmt:"png"}` returns `scale == 1.0`,
  `image.w == source.w`, `image.h == source.h`, and a second identical call is
  byte-identical; plus a source scan asserting the resize branch in `src/cap.rs` stays
  guarded by `sw.max(sh) as i64 > m` (`:1159`), so an upscale cannot be introduced without
  failing the scan. **Passes today**, and must keep passing: it is the guard on the decision,
  not a red test.
- **Effort** 0.25 d.

## A5 `fmt`/`quality` on every image-bearing reply — already D4's

- **Problem** An agent cannot tell lossless data from JPEG data unless every image-bearing
  reply says which it is, and `diff` currently says less than `screenshot` does.
- **Amends** W3.1 only, and only by one key. **This is a cross-reference, not a new
  requirement:** D4 ("perceive losslessly by default; preview scale always reported",
  SPEC.md §"The frozen decisions") already governs it, and W3.2 already states it —
  "the `image.format` field already reports which was used" — and already asserts
  `image.format == "png"` and `image.quality == null` on a default capture.
- **Today** `screenshot` reports `image:{w,h,format,quality,grid}`
  (`src/screen.rs:316-330`); `diff` reports no `image` object at all (`src/diff.rs:208-234`).
  The diff visualisation is always PNG with quality `None` (`src/diff.rs:260,285`), so
  `quality` is structurally `null` there.
- **Contract** W3.1's `image` block carries `format` and `quality` with the same shapes as
  `screenshot`'s; on `diff`, `quality` is always `null` because the visualisation is always
  PNG. Nothing else changes — this adds no field beyond what D4 already implies, and the
  lossless default for the image an agent reasons on is W3.2's, unchanged.
- **Fields** Added on `diff`: `image.quality` (always `null`). `image.format` is W3.1's.
- **Acceptance** A default `screenshot` asserts `image.format == "png"` and
  `image.quality is null` (W3.2's test, unchanged); a changed `diff` asserts
  `image.format == "png"` and `image.quality is null`. **Fails today:** `diff` has no
  `image` object, so the key cannot be read.
- **Effort** 0.25 d — the block is W3.1's to build; this adds one key and one assertion.

---

# Appendix AM-A — effort, and what it does not cover

| Item | Days | Lands in |
|---|---|---|
| A1 | 0.5 | W3.1 |
| A2 | 0.5 | W3.1, W3.2 |
| A3 | 0 | W3.2 (guards only) |
| A4 | 0.25 | W3.2 |
| A5 | 0.25 | W3.1 |
| **Total** | **1.5** | W3's 4.75 becomes 6.25; the release total becomes **36.75** |

The frozen total in [FREEZE.md](FREEZE.md) is not edited and stays as the record of what was
frozen; this table is the correction that applies when W3 is built. Nothing here is a new
workstream, and nothing here changes the sequence.

Explicitly out of scope for this amendment:

- Any change to `max_px`'s default, ceiling (8192) or meaning (A3).
- An area/token-budget clamp alongside the long-edge clamp. The evidence for one lives in
  the model client's own resize rule, not in this server's contract, and adding it would move
  every existing caller's `scale` for a gain measured in one rounding step.
- Upscaling, or a zoom verb that enlarges (A4).
- A second scale report on `do`'s pixel verification — it consumes images, it returns no
  pixels.
