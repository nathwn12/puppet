# Freeze record — desktop control

What this spec covers, what it refuses to cover, and the two values that make it frozen:
the semver call and the hash of the contract.

| | |
|---|---|
| Contract | [SPEC.md](SPEC.md) — 28 items, 865 lines |
| Work order | [PLAN.md](PLAN.md) — six workstreams, fixed sequence |
| Evidence | [`audit/`](audit/) — five lens reports, byte-identical to the originals |
| Repo head at freeze | `8eb1c2d`, `puppet-mcp` 7.0.2 |
| Target release | **8.0.0** |
| Total effort | **35.25 engineer-days** |

## The hash

```
SPEC.md  SHA-256  1EB82F35DEA1F218434BE79D3FAFEC5F701A9C92CFB4E5DA53BFF8F07FD3FB07
         53 686 bytes
```

Verify with:

```powershell
Get-FileHash docs/specs/desktop-control/SPEC.md -Algorithm SHA256
```

The hash covers the contract only. PLAN.md restates problem/change/test in work-shape and
is allowed to be edited as the work is sequenced; SPEC.md is not. An edit to SPEC.md after
this freeze is a new decision and takes a new hash, recorded here with the reason.

## Frozen decisions

Settled before this document existed. They are not reopened, softened or extended by the
items below them; an item that appears to contradict one is a defect in the item.

| | Decision | Where it lands |
|---|---|---|
| D1 | The contract is generated, not written. | W1.1–W1.4 |
| D2 | Never act on a possibly-stale control. `expect_fp` default-on; breaking. | W2.1, W2.2 |
| D3 | `ok` means delivered, never worked; `effect` is a closed set. | W4.1, Appendix B |
| D4 | Perceive losslessly by default; preview scale always reported. | W3.1, W3.2 |
| D5 | State is read, not guessed; `empty:true` is not "empty". | W5.1, W5.2 |
| D6 | The guarantee is asserted, not self-reported. | W6.1, W2.2 |
| D7 | Non-goals named below. | this file, scope out |

## Scope in

Everything in the six workstreams, in this order. Item counts and cost:

| Workstream | Scope | Items | Days |
|---|---|---|---|
| W1 | contract truth — generated `instructions()`, no bricked advice, no dead verb documented, readable budget | 4 | 6 |
| W2 | wrong-control safety — required fingerprint, identity-not-state, wait attribution | 3 | 3.5 |
| W3 | perception truth — diff scale, lossless default, mask, blank measurement, cursor claim | 5 | 4.75 |
| W4 | silent no-ops and UIPI — closed `effect`, routing disclosure, `window` UIPI, `settle_ms`, `count`, clipboard provenance | 6 | 6.5 |
| W5 | capability gaps — element state, completeness and caveat, minimized guard, verified close, job object, desktop probe | 6 | 9.5 |
| W6 | tests that can fail — asserted booleans, fixture capabilities, typed results, dry-run parity | 4 | 5 |
| | | **28** | **35.25** |

## Scope out

Each entry is a decision, not an omission. A later pass that wants one of these is making a
new decision and needs a new spec.

| Out | Cited | Why |
|---|---|---|
| Partial-burst resume journal | L4/§7 | L4 calls it major and the fix honest, but a write-ahead journal is new on-disk state with a lifetime and a cleanup story. Not in the W5 list. The contract states the limit instead (W1.1 `LIMITS`): bursts are not atomic across a service restart, so keep them short and idempotent. |
| Non-client click clamping | L2/§2 | Real: a point outside `GetClientRect` posts into client message space and lands nowhere. Blocked by ordering, not judgement — it changes what coordinates *mean* for every existing caller, so it belongs in the release after the contract is generated, not in the release that changes the contract. |
| IME, dead-key and `AltGr` composition | L2/§3 | No composition path exists and none can be built on posted `WM_CHAR`. An honest limit in `LIMITS`, not a work item: implementing it means an input method, which is not this server. |
| Touch, pen, stylus | D7, L2/§6 | No schema surface, no code path, and no way to synthesize without injecting into the owner's session. |
| Cross-window drag-and-drop | D7, L2/§1 | The `drag` verb is three posted messages to one HWND; OLE `DragEnter`/`Drop` needs a drag source the background model does not have. |
| HDR and wide-gamut colour management | D7, L3/§3 | Bytes are the SDR-composited framebuffer, untagged. Tagging without converting would be worse than untagged; converting is a colour pipeline this server does not own. |
| Game and DirectInput input | D7, L2/§1 | Reads raw input, ignores window messages. No message-based path can reach it. |
| Elevation, and any change to the background-safe model | D7, L5/§5 | No escalation path is the product. `allow_*` stays as a typed refusal (I5). |
| `scroll`-on-element and `wait_for`-element fingerprint enforcement | D2's wording | D2 names four verbs: `invoke`, `set_value`, `click`-with-element, `type`-with-element. Scroll and a wait are not among them; W2.3 reports the wait's winner instead of refusing, and scroll keeps today's behaviour with the fresh refusal still available. Extending D2 is a new decision. |
| Pointers to sibling servers (`gladius`, `oc-rea`) | L5/§7 | L5 is right that a hit `background_unavailable` has no first-party next step. Routing doctrine belongs to the harness that dispatches, not to a tool description that ships to every MCP client. |
| Stale timeout string in the hidden launcher | L4/§7 adjacent | A message that says 15 s while the code waits 20 s. Cosmetic, not agent-facing, and this spec touches no claim an agent branches on. Named here so it is not rediscovered as new. |
| `wait_for stable` region default (foreground monitor) | L2/§4 | Documented behaviour, not a defect. Changing it would silently widen every existing `stable` wait. |
| The helper-thread stack leak per timeout | L4/§1 | Documented trade (`src/deadline.rs:28-31`) and the accepted price of keeping the worker pool alive. |
| WOW64 / 32-bit build assertion | L4/§6 | No 32-bit artifact ships; the guard would protect a build that does not exist. |
| Retry pacing, `verify_capture` retry bound, tail latency | L4/§1 | Inside the budget by construction. |

## Semver call

**8.0.0**, a major.

D2 is the reason and it is enough on its own: `expect_fp` moves from opt-in to required for
`invoke`, `set_value`, `click`-with-element and `type`-with-element. A call that succeeds on
7.0.2 — `invoke` with a selector and no fingerprint — refuses on 8.0.0. That is a breaking
change to the wire contract, not a behaviour change an agent can absorb silently.

Three more changes are breaking for the same reason and would each demand a major on their
own:

| Change | Breaks |
|---|---|
| `effect` renamed (`verified`→`changed`, `unchanged`→`ignored`, `pattern-returned`→`unverified`) | any caller branching on the old strings |
| `screenshot.fmt` default `jpeg`→`png` | any caller sizing responses for JPEG, or diffing saved default captures |
| `window:close` reports `ok:false, closed:false` when the window survives | any caller that treated `ok:true` as closed |

Additive and therefore minor in isolation: `completeness`, `caveat`, `mask`/`masked_px`,
`image`/`scale`/`bbox_space` on `diff`, `state` under `detail`, `routed_to`,
`routed_mismatch`, `click_gap_ms`, `satisfied_by`, `closed`/`waited_ms`, `job_assigned`,
`blank_check`, `expect_fp_required`, `desktop_unavailable`, `close_unverified`, and the
clipboard provenance keys.

Because one major carries the batch, everything above ships in 8.0.0. The alternative —
splitting D2 into 7.1.0 with the additive items — was rejected: it would ship the new
perception and state surface against a contract the same release is about to replace, and
W1.1's generated contract cannot describe two contracts at once.

The CHANGELOG section for 8.0.0 is written at release time from `git log`, per this repo's
convention; it is not part of this freeze and no existing doc is touched by it.

## Baseline defects this spec does not inherit

Where a lens and the source disagree, the source wins. The two that change what an item
does, recorded here so a reviewer can check the reasoning rather than the conclusion:

1. **UIPI coverage (L4/§3 vs source).** L4 reports the pre-check missing for posted
   click/down/up/drag/scroll because `bg_execute` (`src/burst.rs:836-1181`) has no refusal
   in it. True of that function, false of the call path: the caller refuses at
   `src/burst.rs:2820-2832` for every posted verb, and `bg_mode` is true whenever a
   `target_lock` exists (`src/burst.rs:2470`), with seven further element verb sites. L2/§1
   is closer but still lists only four. The measured hole is `src/winop.rs`, which has no
   elevation predicate before `SetWindowPos` (`:188`, `:248`, `:347`) or
   `PostMessageW(WM_CLOSE)` (`:379`). W4.3 targets that, and the burst path keeps the
   coverage it already has.
2. **Minimized `elements` (L1/§2).** L1 states minimized windows serve plausible Win32
   rects, and L1's own "Not checked" section says it was read, not measured. The behaviour
   is frozen (W5.3 refuses) because one behaviour per condition beats two; the *severity* is
   unmeasured until W5.3's test runs. The refusal is correct either way.
3. **Clipboard cap (L2/§5 vs GUIDE).** `docs/GUIDE.md:112` says clipboard text past
   1 048 576 chars is a "typed failure"; `src/burst.rs:480-485` truncates and returns
   `ok:true`. L2 is right and the guide is wrong. W4.6 makes the reply honest
   (`truncated:true`) and W1.1's claims source carries the truth; the guide is corrected
   when the contract is regenerated, not before, because this spec modifies no existing doc.

## Amendments

A change to a frozen clause is made by a written amendment, not by editing SPEC.md, so the
hash above still matches and the freeze holds. Each one is named here with the clauses it
reaches; a second needs one more row.

| Amendment | Amends | Effort | Commit |
|---|---|---|---|
| [AMENDMENT-01-sizing.md](AMENDMENT-01-sizing.md) - what an image reply says about its own pixels | W3.1 and W3.2, the image replies only; W3.3-W3.5, D1-D7, I1-I6 and the sequence in [PLAN.md](PLAN.md) untouched. Supersedes nothing. | 1.5 d | `a93cdfa` |
| [AMENDMENT-02-budget.md](AMENDMENT-02-budget.md) - room for the diff contract W3 adds | W1.4's per-tool byte budget for `diff` (2 400 → 2 800); the 26 700 sum and the other six budgets untouched. Supersedes nothing. | 0 d | `c0b5b06` |
