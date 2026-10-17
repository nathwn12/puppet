#!/usr/bin/env node
'use strict';
//
// puppet-mcp launcher.
//
// Published package model (see docs/PUBLISHING.md): this tarball holds ONLY
// this launcher plus the pinned server manifest (engine-manifest.json).
// It holds NO server bytes, NO `os` field, NO `scripts`/postinstall field.
// npm 12 blocks dependency install scripts by default (measured
// `allow-scripts=false`), and npx has no project package.json for
// `allowScripts` to consult, so a postinstall download would silently never
// run. Everything is therefore acquired on first RUN by the code below.
//
// Provisioning, one cache, verified:
//   - The server (manifest runtime.archive) downloads from the tagged GitHub
//     Release, verifies zip sha256+size BEFORE extraction, extracts
//     atomically (staging dir, then rename) into
//     %LOCALAPPDATA%\puppet\engine\<serverVersion>-<bundleSha8>\, verifies
//     the server binary against the manifest, writes a .ready marker, and is
//     reused on later runs (re-verified each time). Any mismatch refuses and
//     writes nothing.
//   - The server (puppet.exe, no args = MCP stdio serve) spawns with stdio
//     inherited. ALL launcher chatter goes to stderr; stdout belongs to the
//     MCP channel. No stdout writes exist on the run path.
//   - `--doctor` / `--print-manifest` is the no-network diagnostic: it
//     prints the manifest plus the resolved cache path and exits 0. It
//     returns BEFORE any fetch call (all fetch use lives in
//     downloadToMemory, reachable only from ensureRuntime, which the
//     diagnostic path never calls).
//
// Builtins only: node:fs, node:path, node:os, node:crypto,
// node:child_process, node:zlib, and the global fetch (Node >= 22.14).
// No dependencies, so the published tarball stays tiny and auditable.
//

const fs = require('node:fs');
const path = require('node:path');
const os = require('node:os');
const crypto = require('node:crypto');
const zlib = require('node:zlib');
const { spawn } = require('node:child_process');

const PKG_DIR = path.dirname(path.dirname(__filename));
const MANIFEST_PATH = path.join(PKG_DIR, 'engine-manifest.json');
const PLATFORM_OVERRIDE_ENV = 'PUPPET_MCP_ALLOW_NON_WINDOWS';
const RUNTIME_CACHE_OVERRIDE_ENV = 'PUPPET_ENGINE_CACHE';
const DOWNLOAD_TIMEOUT_MS = 15 * 60 * 1000;
// Staging/extraction guards: the real runtime bundle is ~2 MB. These caps
// reject zip bombs, never the real artifact.
const MAX_ENTRIES = 4096;
const MAX_TOTAL_UNCOMPRESSED = 2 * 1024 * 1024 * 1024;
const MAX_ENTRY_SIZE = 1 * 1024 * 1024 * 1024;

function fail(message) {
  process.stderr.write(`puppet-mcp: error: ${message}\n`);
  process.exit(1);
}

function log(message) {
  process.stderr.write(`puppet-mcp: ${message}\n`);
}

function loadManifest() {
  let raw;
  try {
    raw = fs.readFileSync(MANIFEST_PATH, 'utf8');
  } catch (err) {
    fail(`cannot read engine manifest at ${MANIFEST_PATH}: ${err.message}`);
  }
  try {
    return JSON.parse(raw);
  } catch (err) {
    fail(`engine manifest is not valid JSON: ${err.message}`);
  }
}

// A sha256 slot the release workflow has not filled yet. The committed
// manifest carries the sentinel for every hash computed at release time;
// the workflow replaces each with the real 64-hex digest. Anything else
// shaped (64 hex chars) is release-provisioned.
function isPlaceholderSha(sha) {
  return typeof sha !== 'string' || !/^[0-9a-fA-F]{64}$/.test(sha);
}

function shortHash(sha) {
  return isPlaceholderSha(sha) ? 'unpublished' : sha.toLowerCase().slice(0, 8);
}

// Resolved runtime cache directory. Never throws: the diagnostic path must
// print SOMETHING and exit 0 even where provisioning could never run.
function resolveRuntimeDir(manifest) {
  const notes = [];
  let root = process.env[RUNTIME_CACHE_OVERRIDE_ENV];
  let overridden = Boolean(root);
  if (!root) {
    root = process.env.LOCALAPPDATA;
    if (!root) notes.push('LOCALAPPDATA is unset; set it or ' + RUNTIME_CACHE_OVERRIDE_ENV);
  }
  const leaf = `${manifest.serverVersion}-${shortHash(manifest.runtime.archive.sha256)}`;
  if (!root) return { dir: path.join('<unresolvable>', 'puppet', 'engine', leaf), overridden, notes };
  return { dir: path.join(root, 'puppet', 'engine', leaf), overridden, notes };
}

function printDoctor(manifest) {
  const runtime = resolveRuntimeDir(manifest);
  const report = {
    package: manifest.package,
    serverVersion: manifest.serverVersion,
    platform: process.platform,
    node: process.version,
    runtimeCacheDir: runtime.dir,
    runtimeCacheOverridden: runtime.overridden,
    runtimeCacheNotes: runtime.notes,
    runtime: manifest.runtime,
    network: 'none: this diagnostic performs no fetch and exits before any download code',
  };
  process.stdout.write(JSON.stringify(report, null, 2) + '\n');
  process.exit(0);
}

function usage(exitCode, stream) {
  const text = [
    'usage: puppet-mcp [--doctor | --print-manifest | --help]',
    '',
    '  (no args)            provision the pinned server on first run, then serve MCP over stdio',
    '  --doctor,            print the pinned manifest and the resolved cache',
    '  --print-manifest     path, perform no network access, exit 0',
    '  --help, -h           print this usage',
    '',
    'runtime cache: ' + path.join('%LOCALAPPDATA%', 'puppet', 'engine', '<serverVersion>-<bundleSha8>') + ` (override: ${RUNTIME_CACHE_OVERRIDE_ENV})`,
    `platform: windows-x86_64 only (override: ${PLATFORM_OVERRIDE_ENV}=1, diagnostics only)`,
  ].join('\n') + '\n';
  stream.write(text);
  process.exit(exitCode);
}

// --- minimal zip reader (stored + deflate, builtins only) -----------------
// The runtime bundle is an ordinary deflate zip written by PowerShell's
// Compress-Archive. Node has no builtin unzip, and this package is
// dependency-free by design, so the container parse is hand-rolled here
// (~120 lines) over zlib.inflateRawSync. Anything outside the artifact's
// actual shape (encryption, data descriptors, zip64, unknown methods) is an
// explicit error, never a silent misparse.

const CRC_TABLE = (() => {
  const t = new Int32Array(256);
  for (let n = 0; n < 256; n++) {
    let c = n;
    for (let k = 0; k < 8; k++) c = c & 1 ? 0xedb88320 ^ (c >>> 1) : c >>> 1;
    t[n] = c;
  }
  return t;
})();

function crc32(buf) {
  let c = 0xffffffff;
  for (let i = 0; i < buf.length; i++) c = CRC_TABLE[(c ^ buf[i]) & 0xff] ^ (c >>> 8);
  return (c ^ 0xffffffff) >>> 0;
}

function findEocd(buf) {
  // EOCD is >= 22 bytes at the very end, with up to 64 KiB of comment.
  const lo = Math.max(0, buf.length - (65557 + 22));
  for (let i = buf.length - 22; i >= lo; i--) {
    if (buf.readUInt32LE(i) === 0x06054b50) return i;
  }
  return -1;
}

function extractZip(buf, destDir) {
  const eocd = findEocd(buf);
  if (eocd < 0) throw new Error('not a zip archive: end-of-central-directory not found');
  const entries = buf.readUInt16LE(eocd + 10);
  const cdSize = buf.readUInt32LE(eocd + 12);
  const cdOff = buf.readUInt32LE(eocd + 16);
  if (entries === 0xffff || cdSize === 0xffffffff || cdOff === 0xffffffff) {
    throw new Error('zip64 archives are not supported');
  }
  if (entries > MAX_ENTRIES) throw new Error(`too many entries: ${entries}`);
  if (cdOff + cdSize > buf.length) throw new Error('central directory runs past end of buffer');

  let total = 0;
  const extracted = [];
  let p = cdOff;
  for (let n = 0; n < entries; n++) {
    if (buf.readUInt32LE(p) !== 0x02014b50) throw new Error(`bad central-directory entry ${n}`);
    const flags = buf.readUInt16LE(p + 8);
    const method = buf.readUInt16LE(p + 10);
    const crc = buf.readUInt32LE(p + 16);
    const compSize = buf.readUInt32LE(p + 20);
    const uncompSize = buf.readUInt32LE(p + 24);
    const nameLen = buf.readUInt16LE(p + 28);
    const extraLen = buf.readUInt16LE(p + 30);
    const commentLen = buf.readUInt16LE(p + 32);
    const localOff = buf.readUInt32LE(p + 42);
    const rawName = buf.toString('utf8', p + 46, p + 46 + nameLen);
    p += 46 + nameLen + extraLen + commentLen;

    if (flags & 0x0001) throw new Error(`encrypted entry unsupported: ${rawName}`);
    if (flags & 0x0008) throw new Error(`data-descriptor entry unsupported: ${rawName}`);
    if (method !== 0 && method !== 8) throw new Error(`compression method ${method} unsupported: ${rawName}`);
    if (compSize === 0xffffffff || uncompSize === 0xffffffff) throw new Error(`zip64 entry unsupported: ${rawName}`);
    if (uncompSize > MAX_ENTRY_SIZE) throw new Error(`entry too large: ${rawName}`);
    total += uncompSize;
    if (total > MAX_TOTAL_UNCOMPRESSED) throw new Error('archive uncompressed total exceeds cap');

    // Traversal guard: zip names normally use forward slashes, but
    // PowerShell's Compress-Archive can write nested entry names
    // with backslashes. Normalize every backslash to `/` FIRST, then run
    // the unchanged checks below on the normalized name: absolute paths,
    // drive letters, and `..` segments stay rejected, and the resolved
    // target must still sit under destDir.
    // (Spelled via char code so the source never holds a literal
    // double-backslash.)
    const BACKSLASH = String.fromCharCode(92);
    const name = rawName.split(BACKSLASH).join('/');
    if (name.startsWith('/') || /^[A-Za-z]:/.test(name)) {
      throw new Error(`unsafe entry name: ${rawName}`);
    }
    const parts = name.split('/').filter((s) => s !== '' && s !== '.');
    if (parts.length === 0 || parts.includes('..')) throw new Error(`unsafe entry name: ${rawName}`);
    const target = path.join(destDir, ...parts);
    const rel = path.relative(destDir, target);
    if (rel.startsWith('..') || path.isAbsolute(rel)) throw new Error(`entry escapes destination: ${rawName}`);
    const isDir = name.endsWith('/');
    if (isDir) {
      fs.mkdirSync(target, { recursive: true });
      continue;
    }

    if (buf.readUInt32LE(localOff) !== 0x04034b50) throw new Error(`bad local header: ${rawName}`);
    const lhNameLen = buf.readUInt16LE(localOff + 26);
    const lhExtraLen = buf.readUInt16LE(localOff + 28);
    const dataOff = localOff + 30 + lhNameLen + lhExtraLen;
    const comp = buf.subarray(dataOff, dataOff + compSize);
    if (comp.length !== compSize) throw new Error(`truncated entry data: ${rawName}`);
    const data = method === 0 ? Buffer.from(comp) : zlib.inflateRawSync(comp);
    if (data.length !== uncompSize) throw new Error(`size mismatch after inflate: ${rawName}`);
    if (crc32(data) !== crc) throw new Error(`crc mismatch: ${rawName}`);
    fs.mkdirSync(path.dirname(target), { recursive: true });
    fs.writeFileSync(target, data);
    extracted.push(parts.join('/'));
  }
  return extracted;
}

// --- provisioning ---------------------------------------------------------

function sha256File(filePath) {
  const h = crypto.createHash('sha256');
  h.update(fs.readFileSync(filePath));
  return h.digest('hex');
}

function checkFile(filePath, want) {
  // want: {name, sha256, size}. Returns null when good, else a reason.
  let st;
  try {
    st = fs.statSync(filePath);
  } catch {
    return 'missing';
  }
  if (!st.isFile()) return 'not a file';
  if (st.size !== want.size) return `size ${st.size} != manifest ${want.size}`;
  const got = sha256File(filePath);
  if (got !== want.sha256.toLowerCase()) return 'sha256 mismatch';
  return null;
}

// The ONLY fetch call site in this package. The diagnostic path returns
// before the ensure* functions, so --doctor/--print-manifest provably reach
// no network: grep `fetch(` — it appears exactly once, here.
async function downloadToMemory(url) {
  const ctrl = new AbortController();
  const timer = setTimeout(() => ctrl.abort(), DOWNLOAD_TIMEOUT_MS);
  let res;
  try {
    res = await fetch(url, { signal: ctrl.signal });
  } catch (err) {
    throw new Error(`download failed: ${err.message}`);
  } finally {
    clearTimeout(timer);
  }
  if (!res.ok) throw new Error(`download failed: HTTP ${res.status} ${res.statusText}`);
  return Buffer.from(await res.arrayBuffer());
}

async function ensureRuntime(manifest, cacheDir) {
  const rt = manifest.runtime;
  if (isPlaceholderSha(rt.archive.sha256) || !(rt.archive.size > 0)) {
    fail(
      'this package was not published by the release workflow: runtime hashes are placeholders. ' +
      'See docs/PUBLISHING.md: publish only the workflow-built npm-package artifact.'
    );
  }
  if (isPlaceholderSha(rt.server.sha256) || !(rt.server.size > 0)) {
    fail('engine manifest has no release-provisioned server hash; refusing to run.');
  }

  const markerPath = path.join(cacheDir, '.ready');
  const reuse = (() => {
    try {
      const marker = JSON.parse(fs.readFileSync(markerPath, 'utf8'));
      if (marker.serverVersion !== manifest.serverVersion) return false;
      if (String(marker.bundleSha256).toLowerCase() !== rt.archive.sha256.toLowerCase()) return false;
    } catch {
      return false;
    }
    const bad = checkFile(path.join(cacheDir, rt.server.name), rt.server);
    if (bad) {
      log(`cached ${rt.server.name} invalid (${bad}); re-provisioning.`);
      return false;
    }
    return true;
  })();
  if (reuse) {
    log(`runtime cache ready (reused): ${cacheDir}`);
    return cacheDir;
  }

  const url = `${rt.source}/${rt.archive.name}`;
  log(`provisioning runtime cache: ${cacheDir}`);
  log(`downloading ${url} (${rt.archive.size} bytes)`);
  let data;
  try {
    data = await downloadToMemory(url);
  } catch (err) {
    fail(err.message);
  }
  if (data.length !== rt.archive.size) {
    fail(`download size ${data.length} != manifest ${rt.archive.size}; wrote nothing.`);
  }
  const digest = crypto.createHash('sha256').update(data).digest('hex');
  if (digest !== rt.archive.sha256.toLowerCase()) {
    fail('download sha256 mismatch; wrote nothing.');
  }

  // Extract into a staging dir, verify, then rename: the target dir is
  // never observed half-written.
  const staging = `${cacheDir}.staging-${process.pid}`;
  fs.rmSync(staging, { recursive: true, force: true });
  fs.rmSync(cacheDir, { recursive: true, force: true });
  fs.mkdirSync(staging, { recursive: true });
  try {
    const names = extractZip(data, staging);
    log(`extracted ${names.length} entries`);
    const bad = checkFile(path.join(staging, rt.server.name), rt.server);
    if (bad) throw new Error(`extracted ${rt.server.name} invalid (${bad})`);
    fs.renameSync(staging, cacheDir);
    fs.writeFileSync(
      markerPath,
      JSON.stringify({ serverVersion: manifest.serverVersion, bundleSha256: rt.archive.sha256.toLowerCase() }, null, 2)
    );
  } catch (err) {
    fs.rmSync(staging, { recursive: true, force: true });
    fs.rmSync(cacheDir, { recursive: true, force: true });
    fail(`provisioning failed (${err.message}); wrote nothing.`);
  }
  log(`runtime cache ready (provisioned): ${cacheDir}`);
  return cacheDir;
}

function runServer(cacheDir, manifest) {
  const exe = path.join(cacheDir, manifest.runtime.server.name);
  if (!fs.existsSync(exe)) {
    fail(`server binary missing after provisioning: ${exe}`);
  }
  // Bare puppet with no args serves MCP over stdio (src/main.rs: the server
  // path below the --version early-return). stdio is inherited so the child
  // IS the MCP channel; the launcher writes nothing to stdout on this path.
  // Ctrl+C reaches both processes on Windows (shared console), so no
  // signal relay is needed; the exit code is forwarded below.
  const child = spawn(exe, [], { stdio: 'inherit', env: { ...process.env } });
  child.on('error', (err) => fail(`cannot start ${exe}: ${err.message}`));
  child.on('exit', (code, signal) => {
    if (signal) {
      log(`server terminated by signal ${signal}`);
      process.exit(1);
    }
    process.exit(code === null ? 1 : code);
  });
}

async function main() {
  const args = process.argv.slice(2);
  const manifest = loadManifest();
  if (manifest.package !== 'puppet-mcp') fail('engine manifest names the wrong package.');
  if (args.includes('--help') || args.includes('-h')) usage(0, process.stdout);
  if (args.includes('--doctor') || args.includes('--print-manifest')) {
    const known = new Set(['--doctor', '--print-manifest']);
    const unknown = args.filter((a) => !known.has(a));
    if (unknown.length > 0) usage(2, process.stderr);
    printDoctor(manifest); // exits 0; no network on this path by construction
  }
  // Every known flag returns above; anything left is unknown.
  if (args.length > 0) usage(2, process.stderr);

  if (process.platform !== 'win32' && process.env[PLATFORM_OVERRIDE_ENV] !== '1') {
    fail(
      `unsupported platform '${process.platform}': puppet-mcp ships the windows-x86_64 server only. ` +
      `Set ${PLATFORM_OVERRIDE_ENV}=1 to bypass this gate for diagnostics (the server still will not run elsewhere).`
    );
  }

  const runtime = resolveRuntimeDir(manifest);
  if (runtime.dir.startsWith('<unresolvable>')) {
    fail(`cannot resolve runtime cache dir: ${runtime.notes.join('; ')}. Set ${RUNTIME_CACHE_OVERRIDE_ENV} to an absolute directory.`);
  }
  const readyRuntime = await ensureRuntime(manifest, runtime.dir);
  runServer(readyRuntime, manifest);
}

main().catch((err) => fail(err && err.message ? err.message : String(err)));
