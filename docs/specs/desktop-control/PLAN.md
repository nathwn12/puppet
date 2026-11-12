# Desktop control — plan

The 28 items of [SPEC.md](SPEC.md) in work order. Six workstreams, fixed sequence: W1 before
W2, and so on. Each item names the problem, the audit finding it closes, the smallest honest
change, the test that proves it, and its cost. Contracts, field names and acceptance detail
live in SPEC.md and are not restated here.

Release: **8.0.0**. Total: **35.25 engineer-days**. Citation grammar and the frozen
decisions (D1–D7) are defined in SPEC.md.

## Sequence

| Order | Workstream | Why here |
|---|---|---|
| 1 | W6.2 fixture capabilities | Five later items cannot be proven without it. |
| 2 | W1 contract truth | The first text an agent reads; everything else is judged through it. |
| 3 | W2 wrong-control safety | The only class of defect that silently damages someone else's data. |
| 4 | W3 perception truth | Read-only, but every visual verdict depends on it. |
| 5 | W4 silent no-ops and UIPI | Turns "ok" into a fact an agent can branch on. |
| 6 | W5 capability gaps | New surface last: it inherits the contract from W1–W4. |
| 7 | W6.1, W6.3, W6.4 | Close the test gaps the rest exposes. |

W6.2 is numbered last but sequenced first; the workstream order is unchanged and the effort
stays in W6.

---

## W1 — Contract truth

| | | 
|---|---|
| **Closes** | L5/§5, L5/A1, L2/§5 |

### W1.1 `instructions()` is generated from one claims source — 2 d
- **Problem** The first contract every agent reads describes six tools, an escalation path
  that always refuses, flat coordinates and a foreground-touching `focus`.
- **Closes** L5/§5, L5/A1, L2/§5.
- **Change** One `src/claims.rs` holds the tool lines, coordinate rule, refusal table, code
  list, invariants and limits; `instructions()` is assembled from it.
- **Test** `tests/contract_truth.rs`: tool names match `tools/list`, every documented code is
  a real constant, every constant is documented.
- **Contract** SPEC.md, W1.1.

### W1.2 No refusal may recommend a door that is bricked — 1 d
- **Problem** Three error strings tell the agent to pass an option that always refuses.
- **Closes** L1/§4, L5/§5, L5/§4.
- **Change** Rewrite the three texts to name only recoveries that can succeed.
- **Test** `tests/advice_truth.rs`: a scan fails any mention of `allow_*` or
  `fallback:"click"` that is not inside a refusal clause.
- **Contract** SPEC.md, W1.2.

### W1.3 The schema prose may not document a dead verb — 1 d
- **Problem** `set_value` and `clipboard_set` are documented as writable inside the schema
  itself, and `clipboard_write_required` is documented but does not exist.
- **Closes** L1/§4, L5/§4, D1.
- **Change** Every verb is classified `posted|pattern|read|refused`; a refused verb may only
  appear in a refusal clause.
- **Test** `tests/schema_truth.rs`: no description contains a code that does not exist, and
  every refused verb's mention says so.
- **Contract** SPEC.md, W1.3.

### W1.4 The contract must be readable in one page — 2 d
- **Problem** `tools/list` costs ~38 KB, half of it in `do`, so a truncated list loses
  semantics an agent then acts on wrongly.
- **Closes** L5/§3, L5/A2.
- **Change** Byte budgets per tool, achieved by moving per-verb detail into enums; a required
  substring list prevents the cut from deleting truth.
- **Test** `tests/schema_truth.rs`: budgets plus the required claims.
- **Contract** SPEC.md, W1.4.

---

## W2 — Wrong-control safety

| | |
|---|---|
| **Closes** | L1/§3 |

### W2.1 `expect_fp` is required for every element mutation — 2 d
- **Problem** A default `invoke` acts on whatever now sits at the selector and reports
  success.
- **Closes** L1/§3, L1/R1. D2 supersedes L1's prose-only remedy.
- **Change** Absent fingerprint refuses `expect_fp_required` and returns the fresh one; a
  mismatch keeps `stale_ref` and adds the fresh value.
- **Test** Re-render the fixture between snapshot and `invoke`: refusal with zero
  activations, then a one-call retry succeeds.
- **Contract** SPEC.md, W2.1.

### W2.2 `fp` is identity, never state — 0.5 d
- **Problem** Agents assume the fingerprint covers geometry or state, so they trust it for
  something it cannot catch.
- **Closes** L1/§3, D6.
- **Change** State the three exclusions (rect, text, state) and the consequence in the
  generated prose; pin it with a test.
- **Test** `element_fp` equivalence classes plus a live toggle that leaves `fp` unchanged
  while state moves.
- **Contract** SPEC.md, W2.2.

### W2.3 An element wait reports which control satisfied it — 1 d
- **Problem** An element wait can be satisfied by a different control that took the same
  name mid-wait, and says nothing.
- **Closes** L1/§3, L1/R6.
- **Change** Report `satisfied_by{selector,fp,path}`; never turn the wait into an error.
- **Test** Fixture swaps the named control under a wait; the winner's `fp` differs from the
  snapshot's.
- **Contract** SPEC.md, W2.3.

---

## W3 — Perception truth

| | |
|---|---|
| **Closes** | L3/A1–A5 |

### W3.1 A diff reports the scale of the image it returns — 1 d
- **Problem** The bbox is native pixels and the returned image is clamped, with no scale, so
  a box drawn on the image is misplaced.
- **Closes** L3/§4, L3/A1, D4.
- **Change** Report `image{w,h,format}`, `scale` and `bbox_space:"native"` from the values
  `finalize` already returns.
- **Test** 2000×2000 pair, one block changed: box mapped through `scale` finds the red pixel.
- **Contract** SPEC.md, W3.1.

### W3.2 Screenshots are lossless by default — 1 d
- **Problem** Default captures are JPEG q80, so identical pixels are not identical bytes.
- **Closes** L3/§3, L3/A2, D4.
- **Change** `fmt` defaults to `png`; JPEG becomes explicit.
- **Test** Two default captures of a static window are byte-identical.
- **Contract** SPEC.md, W3.2.

### W3.3 A diff can exclude a region — 1.5 d
- **Problem** A caret blink flips `identical` to false with no way to exclude it, so
  `changed` is the default verdict on live UI.
- **Closes** L3/§4, L3/§5, L3/A3.
- **Change** `mask` in native pixels, excluded from counts and reported as `masked_px`.
- **Test** Only-inside-mask change reports `identical:true` with `masked_px>0`.
- **Contract** SPEC.md, W3.3.

### W3.4 Report the blank-frame measurement, not just its verdict — 1 d
- **Problem** The detector catches only a fully black frame and says nothing about what it
  measured, so a partially black frame passes as truth.
- **Closes** L3/§1, L3/A5.
- **Change** Report `capture.blank_check{max_channel,verdict,threshold}`; keep the existing
  full-blank refusal.
- **Test** Partially black fixture reports `content` with the measured maximum.
- **Contract** SPEC.md, W3.4.

### W3.5 Say the pointer is not in the pixels — 0.25 d
- **Problem** An agent looking for the cursor in the image never finds it.
- **Closes** L3/§5, L3/A4.
- **Change** One generated claim in the description and in `instructions()`.
- **Test** `tests/contract_truth.rs` asserts the claim and forbids the opposite.
- **Contract** SPEC.md, W3.5.

---

## W4 — Silent no-ops, disclosed

| | |
|---|---|
| **Closes** | L2/§1, L2/§4, L2/§5, L4/§3 |

### W4.1 `ok` means delivered; `effect` is a closed set — 2 d
- **Problem** A posted click that the app ignored reports `ok:true`, and the vocabulary for
  what happened is not a set an agent can branch on.
- **Closes** L2/§1, L2/§6, L5/§4, D3.
- **Change** Closed `effect` ∈ `{changed, unverified, ignored}`, typed in the schema, with
  `ok` documented as delivery only.
- **Test** Message-swallowing fixture control reports `ignored`; conformer rejects any other
  string.
- **Contract** SPEC.md, W4.1 and Appendix B.

### W4.2 Say which control the point landed on — 1.5 d
- **Problem** The point is posted to the top-level HWND with no child hit-test, and the reply
  never says which window it addressed.
- **Closes** L2/§1.
- **Change** Report `routed_to{hwnd,class}` from a read-only `ChildWindowFromPointEx`, plus
  `routed_mismatch`.
- **Test** Frame with a child button: mismatch reported, child activation count 0.
- **Contract** SPEC.md, W4.2.

### W4.3 `window` verbs get the UIPI pre-check the burst already has — 1 d
- **Problem** `window` has no UIPI pre-check at all, so against an elevated target `close`
  and `move` fail unannounced.
- **Closes** L4/§3 (relocated — see the lens correction in SPEC.md, W4.3).
- **Change** Call the existing `uipi_refusal` in `winop`'s dispatch.
- **Test** Elevated target: `close` refuses `uipi_blocked` and the window is still alive.
- **Contract** SPEC.md, W4.3.

### W4.4 `settle_ms` says what it does — 0.5 d
- **Problem** It is a trailing sleep documented as inter-action pacing.
- **Closes** L2/§4.
- **Change** State the trailing semantics and name `wait`/`wait_for` as the alternative.
- **Test** Two clicks with `settle_ms:200`: the fixture log shows both before the pause.
- **Contract** SPEC.md, W4.4.

### W4.5 `count` honours the OS double-click time — 1 d
- **Problem** `count>1` posts pairs back to back, so a double-click reads as two singles.
- **Closes** L2/§1.
- **Change** Space pairs by `GetDoubleClickTime()/2`, bounded, and report `click_gap_ms`.
- **Test** Fixture logs a real double-click and reports the gap.
- **Contract** SPEC.md, W4.5.

### W4.6 A clipboard read says whose clipboard it is — 0.5 d
- **Problem** The owner's clipboard is copied into the result with no key an agent can branch
  on.
- **Closes** L2/§5.
- **Change** `source`, `sensitive`, `truncated`. No policy change.
- **Test** `tests/clipboard.rs` asserts the three keys and the 1 MiB truncation flag.
- **Contract** SPEC.md, W4.6.

---

## W5 — Capability gaps

| | |
|---|---|
| **Closes** | L1/§2, L1/§6, L4/§1, L4/§4, L4/§7 |

### W5.1 `elements` reports control state — 3 d
- **Problem** No checked/selected/expanded/range/grid state, so the agent screenshots and
  infers what the model could have read.
- **Closes** L1/§6, L1/R2, D5.
- **Change** A `state` object under `detail:true`, keys present only where the pattern
  exists; two new readers (ToggleState, IsSelected) plus GridItem.
- **Test** `tests/p3b_elements.rs` against TrackBar, toggle, tree and list controls.
- **Contract** SPEC.md, W5.1 and Appendix C.

### W5.2 `empty:true` means unseen, not empty — 2 d
- **Problem** A partial or absent accessibility tree is reported exactly like a complete one.
- **Closes** L1/§1, L1/§2, L1/R3, D5.
- **Change** `completeness` and `caveat` on the element envelope; the framework matrix in the
  description.
- **Test** Three fixtures: full, fallback-with-children, fallback-with-nothing.
- **Contract** SPEC.md, W5.2.

### W5.3 `elements` refuses on a minimized window — 0.5 d
- **Problem** `elements` serves Win32 rects for a minimized window that look aim-able.
- **Closes** L1/§2, L1/R4.
- **Change** Mirror the screenshot refusal with `window_minimized`.
- **Test** Minimize, call, expect the refusal; un-minimize, expect entries. The test doubles
  as the live probe L1 could not run.
- **Contract** SPEC.md, W5.3.

### W5.4 `window:close` reports whether the window closed — 1 d
- **Problem** `close` discards the post result and always says `ok:true`.
- **Closes** L4/§1, L4/§7.
- **Change** Check the return, poll `IsWindow` up to 2 s, report `closed` and
  `close_unverified`.
- **Test** A fixture that ignores `WM_CLOSE` reports `closed:false`; one that honours it
  reports `closed:true`.
- **Contract** SPEC.md, W5.4.

### W5.5 A killed server must not orphan the hidden desktop child — 1.5 d
- **Problem** A `TerminateProcess` leaves the hidden child and its desktop alive forever.
- **Closes** L4/§7.
- **Change** Arm a `KILL_ON_JOB_CLOSE` job object at spawn and report `job_assigned`.
- **Test** Kill the server mid-run; the child pid is gone within 2 s.
- **Contract** SPEC.md, W5.5.

### W5.6 A locked or disconnected session says so — 1.5 d
- **Problem** Lock, UAC and RDP states surface as capture or PostMessage failures that can
  never clear themselves.
- **Closes** L4/§4.
- **Change** A read-only input-desktop probe refusing `desktop_unavailable` with a
  resolution.
- **Test** Unit name→verdict mapping plus one gated live lock check.
- **Contract** SPEC.md, W5.6.

---

## W6 — Tests that can fail

| | |
|---|---|
| **Closes** | L5/§6, L5/A3, L4/§5 |

### W6.1 The no-steal booleans are asserted, and `diff` reports them — 1 d
- **Problem** `cursorUnchanged` is discarded by the only test that reads it, and `diff`
  reports neither boolean.
- **Closes** L5/§6, L5/A3, D6.
- **Change** `diff` computes and reports both; the suite asserts each reported value against
  an independent probe instead of ignoring it.
- **Test** A harness pointer nudge during a call must make the reported value false.
- **Contract** SPEC.md, W6.1.

### W6.2 The fixture can prove "ignored" — 2 d
- **Problem** No fixture ignores posted input, so `ignored` and `routed_to` would ship
  unprovable.
- **Closes** L5/§6, L2/§6.
- **Change** Opt-in fixture capabilities: child button in a frame, message-swallowing
  control, checkbox, listbox, grid, stamped message log.
- **Test** It unblocks the four live tests named in SPEC.md and changes nothing for existing
  suites.
- **Contract** SPEC.md, W6.2.

### W6.3 `do.results[]` is typed in the schema — 1 d
- **Problem** `results` is an untyped array, so any `effect` string conforms.
- **Closes** L5/§4, L5/§6.
- **Change** Declare the item shape and the enums; the existing conformance gate then rejects
  strays.
- **Test** `tests/schema_truth.rs` enum assertions plus `tests/envelope_truth.rs` for every
  tool.
- **Contract** SPEC.md, W6.3.

### W6.4 `dry_run` does not capture — 1 d
- **Problem** A dry run captures and polls live state, so its `ok` can flip against the live
  call.
- **Closes** L4/§5.
- **Change** Skip capture and state polling under `dry_run`; report `verified:false`. Policy
  refusals keep firing.
- **Test** Dry run against a minimized target reports `ok:true, verified:false` and never
  `capture_unavailable`.
- **Contract** SPEC.md, W6.4.

---

## Effort

| Workstream | Items | Days |
|---|---|---|
| W1 | 4 | 6 |
| W2 | 3 | 3.5 |
| W3 | 5 | 4.75 |
| W4 | 6 | 6.5 |
| W5 | 6 | 9.5 |
| W6 | 4 | 5 |
| **Total** | **28** | **35.25** |

Roughly seven working weeks for one engineer, or four calendar weeks with two on disjoint
workstreams: W1 and W6.2 are disjoint from each other, and W3 is disjoint from W5 until
W5.2's description work lands.

## What this plan does not do

Named so the next pass does not rediscover them: the partial-burst resume journal, non-client
click clamping, IME and dead-key input, and every position for touch, cross-window drag-and-drop
and HDR. Each is cited with its reason in [FREEZE.md](FREEZE.md#scope-out).
