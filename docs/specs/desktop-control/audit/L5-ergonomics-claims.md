# Lens 5 — Agent ergonomics & repo's own claims (puppet, read-only)

Repo: `Q:\PROJECTS\PERSONAL\puppet` · `Cargo.toml:3` `version = "7.0.2"` · HEAD `8eb1c2d`
(`git describe`: `v7.0.2-1-g8eb1c2d-dirty`; tags include `v7.0.2`; status: `M README.md`, `?? scripts/npm-passkey.mjs`).
Method: read `src/`, `docs/GUIDE.md`, `docs/HEADLESS.md`, `tests/`, `Cargo.toml`, `CHANGELOG.md`;
`git log --oneline -20`, `git status --short`, `git tag | tail`. No build, no launch, no writes in repo
(one helper sizing script was written to scratch `Temp\opencode\lens5sizes.cjs`, outside the repo).

## 1. Selection semantics

- One-window lock + per-action revalidation: PROVEN. `src/target.rs:357-372` (`TargetLock` holds
  `hwnd+pid+created+exe+class+appId`); `revalidate()` at `target.rs:377-448` checks `IsWindow`, owner
  pid, **process creation time** (recycled-PID guard, `target.rs:395-412`), exe stem, class, AppUserModelID.
  Burst resolves once (`src/burst.rs:2447-2454`) and revalidates **before every action**
  (`burst.rs:2613-2615`, `2686-2700`). So a target IS pinned for a whole burst. Across calls there is no
  pin — each call re-locks; a loose selector (`exe:` only) can match a different window next call.
- Ambiguity error is informative: PROVEN. `resolve_from` (`target.rs:454-490`) returns `ambiguous_target`
  with a candidate list built by `candidate_map` (`target.rs:344-355`: `hwnd,pid,exe,title,class[,appId]`);
  `to_map` (`target.rs:56-69`) carries `code,error,candidates`. Unit test pins it (`target.rs:762-785`).
- `titleContains` stability: NO STABILITY MECHANISM. It is a case-insensitive substring
  (`target.rs:255-257`); no debounce, no snapshot, no test for animated/changing titles — UNPROVEN
  (no test located; nothing in `src/target.rs` tests covers title churn).
- `hwnd`-only selectors bypass enumeration (`target.rs:502-524`) but still revalidate; a bare handle is
  recyclable and the creation-time check is what saves it (unit test `target.rs:807-831` forges a
  different creation time and expects `target_gone`).

## 2. The `do` burst contract

- Continue-always with ordered per-action results: PROVEN. `docs/GUIDE.md:454-462`: "A per-action failure
  never aborts the burst"; each result carries `{index,type,ok,ms,…}` (`GUIDE.md:299-303`); code pushes
  the failure map and `continue`s (e.g. `burst.rs:2622-2636`, `2688-2698`). Top-level `ok` is true only if
  **every** action succeeded; `failed` counts; MCP `isError:true` only when **no** action succeeded
  (`GUIDE.md:458-462`; test `burst.rs:4727`, `7022` pins the envelope rule).
- Partial failure leaves half-driven state with NO rollback in plain `do`: PROVEN (by absence + contrast).
  No undo step exists in the burst path; only `desktop:"hidden"` reverts driven text
  (`docs/HEADLESS.md:6`, `:89`). Whether continue-always is the right default is a judgement call, but the
  cost is real: an agent that batches read-then-act (e.g. `click` + `type`) gets `ok:false` mid-list and
  must itself reason about what already landed. The `wait_for`-timeout rule (`ok:true, satisfied:false`,
  `GUIDE.md:463-465`) is the one place the contract avoids this trap — agents must branch on the
  `satisfied` flag, not on `ok`.
- `dry_run` is a REAL simulation, not a schema check: PROVEN with limits. Policy refusals fire identically
  under `dry_run` ("a dry run that said ok while the live call would refuse is a lie", `burst.rs:2616-2619`;
  `dry_run` flags threaded through, e.g. `burst.rs:2632-2634`, `2680-2682`); `invoke` still resolves the
  element and reports `found/not-found/ambiguous` plus the path it would use (`GUIDE.md:467-470`).
  UNPROVEN: whether `wait_for` pixel/stable polling is faithfully simulated under `dry_run` (not traced).

## 3. Discoverability and context cost

- Platform limit stated: PROVEN. `README.md:11` ("Needs Windows 10/11 x64"); `GUIDE.md:16` (launcher
  refuses non-Windows x86_64); `GUIDE.md:146` (server still Windows-x64-only).
- Background/headless guarantee in tool text: PROVEN for `TOOL_DO`/`TOOL_SCREENSHOT`/`TOOL_WINDOW`
  (`src/schemas.rs:8-14` carry the no-`SendInput`/no-activation language). BUT the `initialize`
  `instructions()` contradicts them — see §5.
- Coordinate space + DPI stated: PROVEN in GUIDE/schemas, STALE in `instructions()`. Correct rule:
  under `target`, `x/y` are **client-area physical pixels**; without target, virtual-desktop screen pixels;
  rects stay screen pixels (`GUIDE.md:95-102`; `src/target.rs:11-22`; `TOOL_DO` text in `schemas.rs:12`).
  Per-monitor DPI aware, client px are device px (`GUIDE.md:97-99`). `instructions()` instead says flatly
  "Coordinates are screen/virtual-desktop pixels" (`src/serve.rs:126`) — wrong for every targeted call.
- `tools/list` cost (method: byte length of each `TOOL_*` const chunk in `src/schemas.rs`, 38656 bytes
  file total; descriptions dominate, input/output schemas included): `DO` ~19181, `SCREENSHOT` ~6607,
  `ELEMENTS` ~4394, `WINDOW` ~3553, `DIFF` ~2349, `LIST_WINDOWS` ~1433, `CURSOR` ~580. The tax is `do`
  (half the surface) then `screenshot`. Any agent that lists tools pays ~38 KB before sending one action.

## 4. Error taxonomy

- Documented code set (`GUIDE.md:480-498`, 18 codes): `ambiguous_target, target_gone, bad_target,
  target_required, no_foreground_policy, no_cursor_policy, no_clipboard_policy, background_unavailable,
  bad_coordinates, window_minimized, capture_unavailable, stale_ref, uipi_blocked, burst_budget_exceeded,
  uia_timeout, capture_timeout` (+ table rows for `bad_display|bad_region|…`, `-32603 Server busy`).
  Machine-readable `code` + human `error` shape is uniform in lock/gate paths
  (`target.rs:56-69`, `gate.rs:79-91`).
- "Window gone" vs "asked to steal focus" vs "app ignored the click" ARE separable: PROVEN.
  `target_gone` vs `no_foreground_policy/no_cursor_policy` vs posted-click `delivered/effect`
  (`GUIDE.md:400-407`: raw x/y click always `effect:"unverified"`; element click that changed nothing
  fails `background_unavailable`). The honest-but-awkward branch: a raw-coordinate click that the app
  ignored reports success (`posted/unverified`) — the agent cannot tell "landed" from "ignored" without
  opting into `verify`/`diff`.
- Stringly-typed edges (branches an agent CANNOT cleanly take):
  1. `screenshot` failures arrive as `["error: <code>: <detail>"]` text blocks (`GUIDE.md:204`) — the
     agent must parse the code out of prose, unlike the `{code,error}` JSON elsewhere.
  2. `src/winop.rs:62-71` (`win_code_of`) maps only 4 codes; any other window-verb failure text carries
     no `code` at all.
  3. `src/burst.rs:1004-1018` emits a live `foreground_required: …` refusal (owner-desktop keyboard
     path) that does NOT appear in the GUIDE error table — an undocumented code on a reachable path.
  4. `wait_for` timeout (`ok:true, satisfied:false`) and `set_value`/`type`-on-element/`clipboard_set`
     (always refused: `background_unavailable`/`no_clipboard_policy`) are dead-ends the agent can only
     learn by reading GUIDE, not from the schema alone.

## 5. Repo honesty (claim → verdict)

- "No cursor movement / focus stealing / raised window; background-safe": PROVEN structurally.
  Zero `SendInput/SetCursorPos/SetForegroundWindow/BringWindowToTop/mouse_event` call sites, pinned by
  source scan `tests/no_steal.rs:31-35,94`; policy refusals happen before anything is sent
  (`src/gate.rs:1-33`, `GUIDE.md:62-93`). Live capture test asserts the foreground hold
  (`tests/no_steal.rs:579-586`).
- "Exact and instant, no interpolated journey": PROVEN. Click = one down+up (`burst.rs:875-893`),
  drag = down/move/up at exact points (`burst.rs:949-955`), `hover:true` the only `WM_MOUSEMOVE`.
- 7-tool count: PROVEN at the wire (`src/serve.rs:128-130` serves all 7; 7 `TOOL_*` consts in
  `src/schemas.rs:8-14`; `GUIDE.md:3` states seven; `CHANGELOG.md:52-54` records the fix) — but
  `instructions()` still says "the six tools" (`src/serve.rs:122-124`) and its bullet list has no `diff`:
  FALSE/STALE.
- Windows-only scope: PROVEN (§3).
- Typed-error contract: PARTLY FALSE as advertised to agents. `instructions()` (`src/serve.rs:126`)
  promises `foreground_required / cursor_move_required / clipboard_write_required` refusals "unless the
  call passes `allow_foreground:true / allow_cursor:true / allow_clipboard:true`" — i.e. an escalation
  path. The code has no such codes (only `src/serve.rs:126` and one legacy `burst.rs:1010` string mention
  them) and no such path: `allow_*` is **always refused** (`GUIDE.md:68-71,268-269,295-297`;
  `TOOL_DO` text). An agent following `instructions()` will ask for a flag that can never work.
- "Only action=focus may touch the foreground" (`instructions()`, `serve.rs:126`): FALSE. `focus` is
  refused with `no_foreground_policy` (`GUIDE.md:268-270`; `winop.rs` tests `:636,:699-743`).
- "Elements with zero pixels / capture without activation": PROVEN. Elements walk is read-only by
  construction (`GUIDE.md:331-332`, `readOnlyHint` in `TOOL_ELEMENTS`); capture path never activates
  (`TOOL_SCREENSHOT` text; live no-steal capture proof `tests/no_steal.rs:566-592`).
- Version/tag drift: `Cargo.toml:3` = installed 7.0.2 = tag `v7.0.2` — consistent; HEAD is one commit past
  the tag and dirty (`M README.md`, `?? scripts/npm-passkey.mjs`). `tests/version.rs:8-30` pins
  `--version` = `CARGO_PKG_VERSION` + full 40-hex sha, so the drift is detectable, not silent.

## 6. Tests that can actually fail

- Covered (a regression would go red): forbidden-API symbol scan (`tests/no_steal.rs`, static + live
  foreground-hold with harness-vs-third-party classification per `CHANGELOG.md:42-47` and commit
  `8eb1c2d`); target ambiguity candidates + recycled-PID creation-time refusal + live backgrounded-lock
  (`src/target.rs:762-831,836-973`); burst continue-always / `isError`-only-if-none-succeeded
  (`src/burst.rs:5738,6513,7022`); `dry_run` refusal parity; `offscreen_element`, `stale_ref`
  (`burst.rs:4923,6364`), `uipi_blocked`, `burst_budget_exceeded` (`burst.rs:5699`), `bad_coordinates`
  clamps (`burst.rs:7302-7373`); payload-vs-`outputSchema` conformance
  (`tests/envelope_truth.rs`, `CHANGELOG.md:11-15`); tool-surface truth (`tests/schema_truth.rs`);
  handshake/framing (`tests/mcp_conformance.rs`); UIA/capture deadlines via stall-simulation
  (`src/deadline.rs:219-220`, `tests/timeout_bound.rs`, gated live `PUPPET_LIVE_DESKTOP=1`).
- NOT covered (no test would go red):
  - `cursorUnchanged` is telemetry only — explicitly never a failure (`tests/no_steal.rs:584-585`
    `let _ = …; report_pointer…`). A regression that moved the cursor while reporting correctly would
    pass; only the symbol scan guards the mechanism, not the invariant.
  - `foregroundUnchanged` server boolean is distrusted by the test itself (re-checked externally,
    `no_steal.rs:563-565,579-581`) — good, but nothing pins the *reported value's* accuracy.
  - HWND-recycling of saved restore geometry: `WindowKey=(hwnd,pid,creation_time)` (`winop.rs:28-45`)
    has no located test for stale-geometry refusal (UNPROVEN — located by grep, not exhaustively).
  - Animated-title selector instability (§1): no test.
  - `instructions()` text itself: no test pins it against `tools/list` (schema_truth pins the surface,
    not the prose) — which is exactly why the six-tool/wrong-codes staleness shipped silently.

## 7. Sibling boundary

- No pointers found: grep for `gladius|oc-rea|browser automat|forensic|when to use|instead use` in
  `docs/GUIDE.md` → no matches; `README.md` grep → none; `src/serve.rs` grep → none (only "Server busy").
  `GUIDE.md:519-521` defers WGC capture honestly, and `HEADLESS.md` scopes the hidden desktop, but
  nowhere tells an agent "browser pages belong to gladius, binary forensics to oc-rea."
- Deferral judgement (from repo content, not from pointers): puppet should defer anything needing real
  input semantics (apps that ignore `PostMessage`, `GUIDE.md:533-539`), composited capture (WGC,
  `GUIDE.md:519-521`), and anything outside Win32 windows entirely. The repo documents the limits but
  never names the neighbour to call instead — an agent that hits `background_unavailable` has no
  first-party next step. UNPROVEN whether such pointers exist elsewhere (wiki/Atlas not read this lens).

## Three defects that most endanger trust

1. `initialize`/`discover` `instructions()` (`src/serve.rs:126`) teaches agents the wrong contract —
   six tools (omits `diff`), wrong error codes, a non-existent `allow_*` escalation path, flat screen
   coordinates, and "focus may touch the foreground" — while the schemas enforce the opposite.
2. The two largest tool descriptions (`do` ~19 KB, `screenshot` ~6.6 KB of ~38 KB `tools/list`) carry
   load-bearing semantics (client-vs-screen coordinates, verify defaults, `satisfied` vs `ok`) that an
   agent which skims or truncates `tools/list` will get wrong by acting.
3. Telemetry without teeth: `cursorUnchanged` is never asserted and `foregroundUnchanged` is
   self-reported-but-distrusted, so the headline "background-safe" guarantee rests on the symbol scan
   and refuses — which hold — rather than on any test that would catch a quiet violation of the values
   agents actually read.

Verdict: credible base — the lock, the refuses-before-sending gate, and the no-steal proofs are real and
tested — but `instructions()` needs rebuilding first: it is the first text every agent reads and today it
describes a server this repo does not run.
