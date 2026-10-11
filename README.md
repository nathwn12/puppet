# puppet 4.1.0 — native Rust Windows desktop-control MCP server

What it is: a native-Rust Windows desktop-control MCP server speaking stdio
(JSON-RPC 2.0 over stdin/stdout). It exposes exactly 5 tools — `screenshot`,
`list_windows`, `window`, `do`, `cursor` — and reports MCP
`serverInfo { name: puppet, version: 4.1.0 }`.

Why: a Rust port of the Python incumbent — no Python runtime and no venv at
runtime. Single static binary, fast desktop capture, same 5-tool contract.

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

## Build

```text
cargo build --release
```

## Run

```text
target\release\puppet.exe
```

- Transport: stdio MCP, newline-delimited JSON-RPC 2.0.
- `stdout` carries protocol frames only; logs go to `stderr`.

## Measured

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
