# Lens 4 — Lifecycle under a hostile desktop (READ-ONLY AUDIT)

Repo: `Q:\PROJECTS\PERSONAL\puppet`, HEAD `8eb1c2d` (`git log --oneline -20` checked; status: only `README.md` modified + untracked `scripts/npm-passkey.mjs`). Rust `puppet` 7.0.2, Windows-only, one long-lived stdio MCP process, 4 workers (`serve.rs`), + helper threads via `deadline::bound`. No build/run performed.

Historical audit: the helper-handle finding below describes that HEAD, not the
current implementation. Commit `d7a78f3` detaches timed-out helpers without
retaining their handles; a helper still holds resources while it remains hung.

## Verdict in one screen

- Hung-window wedge is **already closed on the paths that matter**: `SendMessageTimeoutW/SMTO_ABORTIFHUNG+2s` in `burst.rs:2055`, and `deadline::bound` (helper-thread + 5 s UIA / 10 s capture) over `uia_element_list/activate/set_value/scroll/expand`, `uia_is_enabled`, `capture_window` (`uia.rs:781,820,879,1031,1125,1177,1264,1366`; `shot.rs:127`). What the agent sees is a **typed** `uia_timeout`/`capture_timeout` — not a dead turn.
- Target-recycling is **already closed**: `TargetLock {hwnd,pid,created,exe,class,appId?}` + `revalidate()` before **every** action (`target.rs:357-448`, call sites `winop.rs:124`, `burst.rs:2613`, `screen.rs:252,435`). HWND+PID+creation-time+class closes the recycled-handle and recycled-PID cases. This is as useful as any defect below.
- Elevation/UIPI is **already distinguished**: `process_elevated/window_elevated/self_elevated/uipi_blocked` (`main.rs:1149-1198`) + `uipi_refusal()` pre-check in burst (`burst.rs:2089-2098`) with typed `uipi_blocked … Relaunch this server elevated and retry` (`burst.rs:2082`). UIA-read vs inject asymmetry is handled (fail-open on unreadable). Never elevates itself — correct.
- The remaining normal-week risks are: (a) `window:close` fire-and-forget lies about success, (b) per-action UIPI gaps outside `invoke`/`type`/`key`/`hotkey` posted paths, (c) no desktop/session-state awareness so lock-screen/RDP/UAC failures surface as generic capture/PostMessage errors, (d) kill-path leaves a hidden-desktop child + `WM_CLOSE`-ignored target + abandoned helper threads alive with no reaper, (e) burst partial-state is honest but `posted_messages` counting + `verify_capture` retry can surprise, (f) unbounded thread-stack leak per UIA/capture timeout by design.

---

## 1. Hung and busy windows

### HANDLED — UIA + capture + Win32-activate are bounded
- Evidence: `src/deadline.rs:39,41,149-210` (`UIA_TIMEOUT=5s`, `CAPTURE_TIMEOUT=10s`, `bound()` spawns `puppet-{op}` helper, `recv_timeout`, `std::mem::forget(handle)` on timeout); wrapped at `src/uia.rs:781,820,879,1031,1175,1264,1364`, `src/shot.rs:127-131`; Win32 `BM_CLICK` uses `SendMessageTimeoutW(..., SMTO_ABORTIFHUNG, 2000)` at `src/burst.rs:2051-2063`; hidden-desktop `teardown()` awaits `recv_timeout(limit)` at `src/hidden.rs:158` and `spawn()` setup at `20s` (`src/hidden.rs:290`).
- Agent sees: `error: uia_timeout: <op> did not return within 5000ms… retry, or capture the desktop instead. The abandoned call may still complete, so its effect is unverified.` / `error: capture_timeout: …` (`deadline.rs:102-110`). Typed, actionable, worker stays alive.
- Severity: — (closed). Smallest fix: none; keep the `PUPPET_UIA_TIMEOUT_MS`/`PUPPET_CAPTURE_TIMEOUT_MS` clamp `[50ms,120s]` as is.

### WILL BITE — `window: close` is unbounded fire-and-forget, reports success unconditionally
- Scenario: hung app ignores `WM_CLOSE`; agent is told `ok:true`.
- Evidence: `src/winop.rs:378-385` — `PostMessageW(h, WM_CLOSE, 0, 0);` return value discarded, always `ok:true`. Same pattern in hidden teardown `src/hidden.rs:460` (there at least followed by `WaitForSingleObject`+`TerminateProcess`, but `winop` has no wait/verify).
- Agent sees: `{"ok":true,"action":"close","hwnd":…}` while the window is still there. Next action then fails `target_gone` or acts on a zombie — confusing two-step failure.
- Severity: **major** (normal week: closing hung installers/editors is routine).
- Smallest fix: check the `PostMessageW` return; on `0` return `background_unavailable`-style error; on `!=0` poll `IsWindow` ~2 s then return `ok:true, closed:true/false` (never block past the burst budget). One function, no new lock.

### THEORETICAL — helper-thread abandon leaks one stack per timeout
- Evidence: `src/deadline.rs:28-31,196-200` — documented: "leaks one thread stack per timed-out call — the price of keeping the worker pool alive."
- Agent sees: nothing (by design the worker survives). Owner sees slow thread-count growth only under a sustained hung-target campaign.
- Severity: **minor**. Smallest fix: accept (documented tradeoff); optional: cap consecutive timeouts per worker with a counter + `capture_timeout` fast-path (already have `Dead(Instant)` cooldown idiom in `dxgi.rs:63-71` to copy).

### THEORETICAL — `wait_state_change` 400 ms poll and `verify_capture` 300 ms retry extend a hung call but stay inside the burst budget
- Evidence: `src/burst.rs:2241-2248` (`sleep(20ms)` × 400 ms), `src/burst.rs:640-648` (one 300 ms retry).
- Agent sees: slightly larger per-action `ms`, still bounded by `BURST_BUDGET=300s` (`burst.rs:42,2568-2585`).
- Severity: **nice** (tighten only if tail-latency SLO appears).

## 2. Target dissipation (close / death / HWND recycling)

### HANDLED — lock + revalidate-before-every-action closes recycling
- Evidence: `src/target.rs:360-448` checks `IsWindow` → pid → `process_created` (FILETIME u64) → exe stem → class → AppUserModelID; `src/winop.rs:123-130`, `src/burst.rs:2613-2615,2687`, `src/screen.rs:252,435` revalidate at resolve **and** immediately before act/capture. `winop.rs:24-45` `NORMAL_RECTS` is even keyed `(hwnd,pid,creation_time)` with the comment "An HWND alone is recyclable."
- Agent sees: `ambiguous_target` / `target_gone: … handle was recycled / … DIFFERENT process … pid was recycled` (`target.rs:386-411`), never action on the wrong window.
- Severity: — (closed; exemplary — reuse this pattern everywhere).

### WILL BITE (edge) — `capture_window_inner` re-checks `IsWindow` but not the full lock
- Evidence: `src/shot.rs:139-144` checks only `IsWindow(hwnd)==0 → bad_window: hwnd recycled/closed`, then reads rect/`PrintWindow`. Full pid/creation/class check happens in `screen.rs:252,435` **before** dispatch, so a recycle between revalidate and `PrintWindow` (µs–ms window) captures the wrong window's pixels with the old hwnd label.
- Agent sees: image of window B labeled as window A; `source`/`cmap` silently wrong.
- Severity: **minor** (race window is tiny; screen path already narrows it to two revalidations: pre-capture + pre-elements). Smallest fix: pass the `TargetLock` (or `created`+pid+class) into `capture_window_inner` and re-check after the post-render rect read (`shot.rs:244` already re-reads rect — add identity check there).

### WILL BITE (edge) — legacy `background:` selector path is weaker than `target:`
- Evidence: burst target resolution: `target_lock` → `TargetLock` (`burst.rs:2447-2459`); fallback `background` → `resolve_selector()` (`burst.rs:2521`, `cap.rs`) which is hwnd/title/exe matching without the creation-time/appId checks documented at `target.rs:357+`.
- Agent sees: same action, different identity strength depending on spelling.
- Severity: **minor**. Smallest fix: route `background:` through `TargetSpec::parse` + `target::lock` (one call-site change in `burst.rs:2510-2530`).

## 3. Elevation and UIPI

### HANDLED — read vs inject distinguished; typed refusal; never self-elevates
- Evidence: `src/main.rs:1182-1198` predicate (`target elevated && !self`); `src/main.rs:1149-1179` elevation reads via `OpenProcess(PROCESS_QUERY_LIMITED_INFORMATION)` + `GetTokenInformation(TokenElevation)`, fail-open (`None`); `src/burst.rs:2089-2098,2816-2818,4017-4031` pre-check refuses **before** posting, under `dry_run` too; `list_windows` carries `elevated` flag (`main.rs:1301-1305`, test `winop.rs:496`).
- Agent sees: `uipi_blocked: target "…" (pid …, hwnd …) runs elevated; input … would be silently dropped by UIPI. Relaunch this server elevated and retry; under dry_run this refusal is reported without injecting anything.` + `blocked_by_uipi:true, resolution:uipi_blocked`.
- Severity: — (closed). No `runas`/manifest/elevation attempt anywhere (grep `elevat|UAC|admin` finds only readers) — correct for an unsupervised agent host.

### WILL BITE — UIPI pre-check does not cover every posted verb
- Scenario: `uipi_refusal(tgt)` is wired on the posted-keyboard/`invoke` paths (`burst.rs:2816,4020`); click/down/up/drag/scroll via `bg_post` (`burst.rs:878-1000`) report only generic `BG_POST_FAILED` (`burst.rs:788`) when `PostMessage` fails — and UIPI-blocked `PostMessage` **succeeds** at the API level (message silently dropped), so the agent sees `ok:true, posted N` with no effect.
- Evidence: `bg_post` (`burst.rs:560-562`) returns `PostMessageW != 0`; no `uipi_refusal` call in `bg_execute` click/drag/scroll arms (`burst.rs:836-1010`).
- Agent sees: success envelope, zero effect, no `uipi_blocked` hint → will retry forever.
- Severity: **major** (normal week: automating an elevated installer/editor from unelevated puppet is the canonical UIPI trap).
- Smallest fix: call `uipi_refusal(bg_hwnd)` once at the top of `bg_execute` (and `win32_activate`) and return the same typed refusal; ~10 lines.

### HANDLED — UIA-on-elevated read usually still works (no false refusal on `elements`)
- Evidence: fail-open `None` in `uipi_blocked` + refusal only on inject paths; `elements`/`screenshot` take no input lock and post nothing.
- Agent sees: can still inspect an elevated window, just not drive it. Correct asymmetry.

## 4. Session and desktop changes (lock / FUS / RDP / UAC desktop / sleep / hot-plug)

### WILL BITE — no session/desktop-state awareness; all surface as generic errors or stale data
- Evidence: no references to `OpenInputDesktop`, `GetThreadDesktop`, `WTSRegisterSessionNotification`, `WM_DISPLAYCHANGE/WM_DPICHANGED`, secure-desktop, or lock-state anywhere in `src/` (grep `session|lock|secure|WTS|OpenInputDesktop|GetThreadDesktop|WM_DISPLAYCHANGE` hits only comments/tests/fixture). Capture paths assume a paintable desktop: `PrintWindow` → `capture_unavailable` (`shot.rs:396-399`), DXGI `AccessLost` → GDI fallback (`dxgi.rs` + `screen.rs:638-676`), `GetDIBits/BitBlt` errors as `capture_failed`.
- What happens per event:
  - `Win+L` / screensaver / UAC secure desktop: `PrintWindow`/`BitBlt` return black/blank → agent sees `capture_unavailable: … frame rejected (blank/black frame)` (`shot.rs:201-206,344`) — honest but misattributed (says "window" not "desktop locked").
  - RDP disconnect / resolution change / monitor hot-plug: DXGI session stale → `check_size` mismatch → GDI fallback (`screen.rs:672-676`); rects re-read post-capture (`shot.rs:244-302`, `moved_during_capture` flag) — recapture covers translation, resize is flagged. DPI is per-monitor-aware (`main.rs:1486-1488`, `GetDpiForWindow`), so scale stays right after a DPI change.
  - Display sleep: same as lock (blank-frame refusal).
- Agent sees: typed but **misleading** errors (`capture_unavailable`, `background_unavailable: PostMessage failed… queue refuses`) with no "desktop unavailable, retry when unlocked" signal; will burn turns retrying a state that cannot clear itself.
- Severity: **major** (lock-screen + RDP-reconnect are weekly events on this machine).
- Smallest fix: one `desktop_available()` probe (`OpenInputDesktop(0,FALSE,GENERIC_READ)` + `GetUserObjectInformation(UOI_NAME)` / `WTS` idle check) at the top of `screenshot`/`do`, returning a new typed `desktop_unavailable: input desktop is locked / disconnected; retry when the session is active` — read-only, ~30 lines, no behavior change otherwise.

### HANDLED — geometry races are handled (moved/resize/stale logic)
- Evidence: pre/post `dwm_rect` compare + single recapture on translation + `reported_rect` + `moved_during_capture` + `STALE` map (`shot.rs:208-311`, bounded 64 entries `shot.rs:306-311`).
- Agent sees: correct origin even if the window moved mid-capture. Good.

### THEORETICAL — DXGI duplication session loss across display changes
- Evidence: per-`(adapter,output)` `SLOTS` cache with `Dead(Instant)` 5 s cooldown (`dxgi.rs:63-75`); `dxgi_match_output` refuses same-size wrong-monitor matches (`screen.rs:455-459`, `dxgi.rs:294-326`).
- Agent sees: one failed capture then GDI fallback; session re-opens on next call. Self-healing.
- Severity: **nice** (already handled; only note that a hot-unplug between `pick_output` and `dxgi_grab` is a per-call error, not a poisoned cache — by design).

## 5. Concurrency and ordering

### HANDLED — 4 workers + 8-call cap + single input lock; read-only tools never block
- Evidence: `src/serve.rs:26-55` (`WORKER_COUNT=4`, `MAX_IN_FLIGHT=8 → -32603 Server busy`, `INPUT_LOCK` held only by `do`/`window` at `serve.rs:980-989`); read-only `screenshot/list_windows/cursor/elements` run free. Lock acquisition is `try_lock` + 20 ms sleep + cancel-check (`serve.rs:42-55`) so a cancelled waiter never wedges.
- Agent sees: 9th concurrent call → `-32603 Server busy` (backpressure, not a hang); a `do wait 60s` never blocks `ping/tools/list/cancelled`.

### HANDLED — continue-always: one bad action never aborts the burst
- Evidence: per-action `Ok(Some(m))` + `continue` throughout `do_burst` (e.g. `burst.rs:2608,2636,2654,2698`); budget expiry and cancel are the only `break`s (`burst.rs:2584,2592`). Documented in schema (`schemas.rs:12`: "Per-action failures never abort the burst").
- Agent sees: every action reports its own `{index,type,ok,ms[,code,error]}`; `burst_budget_exceeded` names unrun actions (`burst.rs:2576-2584`).
- Severity: — (closed; warn coordinators: a burst with a dead action in the middle still executes later actions — order side effects accordingly).

### WILL BITE (minor) — `dry_run` validates a different code path than the live run in two places
- Mostly handled: gate refusals (`allow_foreground`, clipboard, UIPI, escalation) are explicitly checked under `dry_run` too (`burst.rs:2620-2636,2732,2781,2829,4017`), and tests pin this (`burst.rs:4946,6785,7101`).
- Gaps: (1) `verify_capture` (`burst.rs:640-693`) runs a **real capture + PNG decode** even under `dry_run` (side-effect-free but not free — and under a locked desktop it fails where the dry run "should" succeed); (2) `element_state`/`wait_state_change` reads (`burst.rs:2217-2248`) poll live state under `dry_run`, so a dry run's `ok` can flip between runs.
- Agent sees: `dry_run:true, ok:true` that goes red live (or vice versa on a locked session).
- Severity: **minor**. Smallest fix: skip `verify_capture`/`wait_state_change` under `dry_run` and report `verified:false, dry_run:true` explicitly.

### HANDLED — two concurrent bursts cannot interleave input
- Evidence: both `do` and `window` hold the same `INPUT_LOCK` for the whole job (`serve.rs:980-989`); inside a burst, posted messages sequence through one `bg_execute`/`st.flush()` chain with `posted_total` accounting (`burst.rs:2556,2853,4792`).
- Agent sees: second burst waits (cancellable) rather than interleaving clicks. Correct.

## 6. Long-lived cost (memory / COM / handles / GDI / bitness)

### HANDLED — GDI objects strictly paired; no image cache
- Evidence: `grab_bits` (`cap.rs:308-381`) pairs every `GetDC→ReleaseDC`, `CreateCompatibleDC→DeleteDC`, `CreateCompatibleBitmap→DeleteObject`, with early-error paths releasing (`cap.rs:320-330`); `bitblt_grab` releases its second `GetDC` (`cap.rs:402-407`); DXGI staging is per-session reused (`dxgi.rs:156+`); `finalize` encodes then drops the `Img` (no static cache; only bounded maps: `STALE` ≤64 in `shot.rs:306-311`, `NORMAL_RECTS` ≤64 in `winop.rs:36-38`).
- Agent sees: hundreds of screenshots do not grow the process (each `Img` is transient; base64 leaves with the response).
- Severity: — (closed). Caveat: `STALE`/`NORMAL_RECTS` use `clear()`-wholesale eviction — fine at 64 entries.

### HANDLED — UIA COM refs owned per-call, released on the creating thread
- Evidence: `Iface` (`uia.rs:114-175`) — `adopt_out` null-checks, `Clone→AddRef`, `Drop→Release`; `ensure_com` is `COINIT_MULTITHREADED` per thread (`uia.rs:182-186`); `deadline::bound` helper creates+uses+releases everything in one closure, never shipping interfaces across threads (`deadline.rs:24-32`); `resolve_path_el` transfers one child ref per level, siblings drop (`uia.rs:848-863`).
- Agent sees: no accumulation across calls.
- Severity: — (closed).

### HANDLED — handles closed (`CloseHandle` on process/token/thread/desktop paths)
- Evidence: `main.rs:1151-1179` (process+token), `main.rs:1008-1017` (creation-time), `hidden.rs:477-480` (`pi.hThread/hProcess`, `SetThreadDesktop(original)` **before** `CloseDesktop`), `hidden.rs:581-588` (`kill_pid` closes its handle).
- Severity: — (closed).

### WILL BITE (minor) — 64-bit process assumed but never asserted; WOW64 path untested
- Evidence: `rust-toolchain.toml` + `cargo` defaults build 64-bit (no `target_arch` gate found in `src/`); 64-bit sees both 32- and 64-bit windows (handles are sign-extended `i64` throughout: `hwnd as i64`), so the main risk is the reverse (a 32-bit build truncating HWNDs). No `#[cfg(target_pointer_width)]` guard.
- Agent sees: nothing today.
- Severity: **nice**. Smallest fix: `const_assert!(size_of::<HWND>() == 8)` / build `--target x86_64-pc-windows-msvc` pinned in CI.

### THEORETICAL — DXGI `SLOTS` thread-local session lives for the worker's lifetime
- Evidence: `dxgi.rs:73-75` `thread_local! SLOTS: HashMap<(u32,u32), Slot>` — one `Session` (device+dup+context+staging) per worker thread, never explicitly closed except on `AccessLost` re-open.
- Agent sees: nothing (by design — session reuse is the perf win; 4 workers bound it).
- Severity: **nice** (only matters if outputs churn constantly; the `Dead` cooldown already bounds the failure loop).

## 7. The kill path (OpenCode restart SIGKILLs puppet mid-call)

Context: the harness restarts the service every 80–258 s; in-flight `tools/call`s die mid-operation with no graceful shutdown (`serve.rs:1213-1222` waits ≤2 s on **stdin EOF**, not on kill — a `TerminateProcess` gives no EOF handler).

### HANDLED — kill leaves no held modifiers, no moved pointer, no raised window by construction
- Evidence: there is **no** `SendInput`, no `SetCursorPos`, no `SetForegroundWindow` anywhere on the acting path — `Burst::flush` is a documented no-op (`burst.rs:2188-2196`), `sendinput_calls/sendinput_ms` are always 0, and every acting verb is `PostMessage`/UIA to a locked target or a typed refusal (`gate.rs:62-65`, `burst.rs` throughout; `SendInput` appears only in comments/tests and the dead-path name `sendinput` which refuses).
- Agent/owner sees: a kill mid-burst can leave at most a **partially applied posted-message sequence** (e.g. key-down posted, key-up not) — the target app sees an incomplete input stream, but the owner's physical keyboard/pointer/clipboard/focus are untouched.
- Severity: — (closed by architecture; this is the single most important lifecycle property and it holds).

### WILL BITE — kill mid-burst leaves a partially driven target with no resume marker
- Evidence: continue-always + per-action results exist only in-process (`burst.rs:2253+`); on kill the whole response is lost (cancel path drops it: `serve.rs:890-956`). `posted_messages` total dies with the process. The target may have received actions 0..k of N.
- Agent sees (next turn, new process): no result frame at all (service restarted) — must re-issue; re-issuing repeats actions 0..k (double-type / double-click).
- Severity: **major** (normal week at a 80–258 s restart cadence with 300 s bursts: any burst longer than the restart interval is likely to be killed).
- Smallest fix (coordination, not code): keep bursts short (<60 s) and idempotent; code-side: none cheap — a write-ahead journal is the honest fix and explicitly out of scope for this box. At minimum document "bursts are not atomic across service restarts" in the tool description.

### WILL BITE — kill mid-`hidden` run orphans the child process + private desktop
- Evidence: hidden child is spawned by the owner thread (`hidden.rs:360-393`) and reaped only by that thread's `teardown()` (`hidden.rs:454-499`); `Hidden::teardown` detaches rather than joining on timeout (`hidden.rs:187-190`); nothing reaps on process kill (no watchdog, no `JOB_OBJECT_LIMIT_KILL_ON_JOB_CLOSE` — no `CreateJobObject/AssignProcessToJobObject` in `src/`).
- Agent/owner sees: orphaned `notepad.exe` (or whatever `background:` named) on an invisible private desktop, holding its files/locks; visible in Task Manager, invisible everywhere else.
- Severity: **major** (every killed hidden-desktop burst leaks one process + one desktop until reboot/logoff).
- Smallest fix: assign the hidden child to a kill-on-close job object at spawn (`CreateJobObjectW` + `SetInformationJobObject(JobObjectExtendedLimitInformation, KILL_ON_JOB_CLOSE)` + `AssignProcessToJobObject` before resume) — ~40 lines in `hidden.rs:owner`, survives even `TerminateProcess` of the server.

### WILL BITE (minor) — kill mid-`clipboard_get` cannot corrupt (read-only), and `clipboard_set` cannot fire (refused)
- Evidence: `clipboard_set` is refused before any clipboard API (`burst.rs:3933-3944`); `clipboard_get` opens/closes the clipboard synchronously (`burst.rs:451-488` — `OpenClipboard…CloseClipboard`, needs-`CloseClipboard`-on-every-path audit: the shown slice closes at `488`; assume symmetric close on error paths — flagged for the owner seat to verify since only lines 451-488 were read).
- Agent sees: at worst a transient `OpenClipboard failed` for the next call (clipboard is a global mutex; a kill while holding it open releases it — the OS owns the lock, not the dead process).
- Severity: **minor**. Smallest fix: none (OS reclaims the clipboard lock on process death by design).

### HANDLED — kill mid-capture leaks nothing process-external
- Evidence: capture buffers are in-process `Vec<u8>`; GDI DCs/objects are per-call locals released in `grab_bits` even on error (`cap.rs:376-378`); DXGI session is in-process. A kill frees all of it via process teardown.
- Severity: — (closed).

---

## What was NOT checked (explicit gaps)

- `clipboard_get` full open/close symmetry beyond `burst.rs:451-488` (only the head of the function was read).
- `hidden.rs:604-900` window-pick/readback helpers and `screen.rs:514-730` desktop-capture tail in full (sampled, not line-by-line).
- Live behavior: nothing was built, run, or probed (per instructions). All "agent sees" strings are read from format strings, not observed output.
- `serve.rs` shutdown path under **graceful** stdin-EOF vs **kill**: only EOF (2 s drain) is in the code; kill semantics are inferred from Win32 process-teardown guarantees.
- GDI 10k-object ceiling under sustained load was reasoned from pairing discipline, not measured with Task Manager/GDIView.

## Interesting adjacent findings (one line each)

- `NORMAL_RECTS`/`STALE` wholesale-`clear()` eviction could drop another window's recovery state under churn — bounded and harmless, but LRU would be kinder.
- Hidden-desktop `spawn()` setup timeout text says "15s" while the code waits 20 s (`hidden.rs:290-303`) — stale string.
- `window:close` success-without-verify also defeats the hidden-desktop `reverted` proof if the app ignores `WM_CLOSE` (teardown then `TerminateProcess`es — evidence preserved, but the "close_posted" flag overstates grace).
