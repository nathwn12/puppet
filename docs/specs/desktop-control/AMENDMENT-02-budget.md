# Amendment 02 — budget: room for the diff contract W3 adds

Raises one frozen number so the text W3 is required to add has somewhere to live. It adds no
workstream, reopens no frozen decision, and edits nothing: SPEC.md keeps the hash recorded in
[FREEZE.md](FREEZE.md), and where this file and SPEC.md disagree, SPEC.md wins and this file is
the defect.

| | |
|---|---|
| Amends | W1.4's per-tool byte budget for `diff` (2 400 → 2 800). The 26 700 sum and the other six budgets are untouched. |
| Leaves alone | D1–D7, I1–I6, every item outside W3, the W1.4 required-substring list, [PLAN.md](PLAN.md)'s sequence |
| Changes | one number in the budget table, and the test that pins it |
| Effort | **0 d** — no behaviour, one ceiling |
| Supersedes | nothing |

## Why the number moves

W1.4 froze seven per-tool byte budgets and a 26 700-byte sum so `tools/list` stays readable in
one page, with one rule attached: "No claim may be deleted to make the budget."

W3 adds more contract text to `diff` than its 2 400-byte ceiling can hold:

| Item | What it puts in `diff`'s const |
|---|---|
| W3.1, A1 | `image{w,h,format,quality}`, `scale`, `bbox_space`, `base_source`, `next_source` in the reply, plus the box→image and box→screen mapping rules |
| W3.3 | the `mask` input (`[[l,t,r,b],…]` in native pixels, union, `bad_region`), `masked_px`, and the fact that `total`/`ratio`/`bbox` are the unmasked ones |
| A2, A5 | `warnings`, and `image.quality` on a diff image |
| W3.2, W3.4, W3.5 | nothing — those are `screenshot`'s, whose budget still holds |

Before W3 the const measured 2 324 bytes. With W3.1/A1/A2/A5 it measured 2 400 exactly — at the
ceiling with nothing left — and W3.3's mask needs roughly another 350. The only way to stay
under 2 400 is to delete existing claims (`side-effect-free`, `never upscaling`, "no activation
or input", "errors never raise"), which W1.4 forbids in as many words. So the ceiling moves
instead, by the smallest amount that fits the whole contract with a little room.

## The numbers

Measured on the tree that lands this amendment (`len()` of each `TOOL_*` const, which is the
same measurement `tests/schema_truth.rs` makes):

| Const | Budget before | Measured after W3 | Budget now |
|---|---|---|---|
| `do` | 12 000 | 10 642 | 12 000 |
| `screenshot` | 4 000 | 3 214 | 4 000 |
| `elements` | 3 200 | 3 121 | 3 200 |
| `window` | 3 000 | 2 647 | 3 000 |
| **`diff`** | **2 400** | **2 748** | **2 800** |
| `list_windows` | 1 400 | 1 400 | 1 400 |
| `cursor` | 700 | 553 | 700 |
| **sum** | **26 700** | **24 325** | **26 700** |

The sum is unchanged and still binds, which is the number that actually protects an agent's
context: `tools/list` is 24 325 bytes here, 2 375 under the frozen ceiling. Only the per-tool
split moves, and only on the tool that grew.

## What this does not do

- No other per-tool budget moves, and the sum does not move at all.
- No claim is deleted from any description, and the W1.4 required-substring list keeps its
  meaning: `tests/schema_truth.rs` still fails if the coordinate rule, the `effect` semantics,
  the `satisfied`-vs-`ok` reading, the `expect_fp` requirement, the refusal table or the
  no-`SendInput` invariant drops out of the text an agent reads.
- No new tool, no new field beyond the ones W3 and AMENDMENT-01 already name.
