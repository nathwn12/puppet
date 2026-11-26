# Documentation structure (local adoption)

This repository follows the documentation structure agreed in the canonical
"docs-template" at @793a13a. That canonical spec owns the rules; this file
records only how the agreement applies locally.

## Local shape

- Root entry points are README.md and AGENTS.md. AGENTS.md is policy only: no
  versions, paths, install commands or current-state descriptions.
- spec/checker.md and spec/docs-structure.md describe this adoption. The plan
  is .nstack/plan.yaml and the decision is adr/0001-vendor-docs-gate.yaml.
- One fact has one home. The checker pin is declared in [docs-check.json](../docs-check.json),
  not restated here; verify it with `node docs-check.mjs --print-hash`.

## Phase 1 here

The gate is report-only locally. Strict mode is earned per repository after a
clean report and is run here as part of the local check. Link checking is not
part of the Node gate.
- This repository already had CI. .github/workflows/docs-report.yml is a separate
  pull-request-only report workflow using pinned offline lychee v2.2.0 with
  fail: false. It does not run the strict gate.
