<!-- Header. Text banner per this repo's ART.md verdict; the framed banner replaces the icon image - never both. -->
```text
+---------------------------------------+
|                                 __    |
| .-----.--.--.-----.-----.-----.|  |_  |
| |  _  |  |  |  _  |  _  |  -__||   _| |
| |   __|_____|   __|   __|_____||____| |
| |__|        |__|  |__|                |
+---------------------------------------+
```
<h1 align="center">puppet</h1>
<p align="center"><strong>Control the Windows desktop from any MCP client - screenshot, window, input, cursor - Rust.</strong></p>
<p align="center">
  <a href="LICENSE"><img src="https://img.shields.io/github/license/nathwn12/puppet" alt="License" /></a>
  <img src="https://img.shields.io/github/languages/top/nathwn12/puppet" alt="Language" />
  <img src="https://img.shields.io/badge/platform-windows--64bit-2b3137?logo=windows&logoColor=white" alt="Platform" />
  <a href="https://github.com/nathwn12/puppet/actions/workflows/ci.yml"><img src="https://github.com/nathwn12/puppet/actions/workflows/ci.yml/badge.svg" alt="CI" /></a>
</p>

## What it is

A native-Rust Windows desktop-control MCP server speaking stdio (JSON-RPC 2.0 over stdin/stdout). It exposes exactly 5 tools — `screenshot`, `list_windows`, `window`, `do`, `cursor` — and reports MCP `serverInfo { name: puppet, version: 4.1.0 }`. `stdout` carries protocol frames only; logs go to `stderr`. It ships as a single static binary (`puppet.exe`) with no Python runtime or venv.

## Why

A Rust port of the Python incumbent, so there is no interpreter to install and capture runs on the native DXGI path with a GDI fallback. Measured on the author's machine, desktop capture takes 15.82 ms default / 19.78 ms normalized against the Python incumbent's 35.78 / 35.99 ms, parity with the incumbent holds at 8/8 core, 60/60 do-parity, 9/9 and 18/18 invoke resolution, and a head-to-head bench against a 10-server field ranked it 2/10.

## Tools

| Tool | One line |
| --- | --- |
| screenshot | Capture the desktop (default or normalized) and return it as an image frame. |
| list_windows | List visible top-level windows as JSON. |
| window | Inspect or act on a single window by handle or title match. |
| do | Send input / window operations (mouse, keyboard, window ops). |
| cursor | Report the current cursor position and monitor rects as JSON. |

## Install

1. Download `puppet.exe` from the [GitHub Releases](https://github.com/nathwn12/puppet/releases) page.
2. Point your MCP client at it:

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

3. Replace `<path-to>` with the folder holding `puppet.exe`, then restart the client. Building from source (below) is the alternative.

## Usage

Wire the binary into your MCP client config as in Install above and start the client — the client spawns the server over stdio, so there is nothing else to launch by hand. Manual run (the same binary the client would spawn):

```text
target\release\puppet.exe
```

Transport: stdio MCP, newline-delimited JSON-RPC 2.0. `stdout` carries protocol frames only; logs go to `stderr`.

## Build from source

Requires a Rust toolchain (stable) on Windows.

```text
cargo build --release
```

The binary lands at `target\release\puppet.exe`; configure your client as in Install above.

## Status

**Unverified** — as-is, use at your own risk.
The real SendInput and UIA activation legs were never run because per-test owner permission was never granted.

## License

MIT — see [LICENSE](LICENSE).

<!-- nathwn12 house footer: identical in every repo. Do not edit. -->
<p align="center">
  <a href="https://github.com/nathwn12"><img src="assets/signature.svg" width="96" alt="nathwn12" /></a>
</p>
<p align="center">
  <sub>Built by <a href="https://github.com/nathwn12">nathwn12</a> · <a href="https://github.com/nathwn12?tab=repositories">All projects</a> · MIT</sub>
</p>
