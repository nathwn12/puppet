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
npx -y puppet-mcp@4.5.0 --doctor
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
- A clipboard: `do` `clipboard_set` writes CF_UNICODETEXT (max 1_048_576 chars; over-cap sets fail typed before touching the clipboard) and `clipboard_get` reads it back truncated to 1_048_576 chars; treat `get` results as sensitive (they may be secrets) and never log them beyond the caller.
- Memory caps: `wait_for` `stable` regions refuse edges over 4096px; screenshot grabs refuse edges over 8192px; at most 8 tool calls in flight (`-32603 Server busy` past that).
- Element listing with pages: `elements` accepts `max_depth` (1..32, default 8) and `offset` (default 0); when the 200-entry cap is hit and more controls remain, `next_offset` names the next page, else null.
- Element waits: `do` `wait_for` `kind:"element"` polls for a control (`window` selector optional, `element` name:|auto_id:|path:, `state` appear|disappear|enabled|disabled) with the same timeout semantics as the pixel and window waits.
- Expand/collapse: `do` `invoke` accepts optional `state` `"expand"|"collapse"` driving only the UIA ExpandCollapse pattern (typed error without it; never Invoke), reporting `expand_state` after the action.
- Numeric set: `do` `set_value` falls back to UIA RangeValue for sliders (numeric text, range-checked, read-only honored), reporting the live value in `value`.
- Element text: `elements` with `detail:true` adds TextPattern `text` (capped at 4000 chars, `text_truncated` flag); text never participates in `fp`.
- Relative mouse: `do` `move` accepts `dx`/`dy` (signed deltas, |d|<=32767, no desktop gate) as an alternative to absolute `x`/`y`; never both.
- Held keys: `do` `key` accepts `hold_ms` (0..60000) and `repeat` (1..100, `gap_ms` between repeats) with cancellable sleeps; dry_run assembles every event without sleeping.
- Element scroll: `do` `scroll` accepts `element` (name:|auto_id:|path:) driving UIA ScrollPattern with `axis` v|h instead of the wheel (typed error without the pattern); without it, the plain wheel.
- Provenance: `puppet.py` names in `src` comments are the retired Python prototype this Rust server keeps wire- and behavior-parity with.
- A headless mode: `do` with `desktop:"hidden"` launches one program on a private hidden desktop, drives it with best-effort posted input, captures it, and proves the teardown — see [docs/HEADLESS.md](docs/HEADLESS.md).

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
npx -y puppet-mcp@4.5.0 --doctor
```

## Requirements

- Rust 1.85.0 (`rust-toolchain.toml` pins channel 1.85.0 with rustfmt + clippy; `Cargo.toml` rust-version 1.85).
- Node 22.14 or newer (package engines, checked with `node --version`).
- Windows x64. Other platforms are refused unless you set the diagnostics override named in `--help`.
- The first serve run (no args) downloads the pinned server binary from the tagged GitHub Release, sha256-verified before and after extraction (docs/PUBLISHING.md).

## Links

- Repo: https://github.com/nathwn12/puppet
- Package: https://www.npmjs.com/package/puppet-mcp
- Releases: https://github.com/nathwn12/puppet/releases
- Issues: https://github.com/nathwn12/puppet/issues
- Docs: [publishing](docs/PUBLISHING.md), [headless desktop](docs/HEADLESS.md)
- License: MIT, see [LICENSE](LICENSE)

<!-- nathwn12 house footer: identical in every repo. Do not edit. -->
<p align="center">
  <a href="https://github.com/nathwn12"><img src="assets/signature.svg" width="96" alt="nathwn12" /></a>
</p>
<p align="center">
  <sub>Built by <a href="https://github.com/nathwn12">nathwn12</a> · <a href="https://github.com/nathwn12?tab=repositories">All projects</a></sub>
</p>
