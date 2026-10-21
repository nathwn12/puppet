# Publishing puppet-mcp (owner only)

`npx puppet-mcp@latest` installs the launcher in `npm/` plus the pinned
server manifest. The server binary is fetched on first RUN from the tagged
GitHub Release — never at install. Do NOT publish, tag, or release from any
automated session: every step below is a human act, in order.

Prerequisites: an npm account (first publish is manual, with 2FA or a
granular token), and locally npm >= 11.5.1 with Node >= 22.14.0
(the workflow asserts both; match them for the manual step).

## Steps

1. Account. Sign in to npmjs.com and create/claim the account that will
   own the package. Package name is unscoped `puppet-mcp`; it publishes
   under whoever publishes first — confirm the handle at sign-in.

2. Push the version tag (e.g. `v4.3.0`). The `release` workflow builds the
   crate with the pinned toolchain (1.85.0, per `rust-toolchain.toml`), runs
   the suite, assembles the server runtime bundle, fills `engine-manifest.json` with hashes computed from
   the REAL artifacts, and creates the GitHub Release with the archives
   attached. Its `npm publish` step FAILS (no trusted publisher yet) —
   expected, not a problem. Nothing reaches npm.

3. FIRST publish (manual — this is what makes the package exist).
   Download the run's `npm-package` artifact, unpack it, and from your own
   machine run `npm publish` on that directory exactly as built (it holds
   the real release hashes; the committed manifest holds placeholders the
   launcher refuses to run with). Verify: `npm view puppet-mcp version`
   and `npx puppet-mcp@latest --doctor`.

4. Trusted publisher (same CLI route verified for the sibling `gladius-mcp`
   package). From your own machine, in an interactive shell (it needs a
   one-time password: npm prints a web-auth URL for a person's approval in
   the browser), run exactly:

   ```powershell
   npm trust github puppet-mcp --file release.yml --repo nathwn12/puppet --allow-publish --allow-stage-publish
   ```

   This works even before a first publish (no existing version required).
   Fallback: the package's Settings → Trusted Publisher → GitHub Actions
   panel with exactly Organization or user `nathwn12` (required),
   Repository `puppet` (required), Workflow filename `release.yml`
   (required — filename only, exact case; it exists in
   `.github/workflows/`), optionally an Environment and an allowed-actions
   choice (`npm stage publish` is always allowed).

5. Within 2 DAYS of configuring, land the first workflow publish: cut the
   next version (checklist below) and push its `v*` tag. The publish job
   then publishes over OIDC with automatic provenance — no
   `--provenance` flag, no token. After that no long-lived tokens are
   stored anywhere. If the window lapses, the trust configuration
   expires: reconfigure and retry. Note the manually published version
   cannot be re-published by the workflow, which is why the first trusted
   publish rides the NEXT version tag.

6. Ongoing. Every `v*` tag push is one GitHub Release plus one npm
   publish, always from the same tag and commit. Never `npm publish` by
   hand again, and never publish anything but a workflow-built
   `npm-package` artifact.

## Version checklist (one version everywhere)

A release tag `vX.Y.Z` requires, on that commit: `Cargo.toml` version
`X.Y.Z`, `npm/package.json` version `X.Y.Z`, `npm/engine-manifest.json`
`serverVersion` `X.Y.Z` with matching `runtime.source`/`runtime.archive`
names (the workflow asserts all equal the tag and fills the hashes), and a
`CHANGELOG.md` entry (curated by hand; not workflow-checked).

## Why these exact values

- `repository.url` in `npm/package.json` must match the GitHub repo
  (`https://github.com/nathwn12/puppet`) exactly — trusted
  publishing/provenance requires it.
- `npm/package.json` holds ONLY the `preinstall`/`postinstall`
  global-refusal guards (the workflow asserts exactly these two and
  nothing else): an install-time download would silently never run
  under npx, so the server is acquired on first RUN by the launcher instead.
- Publish runs on a GitHub-hosted runner with `id-token: write`
  (OIDC requirement). Self-hosted runners are not trusted publishers.

## No monthly bump workflow

The sibling's `browser-update` workflow exists only because gladius pins an
external Chromium build that drifts monthly. puppet pins no external
runtime — the only versioned artifact is this repo's own binary — so there
is no sibling to mirror and nothing scheduled to bump.
