#!/usr/bin/env node
// Canonical phase-1 docs gate. Single Node ESM file, zero dependencies.
// Checks: forbidden, versions, generated, checker. No link checking here.
// Run: node docs-check.mjs [--config docs-check.json] [--report|--strict] [--json]

import fs from 'node:fs';
import path from 'node:path';
import crypto from 'node:crypto';
import { fileURLToPath, pathToFileURL } from 'node:url';

const SELF = fileURLToPath(import.meta.url);
const CHECKER_VERSION = '0.2.0';
const DEFAULT_CONFIG = 'docs-check.json';

const HELP = `docs-check (phase 1)

Usage:
  node docs-check.mjs [--config docs-check.json] [--report|--strict] [--json]
  node docs-check.mjs --print-hash

Options:
  --config <path>  config file (default: docs-check.json in cwd)
  --report         report mode: findings do not fail (default)
  --strict         strict mode: any finding exits 1
  --json           emit stable JSON instead of text lines
  --print-hash     print this script's SHA-256 and exit
  --help           show this help

Exit codes: 0 clean or report mode, 1 strict with findings, 2 config/checker error.
`;

function fail(message) {
  process.stderr.write(`docs-check: ${message}\n`);
  process.exit(2);
}

function parseArgs(argv) {
  const opts = { config: DEFAULT_CONFIG, mode: 'report', json: false, printHash: false };
  for (let i = 0; i < argv.length; i += 1) {
    const arg = argv[i];
    if (arg === '--config') {
      i += 1;
      if (i >= argv.length) fail('--config requires a file path');
      opts.config = argv[i];
    } else if (arg.startsWith('--config=')) {
      opts.config = arg.slice('--config='.length);
    } else if (arg === '--report') {
      if (opts.mode === 'strict') fail('--report and --strict are mutually exclusive');
      opts.mode = 'report';
    } else if (arg === '--strict') {
      if (opts.mode === 'strict') fail('--strict specified more than once');
      opts.mode = 'strict';
    } else if (arg === '--json') {
      if (opts.json) fail('--json specified more than once');
      opts.json = true;
    } else if (arg === '--print-hash') {
      opts.printHash = true;
    } else if (arg === '--help' || arg === '-h') {
      process.stdout.write(HELP);
      process.exit(0);
    } else {
      fail(`unknown argument: ${arg}`);
    }
  }
  if (opts.printHash && (opts.json || opts.mode === 'strict' || opts.config !== DEFAULT_CONFIG)) {
    fail('--print-hash cannot be combined with other options');
  }
  if (!opts.config || !opts.config.trim()) fail('--config requires a non-empty file path');
  return opts;
}

function sha256Of(file) {
  const text = fs.readFileSync(file, 'utf8').replace(/\r\n/g, '\n').replace(/\r/g, '\n');
  return crypto.createHash('sha256').update(text, 'utf8').digest('hex');
}

function isPlainObject(value) {
  return value !== null && typeof value === 'object' && !Array.isArray(value);
}

function isNonEmptyString(value) {
  return typeof value === 'string' && value.trim().length > 0;
}

function assertKeys(obj, allowed, where) {
  for (const key of Object.keys(obj)) {
    if (!allowed.includes(key)) fail(`${where} has unknown key "${key}" (allowed: ${allowed.join(', ')})`);
  }
}

function assertStringArray(value, where) {
  if (!Array.isArray(value) || value.length === 0) fail(`${where} must be a non-empty array of strings`);
  value.forEach((item, i) => {
    if (!isNonEmptyString(item)) fail(`${where}[${i}] must be a non-empty string`);
  });
}

function validateConfig(data) {
  if (!isPlainObject(data)) fail('config root must be a JSON object');
  assertKeys(data, ['versions', 'generated', 'forbidden', 'checker'], 'config root');

  if (data.versions !== undefined) {
    if (!Array.isArray(data.versions)) fail('config.versions must be an array');
    data.versions.forEach((group, i) => {
      const where = `config.versions[${i}]`;
      if (!isPlainObject(group)) fail(`${where} must be an object`);
      assertKeys(group, ['name', 'sources'], where);
      if (group.name !== undefined && !isNonEmptyString(group.name)) fail(`${where}.name must be a non-empty string`);
      if (group.sources === undefined) fail(`${where}.sources is required`);
      assertStringArray(group.sources, `${where}.sources`);
    });
  }

  if (data.generated !== undefined) {
    if (!Array.isArray(data.generated)) fail('config.generated must be an array');
    data.generated.forEach((entry, i) => {
      const where = `config.generated[${i}]`;
      if (!isPlainObject(entry)) fail(`${where} must be an object`);
      assertKeys(entry, ['file', 'block', 'renderer', 'context'], where);
      if (!isNonEmptyString(entry.file)) fail(`${where}.file must be a non-empty string`);
      if (!isNonEmptyString(entry.renderer)) fail(`${where}.renderer must be a non-empty string`);
      if (entry.block !== undefined && !isNonEmptyString(entry.block)) fail(`${where}.block must be a non-empty string`);
      if (entry.context !== undefined && !isPlainObject(entry.context)) fail(`${where}.context must be an object`);
    });
  }

  if (data.forbidden !== undefined) {
    if (!Array.isArray(data.forbidden)) fail('config.forbidden must be an array');
    data.forbidden.forEach((entry, i) => {
      const where = `config.forbidden[${i}]`;
      if (!isPlainObject(entry)) fail(`${where} must be an object`);
      assertKeys(entry, ['files', 'patterns'], where);
      if (entry.files === undefined) fail(`${where}.files is required`);
      if (entry.patterns === undefined) fail(`${where}.patterns is required`);
      assertStringArray(entry.files, `${where}.files`);
      assertStringArray(entry.patterns, `${where}.patterns`);
      entry.patterns.forEach((pattern, j) => {
        try {
          new RegExp(pattern, 'i');
        } catch (error) {
          fail(`${where}.patterns[${j}] is not a valid regular expression: ${error.message}`);
        }
      });
    });
  }

  if (data.checker === undefined) fail('config.checker is required');
  if (!isPlainObject(data.checker)) fail('config.checker must be an object');
  assertKeys(data.checker, ['version', 'sha256'], 'config.checker');
  if (!isNonEmptyString(data.checker.version)) fail('config.checker.version must be a non-empty string');
  if (typeof data.checker.sha256 !== 'string' || !/^[0-9a-f]{64}$/i.test(data.checker.sha256)) {
    fail('config.checker.sha256 must be a 64-character hexadecimal SHA-256 string');
  }

  return {
    versions: data.versions ?? [],
    generated: data.generated ?? [],
    forbidden: data.forbidden ?? [],
    checker: data.checker
  };
}

function loadConfig(configArg) {
  const configPath = path.resolve(process.cwd(), configArg);
  let raw;
  try {
    raw = fs.readFileSync(configPath, 'utf8');
  } catch (error) {
    fail(`cannot read config ${configArg}: ${error.message}`);
  }
  let parsed;
  try {
    parsed = JSON.parse(raw);
  } catch (error) {
    fail(`invalid JSON in ${configArg}: ${error.message}`);
  }
  return { config: validateConfig(parsed), root: path.dirname(configPath) };
}

function escapeRegExp(text) {
  return text.replace(/[.*+?^${}()|[\]\\]/g, '\\$&');
}

function globToRegExp(glob) {
  let pattern = glob.replace(/\\/g, '/');
  let source = '^';
  for (let i = 0; i < pattern.length; i += 1) {
    const char = pattern[i];
    if (char === '*') {
      if (pattern[i + 1] === '*') {
        i += 1;
        if (pattern[i + 1] === '/') {
          i += 1;
          source += '(?:.*/)?';
        } else {
          source += '.*';
        }
      } else {
        source += '[^/]*';
      }
    } else if (char === '?') {
      source += '[^/]';
    } else {
      source += escapeRegExp(char);
    }
  }
  return new RegExp(`${source}$`);
}

function walk(dir, root, out) {
  let entries;
  try {
    entries = fs.readdirSync(dir, { withFileTypes: true });
  } catch {
    return;
  }
  entries.sort((a, b) => a.name.localeCompare(b.name));
  for (const entry of entries) {
    const full = path.join(dir, entry.name);
    if (entry.isDirectory()) {
      const isFixtureDir = entry.name === 'fixtures' && path.basename(dir) === 'test';
      if (['.git', 'node_modules', 'dist'].includes(entry.name) || isFixtureDir) continue;
      walk(full, root, out);
    } else if (entry.isFile()) {
      out.push(path.relative(root, full).split(path.sep).join('/'));
    }
  }
}

function resolveFiles(patterns, root) {
  const all = [];
  walk(root, root, all);
  const matched = new Set();
  for (const pattern of patterns) {
    if (!/[*?]/.test(pattern)) {
      const absolute = path.resolve(root, pattern);
      if (fs.existsSync(absolute) && fs.statSync(absolute).isFile()) {
        matched.add(path.relative(root, absolute).split(path.sep).join('/'));
      }
      continue;
    }
    const regex = globToRegExp(pattern);
    for (const file of all) {
      if (regex.test(file)) matched.add(file);
    }
  }
  return [...matched].sort((a, b) => a.localeCompare(b));
}

const DEFAULT_FORBIDDEN = [
  {
    files: ['**/AGENTS.md'],
    patterns: [
      String.raw`(?<!\d\.)\bv?\d+\.\d+\.\d+(?:[-+][0-9A-Za-z.-]+)?\b(?!\.\d)`,
      String.raw`\b[A-Za-z]:[\\/]`,
      String.raw`\\\\[^\\\s]+`,
      String.raw`%LOCALAPPDATA%`,
      String.raw`\b(?:npm|pnpm|yarn)\s+(?:install|i|add)\b`
    ]
  }
];

function checkForbidden(config, root, add) {
  const groups = [...DEFAULT_FORBIDDEN, ...config.forbidden];
  for (const group of groups) {
    for (const file of resolveFiles(group.files, root)) {
      let text;
      try {
        text = fs.readFileSync(path.join(root, file), 'utf8');
      } catch {
        continue;
      }
      text.split(/\r?\n/).forEach((line, index) => {
        for (const source of group.patterns) {
          const regex = new RegExp(source, 'i');
          if (regex.test(line)) {
            add(file, index + 1, 'forbidden', `forbidden pattern /${source}/i matched: ${line.trim()}`);
          }
        }
      });
    }
  }
}

function readVersionSource(spec, root, groupIndex, sourceIndex) {
  const where = `config.versions[${groupIndex}].sources[${sourceIndex}]`;
  const hashAt = spec.indexOf('#');
  const pathPart = hashAt < 0 ? spec : spec.slice(0, hashAt);
  const pointer = hashAt < 0 ? null : spec.slice(hashAt + 1);
  if (hashAt >= 0 && !pointer) fail(`${where}: JSON pointer is empty in "${spec}"`);
  const absolute = path.resolve(root, pathPart);
  const relative = path.relative(root, absolute).split(path.sep).join('/');
  let text;
  try {
    text = fs.readFileSync(absolute, 'utf8');
  } catch (error) {
    fail(`${where}: cannot read ${pathPart}: ${error.message}`);
  }

  if (pointer) {
    let parsed;
    try {
      parsed = JSON.parse(text);
    } catch (error) {
      fail(`${where}: ${pathPart} is not valid JSON: ${error.message}`);
    }
    let value = parsed;
    for (const segment of pointer.split(/\/|\./).filter(Boolean)) {
      if (value === null || typeof value !== 'object' || !(segment in value)) {
        fail(`${where}: JSON pointer "${pointer}" not found in ${pathPart}`);
      }
      value = value[segment];
    }
    if (typeof value !== 'string' && typeof value !== 'number') {
      fail(`${where}: JSON pointer "${pointer}" in ${pathPart} is not a string or number`);
    }
    return { file: relative, line: 1, value: String(value) };
  }

  const regex = /["']?version["']?\s*[=:]\s*["']?([^"'\s,#]+)["']?/im;
  const match = regex.exec(text);
  if (!match) fail(`${where}: no version key found in ${pathPart}`);
  const line = text.slice(0, match.index).split(/\r?\n/).length;
  return { file: relative, line, value: match[1] };
}

function checkVersions(config, root, add) {
  config.versions.forEach((group, groupIndex) => {
    const name = group.name ?? `versions[${groupIndex}]`;
    const values = group.sources.map((spec, sourceIndex) =>
      readVersionSource(spec, root, groupIndex, sourceIndex)
    );
    const allValues = values.map((item) => `${item.file}=${item.value}`).join(', ');
    const expected = values[0].value;
    for (const item of values) {
      if (item.value !== expected) {
        add(item.file, item.line, 'versions', `version mismatch in group "${name}": ${allValues}`);
      }
    }
  });
}

function firstDiffLine(actual, expected) {
  const a = actual.split(/\r?\n/);
  const b = expected.split(/\r?\n/);
  const limit = Math.max(a.length, b.length);
  for (let i = 0; i < limit; i += 1) {
    if (a[i] !== b[i]) return i + 1;
  }
  return 1;
}

function lineAt(text, offset) {
  return text.slice(0, offset).split(/\r?\n/).length;
}

async function checkGenerated(config, root, add) {
  for (let i = 0; i < config.generated.length; i += 1) {
    const entry = config.generated[i];
    const where = `config.generated[${i}]`;
    const rendererPath = path.isAbsolute(entry.renderer) ? entry.renderer : path.resolve(root, entry.renderer);
    let module;
    try {
      module = await import(pathToFileURL(rendererPath).href);
    } catch (error) {
      fail(`${where}: cannot load renderer ${entry.renderer}: ${error.message}`);
    }
    if (typeof module.render !== 'function') fail(`${where}: renderer ${entry.renderer} must export render(context)`);
    let rendered;
    try {
      rendered = await module.render(entry.context ?? {});
    } catch (error) {
      fail(`${where}: render(context) failed: ${error.message}`);
    }
    if (typeof rendered !== 'string') fail(`${where}: render(context) must return a string`);

    const absolute = path.resolve(root, entry.file);
    let actual;
    try {
      actual = fs.readFileSync(absolute, 'utf8');
    } catch (error) {
      add(entry.file, 1, 'generated', `generated file is missing: ${error.message}`);
      continue;
    }

    if (entry.block === undefined) {
      if (actual !== rendered) {
        add(entry.file, firstDiffLine(actual, rendered), 'generated', `whole-file output drift from ${entry.renderer}`);
      }
      continue;
    }

    const open = `<!-- docs:${entry.block} -->`;
    const close = `<!-- /docs:${entry.block} -->`;
    const openAt = actual.indexOf(open);
    const closeAt = openAt < 0 ? -1 : actual.indexOf(close, openAt + open.length);
    if (openAt < 0 || closeAt < 0) {
      add(entry.file, 1, 'generated', `marked block "${entry.block}" markers are missing`);
      continue;
    }
    const bodyStart = actual.indexOf('\n', openAt + open.length);
    if (bodyStart < 0 || bodyStart >= closeAt) {
      add(entry.file, lineAt(actual, openAt), 'generated', `marked block "${entry.block}" has no body line`);
      continue;
    }
    const body = actual.slice(bodyStart + 1, closeAt);
    const expected = rendered.endsWith('\n') ? rendered : `${rendered}\n`;
    if (body !== expected) {
      add(entry.file, lineAt(actual, bodyStart + 1), 'generated', `marked block "${entry.block}" drift from ${entry.renderer}`);
    }
  }
}

function checkChecker(config, add) {
  let actual;
  try {
    actual = sha256Of(SELF);
  } catch (error) {
    fail(`cannot hash running checker: ${error.message}`);
  }
  if (config.checker.version !== CHECKER_VERSION) {
    add('docs-check.mjs', 1, 'checker', `checker version mismatch: config pins ${config.checker.version}, running script is ${CHECKER_VERSION}`);
  }
  if (actual.toLowerCase() !== config.checker.sha256.toLowerCase()) {
    add('docs-check.mjs', 1, 'checker', `checker SHA-256 mismatch: config pins ${config.checker.sha256}, running script is ${actual}`);
  }
}

async function main() {
  const opts = parseArgs(process.argv.slice(2));
  if (opts.printHash) {
    process.stdout.write(`${sha256Of(SELF)}\n`);
    return 0;
  }
  const { config, root } = loadConfig(opts.config);
  const findings = [];
  const add = (file, line, check, message) => {
    findings.push({ file, line, check, message });
  };

  checkForbidden(config, root, add);
  checkVersions(config, root, add);
  await checkGenerated(config, root, add);
  checkChecker(config, add);

  findings.sort((a, b) => a.file.localeCompare(b.file) || a.line - b.line || a.check.localeCompare(b.check));

  if (opts.json) {
    const payload = {
      mode: opts.mode,
      findings,
      summary: { count: findings.length, mode: opts.mode }
    };
    process.stdout.write(`${JSON.stringify(payload, null, 2)}\n`);
  } else {
    for (const finding of findings) {
      process.stdout.write(`${finding.file}:${finding.line} [${finding.check}] ${finding.message}\n`);
    }
    process.stdout.write(`${findings.length} findings (${opts.mode})\n`);
  }
  return opts.mode === 'strict' && findings.length > 0 ? 1 : 0;
}

main().then(
  (code) => {
    process.exitCode = code;
  },
  (error) => {
    fail(`unexpected checker error: ${error?.stack ?? error}`);
  }
);
