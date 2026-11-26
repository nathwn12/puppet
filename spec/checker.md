# Checker contract (local adoption)

This repository vendors the canonical phase-1 gate from "docs-template" at
@793a13a verbatim. The two local files are the checker [docs-check.mjs](../docs-check.mjs)
and its configuration [docs-check.json](../docs-check.json); the gate does not edit either.
Tests live only in the canonical template: no local test/ tree was vendored,
so no local test path is implied.

## What it checks

Phase 1 implements exactly four checks: forbidden (policy facts in AGENTS.md,
with built-in patterns always applied), versions (config-driven equality,
opt-in), generated (byte-exact whole-file or marked-block comparison, opt-in),
and checker (the running script version and SHA-256 against the pin in
docs-check.json). Link checking is not in the Node tool; see
docs-structure.md for the PR workflow where one applies.

## Working commands

From the repository root:

```text
node docs-check.mjs --report               # default: findings print, exit 0
node docs-check.mjs --strict               # exit 1 when findings exist
node docs-check.mjs --report --json        # stable JSON output
node docs-check.mjs --print-hash           # print the script SHA-256, exit 0
node docs-check.mjs --config <path> --report
```

--config defaults to docs-check.json in the current directory; config paths
resolve relative to that file. Exit codes: 0 for no findings or report mode,
1 for findings in strict mode, 2 for a config or checker error.

--report and --strict are mutually exclusive. A repeated --strict, a repeated
--json, an unknown option, and --print-hash with --json, --strict or an
explicit --config are config errors exiting 2. Repeating --report or --config
is accepted; the last --config wins.

The checker hashes the script actually running and normalizes line endings,
so the targeted LF rule in .gitattributes keeps the bytes identical across
checkouts. A mismatch is a finding naming both pins and the actual hash.
