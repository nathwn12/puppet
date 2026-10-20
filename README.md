<p align="center">
  <img src="assets/icon.svg" width="120" height="120" alt="puppet icon" />
</p>
<h1 align="center">puppet</h1>
<p align="center"><strong>Give your coding agent eyes and hands for your Windows desktop.</strong></p>

## Install

```powershell
npx -y puppet-mcp@latest --doctor
```

You need Node 22.14 or newer on Windows 10/11 x64 first. The first serve run (no args) provisions the pinned server binary from the tagged GitHub Release, sha256-verified. `--doctor` only prints the manifest with the resolved cache path and exits 0 without downloading or provisioning anything.

### Or let your AI install it

```text
Install the npm package puppet-mcp from nathwn12/puppet into my MCP client config, served with npx puppet-mcp@latest over stdio.
Then verify with npx puppet-mcp@latest --doctor and npm view puppet-mcp version and show me both outputs.
```

### Wire it yourself

Wire the server into your MCP client config:

```json
{
  "mcpServers": {
    "puppet": {
      "command": "npx",
      "args": [
        "puppet-mcp@latest"
      ]
    }
  }
}
```

### Offline or pinned

Pin an exact version instead of `@latest`:

```powershell
npx -y puppet-mcp@4.4.1 --doctor
```

Build from source when you want the working tree instead of the Release binary:

```text
git clone https://github.com/nathwn12/puppet
cargo build --release --locked
```

Redirect the cache with `PUPPET_ENGINE_CACHE`, or inspect the pin with no network access:

```powershell
npx -y puppet-mcp@latest --print-manifest
```

The names above come from `puppet-mcp --help`. There is no fully offline fresh install: the first serve run still downloads the pinned binary, so treat these as pins and overrides, not as a no-network setup.

## Use it

List what is open with the `list_windows` tool. Read-only, changes nothing:

```json
{}
```

You get back each visible top-level window with `hwnd`, `pid`, `exe`, `title`, `class`, screen `rect`, `dpi`, and whether the process runs `elevated`. See the screen with the `screenshot` tool:

```json
{
  "max_px": 1280
}
```

You get back an image frame plus a coordinate map that turns image pixels back into screen pixels. Send input with the `do` tool, previewed first:

```json
{
  "actions": [{ "type": "type", "text": "hello" }],
  "dry_run": true
}
```

You get back per-action results with `index`, `type`, `ok`, and `ms`. Remove `dry_run` to send the input for real.

## What you get

- Six tools to see and drive the desktop: `screenshot`, `list_windows`, `window`, `do`, `cursor`, and `elements` (src/schemas.rs).
- Coordinates you can trust: every screenshot carries the map that turns image pixels back into screen pixels.
- A small local footprint: one native binary over stdio; the launcher provisions it on the first serve run (no args) and re-verifies the cache each time.
- Input that never asks first: `do` drives the real desktop, so preview a risky burst with `dry_run`; input aimed at an elevated window is refused with a typed error instead of failing silently.

## Configure it

You run with no args to serve, or you pass one diagnostic flag:

- `(no args)` — provision the pinned server on the first serve run, then serve MCP over stdio. This is the default.
- `--doctor` — print the pinned manifest and the resolved cache path, then exit 0 without downloading or provisioning anything. Default off with no network access on this path.
- `--print-manifest` — same output as `--doctor`: print the pinned manifest and the resolved cache path, then exit 0 without downloading or provisioning anything. Default off with no network access on this path.
- `--help, -h` — print usage. Default off.

You override two paths with env vars:

- `PUPPET_ENGINE_CACHE` — absolute directory for the engine cache. Default: the engine cache lives under the OS local-app-data directory; `--help` prints the cache path template; the engine fills in the version and bundle when it provisions.
- `PUPPET_MCP_ALLOW_NON_WINDOWS=1` — bypass the Windows-only gate for diagnostics. Default unset, so other platforms are refused. The server still runs on Windows x64 only.

Pin an exact version with:

```powershell
npx -y puppet-mcp@4.4.1 --doctor
```

## Requirements

- Node 22.14 or newer (package engines, checked with `node --version`).
- Windows x64. Other platforms are refused unless you set the diagnostics override named in `--help`.
- The first serve run (no args) downloads the pinned server binary from the tagged GitHub Release, sha256-verified before and after extraction (docs/PUBLISHING.md).

## Links

- Repo: https://github.com/nathwn12/puppet
- Package: https://www.npmjs.com/package/puppet-mcp
- Releases: https://github.com/nathwn12/puppet/releases
- Issues: https://github.com/nathwn12/puppet/issues
- Docs: [publishing](docs/PUBLISHING.md)
- License: MIT, see [LICENSE](LICENSE)

<!-- nathwn12 house footer: identical in every repo. Do not edit. -->
<p align="center">
  <a href="https://github.com/nathwn12"><img src="assets/signature.svg" width="96" alt="nathwn12" /></a>
</p>
<p align="center">
  <sub>Built by <a href="https://github.com/nathwn12">nathwn12</a> · <a href="https://github.com/nathwn12?tab=repositories">All projects</a></sub>
</p>
