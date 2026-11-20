<p align="center">
  <img src="assets/icon.svg" width="120" height="120" alt="puppet icon" />
</p>

<h1 align="center">puppet</h1>

<p align="center"><strong>Screenshot, list, and drive Windows windows over MCP stdio without stealing focus, moving the pointer, or writing the clipboard.</strong></p>

<p align="center">
  <a href="LICENSE"><img src="https://img.shields.io/github/license/nathwn12/puppet" alt="License" /></a>
  <img src="https://img.shields.io/github/languages/top/nathwn12/puppet" alt="Language" />
  <img src="https://img.shields.io/badge/platform-windows%20x64-blue" alt="Platform" />
  <img src="https://img.shields.io/badge/tools-7-brightgreen" alt="7 tools" />
</p>

One stdio connection. One native binary. No focus steal, no cursor move, no clipboard write.

## Why puppet

Most agent tooling for Windows takes the foreground, moves the pointer, or overwrites the clipboard while an agent works. puppet refuses those by policy.

- **No pointer steal.** Acting verbs post to one locked window. No global mouse, no focus change, no clipboard write. The machine stays usable while the agent works.
- **Evidence, not vibes.** Every capture reports whether the foreground window and cursor stayed put, and whether the frame was blank or the window moved during capture. Stale element refs fail typed instead of acting on the wrong control.
- **Refusals that say why.** Typed error codes name the exact refusal: `uipi_blocked`, `window_minimized`, `no_foreground_policy`, `no_clipboard_policy`, `stale_ref`, and more.
- **One verified binary.** The launcher provisions the pinned server from the tagged GitHub Release and verifies its sha256 twice before use. It installs nothing system-wide.

## Install

Confirm the launcher and its pin first:

```powershell
npx -y puppet-mcp@latest --doctor
```

That command performs no fetch, checks the pinned manifest and the resolved engine path, and exits 0. Then add the server to your MCP client. OpenCode reloads the config on save, so no restart:

```jsonc
{
  "$schema": "https://opencode.ai/config.json",
  "mcp": {
    "servers": {
      "puppet": {
        "type": "local",
        "command": ["cmd", "/c", "npx", "-y", "puppet-mcp@latest"]
      }
    }
  }
}
```

The client spawns npx, which runs the bundled binary and speaks newline-delimited JSON-RPC 2.0 on stdio. stdout carries protocol frames only; diagnostics go to stderr. A global install is refused on purpose; the server belongs in the client's process tree. The first real call provisions the pinned binary from the tagged GitHub Release, sha256-verified.

## Use it

Ask the agent to list the visible windows. Each entry carries `hwnd`, `pid`, `exe`, `title`, screen-space rects and the elevation state.

```js
const windows = await list_windows();
await screenshot({ target: { hwnd: windows[0].hwnd } });
await do({
  target: { hwnd: windows[0].hwnd },
  actions: [{ type: "invoke", element: "Install", expect_fp: "..." }]
});
```

Acting tools lock one target and refuse to fall back to the foreground window. Input aimed at an elevated window is refused with `uipi_blocked`, and a minimized window returns `window_minimized` rather than being restored behind your back. Every argument and typed error is in [docs/GUIDE.md](docs/GUIDE.md).

## Tools

Seven tools, each with an input and output schema. The count is taken from the seven `TOOL_*` schemas registered by `src/serve.rs`.

- `screenshot` — the whole virtual desktop, one monitor, one window or a region. No activation, no cursor in the pixels. `elements:true` adds a structured element block.
- `diff` — pixel diff between two captures or a live target, with changed count, ratio, bounding box and an optional diff image.
- `list_windows` — visible top-level windows with hwnd, pid, exe, title, rects, DPI and elevation state. Read-only.
- `window` — move, resize, minimize, restore, close or pin a window. Focus and maximize are refused.
- `do` — an ordered burst of up to 256 actions against one locked target. UIA patterns first, then window messages. Includes a hidden-desktop mode.
- `cursor` — pointer position and monitor rects. Read-only.
- `elements` — structured UI elements with a stable ref and fingerprint. Zero pixels, no focus change.

## Compatibility

puppet serves any MCP client that speaks stdio. The launcher needs Node 22.14 or newer; the server is a native Windows x64 binary and runs on Windows 10/11 only.

- The same `npx` command registers the server in any stdio MCP client. OpenCode reloads the config on save.
- The `do` burst uses UIA patterns first, then posted window messages, so it works with controls that expose automation patterns and windows that accept only messages.
- UIA reads fall back to Win32 child enumeration when automation is unavailable.

## Configure it

No flag serves MCP over stdio. The launcher also takes `--doctor`, `--print-manifest`, `--version` and `--help`.

| Variable | Default | Effect |
|---|---|---|
| `PUPPET_ENGINE_CACHE` | the engine's own leaf under the per-user cache | Absolute cache directory for the pinned binary. |
| `PUPPET_UIA_TIMEOUT_MS` | `5000` | Deadline for one bounded UIA operation, clamped to 50..120000. |
| `PUPPET_CAPTURE_TIMEOUT_MS` | `10000` | Deadline for one `PrintWindow` capture, clamped to 50..120000. |

Raise the UIA deadline for a slow target. Add this to the `puppet` entry from Install:

```jsonc
"environment": { "PUPPET_UIA_TIMEOUT_MS": "15000" }
```

## Requirements

- Windows 10/11 x64. The server runtime runs on no other platform.
- Node 22.14 or newer, for the `npx` launcher.
- An MCP client that speaks stdio.
- The first run downloads the pinned server binary and verifies its sha256 twice. It installs nothing system-wide.
- The server cannot drive an elevated window, reach a UAC prompt, or restore a minimized one; each is a typed error.

## Development

```sh
cargo check --locked
cargo test --locked
```

The toolchain is pinned by `rust-toolchain.toml` (Rust 1.87). `cargo fmt --all --check` and `cargo clippy --all-targets --locked -- -D warnings` are the static gate. `PUPPET_LIVE_DESKTOP=1` enables the live-gated tests, which skip cleanly without it. The `do` burst is in [docs/GUIDE.md](docs/GUIDE.md), the hidden-desktop mode in [docs/HEADLESS.md](docs/HEADLESS.md), and the release lane in [docs/PUBLISHING.md](docs/PUBLISHING.md).

CI runs `fmt`, `clippy`, `gitleaks`, `cargo check` and the single conformance binary. A change that only touches Markdown, `docs/` or `LICEN[CS]E` skips everything but `gitleaks`.

## Status

**Active** — maintained, issues welcome.

## Links

- [Issues](https://github.com/nathwn12/puppet/issues)
- [Releases](https://github.com/nathwn12/puppet/releases)
- [Guide](docs/GUIDE.md)
- [Headless desktop](docs/HEADLESS.md)
- [Publishing](docs/PUBLISHING.md)
- [Third-party notices](THIRD-PARTY-NOTICES.md)
- [MIT](LICENSE)

Built by nathwn12
