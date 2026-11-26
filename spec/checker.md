# Checker contract

The checker is the phase-1 documentation gate. It is one Node ESM file with zero dependencies and no local install step. There is no shell fallback.

## Scope of phase 1

Phase 1 implements exactly four checks, and nothing else:

1. `forbidden` — facts that must not appear in `AGENTS.md`.
2. `versions` — config-driven equality across listed sources; opt-in and skipped when not configured.
3. `generated` — whole files and marked README blocks, compared byte-for-byte; opt-in.
4. `checker` — the vendored script's SHA-256 matches the pin in `docs-check.json`.

There is deliberately **no** link check in the Node tool. Link checking is a commodity job: where a repository has CI, use `lychee` as a pinned GitHub Action. The pin is a specific version, not a floating major. There is no local install requirement; a repository without CI relies on review for links. Retired: the hand-rolled link resolver.

### Phase 2

Phase 2 turns on blocking enforcement per repository, after that repository's phase-1 report is clean:

- `lychee` blocking where CI exists;
- the three policy checks — `forbidden`, `versions`, and `generated` — blocking per repository;
- `commands`, `versions`, `invariants`, and `adr` are defined but not implemented in phase 1.

`commands` never runs in phase 1. In phase 2 it is driven by an allowlist whose default is empty, so a config with no allowlist executes nothing. In phase 2, a command entry missing `run`, `cwd`, or `timeoutMs` is a **config error** exiting 2, never a finding.

### Phase 3, mentioned only

Phase 3 pilots `repowise doc-drift` for reference, command, heading, and symbol drift: report-only, one pilot first, AGPL-3.0, with a uv toolchain review on that pilot. `docsync` — sentence-level documentation and code bindings plus MCP lookup — is parked as a phase-3 option for the same pilot; it needs a per-repo service. Neither is implemented here.

## CLI

```text
node docs-check.mjs [--config docs-check.json] [--report|--strict] [--json]
node docs-check.mjs --print-hash
```

- `--config <path>` selects the config. Default: `docs-check.json` in the current directory.
- `--report` is the default mode: findings are printed, exit is 0.
- `--strict` exits 1 when there is at least one finding.
- `--json` emits a stable JSON object with `mode`, `findings`, and `summary`.
- `--print-hash` prints the SHA-256 of the running script and exits 0; it takes no other options.
- Unknown, repeated, or mutually exclusive options are config errors exiting 2.

### Exit codes

| Code | Meaning |
| --- | --- |
| 0 | no findings, or findings in report mode |
| 1 | at least one finding in strict mode |
| 2 | config error or checker error |

An unreadable config, invalid JSON, a schema violation, an unresolvable version source, or a renderer that cannot load or return a string is an error, not a finding.

## Config

The config is JSON. All paths are resolved relative to the directory containing the config. Keys are validated strictly: an unknown key, wrong type, or missing required field is an error naming the field. In particular, a `links` key is rejected — links are not this tool's job.

Schema:

```json
{
  "versions": [
    { "name": "release", "sources": ["package.json#version", "Cargo.toml", "pyproject.toml"] }
  ],
  "generated": [
    { "file": "README.md", "block": "NAME", "renderer": "tools/render.mjs", "context": {} },
    { "file": "docs/GENERATED.md", "renderer": "tools/render-all.mjs" }
  ],
  "forbidden": [
    { "files": ["AGENTS.md"], "patterns": ["example"] }
  ],
  "checker": { "version": "0.1.0", "sha256": "64 hex characters" }
}
```

- `versions` is optional and defaults to `[]`: opt-in, skipped when absent. Each group requires `sources`, a non-empty string array; `name` is an optional non-empty string. A group means all listed sources must hold the same version.
- `generated` is optional and defaults to `[]`. Each entry requires `file` and `renderer`; `block` and `context` are optional, and `context` must be an object when present. An entry with `block` is a marked-block entry; without `block` it is a whole-file entry.
- `forbidden` is optional and defaults to `[]`. Each entry requires `files` and `patterns`, both non-empty string arrays. Patterns are case-insensitive JavaScript regular expressions; an invalid pattern is a config error.
- `checker` is required. `version` is a non-empty string and `sha256` is 64 hexadecimal characters.

### Glob semantics

`files` entries accept explicit relative paths and globs. `*` matches within one segment, `?` matches one character, `**/` matches zero or more leading directories. Backslashes are normalized. Matches are deduplicated and sorted. `.git`, `node_modules`, `dist`, and `test/fixtures` are not traversed.

## Check behaviour

### forbidden

The built-in patterns always apply to every `AGENTS.md` found by `**/AGENTS.md`:

- semantic version strings, excluding IPv4 addresses;
- absolute Windows paths, including UNC paths;
- `%LOCALAPPDATA%` references;
- package install commands.

Configured entries add to these; they do not replace them. A finding reports `file:line [forbidden]` and includes the regex source. There is deliberately **no** "current state" phrase check: that is non-deterministic prose, not a policy violation.

### versions

Each source is a string:

- `path#pointer` reads a JSON file and follows a dot- or slash-separated pointer, for example `package.json#version` or `release#dependencies/core/version`.
- `path` without a fragment reads a text file and finds its first `version` key, covering `Cargo.toml` and `pyproject.toml`.

An unreadable source, invalid JSON, missing pointer, or missing version key is a config error exiting 2. Every source whose value differs from the group's first value yields a finding on its own file and line, and the message lists **all** values in the group as `file=value` pairs. With one source, or with equal values, there is no finding.

### generated

A renderer is a repository-local ESM module exporting `render(context)`, resolved relative to the config directory. Load or render failures are checker errors.

- **Whole-file entry**: `render(context)` must equal the file's bytes exactly.
- **Marked-block entry**: the file must contain `<!-- docs:NAME -->` and `<!-- /docs:NAME -->`, each on its own line. The body is the bytes strictly between the newline after the opening marker and the close marker. The expected body is `render(context)` with exactly one trailing newline.

Comparison is byte-exact; there is no whitespace normalization. Missing files and missing marker pairs are findings.

### checker

The checker hashes the script actually running and compares it with `config.checker.sha256`, and compares its version constant with `config.checker.version`. Either mismatch is a finding on `docs-check.mjs`. `--print-hash` produces the pin after an intentional checker change.

## Output

Text output is one line per finding followed by a summary:

```text
path:line [check] message
N findings (report|strict)
```

`--json` emits the same findings in a stable object: `mode`, `findings` (each with `file`, `line`, `check`, `message`), and `summary` with `count` and `mode`.

## Vendoring

The vendored pair is [docs-check.mjs](../docs-check.mjs) and [docs-check.json](../docs-check.json). The config pins the checker version and SHA-256, so a silent edit to the vendored script is visible. The template's tests are in [test/checker.test.mjs](../test/checker.test.mjs).

Placement rules are in [docs-structure.md](docs-structure.md).
