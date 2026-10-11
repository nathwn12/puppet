<p align="center"><img src="assets/icon.svg" width="96" alt="puppet"></p>

<h1 align="center">puppet</h1>

<p align="center">Native Rust Windows desktop-control MCP server — stdio in, pixels out.</p>

<p align="center">
  <a href="LICENSE"><img src="https://img.shields.io/badge/License-MIT-yellow.svg" alt="license: MIT"></a>
  <img src="https://img.shields.io/badge/language-Rust-orange.svg" alt="language: Rust">
  <img src="https://img.shields.io/badge/platform-Windows-blue.svg" alt="platform: Windows">
  <a href="https://github.com/nathwn12/puppet/actions/workflows/ci.yml"><img src="https://github.com/nathwn12/puppet/actions/workflows/ci.yml/badge.svg" alt="CI status"></a>
</p>

## What it is

A native-Rust Windows desktop-control MCP server speaking stdio
(JSON-RPC 2.0 over stdin/stdout). It exposes exactly 5 tools — `screenshot`,
`list_windows`, `window`, `do`, `cursor` — and reports MCP
`serverInfo { name: puppet, version: 4.1.0 }`.

Transport: stdio MCP, newline-delimited JSON-RPC 2.0. `stdout` carries
protocol frames only; logs go to `stderr`.

## Why

A Rust port of the Python incumbent — no Python runtime and no venv at
runtime. Single static binary, fast desktop capture, same 5-tool contract.

- Native Rust, no Python or venv at runtime.
- Single static binary (`puppet.exe`).
- Fast desktop capture via the DXGI desktop-capture path.
- Same 5-tool contract as the Python incumbent.

## Tools

| Tool | One line |
| --- | --- |
| screenshot | Capture the desktop (default or normalized) and return it as an image frame. |
| list_windows | List visible top-level windows as JSON. |
| window | Inspect or act on a single window by handle or title match. |
| do | Send input / window operations (mouse, keyboard, window ops). |
| cursor | Report the current cursor position and monitor rects as JSON. |

## Requirements

- Windows (Win32 APIs: GDI/DXGI, UIA, SendInput). No Python required at runtime.

## Install

A prebuilt Windows binary (`puppet.exe`) is attached to the
[GitHub Releases](https://github.com/nathwn12/puppet/releases) page.
Download it, then point your MCP client at it. Building from source (below)
is the alternative.

```jsonc
{
  "mcp": {
    "puppet": {
      "type": "local",
      "command": ["<path-to>\\puppet.exe"]
    }
  }
}
```

Replace `<path-to>` with the folder holding `puppet.exe`.

## Usage

Wire the binary into your MCP client config and start the client. The server
speaks stdio (JSON-RPC 2.0 over stdin/stdout), so there is nothing else to
launch by hand — the client spawns it.

```jsonc
{
  "mcp": {
    "puppet": {
      "type": "local",
      "command": ["<path-to>\\puppet.exe"]
    }
  }
}
```

Manual run (same binary the client would spawn):

```text
target\release\puppet.exe
```

- Transport: stdio MCP, newline-delimited JSON-RPC 2.0.
- `stdout` carries protocol frames only; logs go to `stderr`.

## Build from source

Requires a Rust toolchain (stable) on Windows.

```text
cargo build --release
```

The binary lands at `target\release\puppet.exe`, then configure your client
as in Install above.

## Measured performance

Numbers below were measured on the author's machine, not in CI.

- Desktop capture: 15.82 ms default / 19.78 ms normalized, versus the Python
  incumbent at 35.78 / 35.99 ms.
- Head-to-head bench against a 10-server field: ranked 2/10.
- Parity against the Python incumbent: 8/8 core, 60/60 do-parity, 9/9, 18/18
  invoke resolution.

## Unverified

- The real SendInput and UIA activation legs are UNVERIFIED — they were never
  run because the owner's per-test permission was never granted.

## License

MIT — see LICENSE.
