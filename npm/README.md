# puppet-mcp

Windows desktop-control MCP server: a native Rust binary served over stdio.
This package is a dependency-free Node launcher plus a pinned server
manifest — it contains no server bytes.

## Run

```sh
npx puppet-mcp@latest
```

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
npx puppet-mcp@latest --doctor
```

Prints the pinned manifest and the resolved cache path, exits 0, performs
no fetch. `--print-manifest` is an alias.

## Requirements

- Windows x86_64. Any other platform refuses at startup
  (override for diagnostics only: `PUPPET_MCP_ALLOW_NON_WINDOWS=1`).
- Node.js >= 22.14.0 (global `fetch` plus `crypto`/`zlib` builtins only;
  zero dependencies).

## License

MIT — see LICENSE.
