# puppet-mcp

Windows desktop-control MCP server: a native Rust binary served over stdio.
This package is a dependency-free Node launcher plus a pinned server
manifest — it contains no server bytes.

## Run

```sh
npx puppet-mcp@4.5.3
```

Global installs are refused by design (`npm i -g` exits non-zero): a
global copy would shadow the pinned `npx` invocation and run a stale
engine. Always invoke the pinned version above.

Or pin it in an MCP client as a stdio server with command and args:

```json
["cmd", "/c", "npx", "-y", "puppet-mcp@latest"]
```

First run provisions the pinned server from the tagged GitHub Release into
`%LOCALAPPDATA%\puppet\engine\<serverVersion>-<bundleSha8>\`
(sha256-verified before and after extraction), then spawns it with stdio
passthrough. The cache is reused on later runs (re-verified each time); any
mismatch refuses and writes nothing. All launcher chatter goes to stderr;
stdout belongs to the MCP channel.

## Diagnostics (no network)

```sh
npx puppet-mcp@4.5.3 --doctor
```

Prints the launcher version, the pinned manifest, the resolved engine
path, the engine version parsed from the cache directory name, and
whether they match; exits 0, performs no fetch. `--print-manifest` is
an alias. `npx puppet-mcp@4.5.3 --version` prints `puppet-mcp 4.5.3`.

## Requirements

- Windows x86_64. Any other platform refuses at startup
  (override for diagnostics only: `PUPPET_MCP_ALLOW_NON_WINDOWS=1`).
- Node.js >= 22.14.0 (global `fetch` plus `crypto`/`zlib` builtins only;
  zero dependencies).

## Overrides

Use `npx puppet-mcp@latest`; a pinned version is only installable while its
GitHub release exists; the one-release policy deletes older releases.

Redirect the cache with `PUPPET_ENGINE_CACHE`, or inspect the pin with no
network access (`--print-manifest` is an alias for `--doctor` above).
There is no fully offline fresh install: the first serve run still downloads
the pinned binary, so treat these as pins and overrides, not as a no-network
setup.

## License

MIT — see LICENSE.
