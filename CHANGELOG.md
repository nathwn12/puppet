# Changelog

All notable changes to this project will be documented in this file.

The format is based on [Keep a Changelog](https://keepachangelog.com/en/1.0.0/),
and this project adheres to [Semantic Versioning](https://semver.org/spec/v2.0.0.html).

## [Unreleased]

### Changed (breaking)
- `do` element mutations now require `expect_fp`: `invoke` and `click` with an
  element and no fingerprint refuse `expect_fp_required` (returning the fresh
  fingerprint for a one-call retry) instead of acting, and a mismatch keeps
  refusing `stale_ref`. A 7.0.2 call that omitted the field and succeeded now
  fails. (`set_value` and `type` with an element still refuse
  `background_unavailable` first, unchanged; element `scroll` stays
  stale-only.)
- `screenshot` captures losslessly by default: `fmt` defaults to `png` (was
  `jpeg` unless `out` was set). Callers that omitted `fmt` and sized, stored,
  or diffed JPEG responses now receive PNG bytes (`image.format:"png"`,
  `quality:null`). JPEG needs an explicit `fmt:"jpeg"`.

Only the two breaks above ship in 8.0.0. The effect-vocabulary rename and the
`window:close` verification named in the frozen plan are deferred, not
shipped, and nothing below claims them.

### Added
- A payload-vs-schema conformance gate: one `tools/call` per listed tool, each
  `structuredContent` validated against that tool's own declared
  `outputSchema` (`scripts/conformance-check.mjs` over
  `tests/envelope_truth.rs`), so a payload that violates its own schema fails
  the build instead of shipping. `ci.yml` runs the same conformance binary.
- The served contract is generated from a single claims source (`src/claims.rs`):
  the seven tool lines, the coordinate rule, the refusal table, the error
  codes, the invariants and the limits. Refusals name only recoveries that can
  succeed, dead verbs are documented as refused, and per-tool byte budgets keep
  `tools/list` in one page.
- An element `wait_for` reports `satisfied_by{selector,fp,path}` for the control
  that won (null when nothing won, never an error); the element fingerprint is
  documented as identity, never state.
- `diff` reports the image it compared (`image`, `scale`, `bbox_space`) and
  accepts a `mask` excluded from the counts (`masked_px`); `screenshot` reports
  the blank-frame measurement (`blank_check`); captures disclose a clamp that
  altered the request.

### Changed
- Refresh compatible locked dependencies, including crc32fast 1.5.4, flate2
  1.1.11, itoa 1.0.19 and syn 3.0.10; no API or behaviour change.

### Fixed
- The npm package stage carries `npm/assets` (the README's
  `assets/signature.svg`) and the repo-root `THIRD-PARTY-NOTICES.md`, then
  asserts every staged file and every local path the staged README references
  before packing, so the published tarball cannot show a broken image or ship
  without its notices.
- `ci.yml` also runs on every push to `main`, not only on `pull_request` and
  `workflow_dispatch`: the linear direct-to-main history was previously
  untested by CI.
- The release derivation accepts git's own revert subject. `Revert
  "<conventional subject>"` and `revert: <conventional subject>` classify as
  `revert` - no hop of their own, the hop belongs to the change being reverted
  - instead of refusing the range as non-conventional; a revert of a
  non-conventional subject and a plain `Revert the thing` are still refused.
  `--check` covers both directions.
- The release derivation cancels the hop of a change that a later revert in the
  same range undoes, because the tree after the revert is byte-identical to the
  tree before the original: a released hop can no longer be burned on a
  zero-delta range. A revert that carries its own `!` or `BREAKING CHANGE:`
  marker keeps its major hop and cancels nothing, so a breaking revert is never
  shipped as no release.
- The burst foreground gate tells a third party's focus change from a harness
  steal: it captures the harness-owned window set before the operation, fails
  at once on a foreground window this process owns, and re-baselines on
  somebody else's window instead of blaming the tool. The no-steal property is
  unchanged, and the tests prove the immediate fail, the fail after churn, and
  the re-baseline decision paths.
- The runtime zip stages `THIRD-PARTY-NOTICES.md` beside `puppet.exe`,
  `LICENSE` and `README.md` (the shipped README links to it), and the assemble
  step asserts all four files are in the stage before compressing, so a
  missing licence or notice fails the run instead of shipping.
- Docs: `docs/GUIDE.md:3` states the real tool count (seven); the README links
  `THIRD-PARTY-NOTICES.md` and the notices file ships with it; both READMEs
  carry the house footer and signature.
- A timed-out helper thread no longer leaks a kernel handle: the timeout arm
  detaches instead of setting the handle aside, and a helper that later returns
  leaves nothing behind (isolated differential test).
- `set_value` reports `background_unavailable` whatever the flag: the verb's own
  refusal now runs before the `allow_foreground` pre-check.
- `do` reports a wait's winner only while the selector still matches it, so an
  enabled wait satisfied by a same-named replacement names the actual winner.

## [7.0.1] - 2026-11-03

_Reconstructed after the fact from bd246f9: the v7.0.1 release shipped with no
section in this file._

### Fixed
- `diff` accepts its own identical result. Its `outputSchema` now allows
  `bbox:null` (and no `diff_out`) instead of declaring the unchanged case
  impossible, and a unit test proves the payload validates against the schema
  the tool advertises.

## [7.0.0] - 2026-11-03

### Added
- `screenshot` gains `out`: an optional absolute path. The exact returned
  bytes are also written to disk (parent dirs created) and the payload gains
  `out:{path,bytes,fmt}`. PNG when `out` is set and `fmt` is omitted (a diff
  needs lossless pixels). The inline image still returns.
- New `diff` tool: compare two captures pixel by pixel. Each of `base`/`next`
  is `{path:"<png>"}` or a capture target (`target`/`window`/`display`/
  `region`, same selector grammar as `screenshot`); optional `threshold`
  (0-255 per channel, default 0), `max_px` (default 1280 long edge for the
  returned diff image), `out` (path for the diff PNG), `bbox` (default true).
  Both sides must decode/capture to the SAME pixel dimensions, else a typed
  `size_mismatch` naming both sizes — never a silent scale. Returns
  `{changed,total,ratio,bbox|null,identical,base,next,diff_out?}`; the diff
  image marks changed pixels solid red over a dimmed greyscale copy of base.
  Read-only (`readOnlyHint:true`); errors never raise. The per-pixel rule is
  ported from `gladius`'s `visual.rs` (`png_diff`/`DiffOpts`).
- Posted `key`/`hotkey` delivery: `delivery:"auto"|"posted"|"sendinput"`
  (default `auto`; `posted` requires a locked target) posts
  `WM_KEYDOWN`/`WM_SYSKEYDOWN` (+UP) with a correct lParam (repeat 1, scan
  code from `MapVirtualKeyW`, extended bit, Alt context code, key-up bits
  30/31) and an 80 ms default hold. Verified by default: the target is
  captured before and after and diffed with the pixel engine, reporting
  `{delivered:"posted", effect:"changed"|"unchanged", changed, bbox}`;
  `unchanged` is a result, not an error.

### Changed (breaking)
- Background-only policy, absolute: `SendInput` is never called (the
  escalation path is deleted — `src/gate.rs` holds no injection API at all),
  so `delivery:"sendinput"` and `allow_foreground:true` return a typed
  `no_foreground_policy` error naming the rule, instead of acting. No flag
  buys the foreground, the pointer, or the clipboard.
- Pointer acts that need a real cursor (`move`, cursor-less click/drag/
  scroll) return `no_cursor_policy`; `focus_window`, `window`
  `action:focus` and `action:maximize` return `no_foreground_policy`;
  `clipboard_set` returns `no_clipboard_policy` (reads stay free);
  `set_value` / `type`-on-an-element return `background_unavailable` naming
  the control. `allow_foreground` / `allow_cursor` / `allow_clipboard`
  survive in the schemas so old callers get a typed refusal instead of an
  "unexpected keyword" error. Every refusal happens before anything is sent,
  under `dry_run` too.
- Plain `type` posts `WM_CHAR` to the locked target's writable text control
  (a read-only control is skipped); `focus_guard` no longer restores (it
  reports `focus_guard_error` with the policy code instead).
- `sendinput_calls` / `sendinput_ms` are always 0 (fields kept for envelope
  compatibility); `docs/GUIDE.md` documents the posted-only surface and the
  six refusal codes.

### Changed
- The release lane derives its version from history instead of taking a typed
  one. `scripts/derive-release.mjs` walks the non-merge commits since the
  highest `vX.Y.Z` tag (`fix`/`perf` → patch, `feat` → minor, `!` or
  `BREAKING CHANGE:` → major; on 0.x a major hop moves the minor) and refuses
  on an empty range, a non-conventional subject, a range with no releasable
  commit, an empty `## [Unreleased]` (unless the derived version's section
  already exists), or a manifest that is not already the derived version. It
  never writes a version: the lane asserts them, it does not repair them.
- The `release` workflow's `version` dispatch input is gone. `commit` is an
  assertion that the dispatched ref contains it (the checkout follows the
  dispatched ref, never the input), the version is derived before every
  version assert, the `## [Unreleased]` section closes after the test and
  doubles as the release body, and the tag is created in the publish job
  immediately before the release is attached.
  `scripts/check-release-workflow.mjs` asserts that shape and runs as the
  lane's first step.
- The README was rewritten to the house standard and states plainly that the
  project is experimental: interfaces may change and a release can break the
  one before it.

## [6.1.4] - 2026-11-02

### Fixed
- Packaging-only: `npm/package.json` restored to the two guards. 6.1.3 added a
  `"test": "node --test test/"` script, and the release gate fails closed on
  any script beyond the global-install-refusal `preinstall`/`postinstall`
  pair, so the tag run died at `assert versions + fill manifest`. The test file
  stays and documents its direct command (`node --test npm/test/`). No runtime
  change: the crate and the launcher are byte-identical in behaviour to 6.1.3.

## [6.1.3] - 2026-11-02

### Fixed
- The `do` wall-clock budget now bounds the SLEEPS, not only the action loop.
  The trailing `settle_ms`, the chunked `type` pacing (`chunk_chars` /
  `chunk_ms`) and the `key` hold/repeat pacing (`hold_ms` / `gap_ms` ×
  `repeat` 1..100) each sleep without consulting the budget before, so one
  action could hold the global input lock for hours while the budget read as
  enforced. Every pacing sleep is now clamped to what is left of the budget,
  an exhausted budget ends the action with the typed
  `burst_budget_exceeded` result, and the settle reports both what was asked
  (`settle_requested_ms`) and what the budget allowed (`settle_ms`). A 1 MiB
  text at `chunk_chars:1, chunk_ms:60000` now ends typed instead of holding
  the lock.
- Every cross-process COM / `PrintWindow` entry point is bounded, not just
  the three that were. `uia_is_enabled`, `uia_expand_collapse`,
  `uia_expand_state`, `uia_range_value`, `uia_scroll` and `uia_element_text`
  now run under the same `deadline::bound` UIA deadline (and therefore honour
  `PUPPET_UIA_TIMEOUT_MS`), returning the typed `uia_timeout` line instead of
  wedging a worker on a provider that stops answering. The hidden-desktop
  capture cannot move off its own desktop thread, so its wait takes the same
  `Capture` family deadline (`PUPPET_CAPTURE_TIMEOUT_MS`) and reports the
  typed `capture_timeout` line.
- `do`'s `focus_window` action resolved its `hwnd` with a bare `IsWindow`
  check and focused whatever owned that handle. It now resolves through the
  same `TargetSpec`/`TargetLock` every other verb uses (`hwnd` -> `{hwnd}`,
  `title` -> `{titleContains}`) and revalidates pid, process creation time,
  image and class immediately before the focus call, so a recycled handle is
  the typed `target_gone` (with the target's own `code`) instead of a focus
  on the wrong window.
- A `dry_run` burst reported `ok:true` for a `click` on an element the live
  path refuses with `offscreen_element`. The geometry guard now runs before
  the dry-run return, so a dry run reports the refusal the live call would
  give (UIA-pattern controls, which need no pixel, are unaffected).
- `screenshot` adopted the POST-render rect as `source` before the recapture
  that re-renders the pixels had succeeded. On a failed recapture the image
  still came from the pre-capture rect while `source` (and `rect_source`)
  described where the window had moved to, so a caller mapped stale pixels
  through the current geometry. A failed recapture now keeps the pre-capture
  rect and adds a `note` naming both rects.
- `puppet-mcp` (npm launcher) died with `EPERM` when two launchers started
  the same version at once: both staged and both renamed, and Windows refuses
  the loser's directory rename onto the winner's (observed live as
  `Connection closed ... rename staging-34508 -> 6.1.2-e8f96a59`, breaking an
  MCP session mid-handshake). Provisioning is now concurrency-safe: the
  `.ready` marker is written inside the staging dir so a published directory
  is verifiable the instant it appears, and a lost rename is retried briefly
  and then ADOPTED once the sibling's cache verifies against the manifest. A
  failing attempt never deletes a cache a sibling published. Covered by
  `npm test` (`node --test npm/test/`).

## [6.1.2] - 2026-11-01

### Fixed
- Test-only change: the live capture test is gated behind `PUPPET_LIVE_DESKTOP`
  because CI renders the offscreen fixture black; no runtime change.

## [6.1.1] - 2026-11-01

### Fixed
- `screenshot` with `elements:true` (desktop/region) samples the foreground
  window once per call and carries that handle through both the element walk
  and `elements_window`. It previously read `GetForegroundWindow()` again when
  naming the window, so a foreground switch between the two reads could make
  the reply enumerate one window and name another; the release suite
  (`cargo test --release --locked`) caught it on a CI runner.
- The suite's live-desktop tests now serialize on one process-wide guard
  (`fixture::desktop()`): the fixture message counters, the foreground handle
  and the UIA provider's focus are process-wide, so the parallel harness let
  one test observe a sibling's window, focus or posted messages. The posted
  click/drag tests also wait for the whole message group before reading the
  counters. `cargo test --release --locked` is deterministic in parallel.

## [6.1.0] - 2026-11-01

### Added
- `do` results carry top-level `ok` (true only when every action succeeded) and
  `failed` (how many did not), so a burst has a call-level verdict; `isError` is
  now true when **no** action succeeded, while continue-always and the
  "a timeout is not an error" rule are unchanged.
- `elements` (and the screenshot element block) mark an entry whose rect has no
  area with `offscreen:true` instead of letting `[0,0,0,0]` read as geometry.
- `screenshot` re-reads the DWM frame rect after the render: a window that moves
  during the capture reports the POST-capture origin as `source` and sets
  `capture.moved_during_capture:true` (a pure translation is recaptured once),
  so the image-to-screen map never presents a stale origin as current. The
  foreground-crop fallback follows the same rule.
- `screenshot` `monitors` now carries `rects`, per-monitor `[l,t,r,b]` in the
  same shape `cursor.screens` uses. With `display`/`region` + `elements:true`
  the payload names the foreground window the element block describes in
  `elements_window{hwnd,exe,title}`.
- `list_windows` accepts `include_app_ids` (default `false`): each entry gains
  `appId` from the window's AppUserModelID property store, or `null` when
  absent. Opt-in because the store read is a per-window COM call.
- The `do` `type` action schema declares `element`, `index` and `expect_fp`
  (the runtime UIA `ValuePattern.SetValue` path already accepted them).
- `PUPPET_UIA_TIMEOUT_MS` (default 5000) and `PUPPET_CAPTURE_TIMEOUT_MS`
  (default 10000) set the wall-clock deadline of one bounded UIA call or one
  capture; both are clamped to 50..120000 ms.

### Fixed
- `click` on an element with no on-screen rectangle is refused before any input
  with the typed `offscreen_element` error, naming the fix. Previously it posted
  a button at the target's client origin — a wrong click on the window frame.
  Every element-rect to client-pixel conversion in the burst path now goes
  through one helper that owns the guard, including the `invoke` pixel-click
  fallback.
- Bump jpeg-encoder to 0.7, whose output is byte-identical to 0.6, and raise
  the minimum supported Rust version to 1.87 because that crate requires it.
  `rust-toolchain.toml` and both CI workflows now pin 1.87.0.
- The legacy `window:` selector on `screenshot` and the `title`/`hwnd`
  arguments on `window` and `elements` now resolve through the same
  `TargetSpec`/`TargetLock` the `target:` argument uses (`hwnd` -> `{hwnd}`,
  `title` -> `{titleContains}`). They are revalidated against pid, process
  creation time, image and class before acting; a recycled handle is the typed
  `target_gone` instead of a bare `IsWindow` check.
- `do` is bounded to 256 actions and 300 s of total wall-clock time per call.
  Exceeding either returns a typed `burst_budget_exceeded` result naming the
  limit and the actual value in the same per-result shape as a failed action;
  a `wait`/`wait_for` that would outlive the remaining budget is refused or
  clamped.

## [6.0.1] - 2026-10-28
### Changed
- Refresh compatible locked dependencies, including crc32fast, equivalent,
  serde_json, syn, and unicode-ident; add transitive foldhash.
- Keep the Rust 1.85 minimum supported version.

## [6.0.0] - 2026-10-24

### Changed (breaking)

- **`clipboard_set` is deny-by-default.** Overwriting the owner's global
  clipboard is a side effect *outside* the target window (every application
  shares that clipboard), so `do` now requires `allow_clipboard: true` for a
  `clipboard_set` action. Without it the action fails with the typed refusal
  `clipboard_write_required`, naming the exact flag, **before any clipboard
  access**: no `OpenClipboard`, no `EmptyClipboard`, no
  `SetClipboardData`, so a refused call cannot have altered the clipboard.
  The refusal is per-action like every other gate refusal - it never aborts
  the rest of the burst. `clipboard_get` is a read and stays free, and
  `restore`/`dry_run` semantics are unchanged (`dry_run` still touches
  nothing; it refuses identically, because a dry run that reports ok while
  the live call would refuse is a lie).
- `ClipboardPermit` joins `ForegroundPermit`/`CursorPermit`: the sole mint is
  the flag, `clipboard_set` demands the token, and `tests/no_steal.rs` pins
  both the mint audit and the permit-typed signature.

## [5.0.0] - 2026-10-23

### Changed (breaking)

- **Background text now requires `allow_foreground:true`.** `type` with an
  element and `set_value` write through UIA `ValuePattern.SetValue`, whose
  provider may focus the control it writes. They are no longer
  background-safe: without the flag they return `foreground_required`
  before any provider call. `invoke`/`toggle`/`expand`/`collapse`/`scroll`
  patterns stay background-safe.
- **Visible `maximize` requires `allow_foreground:true`.** A maximized
  window covers its monitor and Win32 documents no no-activate form, so the
  default returns `foreground_required` and the window is untouched. This
  is a gate, not a focus-safety claim: passing the flag makes the verb an
  explicit opt-in whose activation is measured and reported, never
  silent.
- **Posted keyboard removed from the default path.** `WM_CHAR`/`WM_KEYDOWN`
  bypass the input queue, desync the target and are ignored by many
  controls, so plain `type`/`key`/`hotkey` are never posted: they need
  `allow_foreground:true` and escalate to real `SendInput`. Posted keyboard
  survives only on a desktop the server created *and* that the target
  window actually lives on — `desktop:"hidden"` is earned by desktop-name
  comparison, never granted by the argument alone.
- Typed `foreground_required` / `cursor_move_required` refusals replace
  silent fallbacks: the foreground family is refused *before* anything is
  sent, and pointer acts need both `allow_cursor:true` and
  `allow_foreground:true`.

### Added

- **Target lock.** Every acting verb takes
  `target={hwnd?,pid?,exe?,appId?,titleContains?,className?}` (or a selector
  string) that resolves ONE window for the call and is revalidated before
  every action — `hwnd`, `pid` with process start time, image name and
  class, plus `appId`. Ambiguous or vanished targets are typed
  (`ambiguous_target` / `target_gone`); there is no foreground fallback.
- **No-steal default input path.** Under a lock, clicks/keys go UIA-pattern
  first, then `PostMessage`/`SendMessage` with exact client coordinates —
  no `SendInput`, no cursor movement, no activation.
- **Non-activating capture.** `screenshot` can capture a window by
  selector without activating it, reporting
  `foregroundUnchanged`/`cursorUnchanged`.
- **Posted-click honesty.** Every click reports what it proved: `delivered`
  (`uia` | `posted`) and `effect`
  (`verified` | `pattern-returned` | `unchanged` | `unverified`). A posted
  element click whose readable control state did not change fails with
  `background_unavailable`; a raw x/y click always reports
  `delivered:"posted"`, `effect:"unverified"`.

## [4.5.5] - 2026-10-23

### Fixed

- Release gate: the pre-publish cleanroom installed the tarball and booted
  it, but the launcher fetches `runtime.archive` from the GitHub Release —
  an asset the `publish` job creates *after* `needs: [build]`. Any new
  version therefore 404'd at the gate, which is what turned the 4.5.4 tag
  red. The launcher gained `PUPPET_ENGINE_BUNDLE`, a diagnostics-only
  override that reads a local zip instead of downloading, and
  `scripts/cleanroom-check.mjs` gained `--bundle` to point it at the
  archive built in the same run. The size and sha256 checks are unchanged
  and still run on the local bytes, and `--registry` mode scrubs the
  override, so the gate keeps its teeth. The gate is not weakened: it is
  fed the real artifact.

### Added

- `release.yml` accepts `workflow_dispatch` with `version` and `dry_run`
  (default `true`), so the whole build + pre-publish gate can be rehearsed
  under a `v<version>` tag before one exists. A dry run skips exactly the
  irreversible steps — release creation, npm publish, outgoing-release
  deletion — and the public-registry gate that depends on them.

No engine behaviour change.

## [4.5.4] - 2026-10-23

### Changed

- Version-only rebuild of unchanged engine source, cut to exercise the
  release gate. The only change since `v4.5.3` is CI: releases are now
  gated on a fresh-profile install and boot. No behaviour change, no
  dependency change.

## [4.5.3] - 2026-10-21

### Fixed

- fix: npm package shipped a manifest with stale engine hashes; packaged manifest now verified against the built assets before publish.

## [4.5.2] - 2026-10-21

### Added

- Launcher `--version` prints `puppet-mcp <version>` and exits 0
  (previously exited 2 with usage); `--doctor` also reports the launcher
  version, the resolved engine path, the engine version parsed from the
  cache directory name, and whether they match.
- Fail closed on version skew: before downloading or spawning, the
  launcher compares its `package.json` version against the version in the
  engine cache path it is about to run and refuses with the re-provision
  command instead of silently running a stale engine.
- Global installs refused by design: `preinstall`/`postinstall` guards
  reject `npm i -g` so a global copy can never shadow a pinned `npx` run.
  Install with `npx puppet-mcp@4.5.2`.

## [4.5.1] - 2026-10-20

### Fixed

- Docs: one-release policy — use `npx puppet-mcp@latest`; a pinned version is only installable while its GitHub release exists.

## [4.4.1] - 2026-10-18

### Fixed

- Docs-only patch: the README rewritten to the house skeleton with the
  npx install front door (`npx -y puppet-mcp@latest`); no behaviour
  changes.

## [4.4.0] - 2026-10-18

### Added

- `instructions` in the `initialize` result: a usage contract for a
  client/model that has never seen this server — the six tools in one line
  each, the coordinate rule with the image-to-screen formula, batch +
  `dry_run`, and read-only-first on the owner's real desktop.

### Fixed

- Typed stale-element-ref refusal: every `elements` entry carries `fp`, a
  stable identity fingerprint, and `invoke`/`set_value` accept `expect_fp`
  to refuse a stale ref instead of acting on the wrong control.
- UIPI/elevation refusal: every `list_windows` entry carries `elevated` so
  a caller can tell an elevated target apart from an unresponsive one, and
  UIPI-blocked input is refused with a typed error instead of silently
  doing nothing.
- Test fix: the foreground assertions are environment-independent (new
  `src/fixture.rs`), so the suite passes the same on a bare runner as on
  the owner's desktop.

## [4.5.0] - 2026-10-20

### Added

- `do` parameter `desktop: "hidden"` (opt-in, default null): launches the
  program named by `background` (`exe:notepad.exe` or a path) on a private
  desktop, resolves its window with `EnumDesktopWindows` (never the owner's
  `EnumWindows`), drives it through the existing `background` PostMessage
  path, captures it with `PrintWindow(PW_RENDERFULLCONTENT)`, closes that
  process (and any process owning a window on that private desktop), and
  destroys the desktop. Teardown moves the thread off the desktop before
  `CloseDesktop` and proves it gone with a NULL `OpenDesktopW`. The owner's
  desktop, cursor, foreground window, and focus are never touched; omitting
  the parameter leaves plain `do` byte-identical.
- Clipboard: `do` `clipboard_set` writes CF_UNICODETEXT (max 1_048_576
  chars; over-cap sets fail typed before touching the clipboard) and
  `clipboard_get` reads it back truncated to 1_048_576 chars, NUL-checked
  against the GlobalSize bound.
- Elements paging: `elements` accepts `max_depth` (1..32, default 8) and
  `offset` (default 0); when the 200-entry cap is hit and more controls
  remain, `next_offset` names the next page, else null.
- `wait_for` `kind:"element"`: polls for a control (`window` selector
  optional, `element` name:|auto_id:|path:, `state`
  appear|disappear|enabled|disabled) with the same timeout semantics as
  the pixel and window waits; a timeout reports `satisfied:false`, never
  an error.
- UIA actuation: `do` `invoke` accepts `state` `"expand"|"collapse"`
  driving only the UIA ExpandCollapse pattern (typed error without it);
  `do` `set_value` falls back to UIA RangeValue for sliders (numeric
  text, range-checked, read-only honored); `elements` with `detail:true`
  adds TextPattern `text` (capped at 4000 chars, `text_truncated` flag).
- Structured output: tool results carry typed JSON content blocks
  (coordinate map, element lists, per-action results); dual-era MCP
  protocol versions negotiated at `initialize`.
- Input: `do` `key` holds Shift when the layout requires it; DXGI
  desktop-capture fast path with GDI fallback; `scroll` supports the
  horizontal wheel; `move` accepts relative `dx`/`dy`; `key` accepts
  `hold_ms`/`repeat`/`gap_ms`; `scroll` accepts `element` driving UIA
  ScrollPattern instead of the wheel.
- Gates and caps: toolchain pinned to 1.85.0 (rustfmt + clippy), CI with
  fmt/clippy/gitleaks/build/test; memory caps (`stable` regions 4096px,
  grabs 8192px, shared overflow-safe region validator); `initialize`
  `instructions` now enumerate all 16 `do` action types.

## [4.3.0] - 2026-10-12

### Added

- `do` action `set_value`: writes text via UIA `ValuePattern.SetValue`
  on the resolved control (`name:`/`auto_id:`/`path:` selector), with no
  click, no coordinates, and no focus change. It fails closed with typed
  errors (element not found, no ValuePattern, read-only, re-resolve
  failure), reports the target and the value length, and never echoes
  the text value back.

## [4.2.1] - 2026-10-12

### Fixed

- Removed the `ref:` invoke selector: a `ref` token pins only window +
  path and cannot promise element identity, so invoking by `ref:` could
  silently retarget activation to a replacement control when the UI
  changed between enumeration and invoke. Invoke accepts `name:`,
  `auto_id:`, and `path:`; `ref` stays on element entries as a
  per-enumeration correlation identifier, not a durable handle.
- Corrected the `elements` tool description: the compact default entry
  omits `win` (and `hwnd` on the Win32 fallback), which `detail=true`
  retains; fixed the `hwmd` typo to `hwnd`.

## [4.2.0] - 2026-10-12

### Added

- Standalone `elements` tool: zero-pixel structured UI enumeration for a
  window (title/hwmd selector plus `detail`), returning the same
  top-level object the screenshot `elements` block carries as
  structured JSON (per entry, the compact default shape omits `win`,
  and `hwnd` on the Win32 fallback, which `detail=true` retains).
- Per-enumeration `ref` token (`"<hwnd>.<path>"`) on every element from
  both the UIA and Win32 backends, for correlating entries between
  listings; it is not an invoke selector and not an identity guarantee.

## [4.1.0] - 2026-10-11

### Added

- Native Rust port of the Python incumbent — no Python runtime or venv at
  runtime; single static Windows binary.
- 5-tool stdio surface over JSON-RPC 2.0 (`screenshot`, `list_windows`,
  `window`, `do`, `cursor`), reporting
  `serverInfo { name: puppet, version: 4.1.0 }`.
- DXGI desktop-capture path alongside the GDI fallback.

### Measured

- Parity against the Python incumbent: 8/8 core, 60/60 do-parity, 9/9, 18/18
  invoke resolution (measured on the author's machine).
- Head-to-head bench against a 10-server field: ranked 2/10.
