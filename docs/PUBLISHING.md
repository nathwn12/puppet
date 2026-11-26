# Publishing puppet-mcp (owner only)

`npx puppet-mcp@latest` installs the launcher in `npm/` plus the pinned
server manifest. The server runtime zip rides inside this tarball; the launcher
provisions and verifies it on first RUN — never at install. Older versions whose
tarball carried no bundle fall back to the Release URL their manifest pins.
Do NOT publish, tag, or release from any automated session: every step below
is a human act, in order.

Prerequisites: an npm account (first publish is manual, with 2FA or a
granular token), and locally npm >= 11.5.1 with Node >= 22.14.0
(the workflow asserts both; match them for the manual step).

## Steps

1. Account. Sign in to npmjs.com and create/claim the account that will
   own the package. Package name is unscoped `puppet-mcp`; it publishes
   under whoever publishes first — confirm the handle at sign-in.

2. Land the release commit, then dispatch the `release` workflow on the
   branch that holds it (Actions → release → Run workflow, `dry_run` off).
   NOBODY TYPES A VERSION: the run derives it from the commits since the last
   `v*` tag (`fix`/`perf` → patch, `feat` → minor, `!` or `BREAKING CHANGE:`
   → major, and on 0.x a major hop moves the minor) and refuses unless the
   tree already carries it. To release a specific commit, dispatch on a ref
   that contains it and put its SHA in the `commit` input: that input only
   asserts ancestry, it never picks the tree.

   The run builds the crate with the pinned toolchain (1.87.0, per
   `rust-toolchain.toml`), runs the suite, closes `## [Unreleased]` into
   `## [X.Y.Z]` and uses that section as the release body, assembles the
   server runtime bundle, and fills `engine-manifest.json` with hashes
   computed from the REAL artifacts. The tag is created by the publish job
   immediately before `npm publish`; no GitHub Release page is created or
   amended — so nothing exists until the whole gate is green. Its `npm publish`
   step FAILS on the very first release (no trusted publisher yet) — expected,
   not a problem. Nothing
   reaches npm.

   A `v*` tag push still runs the workflow, and is now only for re-running an
   existing release: the tag must name the version the history derives.

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
   next version (checklist below) and dispatch the workflow with `dry_run`
   off. The publish job then publishes over OIDC with automatic provenance —
   no `--provenance` flag, no token. After that no long-lived tokens are
   stored anywhere. If the window lapses, the trust configuration
   expires: reconfigure and retry. Note the manually published version
   cannot be re-published by the workflow, which is why the first trusted
   publish rides the NEXT version.

6. Ongoing. Every release dispatch (and every `v*` tag push) creates one tag
   and one npm publish — no GitHub Release page — always from the same derived
   version and commit. Never `npm publish` by hand again, and never publish
   anything but a workflow-built `npm-package` artifact.

## Version checklist (one version everywhere)

The run derives `X.Y.Z` and then ASSERTS it: it never writes a version into
the manifests, because a tree that disagrees with its own history is a defect
to land, not a number for CI to guess. So the release commit must already
carry, on that commit:

- `Cargo.toml` version `X.Y.Z`, and the same in the `Cargo.lock` crate stanza
  (a `--locked` build would otherwise ship a stale version);
- `npm/package.json` version `X.Y.Z`, with both global-refusal guard hints
  updated to `npx puppet-mcp@X.Y.Z`;
- `npm/engine-manifest.json` `serverVersion` `X.Y.Z`, with `runtime.source`
  and `runtime.archive.name` naming `vX.Y.Z` (the workflow fills the hashes);
- at least one `- ` entry under `## [Unreleased]` in `CHANGELOG.md` (the run
  closes it into `## [X.Y.Z]` after the test; a section for `X.Y.Z` that
  already exists satisfies this on a rerun).

Any mismatch refuses the run in seconds. `node scripts/derive-release.mjs
--dry-run` performs the same derivation and the same checks locally and
prints the version it would release and the base tag it derived it from.

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

## Local release preflight (no publish)

Before landing a release commit, in this order (all three are read-only):

1. `node scripts/derive-release.mjs --dry-run` — derives the version from the
   commits since the last `v*` tag, prints the base tag and the per-commit
   hops, and refuses on a non-conventional subject, a release with no
   `feat`/`fix`/`perf`/breaking commit, an empty `## [Unreleased]`, or a
   manifest that does not already carry the derived version.
2. `node scripts/check-release-workflow.mjs` — asserts the release lane's own
   shape: derive before every version assert, no step that writes a version,
   the changelog closed after the test, the tag created in the publish job
   immediately before `npm publish`, no GitHub Release page, and no `version`
   input.
3. `node scripts/release-check.mjs` — packs `npm/` exactly as npm would,
   extracts `engine-manifest.json` from the tarball with Node's `zlib` plus a
   minimal tar reader (no new dependency), and asserts `serverVersion` equals
   `npm/package.json`, zero `RELEASE_WORKFLOW_FILLS_THIS` placeholders (pass
   `--allow-placeholders` on a dev tree where the workflow has not filled
   hashes yet), and no `.exe`, `.env`, `*secret*`, or `node_modules` in the
   tarball. It prints a PASS/FAIL table and exits non-zero on any failure.
4. `node scripts/derive-release.mjs --check` — the derivation's own fixture
   suite (hops, refusals, changelog close); exit 0 only when every case
   passes.
