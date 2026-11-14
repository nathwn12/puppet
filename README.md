<p align="center">
  <img src="assets/icon.svg" width="120" height="120" alt="puppet icon" />
</p>
<h1 align="center">puppet</h1>
<p align="center"><strong>Screenshot, list, and drive Windows windows over MCP stdio without stealing focus, moving the pointer, or writing the clipboard.</strong></p>

## Install

Needs Windows 10/11 x64 with Node 22.14 or newer. Add the server to the OpenCode config, then save; OpenCode reloads config on save, no restart:

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

Confirm the launcher and its pin:

```powershell
npx -y puppet-mcp@latest --doctor
```

The first real call provisions the pinned server binary from the tagged GitHub Release, sha256-verified before and after extraction.

## Usage

Ask the agent to list the visible windows. Expected output: one entry per window with `hwnd`, `pid`, `exe`, `title`, and rects.

The server runs at its own integrity level. Input aimed at an elevated window is refused with a typed error instead of failing silently.

## Configuration

Every tool, option, and environment variable: [docs/GUIDE.md](docs/GUIDE.md).

## Development

```sh
cargo check --locked
cargo test --locked
```

`cargo fmt --all --check` and `cargo clippy --all-targets --locked -- -D warnings` are the static gate.

## Status

**Active** — maintained, issues welcome.

## Links

- [Guide](docs/GUIDE.md)
- [Releases](https://github.com/nathwn12/puppet/releases)
- [Third-party notices](THIRD-PARTY-NOTICES.md)
- [MIT](LICENSE)

<!-- nathwn12 house footer: identical in every repo. Do not edit. -->
<p align="center">
  <a href="https://github.com/nathwn12"><img src="assets/signature.svg" width="96" alt="nathwn12" /></a>
</p>
<p align="center">
  <sub>Built by <a href="https://github.com/nathwn12">nathwn12</a> · <a href="https://github.com/nathwn12?tab=repositories">All projects</a></sub>
</p>
