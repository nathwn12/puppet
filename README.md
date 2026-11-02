# puppet

Native-Rust MCP server over stdio that screenshots, lists and drives Windows windows — background-safe by default, without taking focus, pointer or keyboard.

## Install

Add the server to the OpenCode config — `~/.config/opencode/opencode.jsonc` for every project, or `./opencode.jsonc` for one — and save; OpenCode reloads config on save, no restart:

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

Then confirm the launcher and its pin — prints the manifest and the resolved engine cache path, provisions nothing:

```powershell
npx -y puppet-mcp@latest --doctor
```

The first real call provisions the pinned server binary from the tagged GitHub Release, sha256-verified before and after extraction.

## What it does

| Tool | Behavior |
|---|---|
| `screenshot` | Captures the whole desktop, one monitor, a region, or one window — without activating it — and returns the image plus the map from image pixels back to screen pixels. `monitors.rects` lists every monitor, and a window that moves mid-capture is flagged instead of being mapped with a stale origin. A window whose UI thread stops answering returns the typed `capture_timeout` inside 10 s. |
| `list_windows` | Read-only list of every visible top-level window with `hwnd`, `pid`, `exe`, `title`, rects, DPI and whether it runs elevated: the starting point for any selector. Pass `include_app_ids:true` to add each window's AppUserModelID (`appId`, `null` when absent). |
| `window` | Focus, move, resize, minimize, maximize, restore, close or topmost a window by selector; every verb except an explicit `focus` is no-steal. |
| `do` | Runs an ordered batch of input actions — click, drag, scroll, type, keys, waits, UIA `invoke`/`set_value`, clipboard — against one locked target in a single round-trip. Reports a call-level `ok`/`failed` verdict: a burst where nothing succeeded is a tool error, one bad action among good ones is not. One burst is capped at 256 actions and 300 s total; exceeding either returns a typed `burst_budget_exceeded`. A UIA `invoke`/`set_value` that does not return within 5 s fails that action with the typed `uia_timeout` instead of holding the input lock. |
| `cursor` | Read-only cursor position plus monitor rectangles. |
| `elements` | Read-only structured UIA control tree for a window, with stable `ref`/`fp` identities, paging and an `offscreen` flag for controls with no on-screen rectangle. The walk is bounded at 5 s: a provider that stops answering returns `uia_timeout` on `uia_error` and the Win32 fallback still fills the page. |

Every acting call takes one `target` selector and locks exactly one window; there is no fallback to the foreground window, ever.

## Requirements

Windows 10/11 x64 with Node 22.14 or newer — the server runs at its own integrity level, so input aimed at an elevated window is refused with a typed error rather than failing silently, and nothing takes focus, raises a window or moves the pointer unless a call explicitly passes `allow_foreground` / `allow_cursor`.

## Development

```sh
cargo test
cargo build
```

## Guide

The manual — provisioning model, every option and environment variable, every tool and argument, the `do` burst, troubleshooting and limitations: [docs/GUIDE.md](docs/GUIDE.md).

## License

MIT. See [LICENSE](LICENSE).
