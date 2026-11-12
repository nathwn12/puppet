# Desktop control — frozen specification

The contract this server will satisfy next. Frozen: the decisions below are settled and
the workstream order is fixed. [PLAN.md](PLAN.md) is the same 28 items in work-shape;
this file is the contract they must meet. [FREEZE.md](FREEZE.md) records scope, the semver
call and this file's hash.

Target release: **`puppet-mcp` 8.0.0**. Every reply shape named here is final.

## How to read this

Each item carries the same seven fields:

| Field | Meaning |
|---|---|
| **Problem** | What is wrong today, in one sentence. |
| **Closes** | The audit finding, cited. |
| **Today** | The current behaviour and its `file:line`. |
| **Contract** | The exact behaviour after the change. |
| **Fields** | Reply fields added, renamed or removed. |
| **Acceptance** | A test that fails on today's code, naming the behaviour that fails. |
| **Effort** | Engineer-days, honest, summed in [Appendix E](#appendix-e--effort). |

Citation grammar, matching the audit copies in [`audit/`](audit/):

- `Ln/§m` — section `m` of lens `n` (`L1/§3`).
- `Ln/Rk` — item `k` of lens `n`'s own ranked fix list (`L1/R1`).
- `Ln/Ak` — item `k` of lens `n`'s answer-first list (`L3/A1`).

Where two lenses disagree, the item says so and states which wins. Nothing here is
asserted from a lens alone where source could settle it; where the lenses disagree with
source, **source wins** and the item says what the source shows.

## The frozen decisions

| | Decision | Items |
|---|---|---|
| D1 | The contract is generated, not written. | W1.1, W1.2, W1.3, W1.4 |
| D2 | Never act on a possibly-stale control. | W2.1, W2.2 |
| D3 | `ok` means delivered, never worked. | W4.1 |
| D4 | Perceive losslessly by default. | W3.1, W3.2 |
| D5 | State is read, not guessed. | W5.1, W5.2 |
| D6 | The guarantee is asserted, not self-reported. | W6.1, W2.2 |
| D7 | Named non-goals. | [FREEZE.md](FREEZE.md#scope-out) |

## Invariants

True before this spec and required to stay true; a change that breaks one is a defect
regardless of which item it lands in.

| | Invariant | Held by |
|---|---|---|
| I1 | No `SendInput`, `SetCursorPos`, `SetForegroundWindow`, `BringWindowToTop`, `mouse_event` call site anywhere. | `tests/no_steal.rs` symbol scan |
| I2 | Every refusal happens before anything is sent, under `dry_run` too. | `src/gate.rs`, per-action pre-checks |
| I3 | The target lock is revalidated before every action. | `src/target.rs:357-448` |
| I4 | Nothing falls back to the foreground window. | `src/target.rs`, every acting path |
| I5 | The server never elevates itself and never escalates a permission. | no `runas` path exists |
| I6 | Props that never appear mean "the control has no such pattern", never "false". | W5.1 |

---

# W1 — Contract truth

The first text an agent reads must describe this server and no other.

## W1.1 `instructions()` is generated from one claims source

- **Problem** The `initialize` contract teaches six tools, an escalation path that always
  refuses, flat coordinates and a foreground-touching `focus` — none of which this server
  does, and all of which the schemas contradict.
- **Closes** L5/§5 (`instructions()`), L5/A1 (three defects), L2/§5 (stale copy).
- **Today** `INIT_INSTRUCTIONS` is one hand-written string literal at `src/serve.rs:126`;
  it lists six tools (no `diff`), names `foreground_required` / `cursor_move_required` /
  `clipboard_write_required` "unless the call passes `allow_foreground:true` /
  `allow_cursor:true` / `allow_clipboard:true`", says "Coordinates are
  screen/virtual-desktop pixels", and says "only action=focus may touch the foreground
  window". The real refusal codes are six constants at `src/gate.rs:41-56`
  (`no_foreground_policy`, `no_cursor_policy`, `no_clipboard_policy`,
  `background_unavailable`, `offscreen_element`, `target_required`); the tools are seven
  (`src/serve.rs:129`); `focus` is refused (`TOOL_WINDOW`, `src/schemas.rs:11`). Even the
  doc comment above the constant says "the six tools" (`src/serve.rs:122-125`).
- **Contract** A new `src/claims.rs` is the single source for every load-bearing claim an
  agent can act on: `TOOL_LINES` (seven one-line tool purposes, in `tools_list_result`
  order), `COORD_RULE` (client pixels under a target, virtual-desktop pixels without one,
  rects always screen pixels), `REFUSALS` (verb, class, code — derived from the gate
  matrix), `ERROR_CODES` (the authoritative code list), `INVARIANTS` (I1–I6, one line
  each), `LIMITS` (the named non-capabilities). `instructions()` is built from these at
  compile time — no build script, no serialisation step, no second copy. Every tool
  description that states a claim takes it from the same consts (string literals may
  interpolate them via `concat!`; where a description cannot be split, the test in
  **Acceptance** pins the claim text instead).
- **Fields** None on the wire. `initialize`/`server/discover` `instructions` is the same
  field with generated content.
- **Acceptance** `tests/contract_truth.rs`: (a) every tool name in `TOOL_LINES` appears in
  `tools/list` and the counts are equal; (b) every code in `ERROR_CODES` appears in the
  generated instructions and every `pub const *_POLICY: &str` / `*_UNAVAILABLE: &str` in
  `src/gate.rs` appears in `ERROR_CODES` (source scan, the `tests/no_steal.rs` idiom);
  (c) every `ERROR_CODES` entry appears at least once outside `claims.rs` (so no invented
  code); (d) the instructions contain the client-vs-screen coordinate rule and no
  occurrence of `allow_foreground:true` outside a refusal clause (W1.2's rule).
  **Fails today:** instructions name six tools, `ERROR_CODES` does not exist, and the
  `allow_*` clause reads as an escalation path.
- **Effort** 2 d

## W1.2 No refusal may recommend a door that is bricked

- **Problem** Three error strings tell the agent to pass an option that always refuses, so
  a refusal's own advice cannot succeed and the agent loops.
- **Closes** L1/§4 (fallback suggestion), L5/§5 (escalation path), L5/§4 item 3.
- **Today** `src/burst.rs:1010-1015` ends `foreground_required: … use type with an element
  (UIA SetValue) or allow_foreground:true` — `type`-with-element is itself refused
  (`background_unavailable`) and `allow_foreground:true` refuses every action.
  `src/burst.rs:3285-3292` ends `Use invoke for the UIA pattern, or allow_foreground:true
  for a real click`. `src/uia.rs:1013-1020` ends `(pass "fallback":"click" for a pixel
  click)` — that fallback is refused with `no_cursor_policy`.
- **Contract** A refusal names only recoveries that can succeed. Each of the three texts
  is replaced: the foreground-input refusal names `type` with an element on a hidden
  desktop or the honest "this cannot be done under the background-only policy"; the
  element-click refusal names `invoke` and, when no pattern exists, says no pixel fallback
  exists; the UIA activation refusal names the Win32 `BM_CLICK` path when `invokable` is
  true and otherwise says the control cannot be activated in the background.
- **Fields** None. `code`, `resolution` and `error` keep their names and shapes.
- **Acceptance** `tests/advice_truth.rs`: scan every string literal in `src/` that names
  `allow_foreground`, `allow_cursor`, `allow_clipboard` or `fallback:"click"`; each
  occurrence must be within 120 characters of `refus`, `no flag` or `no pixel fallback`.
  **Fails today:** `burst.rs:1012`, `burst.rs:3290` and `uia.rs:1017` recommend without
  refusing, and the scan reports three violations.
- **Effort** 1 d

## W1.3 The schema prose may not document a dead verb

- **Problem** `set_value` and `clipboard_set` are documented as writable in the schema
  itself, so the generated contract would inherit the same lies if it were built from the
  schema text.
- **Closes** L1/§4 (`set_value` documented, refused), L5/§4 (stringly edges), D1.
- **Today** `TOOL_ELEMENTS` (`src/schemas.rs:14`) says "invoke/set_value accept an optional
  expect_fp" — `set_value` accepts nothing and always refuses (`src/burst.rs:4312` is
  unreachable code behind the gate at `src/burst.rs:2712-2730`). `TOOL_DO`
  (`src/schemas.rs:12`) carries the `allow_clipboard` description "clipboard_set refuses
  with clipboard_write_required unless this is true" — there is no
  `clipboard_write_required` code and `clipboard_set` refuses unconditionally
  (`src/burst.rs:3933-3944`). `TOOL_DO` still advertises `set_value` as a writer.
- **Contract** `REFUSALS` in `src/claims.rs` classifies every verb as `posted`,
  `pattern`, `read` or `refused`. A verb classified `refused` may appear in a description
  only inside a "refused" clause, and its `inputSchema` entry stays (so old callers get a
  typed refusal, matching `allow_foreground`'s existing treatment). `clipboard_write_required`,
  `cursor_move_required` and `foreground_required` stop existing as documented codes;
  `foreground_required` is removed from the message at `src/burst.rs:1010` and the code
  becomes `no_foreground_policy`.
- **Fields** No reply field changes. Three string values change: the `allow_clipboard`
  schema description, the `TOOL_ELEMENTS` `expect_fp` sentence, and the
  `src/burst.rs:1010` refusal `error` text (its `code` becomes `no_foreground_policy`).
- **Acceptance** `tests/schema_truth.rs` addition: for every action type whose claims
  classification is `refused`, the `do` description must place it in a refusal clause
  (`must contain "refused"`), and no tool description may contain
  `clipboard_write_required|cursor_move_required|foreground_required`.
  **Fails today:** the `allow_clipboard` description matches the forbidden-code scan and
  `set_value` is described as writing.
- **Effort** 1 d

## W1.4 The contract must be readable in one page

- **Problem** `tools/list` costs ~38 KB before an agent sends one action, half of it in
  `do`, so a client that truncates the list loses load-bearing semantics and gets them
  wrong by acting.
- **Closes** L5/§3 (context cost), L5/A2 (two largest descriptions).
- **Today** `src/schemas.rs:12` (`do`) is ~19.2 KB, `:8` (`screenshot`) ~6.6 KB, `:14`
  (`elements`) ~4.4 KB of a 38 656-byte file (L5/§3, measured by byte length).
- **Contract** Each `TOOL_*` const stays under its budget — `do` 12 000 bytes, `screenshot`
  4 000, `elements` 3 200, `window` 3 000, `diff` 2 400, `list_windows` 1 400, `cursor` 700,
  so the whole `tools/list` is ≤ 26 700 (the sum). No claim may be deleted to make the
  budget: the same test pins a required-substring list (client-vs-screen coordinates,
  `satisfied` vs `ok`, `effect` semantics, `expect_fp` requirement, the refusal table,
  `SendInput` never called) drawn from `claims.rs`.
- **Fields** None.
- **Acceptance** `tests/schema_truth.rs`: the seven byte budgets plus the required
  substrings. **Fails today:** `do` is 19 181 bytes and the full surface 38 656.
- **Effort** 2 d

---

# W2 — Wrong-control safety

An action either reaches the control the agent saw, or it refuses and says so.

## W2.1 `expect_fp` is required for every element mutation

- **Problem** A plain `invoke` re-walks and re-matches by name/auto_id/path, so after a
  re-render it acts on whatever now sits there and reports success.
- **Closes** L1/§3 (`expect_fp` opt-in), L1/R1. D2 supersedes L1's doc-only remedy: L1
  proposed "always pass expect_fp" in prose; D2 makes the refusal structural.
- **Today** `src/burst.rs:1981-2000` returns `None` when `expect_fp` is absent or empty, so
  the gate passes and the action proceeds; wired at `:3035` (scroll-element), `:3164`
  (click-element), `:3420` (type-element), `:4067` (invoke), `:4312` (set_value). A stale
  UI therefore yields `ok:true, via:uia-invoke` on the wrong control.
- **Contract** For the four mutating element verbs — `invoke`, `set_value`, `click` with an
  `element`, `type` with an `element` — an element selector without `expect_fp` refuses
  with `code:"expect_fp_required"`, `resolution:"expect_fp_required"`, and the fresh
  fingerprint in the error so no second round-trip is needed to recover:

  ```json
  {"ok": false, "type": "invoke", "code": "expect_fp_required",
   "resolution": "expect_fp_required",
   "error": "expect_fp_required: element \"name:OK\" was resolved fresh (fp 3f2a…). Pass expect_fp with the fingerprint from your elements snapshot; a mutation without one may act on a different control."}
  ```

  A supplied `expect_fp` that does not match keeps today's `stale_ref` shape and adds the
  fresh value: `resolution:"stale"`, `code:"stale_ref"`, and the error names
  `expected fp <want>, now <got>` (already true at `src/burst.rs:1997-1999`).
  An ambiguous match still returns `candidates` first; the candidate list carries `fp` per
  entry (already true), so the retry is one call. Under `dry_run` the same refusals fire.
- **Fields** Added: `code:"expect_fp_required"`, `resolution:"expect_fp_required"` on the
  four verbs. Unchanged: `code:"stale_ref"`, `resolution:"stale"`, `candidates[].fp`.
  Removed: nothing. Schema: `expect_fp` on those four input objects gains
  `"description": "Required for element mutations as of 8.0.0"`.
- **Acceptance** In `tests/p4c_patterns.rs` (live, `PUPPET_LIVE_DESKTOP=1`): snapshot the
  fixture's button, re-render it so its `name` changes, then `invoke` with the snapshot's
  selector and no `expect_fp` → expect `ok:false, code:"expect_fp_required"` and zero
  patterns fired (fixture reports activation count 0). Then retry with the returned `fp` →
  `ok:true, effect:"changed"`. **Fails today:** the first call succeeds with
  `ok:true, via:uia-invoke` and the fixture counts one activation.
- **Effort** 2 d

## W2.2 `fp` is identity, never state — and the contract says so

- **Problem** An agent that assumes `fp` covers geometry or state will pass it expecting a
  staleness catch it cannot give, or skip the check because it moved a slider.
- **Closes** L1/§3 (fp scope), D6.
- **Today** `src/shot.rs:525-533` hashes exactly `role\0name\0auto_id\0invokable`; rect,
  path, text and every pattern state are excluded by construction. The schema says "Text
  never participates in fp" (`src/schemas.rs:14`) but nothing states that a toggle leaves
  the fingerprint unchanged, and nothing tests it.
- **Contract** The `elements`/`invoke` prose states the three exclusions (rect, text,
  state) and their consequence: an `fp` match means *the same control*, not *the same
  control in the same condition*; a state-sensitive action needs a readback
  (`effect`/`satisfied_by`), not a fingerprint. This is what makes W2.1 safe and W5.1
  possible: state reads never change `fp`, so default-on `expect_fp` does not refuse every
  toggle.
- **Fields** None.
- **Acceptance** Unit test on `element_fp`: two tuples differing only in rect/path/text
  hash equal; differing in `role|name|auto_id|invokable` hash different. Live addition:
  toggle a fixture checkbox, re-snapshot, `fp` unchanged while `state.toggled` changed.
  **Fails today:** the live assertion has no `state` to compare (`None`), so the test
  cannot pass until W5.1 lands; the prose assertion fails outright.
- **Effort** 0.5 d

## W2.3 An element wait reports which control satisfied it

- **Problem** `wait_for kind:"element"` polls without any staleness check, so an `enabled`
  wait can be satisfied by a different control that took the same name mid-wait.
- **Closes** L1/§3 (`wait_for` gap), L1/R6.
- **Today** the poll loop `src/burst.rs:1485-1680` never calls `check_expect_fp`; the
  result carries `satisfied` only.
- **Contract** The wait does not refuse — polling must survive a churning UI — but it
  reports the winner: `satisfied_by:{selector, fp, path}` on a satisfied element wait, and
  `satisfied_by:null` on timeout. An optional `expect_fp` on the wait, when supplied, is
  reported as `fp_matched:true|false` and never turns the wait into an error.
- **Fields** Added: `results[].satisfied_by` on `wait_for kind:"element"` only; absent on
  every other kind. No renames.
- **Acceptance** Fixture swaps the named control for a different one after 200 ms; the
  wait's `satisfied_by.fp` differs from the caller's snapshot `fp`, and `satisfied` is
  still `true`. **Fails today:** `satisfied_by` does not exist, so nothing can compare.
- **Effort** 1 d

---

# W3 — Perception truth

What the agent sees must be what the pixels and the numbers say.

## W3.1 A diff reports the scale of the image it returns

- **Problem** Counts and `bbox` are computed on native pixels while the returned
  visualisation is long-edge clamped, and the payload carries no scale — so a box drawn on
  the returned image is misplaced whenever the clamp bites.
- **Closes** L3/§4 (bbox/viz mismatch), L3/A1. **D4.**
- **Today** `src/diff.rs:208-234` emits `{ok,changed,total,ratio,bbox,identical,base,next,threshold,diff_out?}`
  with `bbox` in native pixels; `src/diff.rs:281-288` clamps the viz with `a.max_px`
  (default 1280) and discards the dimensions that `finalize` already returns
  (`cap.rs:1122`, the tuple is `(data, iw, ih, scale, fmt, quality)`).
- **Contract** The diff payload carries the same three facts a screenshot already carries
  (`src/screen.rs:314-330`): `image:{w,h,format}`, `scale`, and `bbox_space:"native"`.
  `bbox`, `changed` and `total` stay native and are stated as such; `image.w/h` describe
  the returned bytes. A caller maps a native box onto the returned image by dividing by
  `scale` — the same rule `screenshot.formula` uses, applied to the diff.
- **Fields** Added on `diff`: `image:{w,h,format}`, `scale`, `bbox_space:"native"`.
  Unchanged: `changed`, `total`, `ratio`, `bbox`, `identical`, `base`, `next`, `threshold`,
  `diff_out`. Removed: none.
- **Acceptance** Capture a 2000×2000 synthetic PNG pair differing in one 4×4 block; assert
  `scale < 1`, `image.w == round(native_w / scale)`, `bbox` equals the native block, and
  decoding the returned image and reading the pixel at `floor(bbox.xy / scale)` finds red.
  **Fails today:** `image` and `scale` are absent from the payload, and the mapped read
  lands wherever the clamp put it rather than where the box claims.
- **Effort** 1 d

## W3.2 Screenshots are lossless by default

- **Problem** Default captures are JPEG q80, so two captures of identical pixels are not
  byte-identical and any agent-side pixel reasoning inherits compression noise.
- **Closes** L3/§3 (fmt default), L3/A2. **D4.**
- **Today** `src/serve.rs:456-462` selects `png` only when `out` is set and `fmt` is
  omitted, otherwise `jpeg`; `src/schemas.rs:8` declares `"default": "jpeg"`.
- **Contract** `fmt` defaults to `png` for every capture path. JPEG requires an explicit
  `fmt:"jpeg"` and keeps `quality` clamped 1..100 (ignored for PNG, as today). Two default
  captures of a static target are byte-identical; that is the definition of lossless here,
  and the `image.format` field already reports which was used.
- **Fields** Changed default only: `inputSchema.properties.fmt.default` becomes `"png"`.
  No field added, renamed or removed.
- **Acceptance** Capture the same static fixture window twice with no arguments; assert the
  two image blocks are byte-identical, `image.format == "png"`, `image.quality == null`.
  **Fails today:** both are `image/jpeg` q80 and the base64 differs.
- **Effort** 1 d

## W3.3 A diff can exclude a region

- **Problem** A caret blink or a taskbar clock flips `identical` to false with no way to
  exclude it, so `changed` is the default verdict on live UI and the only remedy is
  raising `threshold`, which hides real sub-threshold drift.
- **Closes** L3/§4 (masking), L3/§5 (no freeze), L3/A3.
- **Today** `DiffArgs` is exactly `{base, next, threshold, max_px, out, bbox}`
  (`src/diff.rs:20-27`); there is no exclusion.
- **Contract** `mask:[[l,t,r,b],…]` in native pixel space; masked pixels are excluded from
  `changed`, `total`, `ratio` and `bbox`, and counted in `masked_px`. Overlapping rects
  union silently; a rect outside the frame is a typed `bad_region`; `mask` with any masked
  pixel set reports `masked_px > 0` so the caller can see the exclusion happened.
  `threshold` keeps its meaning and its "only tolerance" contract, and `ratio` becomes
  `changed / (total - masked)`.
- **Fields** Added on `diff`: `mask` input; `masked_px` in the payload. `total` now means
  unmasked total. No renames; `bbox` stays native.
- **Acceptance** Diff two captures whose only difference is inside a masked rect → expect
  `identical:true, changed:0, masked_px == rect_area` and no image block. **Fails today:**
  `mask` is an unknown property and the call reports `changed>0, identical:false`.
- **Effort** 1.5 d

## W3.4 Report the blank-frame measurement, not just its verdict

- **Problem** The blank detector catches only a fully black frame, so a partially black or
  plausible-but-stale frame passes as truth with nothing in the reply saying what was
  measured.
- **Closes** L3/§1 (detector scope), L3/A5.
- **Today** `src/cap.rs:489-497` rejects `max(R,G,B) < 8` over the whole frame and returns a
  refusal; nothing reports the statistic and a partial black region is indistinguishable
  from real content.
- **Contract** Every successful capture reports
  `capture.blank_check:{max_channel:int, verdict:"content"|"full_blank", threshold:8}`;
  `full_blank` still refuses as today. A frame with a black region and content reports
  `verdict:"content"` — legal content, visibly measured. The `screenshot` description states
  the detector's scope in one clause: full-frame blank only, not partial or stale.
- **Fields** Added: `capture.blank_check{max_channel,verdict,threshold}` on `screenshot`.
  No renames.
- **Acceptance** Capture the fixture window with a painted black rectangle over part of it →
  `capture.blank_check.verdict == "content"` with `max_channel >= 8`; capture a fully black
  offscreen window → the existing refusal, plus `blank_check.verdict == "full_blank"`.
  **Fails today:** `capture.blank_check` does not exist, so the assertion cannot be made and
  the partial case reports nothing at all.
- **Effort** 1 d

## W3.5 Say the pointer is not in the pixels

- **Problem** Neither backend composites the cursor, so an agent looking for the pointer in
  the image never finds it and reports a false negative.
- **Closes** L3/§5 (cursor), L3/A4.
- **Today** no `DrawIcon`/`GetCursorInfo` anywhere (L3/§5 grep); `cursor{x,y}` in the map
  (`src/screen.rs:367`) is a position report. No doc claims otherwise, and none says it
  either.
- **Contract** One generated claim (W1.1): "the pointer is never in the pixels — neither
  capture backend composites it; `cursor{x,y}` is a position report". It appears in the
  `screenshot` description and in `instructions()`.
- **Fields** None.
- **Acceptance** `tests/contract_truth.rs` asserts the claim string is present in both
  generated texts, and that no description contains `cursor is visible` or
  `see the cursor`. **Fails today:** the claim text exists nowhere.
- **Effort** 0.25 d

---

# W4 — Silent no-ops, disclosed

A posted message proves delivery to a queue. The reply must not be mistaken for a result.

## W4.1 `ok` means delivered; `effect` is a closed set

- **Problem** A posted click returns `ok:true, effect:"unverified"` even when the target
  ignored every message, and the vocabulary for "what happened" is not a set an agent can
  branch on.
- **Closes** L2/§1 (raw click effect), L2/§6, L5/§4 item 3. **D3.**
- **Today** `src/burst.rs:2862-2869` sets `delivered:"posted", effect:"unverified"` on every
  raw click; element clicks use `{verified, pattern-returned, unchanged, unverified}`
  (`src/burst.rs:3240-3340`); keyboard verification uses `{changed, unchanged, unverified}`
  (`src/burst.rs:3725-3747`); `do`'s `outputSchema.results` is `{"type":"array"}` with no
  item shape (`src/schemas.rs:12`), so any string conforms.
- **Contract** Every acting result carries `delivered` ∈ `{uia, posted, none}` and `effect`
  ∈ `{changed, unverified, ignored}` defined in [Appendix B](#appendix-b--the-effect-enum-d3).
  `ok` is `true` when the action was delivered and not disproven; `ignored` means it was
  delivered and the target provably did not act, so `ignored` always arrives with `ok:false`
  and a typed `code`. The `ok` field's own schema description says exactly that. Where
  detection is impossible, `effect` is `unverified` and `ok` may be `true`: the agent
  branches on `effect`, never on prose.
- **Fields** Renamed: `verified`→`changed`; `unchanged`→`ignored`; `pattern-returned`→
  `unverified` (the pattern path survives in `via`, e.g. `uia-invoke`). Added: `delivered`
  and `effect` become typed enums in `outputSchema` for `do.results[]`, `window`, and the
  element verbs. Unchanged: `delivered` values, `via` values, `posted`, `changed`, `bbox`.
- **Acceptance** The fixture gains a control that swallows posted button messages. Click it
  by coordinates → expect `ok:false, delivered:"posted", effect:"ignored",
  code:"background_unavailable"` (delivered, proven not acted) and the fixture's activation
  count 0; `invoke` on a toggling control → `effect:"changed"`; `invoke` on a control with no
  readable state → `unverified`. The conformance pass (`tests/envelope_truth.rs`) then
  rejects any other `effect` string.
  **Fails today:** the click reports `effect:"unverified"`, `ignored` does not exist as a
  value, and `pattern-returned` is in the vocabulary.
- **Effort** 2 d

## W4.2 Say which control the point landed on

- **Problem** A posted click goes to the locked top-level HWND with no child hit-test, so
  the button the agent aimed at never sees it and the reply never says which HWND it
  actually addressed.
- **Closes** L2/§1 (child hit-test routing), L2/§6.
- **Today** `src/burst.rs:836-1181` posts to `bg_hwnd` only; the only child resolution in
  the crate is `bg_text_target` for keyboard (`src/burst.rs:751-784`, `Edit`/`RichEdit`
  only).
- **Contract** Every posted pointer action reports the window its point resolves to,
  computed read-only with `ChildWindowFromPointEx` on the locked HWND:
  `routed_to:{hwnd,class}` where `hwnd` is the child whose rect contains the point (or the
  locked top-level when there is none). When `routed_to.hwnd != target_hwnd` the action
  fails: `ok:false`, `effect:"ignored"`, `code:"background_unavailable"`, because the
  message reached a window other than the control the caller addressed. Delivery behaviour
  does not change in this release — the reply stops implying the child was addressed.
- **Fields** Added on posted pointer results: `routed_to:{hwnd,class}` and, when it
  differs, `routed_mismatch:true` with `ok:false`, `effect:"ignored"`,
  `code:"background_unavailable"`. Unchanged: `posted`, `mode`, `bg_hwnd`, `delivered`.
- **Acceptance** Fixture frame with a child `Button` at a known client rect; click its
  centre by coordinates → `routed_to.hwnd` is the child, `routed_to.hwnd != target_hwnd`,
  `routed_mismatch:true`, `effect:"ignored"`, child activation count 0. **Fails today:**
  neither field exists and the common case reports `effect:"unverified"` with no hint that
  the point was addressed elsewhere.
- **Effort** 1.5 d

## W4.3 `window` verbs get the UIPI pre-check the burst already has

- **Problem** The `window` tool has no UIPI pre-check at all, so against an elevated target
  `move`/`close`/`topmost` fail at the API silently or with a generic error.
- **Closes** L4/§3 (UIPI coverage). **Lens correction:** L4 says the gap is in `bg_execute`'s
  click/drag/scroll arms; source at this HEAD shows the caller refuses before
  `bg_execute` for every posted verb (`src/burst.rs:2820-2832`, with `bg_mode` true
  whenever a `target_lock` exists — `src/burst.rs:2470`), plus click-element `:3127`,
  type-element `:3382`, type `:3498`, key `:3595`, hotkey `:3800`, invoke `:4020`,
  set_value `:4250`, scroll-element `:2993` and the foreground loop `:2782`. L2/§1's list
  ("background, keyboard, scroll-element, invoke") is closer but still incomplete. The
  measured remaining hole is `src/winop.rs`, which calls `SetWindowPos` (`:188`, `:248`,
  `:347`) and `PostMessageW(WM_CLOSE)` (`:379`) with no elevation predicate.
- **Contract** `window` dispatch calls the same `uipi_refusal` predicate before acting, for
  every action, and returns `code:"uipi_blocked"`, `blocked_by_uipi:true`,
  `resolution:"uipi_blocked"` with the existing message text. Read-only verbs
  (`list_windows`, `screenshot`, `elements`) keep the correct asymmetry and never refuse.
- **Fields** Added on `window` refusals: `code`, `blocked_by_uipi`, `resolution` (all
  already exist on burst results; `window` currently emits only `code` for 4 mapped
  errors, `src/winop.rs:62-71`).
- **Acceptance** Live, elevated fixture (gated like `tests/timeout_bound.rs`): `window
  action:"close"` against an elevated target → `ok:false, code:"uipi_blocked"`,
  `blocked_by_uipi:true`, and the target is still alive afterwards. **Fails today:**
  `close` returns `ok:true` (`src/winop.rs:381`) and the window survives.
- **Effort** 1 d

## W4.4 `settle_ms` says what it does

- **Problem** `settle_ms` is one trailing sleep after all actions, but the guide describes it
  as "sleep between actions", so an agent that inserts it to wait between a click and a
  read gets a stale frame.
- **Closes** L2/§4 (`settle_ms` trailing-only).
- **Today** applied once after the loop (`src/burst.rs:4526-4534`, reported at `:4750`);
  `docs/GUIDE.md:290` says "Sleep between actions".
- **Contract** The contract states it as a trailing pause and names the supported
  alternative for intra-burst waiting (`wait` or `wait_for` as actions in the list). No
  behaviour change, no new field: `settle_ms` in the payload keeps its name and now means
  what the description says.
- **Fields** None. (`settle_ms` in the reply is the applied trailing value, unchanged.)
- **Acceptance** `tests/contract_truth.rs`: the generated `settle_ms` claim contains
  "trailing" and no tool description claims inter-action pacing. Behavioural half, pinned so
  it cannot silently become false: a two-click burst with `settle_ms:200` shows both clicks
  in the fixture's stamped log before the pause, and `total_ms >= 200`. **Fails today:** the
  generated claim does not exist and `GUIDE.md:290` plus `TOOL_DO` describe pacing the code
  does not do.
- **Effort** 0.5 d

## W4.5 `count` honours the OS double-click time

- **Problem** `count>1` posts N down/up pairs back to back with no gap, so a control that
  reads `GetDoubleClickTime` sees N single clicks and the result still says delivered.
- **Closes** L2/§1 (double-click timing).
- **Today** the pair loop `src/burst.rs:858-893` has no inter-pair delay; `GetDoubleClickTime`
  has zero call sites in `src/`.
- **Contract** For `count > 1`, pairs are separated by `GetDoubleClickTime()/2` (bounded
  100..500 ms) so consecutive pairs within the window register as multiple clicks. The
  spacing is reported as `click_gap_ms` on the result, and the whole sequence stays inside
  the burst budget (a `count` that would exceed the remaining budget is clamped and
  reported like any budget refusal).
- **Fields** Added: `click_gap_ms` on a `count>1` click result.
- **Acceptance** Fixture control that logs `WM_LBUTTONDBLCLK` by timing: `click count:2` →
  the fixture records a double-click and the result reports `click_gap_ms >= 100`.
  **Fails today:** the fixture records two single clicks, `click_gap_ms` is absent.
- **Effort** 1 d

## W4.6 A clipboard read says whose clipboard it is

- **Problem** `clipboard_get` copies the owner's global clipboard into the tool result with
  only prose as a guard, so an agent cannot tell a sensitive read from an ordinary one.
- **Closes** L2/§5 (clipboard read).
- **Today** `src/burst.rs:3946-3968` returns `{ok,text}`; the read needs no target and no
  flag (`src/gate.rs:434-439`). Over 1 MiB it **truncates** silently
  (`src/burst.rs:480-485`) — and `docs/GUIDE.md:112` says the opposite ("typed failure past
  it"), so today neither the reply nor the guide tells the truth about the cap.
- **Contract** The read stays (reading cannot disturb — the existing rationale) and the
  reply states its provenance: `source:"owner-clipboard"`, `sensitive:true`,
  `truncated:true|false` reflecting the real 1 MiB truncation. No policy change, no new
  flag: D7 freezes the background-safe model, and this is a disclosure, not a gate.
- **Fields** Added on `clipboard_get`: `source`, `sensitive`, `truncated`. Unchanged:
  `ok`, `text`, `dry_run`.
- **Acceptance** `tests/clipboard.rs`: `clipboard_get` reply carries `sensitive:true` and
  `source:"owner-clipboard"`; setting a >1 MiB clipboard reports `truncated:true`.
  **Fails today:** none of the three keys exist.
- **Effort** 0.5 d

---

# W5 — Capability gaps

## W5.1 `elements` reports control state

- **Problem** Entries carry no checked/selected/expanded/range/grid state and no enabled
  flag, so the agent screenshots and infers what the model could have read.
- **Closes** L1/§6 (state table), L1/R2. **D5.**
- **Today** the compact entry is `{ref,role,name,auto_id,rect,offscreen,invokable,path,fp}`
  (`src/shot.rs:525-533` for `fp`, `src/uia.rs:776-800` for the walk); state is read only
  inside `element_state` (`src/burst.rs:2217-2237`) and only for expand, range value, text
  and enabled — and only for click verification, never for the listing. ToggleState and
  IsSelected have no reader anywhere: `uia.rs` exposes `uia_is_enabled` (`:818`),
  `uia_expand_state` (`:1123`), `uia_range_value` (`:1149`), `uia_element_text` (`:1262`)
  and pattern acquisition (`el_get_pattern`, `:301`), nothing more.
- **Contract** With `detail:true`, each UIA-backend entry carries a `state` object
  ([Appendix C](#appendix-c--the-element-state-object-d5)). A key is present only when the
  control implements the pattern that answers it: `enabled` (always, via IsEnabled),
  `toggled` (Toggle), `selected` (SelectionItem), `expanded` (ExpandCollapse), `value`
  (RangeValue: `{value,min,max,readonly}`), `grid` (GridItem: `{row,column,row_span,column_span}`).
  Absent means "no such pattern" (I6) — never `false`. `text` keeps today's shape. The
  default (compact) shape does not change and pays no extra COM calls: state is opt-in by
  `detail`, and the description says what that costs (one read per entry, bounded by the
  200-entry page).
- **Fields** Added: `elements[].state{…}` under `detail:true`. Unchanged: every compact
  field, `text`, `text_truncated`. Removed: none. `state` never enters `fp` (W2.2).
- **Acceptance** `tests/p3b_elements.rs`: the fixture's TrackBar reports
  `state.value:{value,min,max}`; its toggle button reports `state.toggled:"On"` after a
  state-directed `invoke`; its `TreeView` node reports `state.expanded:"Collapsed"`; a plain
  `Edit` reports `state.enabled:true` and **no** `toggled` key. **Fails today:** no entry
  carries `state`, so every assertion reads `null`.
- **Effort** 3 d

## W5.2 `empty:true` means unseen, not empty

- **Problem** A partial or absent accessibility tree is reported exactly like a complete
  one, so "no UI here" and "I could not see the UI" are the same reply.
- **Closes** L1/§1 (completeness), L1/§2 (framework table), L1/R3. **D5.**
- **Today** the envelope is `{backend:"uia"|"win32", uia_available, uia_error, fallback,
  empty, empty_reason, rect_space:"screen"}` (`src/shot.rs:535-600`); the empty case says
  "UIA yielded no entries (…) and the win32 fallback found no children"
  (`src/shot.rs:594-597`). Nothing distinguishes a Chromium window's 1–3 HWNDs from a full
  enumeration, and Java/Flutter/Unity windows read as `empty:true` with no statement that
  the window is pixel-only.
- **Contract** The envelope gains `completeness:"full"|"partial"|"fallback"` and
  `caveat:null|"accessibility_disabled"|"no_uia_provider"|"win32_only"`:
  `full` — UIA served and its entry count is ≥ the Win32 top-level child count;
  `partial` — UIA served but enumerates fewer controls than Win32 has children (the common
  browser-without-a11y case);
  `fallback` — Win32 served because UIA yielded nothing;
  `caveat:"no_uia_provider"` when both backends returned nothing (the Java/Flutter/Unity
  case), with `empty:true` retained for shape compatibility. The `elements` and
  `screenshot` descriptions carry the framework matrix in two generated sentences and state
  that MSAA-only providers are visible only through the OS UIA→MSAA proxy, which this
  server neither requests nor reports (L1/§1).
- **Fields** Added: `completeness`, `caveat`. Unchanged: `backend`, `uia_available`,
  `uia_error`, `fallback`, `empty`, `empty_reason`, `rect_space`.
- **Acceptance** Fixture A (UIA-provider window) → `completeness:"full", caveat:null`.
  Fixture B (a window whose children exist but whose UIA provider returns nothing) →
  `completeness:"fallback", caveat:"win32_only", empty:false`. Fixture C (one HWND, no
  children, no provider) → `completeness:"fallback", caveat:"no_uia_provider",
  empty:true`. **Fails today:** neither field exists, and B and C are indistinguishable
  from A in the reply.
- **Effort** 2 d

## W5.3 `elements` refuses on a minimized window

- **Problem** `elements` has no minimized guard, so it can serve Win32 rects for a
  minimized window that look aim-able.
- **Closes** L1/§2 (minimized window), L1/R4.
- **Today** `screenshot` refuses with `window_minimized` at `src/shot.rs:147-149`;
  `element_model` (`src/shot.rs:535`) never calls `IsIconic`, so the walk proceeds and the
  Win32 fallback serves `GetWindowRect` values.
- **Contract** `elements` mirrors the capture refusal: `IsIconic` → `empty:true`,
  `empty_reason:"window_minimized"`, `code:"window_minimized"`, `completeness:"fallback"`,
  no entries. One behaviour per condition across tools.
- **Fields** Added: `code:"window_minimized"` on the `elements` envelope. Today's envelope
  carries no `code` field at all, so this is also the first `code` on that output schema;
  it stays absent when nothing refuses.
- **Acceptance** Minimize the fixture, call `elements` → `empty:true`,
  `empty_reason:"window_minimized"`, `elements:[]`. Un-minimize → entries return.
  **Fails today:** the reply carries entries and no `code`; L1 notes the claim is
  code-read, not measured — **this test is the probe**, and if the walk turns out to yield
  nothing on a minimized window the guard still lands (the refusal is the contract either
  way) but the item's severity drops in the report, not the behaviour.
- **Effort** 0.5 d

## W5.4 `window:close` reports whether the window closed

- **Problem** `close` posts `WM_CLOSE`, discards the return value and reports `ok:true`, so
  a hung app that ignores it is reported as closed and the next action hits a zombie.
- **Closes** L4/§1 (close), L4/§7 (hidden `close_posted` overclaim).
- **Today** `src/winop.rs:378-385`: `PostMessageW(h, WM_CLOSE, 0, 0);` then `ok:true`
  unconditionally.
- **Contract** Check the `PostMessageW` return; on zero return the action fails
  `background_unavailable` with "the message queue refused it". On success, poll `IsWindow`
  up to 2 s (bounded by the burst budget on the `do` path, never blocking past it) and
  report `closed:true|false` with `waited_ms`. `closed:false` is `ok:false`,
  `code:"close_unverified"`, `effect:"ignored"` — the window is still there and the reply
  says so. The hidden-desktop teardown's `close_posted` is derived from this verdict
  instead of from the post alone.
- **Fields** Added on `window action:"close"`: `closed`, `waited_ms`,
  `code:"close_unverified"` when it survives, `effect`. Unchanged: `ok`, `action`, `hwnd`.
- **Acceptance** Fixture that ignores `WM_CLOSE` → `ok:false, closed:false,
  code:"close_unverified"` after ≤2 s; fixture that honours it → `ok:true, closed:true`.
  **Fails today:** both return `ok:true` with no `closed` field, and the ignoring fixture
  is still alive.
- **Effort** 1 d

## W5.5 A killed server must not orphan the hidden desktop child

- **Problem** The hidden-desktop child is reaped only by the owner thread's teardown, so a
  `TerminateProcess` of the server leaves the child and its private desktop forever.
- **Closes** L4/§7 (hidden orphan).
- **Today** the child is spawned at `src/hidden.rs:360-393` and reaped in `teardown()`
  (`src/hidden.rs:454-499`); there is no `CreateJobObject` / `AssignProcessToJobObject` /
  `JobObjectExtendedLimitInformation` anywhere in `src/` (grep: zero hits).
- **Contract** The child is assigned to a job object with
  `JOB_OBJECT_LIMIT_KILL_ON_JOB_CLOSE` between creation and resume, so the OS kills it when
  the server dies for any reason, including `TerminateProcess`. Teardown closes the job
  handle after the existing wait, and `teardown.job_assigned:true` reports that the
  guarantee was armed (a spawn that cannot arm it fails before driving the window).
- **Fields** Added: `hidden_desktop.teardown.job_assigned`.
- **Acceptance** `tests/hidden_desktop.rs` (live, gated): start a hidden run, kill the
  server process mid-run, assert the launched pid is gone within 2 s and the desktop name
  no longer opens. **Fails today:** the child survives the kill (`job_assigned` absent, no
  job object exists).
- **Effort** 1.5 d

## W5.6 A locked or disconnected session says so

- **Problem** A locked desktop, a UAC secure desktop or a disconnected RDP session surfaces
  as `capture_unavailable` or a generic `PostMessage` failure, so the agent retries a state
  that cannot clear itself.
- **Closes** L4/§4 (session and desktop state).
- **Today** no reference to `OpenInputDesktop`, `GetThreadDesktop` (outside `hidden.rs`'s
  own creation) or `WTSRegisterSessionNotification` exists; a locked session reaches
  `PrintWindow` and is refused as "blank/black frame" (`src/shot.rs:201-206,344`), which
  names the window and not the desktop.
- **Contract** A read-only `desktop_available()` probe (`OpenInputDesktop(0,FALSE,GENERIC_READ)`
  plus `GetUserObjectInformation(UOI_NAME)`) runs at the top of `screenshot` and `do` and
  refuses with `code:"desktop_unavailable"`, `resolution:"desktop_locked"` when the input
  desktop is not the thread's, `resolution:"session_disconnected"` when there is no input
  desktop at all. The probe never blocks, never opens the secure desktop and changes
  nothing else: an available desktop proceeds exactly as today.
- **Fields** Added: `code:"desktop_unavailable"`, `resolution:"desktop_locked"|"session_disconnected"`.
- **Acceptance** Unit: the name→verdict mapping (`"Winlogon"` → `desktop_locked`,
  `"Default"` → available, no handle → `session_disconnected`). Live (documented as a manual
  check, gated like `PUPPET_LIVE_VISIBLE`): lock the session, call `screenshot` →
  `code:"desktop_unavailable"`, never `capture_unavailable`. **Fails today:** the unit
  mapping does not exist and the locked call reports `capture_unavailable`.
- **Effort** 1.5 d

---

# W6 — Tests that can fail

## W6.1 The no-steal booleans are asserted, and `diff` reports them

- **Problem** `cursorUnchanged` is discarded by the only test that reads it and no test
  fails when the reported value is wrong, while `diff` — which can capture two windows —
  reports neither boolean at all.
- **Closes** L5/§6 (telemetry), L5/A3, L3/§5 (self-report misread). **D6.**
- **Today** `tests/no_steal.rs:582-585` does `let _ = shot_sc["cursorUnchanged"].as_bool();`
  and calls `report_pointer`, so a pointer move that still reported correctly would pass;
  `foregroundUnchanged` is re-checked externally but its reported value is not pinned.
  `src/diff.rs:208-234` carries no `foregroundUnchanged`/`cursorUnchanged` although
  `base`/`next` may each capture a target (`src/diff.rs:20-27`).
- **Contract** `diff` reports `foregroundUnchanged` and `cursorUnchanged` computed from its
  own before/after pair, exactly as `screenshot` does (`src/screen.rs:400-419`). The suite
  asserts the *reported* value against an independently sampled probe: a quiet call must
  report `true`, and a call disturbed by the harness's own helper must report `false`
  (a third-party focus change keeps the existing re-baseline rule from `tests/no_steal.rs`).
  The structural half of the guarantee is unchanged: I1's symbol scan stays the proof that
  the mechanism cannot steal.
- **Fields** Added on `diff`: `foregroundUnchanged`, `cursorUnchanged`.
- **Acceptance** `tests/no_steal.rs`: for `screenshot`, `elements`, `diff`, `window` and
  `do`, the reply carries both booleans and each equals the independently observed
  before/after comparison; a harness-initiated pointer nudge during a call makes
  `cursorUnchanged:false` and the test asserts that. **Fails today:** `diff` has no such
  keys, so the assertion fails on the first tool in the list, and the cursor half of the
  existing test never fails at all.
- **Effort** 1 d

## W6.2 The fixture can prove "ignored"

- **Problem** No fixture ignores posted input, so `effect:"ignored"` and `routed_to` would
  ship unprovable, and the state object has nothing to read.
- **Closes** L5/§6 (not covered), L2/§6 (silent no-ops unprovable).
- **Today** `src/bin/puppet_fixture_app.rs` offers a window, one `Edit`, optional named
  buttons, a `TreeView`, `TrackBar` and `RichEdit` (L1/§"Not checked" cites `:165`), all of
  which honour posted messages.
- **Contract** The fixture gains: a frame hosting a child `Button` at a known client rect
  (for W4.2), a control that swallows `WM_*BUTTON*` and logs the swallow (for W4.1), a
  checkbox (`ToggleButton`), a `ListBox` (SelectionItem) and a `GridItem`-shaped table (for
  W5.1), and a stamped message log readable from the harness (for W4.4, W4.5). Every
  capability is opt-in by argv so existing tests keep their exact window.
- **Fields** None (test binary).
- **Acceptance** The live tests named in W4.1, W4.2 and W4.5 flip from "cannot assert" to
  red against today's behaviour, and W4.4's ordering half becomes observable. **Fails
  today:** the controls and the log do not exist, so each of those tests fails to compile or
  cannot observe anything.
- **Effort** 2 d

## W6.3 `do.results[]` is typed in the schema

- **Problem** `results` is declared `{"type":"array"}` with no item shape, so a payload can
  carry any `effect`/`code` string and still conform to its own `outputSchema`.
- **Closes** L5/§4 (stringly edges), L5/§6.
- **Today** `src/schemas.rs:12` `do.outputSchema.properties.results` is an untyped array;
  `effect` appears in `TOOL_DO`'s prose only.
- **Contract** `results.items` declares `type`, `index`, `ok`, `ms`, `delivered` (enum),
  `effect` (enum), `code` (string), `error` (string), `via` (enum where it applies),
  `posted`, `changed`, `bbox`, `click_gap_ms`, `routed_to`, `satisfied_by`, with
  `additionalProperties:true` so a per-verb field is never a conformance failure. The
  existing payload-vs-schema gate (`tests/envelope_truth.rs`,
  `scripts/conformance-check.mjs`) then rejects a stray `effect` value at build time.
- **Fields** Schema-only; the payload additions are those of W4.1, W4.2, W4.5, W2.3.
- **Acceptance** `tests/schema_truth.rs` asserts the enum sets, and `tests/envelope_truth.rs`
  passes for every tool. **Fails today:** `results.items` does not exist, so the enum
  assertion has nothing to read.
- **Effort** 1 d

## W6.4 `dry_run` does not capture

- **Problem** `dry_run` performs a real capture for `verify_capture` and polls live element
  state, so a dry run's `ok` can flip against the live call and fails on a locked desktop
  where the live call would have succeeded.
- **Closes** L4/§5 (dry_run path divergence).
- **Today** `verify_capture` (`src/burst.rs:640-693`) runs under `dry_run`; the
  `element_state`/`wait_state_change` reads (`src/burst.rs:2217-2248`) poll live state under
  `dry_run`.
- **Contract** Under `dry_run`, no capture is taken and no state is polled: the result
  reports `verified:false, dry_run:true` and the policy refusals keep firing exactly as
  they do now (they are the part of `dry_run` that must never be skipped — I2).
- **Fields** Added on a `dry_run` result that would have verified: `verified:false`
  alongside the existing `dry_run:true`.
- **Acceptance** `do dry_run` with a `key` action against a minimized fixture (whose live
  capture would refuse): expect `ok:true, dry_run:true, verified:false` and no
  `capture_unavailable`. **Fails today:** the dry run attempts the capture and reports
  failure.
- **Effort** 1 d

---

# Appendix A — the claims source (D1)

`src/claims.rs` is the only place a load-bearing claim is written. Shape, exact:

```rust
pub const TOOL_LINES: [(&str, &str); 7];   // (name, one line), tools_list_result order
pub const COORD_RULE: &str;                // client px under target; screen px without; rects screen
pub const ERROR_CODES: &[&str];            // every code the server can emit, one entry each
pub const REFUSALS: &[(&str, &str, &str)]; // (verb, class, code); class: posted|pattern|read|refused
pub const INVARIANTS: &[(&str, &str)];     // I1..I6 -> the one line agents read
pub const LIMITS: &[(&str, &str)];         // named non-capabilities (D7 list + L2/§6)
```

Rules the generating code and the test enforce:

1. `instructions()` is assembled from these; no claim may be typed twice.
2. Every `ERROR_CODES` entry exists as a literal outside `claims.rs`, and every
   `pub const *: &str` refusing code in `src/gate.rs` is listed.
3. Every `LIMITS` entry appears in the generated instructions — a limit an agent cannot
   read is not a limit.
4. A tool description that contradicts `TOOL_LINES` fails `tests/contract_truth.rs`.

# Appendix B — the effect enum (D3)

| Value | Means | Emitted when |
|---|---|---|
| `changed` | The target's own state changed in a way attributable to the action. | Element state readback differs, or a lossless pixel diff shows a change after a verified action. |
| `ignored` | Delivered, a readback was possible, and nothing changed. | Posted click with readable state that did not change; `window:close` that leaves the window; a posted click whose `routed_to.hwnd` differs from the posted HWND. |
| `unverified` | Delivered (or a pattern returned), no readback was possible. Never a claim that it worked. | Raw `x/y` clicks, `type`, unverified keyboard, pattern activations with no readable state. |

`delivered` ∈ `{uia, posted, none}` is unchanged. `ok` is `true` when the action was
delivered and not disproven; `ignored` always arrives with `ok:false` and a typed `code`,
because "the app did not act" is a result the caller must handle, and the enum says which
kind of failure it is.

Rename map from today's vocabulary:

| Today | 8.0.0 |
|---|---|
| `verified` | `changed` |
| `unchanged` | `ignored` |
| `pattern-returned` | `unverified` (path preserved in `via`) |
| `unverified` | `unverified` (unchanged) |

# Appendix C — the element state object (D5)

`elements[].state`, present only with `detail:true`, UIA backend only:

| Key | Shape | Source |
|---|---|---|
| `enabled` | bool | `IsEnabled` (`src/uia.rs:818`) |
| `toggled` | `"On" \| "Off" \| "Indeterminate"` | TogglePattern::ToggleState (new reader) |
| `selected` | bool | SelectionItemPattern::IsSelected (new reader) |
| `expanded` | `"Collapsed" \| "Expanded" \| "PartiallyExpanded" \| "LeafNode"` | `src/uia.rs:1123` via `expand_state_name` (`:409`) |
| `value` | `{value:number, min:number\|null, max:number\|null, readonly:bool}` | RangeValue (`src/uia.rs:1149`) |
| `grid` | `{row,column,row_span,column_span}` | GridItemPattern (new reader) |
| `text` | string (+ `text_truncated`) | `src/uia.rs:1262`, unchanged |

An absent key means the control does not implement that pattern (I6). A Win32-backend entry
carries `state:{enabled}` only, because `IsWindowEnabled` is all that backend can answer —
stated in the description so the two backends are never confused for each other.

# Appendix D — codes added by this spec

| Code | Item | Meaning |
|---|---|---|
| `expect_fp_required` | W2.1 | Element mutation with no fingerprint. |
| `desktop_unavailable` | W5.6 | Input desktop locked or session disconnected. |
| `close_unverified` | W5.4 | `WM_CLOSE` posted, window still alive after the wait. |

Codes removed from the documentation (never existed as behaviour): `foreground_required`,
`cursor_move_required`, `clipboard_write_required`. `foreground_required`'s one live use
(`src/burst.rs:1010`) becomes `no_foreground_policy`.

Existing codes keep their spelling: `ambiguous_target`, `target_gone`, `bad_target`,
`target_required`, `no_foreground_policy`, `no_cursor_policy`, `no_clipboard_policy`,
`background_unavailable`, `bad_coordinates`, `window_minimized`, `capture_unavailable`,
`stale_ref`, `uipi_blocked`, `burst_budget_exceeded`, `uia_timeout`, `capture_timeout`,
`offscreen_element`, `bad_display`, `bad_region`, `bad_window`, `ambiguous_window`,
`empty_crop`, `capture_failed`, `size_mismatch`, `bad_out`.

# Appendix E — effort

| Workstream | Items | Days |
|---|---|---|
| W1 contract truth | 4 | 6 |
| W2 wrong-control safety | 3 | 3.5 |
| W3 perception truth | 5 | 4.75 |
| W4 silent no-ops and UIPI | 6 | 6.5 |
| W5 capability gaps | 6 | 9.5 |
| W6 tests that can fail | 4 | 5 |
| **Total** | **28** | **35.25** |

Estimates assume the fixture work in W6.2 lands early: W4.1, W4.2, W4.5, W5.1 and W5.4 are
unprovable without it, so W6.2 is sequenced first in [PLAN.md](PLAN.md) despite its
workstream number.
