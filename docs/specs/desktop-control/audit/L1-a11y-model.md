# Lens 1 — The Accessibility Model (read-only audit)

Repo: `Q:\PROJECTS\PERSONAL\puppet` (Rust, `puppet-mcp` 7.0.2), head `8eb1c2d`. All claims from source read; nothing built or run.

## TL;DR

- Single tree only: **raw-view UIA walk** (`src/uia.rs`) with **flat `EnumChildWindows` fallback** (`src/shot.rs:535`). No MSAA/IAccessible code anywhere; MSAA-only apps are visible only via the OS UIA→MSAA proxy, which puppet neither requests nor reports.
- The `backend`/`uia_error`/`fallback`/`empty`+`empty_reason` envelope is honest about *which backend served* — but says nothing about *completeness*. A partial UIA tree (the common Chromium case) is reported exactly like a complete one.
- Biggest live hazards: (a) `set_value`/`type`-on-element are **dead verbs** — always refused at the gate while one tool description still documents their `expect_fp`; (b) `expect_fp` is **opt-in**, so a default `invoke` on a re-rendered UI silently acts on the wrong control; (c) `elements` entries carry **no state** (no checked/selected/toggled, no grid values, no slider position as data); (d) `elements` on a **minimized** window has no guard and can serve stale Win32 rects that look aim-able.

---

## 1. Which tree, fallback order, does the agent see it?

**One UIA tree + one Win32 fallback. No MSAA, no child-window/ Inno hybrid beyond that.**

- UIA is raw COM through hand-rolled vtable slots (`src/uia.rs:10-27`, `66-112`): `CUIAutomation` → `ElementFromHandle` → `RawViewWalker` (`GetFirstChild`/`GetNextSibling` loop, `raw_children` at `uia.rs:559`). `uia_available()` (`uia.rs:214`) only proves COM loaded, not that any window answered.
- Fallback is flat `EnumChildWindows` + `GetWindowText`/`GetClassName`/`GetDlgCtrlID`/`GetWindowRect` (`src/shot.rs:428`, `src/main.rs:856`). One level only, `path` is a bare sibling index (`shot.rs:473-477`), `role` is a 13-entry class-name map (`shot.rs:404`), only `cls=="Button"` is `invokable` (`shot.rs:445`).
- Order (`src/shot.rs:535-567`): UIA wins iff it yields **≥1 accepted entry**; any UIA `Err` **or** zero entries → Win32. Zero entries in both → `empty:true` + `empty_reason`.
- The agent **can** tell which served: every reply carries `backend:"uia"|"win32"`, `uia_available`, `uia_error` (null on the UIA path — so a partial UIA success erases the error channel), `fallback:"win32"|null`, `rect_space:"screen"` (`src/shot.rs:653-703`). **Gap (minor):** nothing reports *completeness* — `backend:"uia"` with 3 entries for a Chromium window looks identical to a full enumeration. Smallest change: add `uia_entry_count` vs Win32 child count, or a `completeness:"full|partial|fallback"` hint when UIA succeeds but yields fewer nodes than `EnumChildWindows` top-level children.
- **No MSAA/IAccessible code exists** (grep over `src/` for `MSAA|AccessibleObject|LegacyIAccessible|JAB` finds only comments/docs). MSAA-only providers appear only through the OS-side proxy, invisibly. Smallest change: document this in the `elements` description; no code needed.

## 2. Where the model collapses (per framework)

No per-framework code exists, so every case below funnels into the same two outcomes. Ranked by likelihood × wrongness:

| Scenario | Result (from code) | Lie or gap | Severity |
|---|---|---|---|
| Chromium/Electron **without** accessibility flag | UIA yields ~nothing → `backend:"win32"` with 1–3 HWNDs (the "wrong elements": a near-empty list that looks authoritative) | gap | **major** |
| Chromium/Electron **with** a11y on | Full UIA tree; works. But nothing tells the agent which mode it's in except entry count | gap | minor |
| Java Swing/SWT (no JAB bridge) | Single HWND, no UIA nodes, no children → `empty:true` + `empty_reason` ("UIA yielded no entries… win32 fallback found no children", `shot.rs:594`). Reads as "empty UI" — the dangerous silence | gap | **major** |
| Qt (MSAA providers via OS proxy) | Proxy nodes often nameless → dropped as anonymous (accept filter `uia.rs:714`: needs name OR auto_id OR invokable) → partial or empty list, indistinguishable from complete | gap | major |
| Flutter / Unity / game renderers / canvas-drawn / custom MFC | Same as Swing: `empty:true`. No signal that the window is pixel-only | gap | major |
| WPF with virtualization | Virtualized items absent from raw view until scrolled; `offscreen` flag (`uia.rs:719-733`) only marks zero-area nodes that *are* returned, never missing ones | gap (silent omission) | major |
| `RichEdit` / edit controls | Listed; text readable via `detail:true` TextPattern path (`serve.rs:342-359`) | works | — |
| UWP / immersive XAML | UIA providers are good; works. No special handling needed | works | — |
| Elevated window, unelevated server | `ElementFromHandle` fails → `uia_error` set → Win32 fallback enumerates (works cross-integrity) → `backend:"win32"`; input separately refused `uipi_blocked` (`burst.rs:2777`). Labeled, honest | works | — |
| Secure desktop / UAC prompt | Unreachable HWND; resolve fails → typed error, no data | works (refusal, not silence) | — |
| Minimized window | **No minimized guard in `element_model`** (`shot.rs:535`; contrast screenshot's `window_minimized` refusal, `GUIDE.md:509`). UIA likely empty → Win32 fallback serves `GetWindowRect` values of a minimized window (often off-screen `-32000` coords or stale rects) with `empty:false`. Plausible-looking, aim-able garbage | **lie-adjacent gap** | **major** |
| Off-screen / cloaked (but not minimized) | Nodes returned, `rect` kept, `offscreen:true`; `click` refuses with `offscreen_element`, `invoke` (pattern, no geometry) still allowed (`burst.rs:3226-3249`) — correct split | works | — |
| UIA provider hang | Per-op deadline `PUPPET_UIA_TIMEOUT_MS` default 5000 (`GUIDE.md:150`); `elements` still answers from Win32 with `uia_error:"uia_timeout"` (`GUIDE.md:496`). Honest degradation | works | — |

Smallest changes for the whole table: (1) refuse or annotate `elements` on minimized windows (`IsIconic` check → `empty_reason`, mirroring screenshot); (2) document the framework matrix in `TOOL_ELEMENTS` description — two sentences ("Chromium without `--force-renderer-accessibility`…; Java/Flutter/Unity expose nothing: `empty:true` means *unseen*, not *empty*").

## 3. Identity and staleness (`fp`, `ref`, `expect_fp`)

- `fp` = FNV-1a-64 over `role\0name\0auto_id\0invokable`, 16 hex chars (`src/shot.rs:525-533`). Deterministic across processes (deliberately not `DefaultHasher`). Covers **identity only**: not rect, not path, not text (`shot.rs:502-513`, `GUIDE.md:545`).
- Validity: unbounded in time, **positional, not durable**. Every mutating action re-walks and re-matches by `name:|auto_id:|path:` on the *fresh* walk; `fp` is compared only if the caller passed `expect_fp`. A renamed/reordered UI silently re-matches a different control.
- `ref` (`"<hwnd>.<path>"`) is explicitly **not** an invoke selector and not an identity guarantee — documented in `TOOL_ELEMENTS` (`schemas.rs:14`) and enforced (`match_element` rejects unknown kinds like `ref:`, `burst.rs:1837-1845`). Good.
- `expect_fp` guard (`check_expect_fp`, `burst.rs:1981-2000`) exists on: `scroll`-on-element (`3035`), `click`-on-element (`3164`), `invoke` (`4067`), and `set_value` (dead code, `4312`). **Missing on `wait_for kind:element`** (poll loop `burst.rs:1485-1680` has no `check_expect_fp`) — defensible for polling, but an `enabled`-wait can be satisfied by a *different* control that took the same name mid-wait. Minor.
- **The worst case is the default:** `expect_fp` is opt-in, so a plain `invoke` after a re-render acts on whatever now sits at that name/path, returns `ok:true, via:uia-invoke`, and the agent cannot distinguish it from the intended control. No `resolution:stale` unless the caller opted in. **Gap, blocker-adjacent (major):** invert the default — either require `expect_fp` for `path:` selectors (indices shift on any insert) or emit `fp_changed:true` on every action whose re-walked entry `fp` differs from the entry the agent last saw (server would need to remember last-served fps per window — heavier). Cheapest honest step: when `expect_fp` is absent and the matched entry is ambiguous-by-history (multiple past fps for one selector — needs the same memory), at minimum document "always pass `expect_fp`" in the `invoke` description; today it is one clause in the `elements` text.
- Stale `path:` (child index gone) *is* typed: `"UIA path … went stale"` (`uia.rs:856-861`), Win32 analog (`burst.rs:2043-2048`). Good — index deletion is safe; index *shift* is not.

## 4. Value semantics (`set_value`, `invoke`, `via`)

- **`set_value` and `type`-on-element are currently dead verbs.** The gate refuses both unconditionally (`gate.rs:325-333`, `burst.rs:2712-2730`) with `background_unavailable` ("the UIA provider can focus its control"), before dry-run differentiation. The live `uia_set_value` implementation (ValuePattern → RangeValue-slider fallback, `uia.rs:1357-1446`) and the whole `set_value` dispatch arm (`burst.rs:4195-4399`) sit behind `#[allow(unreachable_code)]`. GUIDE documents the refusal honestly (`GUIDE.md:81,394,417-418`).
- **Lie (minor, fix by editing one string):** `TOOL_ELEMENTS` description still says "`invoke`/`set_value` accept an optional `expect_fp`" (`schemas.rs:14`) — `set_value` accepts nothing; it always refuses. Same for any `do` text implying `set_value` writes. Remove `set_value` from that sentence.
- *If/when re-enabled*, the implementation answers the sub-questions: ValuePattern-only controls work (`value_set`, `uia.rs:324`); RangeValue-only (sliders) fall back with min/max-clamped numeric parse (`uia_set_value_range`, `uia.rs:1291`, `parse_range_value` `uia.rs:459`); read-only is refused typed on both paths (`uia.rs:1426`, `1319`); **password fields have no `IsPassword` check** — a `SetValue` attempt would surface the provider's HRESULT as a typed error, not a polite refusal. Minor; add only if the verb is revived.
- `invoke` ordering: Invoke → SelectionItem → Toggle (`uia.rs:926-1012`), ExpandCollapse only via explicit `state:` (`burst.rs:4079-4084` — SplitButton-safe, never blind-fires Invoke). Missing all three → typed error suggesting `fallback:"click"`; but that fallback is **refused** by `no_cursor_policy` (`burst.rs:4138-4151`). So the suggestion inside the error text (`uia.rs:1017`) points at a door that is bricked. **Lie (minor):** reword to "no pixel fallback exists under the background-only policy".
- `via` reporting is honest and is the best part of this surface: `uia-invoke|uia-select|uia-toggle|uia-expand|uia-collapse|uia-scroll|uia-set-value|win32-click|posted-click|none`, plus `delivered`/`effect` (`verified|pattern-returned|unverified|unchanged`) on element clicks (`burst.rs:3220-3330`). An agent **can** tell pattern invocation from posted-message fallback. `wait_for`/`invoke` dry-runs report `via:none` + `dry_run:true` rather than predicting. Good.

## 5. Reply shape (context cost)

- Defaults: `max_depth` 8 (allowed 1–32, `serve.rs:272`), node cap 200 **both backends** (`main.rs:51`, `shot.rs:434`), `offset` paging with `next_offset` (`uia.rs:666`). `truncated:true` is also set on an exactly-full page (legacy semantics, pinned by tests `uia.rs:1459`) — paging clients must use `next_offset`, not `truncated`, to decide. Documented nowhere in the tool text. **Gap (minor):** one clause — "`truncated` may be true on an exactly-full page; `next_offset:null` means done."
- Per entry (compact default): `{ref,role,name,auto_id,rect,offscreen,invokable,path,fp}` (`shot.rs:624-634`); `detail:true` adds back `win`/`hwnd` and per-entry TextPattern `text` + `text_truncated` (`serve.rs:342-359`). **No text in the default shape** — the cheap call is name-geometry only.
- Text cost: `detail:true` does one `DocumentRange`+`GetText(-1)` round-trip **per entry** (up to 200), each capped at 4000 chars (`ELEMENT_TEXT_CAP`, `shot.rs:513`). No truncation of entry *count* for text. An agent pulling `detail:true` on a document window pays up to 200 COM reads. Worth one sentence in the description (`nasty` cost, not incorrectness — nice-to-have).
- Filtering: anonymous layout nodes (no name, no auto_id, not invokable) dropped (`uia.rs:712-714`); **no** enabled/visible filtering — disabled and invisible nodes are included (only geometry-less ones flagged). Ordering is RawViewWalker child order, depth-first pre-order; `path` = sibling indices (`uia.rs:678-761`). Both honest and stable choices.
- Coordinates are **screen** pixels `[l,t,w,h]` (`rect_space:"screen"`), composable with the screenshot coordinate map. Win32 fallback rects come from `GetWindowRect` (compat rect, includes invisible border — slight systematic offset vs UIA's content rect; the two backends' `rect` for the same control can disagree by border width). **Gap (nice):** note the Win32 rect bias in one clause.

## 6. Reading state

| State | Readable? | Evidence |
|---|---|---|
| enabled/disabled | **Yes** — `uia_is_enabled` (slot 28, `uia.rs:818`) per path; `wait_for state:enabled/disabled` polls it; Win32 path uses `IsWindowEnabled` (`burst.rs:1202-1213`) | works |
| expanded/collapsed | **Partially** — `uia_expand_state` exists and is returned as `expand_state` after state-directed invoke (`burst.rs:4172-4179`) and used in click verification (`burst.rs:2219`), but **never appears in `elements` entries** | gap (major for trees/menus) |
| slider/range position | Same: `uia_range_value` read internally (`burst.rs:2222`, set_value readback `4387`), **not exposed per-entry or as a read verb** | gap (major for sliders) |
| element text | Only via `detail:true` TextPattern dump (`serve.rs:342`), capped 4000 chars; compact listing has no text at all | works, expensive |
| checked / toggled / selected | **No.** TogglePattern/SelectionItem are fire-only (`uia.rs:955-1011`); no `ToggleState`/`IsSelected` reads anywhere (grep confirms) | **gap, blocker for settings UIs** |
| grid/table cells, list selection | **No.** No Grid/Table/Selection patterns, no cell-value reads; `element_state` covers only expand+range+text+enabled (`burst.rs:2217-2237`) | gap (major) |
| success verification | Clicks verify by pre/post `element_state` compare → `effect:verified|unchanged` (`burst.rs:3250-3300`); a posted click whose readable state doesn't change **fails** (`background_unavailable`) rather than claiming success. Best-in-class honesty | works |

Smallest change covering most of §6: add read-only per-entry `state` in `detail:true` (ToggleState, IsSelected, ExpandCollapseState, RangeValue value, `IsEnabled`) — all are cheap property reads on already-resolved elements, no new patterns to fire.

## Not checked

- No build/run (per instructions): minimized-window rect behavior, Chromium-no-flag emptiness, and UIPI cross-integrity paths are code-read + platform knowledge, not measured. A headed harness run (`puppet_fixture_app` covers Button/Edit/TreeView/TrackBar/RichEdit, `src/bin/puppet_fixture_app.rs:165`) could confirm each row.
- Screenshot/`diff`/`cursor`/`window` tools: out of lens scope except where noted.
- `deadline`/timeout internals beyond the `uia_timeout` contract.

## Ranked fix list (frequency × wrongness)

1. `expect_fp` opt-in → silent wrong-control `invoke` after re-render (major; §3). Cheapest: document "always pass expect_fp" in `invoke` text; real fix: `fp_changed` signal.
2. No checked/selected/grid/slider state in `elements` (major; §6). Cheapest: `detail:true` `state` object from already-available pattern reads.
3. `empty:true` means *unseen* for Java/Flutter/Unity/canvas, and `backend:"win32"` means *near-blind* for Chromium — neither says so (major; §2). Two sentences in `TOOL_ELEMENTS`.
4. Minimized windows serve stale Win32 rects as `empty:false` (major; §2). `IsIconic` guard.
5. `set_value` documented-but-dead `expect_fp` + error text suggesting a refused `fallback:"click"` (minor ×2; §4). Two string edits.
6. `wait_for kind:element` has no `expect_fp` (minor; §3). Accept-or-document.
7. `truncated`-on-exactly-full + `detail:true` text cost + Win32 rect bias undocumented (nice; §5). Three clauses.
