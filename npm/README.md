<h1 align="center">puppet-mcp</h1>
<p align="center"><strong>A Windows desktop-control MCP server served over stdio by a pinned native engine.</strong></p>

<p align="center">
  <a href="https://www.npmjs.com/package/puppet-mcp"><img src="https://img.shields.io/npm/v/puppet-mcp" alt="npm version" /></a>
  <img src="https://img.shields.io/badge/platform-windows%20x64-blue" alt="Windows x64" />
  <img src="https://img.shields.io/badge/license-MIT-green" alt="MIT licence" />
</p>

## Install

```sh
npx puppet-mcp@latest
```

Global installs are refused by design (`npm i -g` exits non-zero): a global copy would shadow the `npx` invocation and run a stale engine. Always invoke it through `npx` as above.

Or pin it in an MCP client as a stdio server with command and args:

```json
["cmd", "/c", "npx", "-y", "puppet-mcp@latest"]
```

First run provisions the pinned server from the runtime zip bundled inside this npm tarball (sha256- and size-verified before extraction, the extracted server verified after) into a per-user engine cache, then spawns it with stdio passthrough. npm is the release: no GitHub Release page exists. Versions published before the tarball carried the zip fall back to the Release URL their manifest pinned. The cache is reused on later runs (re-verified each time); any mismatch refuses and writes nothing. All launcher chatter goes to stderr; stdout belongs to the MCP channel.

## Use it

`npx puppet-mcp@latest --doctor` prints the launcher version, the pinned manifest, the resolved engine path, the engine version parsed from the cache directory name, and whether they match; exits 0, performs no fetch. `--print-manifest` is an alias. `npx puppet-mcp@latest --version` prints the package name and the version it is running.

This package is a dependency-free Node launcher plus a pinned server manifest and the runtime zip it provisions from: npm is the release, so no GitHub Release page is needed. Use `npx puppet-mcp@latest`; a pinned version stays usable because its runtime ships inside that version's tarball. Fresh installs are fully offline — the first serve run provisions from the bundled zip with no network fetch. Redirect the cache with `PUPPET_ENGINE_CACHE`, or inspect the pin with no network access via `--doctor` (`--print-manifest` is an alias), which performs no fetch and exits 0.

## Requirements

- Windows x86_64. Any other platform refuses at startup (override for diagnostics only: `PUPPET_MCP_ALLOW_NON_WINDOWS=1`).
- Node.js >= 22.14.0 (global `fetch` plus `crypto`/`zlib` builtins only; zero dependencies).

## Status

**Active** — maintained, issues welcome.

## Links

- [Issues](https://github.com/nathwn12/puppet/issues)
- [Version tags](https://github.com/nathwn12/puppet/tags) (history markers only; npm is the release)
- [npm package](https://www.npmjs.com/package/puppet-mcp)
- [MIT licence](LICENSE)
