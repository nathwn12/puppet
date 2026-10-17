# puppet

**Give your coding agent eyes and hands for your Windows desktop.**

## Install

```powershell
npx -y puppet-mcp@latest --doctor
```

You need Node 22.14 or newer on Windows 10/11 x64 first. The first run provisions the pinned server binary from the tagged GitHub Release, sha256-verified, then `--doctor` prints the manifest with the resolved cache path and exits 0.

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
npx -y puppet-mcp@4.4.0 --doctor
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

The names above come from `puppet-mcp --help`. There is no fully offline fresh install: the first run still downloads the pinned binary, so treat these as pins and overrides, not as a no-network setup.

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
- A small local footprint: one native binary over stdio; the launcher provisions it on first run and re-verifies the cache each time.
- Input that never asks first: `do` drives the real desktop, so preview a risky burst with `dry_run`; input aimed at an elevated window is refused with a typed error instead of failing silently.

## Requirements

- Node 22.14 or newer (package engines, checked with `node --version`).
- Windows x64. Other platforms are refused unless you set the diagnostics override named in `--help`.
- The first run downloads the pinned server binary from the tagged GitHub Release, sha256-verified before and after extraction (docs/PUBLISHING.md).

## Links

- Repo: https://github.com/nathwn12/puppet
- Package: https://www.npmjs.com/package/puppet-mcp
- Releases: https://github.com/nathwn12/puppet/releases
- Issues: https://github.com/nathwn12/puppet/issues
- License: MIT, see [LICENSE](LICENSE)
- Docs: [publishing](docs/PUBLISHING.md)
