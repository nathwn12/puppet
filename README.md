<p align="center">
  <img src="assets/icon.svg" alt="puppet" width="120" height="120">
</p>
<h1 align="center">puppet</h1>
<p align="center"><strong>Control the Windows desktop from any MCP client - screenshot, window, input, cursor - Rust.</strong></p>
<p align="center">
  <a href="LICENSE"><img src="https://img.shields.io/github/license/nathwn12/puppet" alt="License" /></a>
  <img src="https://img.shields.io/github/languages/top/nathwn12/puppet" alt="Language" />
  <img src="https://img.shields.io/badge/platform-windows--64bit-2b3137?logo=windows&logoColor=white" alt="Platform" />
</p>

## What it is

A native-Rust Windows desktop-control MCP server speaking stdio (JSON-RPC 2.0
over stdin/stdout). It exposes exactly 6 tools — `screenshot`, `list_windows`,
`window`, `do`, `cursor`, `elements` — and reports MCP
`serverInfo { name: "puppet", version: "4.3.0" }`. `stdout` carries protocol
frames only; logs go to `stderr`. It ships as a single binary
(`puppet.exe`) with no Python runtime or venv.

## Why

A Rust port of the Python incumbent, so there is no interpreter to install and
capture runs on the native DXGI path with a GDI fallback. Self-measured on the
author's machine: desktop capture takes 15.82 ms default / 19.78 ms normalized
against the Python incumbent's 35.78 / 35.99 ms; parity with the incumbent holds
at 8/8 core, 60/60 do-parity, 9/9 and 18/18 invoke resolution; and a
self-measured head-to-head bench against a 10-server field ranked it 2/10.
These figures are self-measured on one machine, not an external benchmark.

## Tools

| Tool | One line |
| --- | --- |
| screenshot | Capture the desktop (default or normalized) and return it as an image frame. |
| list_windows | List visible top-level windows as JSON. |
| window | Inspect or act on a single window by handle or title match. |
| do | Send input / window operations (mouse, keyboard, window ops), including `set_value` text writes via UIA ValuePattern (no click, no focus change; value never echoed), and the opt-in `desktop:"hidden"` mode that launches a target on a private desktop, drives it through the PostMessage path, captures it with PrintWindow, then tears the desktop down — the owner's cursor is never moved and foreground/focus stay untouched, except that an opt-in `focus_guard:true` burst re-foregrounds the pre-burst window when it observes a focus change (`src/burst.rs`). |
| cursor | Report the current cursor position and monitor rects as JSON. |
| elements | List structured UI elements for a window as JSON (zero-pixel). |

## Requirements

- Windows 10/11, x86-64 only. `puppet` is Windows-only by nature: it drives the
  Win32 desktop (DXGI/GDI capture, SendInput, UIA), so there is no macOS or
  Linux build and WSL cannot substitute for it. Other platforms are not
  supported — GUI automation here means the Windows desktop API surface, which
  does not exist elsewhere.
- No interpreter or VM to install: no Python, no venv, no browser, no Node. One
  binary (`puppet.exe`); the Rust toolchain is needed only to build from source.
  It links the MSVC runtime (`VCRUNTIME140.dll`), so the Microsoft Visual C++
  Redistributable is required — it is not a self-contained static build.

## Install

1. Download `puppet-<version>-windows-x86_64.zip` from the [GitHub Releases](https://github.com/nathwn12/puppet/releases) page and unzip it anywhere (it contains `puppet.exe`, `LICENSE`, `README.md`).
2. Point your MCP client at it (pick your client). Replace `C:\\tools\\puppet\\puppet.exe` with the folder holding `puppet.exe`:

OpenCode V2 (`opencode.json`):

```jsonc
{
  "mcp": {
    "servers": {
      "puppet": {
        "type": "local",
        "command": ["C:\\tools\\puppet\\puppet.exe"]
      }
    }
  }
}
```

Claude Desktop (`claude_desktop_config.json`):

```jsonc
{
  "mcpServers": {
    "puppet": {
      "command": "C:\\tools\\puppet\\puppet.exe",
      "args": []
    }
  }
}
```

3. Restart the client. Building from source (below) is the alternative.

## Usage

Wire the binary into your MCP client config as in Install above and start the
client — the client spawns the server over stdio, so there is nothing else to
launch by hand. Manual run (the same binary the client would spawn):

```text
target\release\puppet.exe
```

Transport: stdio MCP, newline-delimited JSON-RPC 2.0. `stdout` carries protocol
frames only; logs go to `stderr`.

## Build from source

Requires a Rust toolchain (stable) on Windows. `--locked` builds against the
committed `Cargo.lock` for a reproducible dependency set:

```text
cargo build --release --locked
```

The binary lands at `target\release\puppet.exe`; configure your client as in
Install above.

## Test

```text
cargo test --locked
```

The suite is 50 unit and live-fixture tests; CI gates on `cargo build --locked`
and `cargo test --locked` (`.github/workflows/ci.yml`).

## Status

**Unverified** — as-is, use at your own risk.
The live-input legs (`SendInput` and UIA activation against a live third-party application) are still unexercised: no real pointer, keyboard input, or focus change has ever been sent. Proven so far only against a self-owned offscreen fixture (a test-process window at -4000/-4000 shown with `SW_SHOWNOACTIVATE`, never activated): the UIA walk enumerated an Edit, a read-only Edit, and an OK button with populated `ref` values, and `set_value` wrote a value read back via `GetWindowTextW`.

## License

MIT — see [LICENSE](LICENSE).

<!-- nathwn12 house footer: identical in every repo. Do not edit. -->
<p align="center">
  <a href="https://github.com/nathwn12"><img src="assets/signature.svg" width="96" alt="nathwn12" /></a>
</p>
<p align="center">
  <sub>Built by <a href="https://github.com/nathwn12">nathwn12</a> · <a href="https://github.com/nathwn12?tab=repositories">All projects</a></sub>
</p>
