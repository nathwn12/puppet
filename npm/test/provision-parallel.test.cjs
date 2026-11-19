'use strict';
//
// Two launchers starting the same version at the same moment must both end
// with a usable engine cache. The defect this pins: both stage and both
// rename, Windows refuses the loser's directory rename onto the winner's
// with EPERM, and the loser died while a live MCP session was starting
// (`Connection closed ... rename staging-34508 -> 6.1.2-e8f96a59`).
//
// Two levels:
//   1. publishStaging() unit test: a cache a sibling already published is
//      ADOPTED (verified), never raced and never an error. Deterministic.
//   2. end-to-end: N real launcher processes, started together, one shared
//      fresh cache each round. Every one must report a ready cache and none
//      may report `provisioning failed`.
//
// Builtins only (node:test, node:assert, node:fs, node:zlib, node:crypto),
// same rule as the launcher it tests. The bundle is assembled here because
// the real one is fetched from the GitHub Release at first run.
//
// Run directly: `node --test npm/test/` from the repo root. The published
// package's scripts field holds ONLY the global-install-refusal preinstall/
// postinstall guards (release.yml fails closed on anything else), so there is
// deliberately no `npm test` script; this file is not in the published
// `files` list either.

const test = require('node:test');
const assert = require('node:assert');
const fs = require('node:fs');
const os = require('node:os');
const path = require('node:path');
const zlib = require('node:zlib');
const crypto = require('node:crypto');
const { spawn } = require('node:child_process');

const LAUNCHER = path.join(__dirname, '..', 'bin', 'puppet-mcp.js');
const launcher = require(LAUNCHER);

const VERSION = '9.9.9';
const SERVER_BYTES = Buffer.from('not a real PE, the launcher only has to provision it\n', 'utf8');

// --- minimal stored-zip writer (the launcher's reader accepts method 0) -----

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

function storedZip(entries) {
  const locals = [];
  const central = [];
  let offset = 0;
  for (const [name, data] of entries) {
    const nameBuf = Buffer.from(name.split(path.sep).join('/'), 'utf8');
    const crc = crc32(data);
    const local = Buffer.alloc(30);
    local.writeUInt32LE(0x04034b50, 0);
    local.writeUInt16LE(20, 4);
    local.writeUInt16LE(0x0800, 6); // UTF-8 names
    local.writeUInt16LE(0, 8); // stored
    local.writeUInt32LE(crc, 14);
    local.writeUInt32LE(data.length, 18);
    local.writeUInt32LE(data.length, 22);
    local.writeUInt16LE(nameBuf.length, 26);
    local.writeUInt16LE(0, 28);
    locals.push(local, nameBuf, data);

    const cen = Buffer.alloc(46);
    cen.writeUInt32LE(0x02014b50, 0);
    cen.writeUInt16LE(20, 4);
    cen.writeUInt16LE(20, 6);
    cen.writeUInt16LE(0x0800, 8);
    cen.writeUInt16LE(0, 10);
    cen.writeUInt32LE(crc, 16);
    cen.writeUInt32LE(data.length, 20);
    cen.writeUInt32LE(data.length, 24);
    cen.writeUInt16LE(nameBuf.length, 28);
    cen.writeUInt32LE(offset, 42);
    central.push(cen, nameBuf);
    offset += local.length + nameBuf.length + data.length;
  }
  const cd = Buffer.concat(central);
  const eocd = Buffer.alloc(22);
  eocd.writeUInt32LE(0x06054b50, 0);
  eocd.writeUInt16LE(entries.length, 8);
  eocd.writeUInt16LE(entries.length, 10);
  eocd.writeUInt32LE(cd.length, 12);
  eocd.writeUInt32LE(offset, 16);
  return Buffer.concat([...locals, cd, eocd]);
}


function sha256(buf) {
  return crypto.createHash('sha256').update(buf).digest('hex');
}

function makeRoot(prefix) {
  return fs.mkdtempSync(path.join(os.tmpdir(), `${prefix}-`));
}

/// A self-contained launcher package (bin + package.json + manifest) plus a
/// bundle and the cache leaf it pins, all under one temp root.
function makePackage() {
  const root = makeRoot('puppet-provision');
  fs.mkdirSync(path.join(root, 'pkg', 'bin'), { recursive: true });
  fs.copyFileSync(LAUNCHER, path.join(root, 'pkg', 'bin', 'puppet-mcp.js'));
  fs.writeFileSync(
    path.join(root, 'pkg', 'package.json'),
    JSON.stringify({ name: 'puppet-mcp', version: VERSION }, null, 2)
  );
  const entries = [['puppet.exe', SERVER_BYTES]];
  // Enough entries that extraction takes long enough for two processes to
  // overlap in the staging window.
  for (let i = 0; i < 150; i++) {
    entries.push([`assets/filler-${i}.bin`, Buffer.alloc(512, i & 0xff)]);
  }
  const zip = storedZip(entries);
  const manifest = {
    package: 'puppet-mcp',
    serverVersion: VERSION,
    runtime: {
      source: `https://example.invalid/releases/download/v${VERSION}`,
      archive: { name: `puppet-v${VERSION}-windows-x86_64.zip`, sha256: sha256(zip), size: zip.length },
      server: { name: 'puppet.exe', sha256: sha256(SERVER_BYTES), size: SERVER_BYTES.length },
    },
  };
  fs.writeFileSync(path.join(root, 'pkg', 'engine-manifest.json'), JSON.stringify(manifest, null, 2));
  const bundle = path.join(root, 'bundle.zip');
  fs.writeFileSync(bundle, zip);
  const cache = path.join(root, 'engine', `${VERSION}-${sha256(zip).slice(0, 8)}`);
  return { root, pkg: path.join(root, 'pkg'), manifest, bundle, cache };
}

function runLauncher(pkgDir, bundle, cacheDir) {
  return new Promise((resolve) => {
    const child = spawn(process.execPath, [path.join(pkgDir, 'bin', 'puppet-mcp.js')], {
      env: {
        ...process.env,
        PUPPET_ENGINE_BUNDLE: bundle,
        PUPPET_ENGINE_CACHE: cacheDir,
        PUPPET_MCP_ALLOW_NON_WINDOWS: '1',
      },
      stdio: ['ignore', 'pipe', 'pipe'],
    });
    let out = '';
    let err = '';
    child.stdout.on('data', (d) => (out += d.toString()));
    child.stderr.on('data', (d) => (err += d.toString()));
    child.on('exit', (code) => resolve({ code, out, err }));
    child.on('error', (e) => resolve({ code: -1, out, err: `${err}\nspawn error: ${e.message}` }));
  });
}

// --- 1. deterministic: a published sibling cache is adopted ----------------

test('publishStaging adopts a cache a sibling already published', () => {
  const fx = makePackage();
  try {
    // The sibling's completed cache: marker + verified server, exactly what
    // targetReady() demands.
    fs.mkdirSync(fx.cache, { recursive: true });
    fs.writeFileSync(path.join(fx.cache, 'puppet.exe'), SERVER_BYTES);
    fs.writeFileSync(
      path.join(fx.cache, '.ready'),
      JSON.stringify({
        serverVersion: fx.manifest.serverVersion,
        bundleSha256: fx.manifest.runtime.archive.sha256,
      })
    );
    const rt = fx.manifest.runtime;
    assert.strictEqual(launcher.targetReady(fx.cache, rt, fx.manifest), true);

    // Our own staged copy, ready to publish: the rename MUST lose EPERM.
    const staging = `${fx.cache}.staging-1`;
    fs.mkdirSync(staging, { recursive: true });
    fs.writeFileSync(path.join(staging, 'puppet.exe'), SERVER_BYTES);

    const how = launcher.publishStaging(staging, fx.cache, rt, fx.manifest);
    assert.strictEqual(how, 'adopted', 'a sibling that won must be adopted, not raced');
    assert.ok(fs.existsSync(staging), 'the loser keeps its own staging dir (it is cleaned by the caller)');
    assert.strictEqual(launcher.targetReady(fx.cache, rt, fx.manifest), true, 'the adopted cache stays valid');
  } finally {
    fs.rmSync(fx.root, { recursive: true, force: true });
  }
});

test('publishStaging publishes when no sibling got there first', () => {
  const fx = makePackage();
  try {
    const staging = `${fx.cache}.staging-2`;
    fs.mkdirSync(staging, { recursive: true });
    fs.writeFileSync(path.join(staging, 'puppet.exe'), SERVER_BYTES);
    fs.writeFileSync(
      path.join(staging, '.ready'),
      JSON.stringify({
        serverVersion: fx.manifest.serverVersion,
        bundleSha256: fx.manifest.runtime.archive.sha256,
      })
    );
    assert.strictEqual(launcher.publishStaging(staging, fx.cache, fx.manifest.runtime, fx.manifest), 'renamed');
    assert.strictEqual(launcher.targetReady(fx.cache, fx.manifest.runtime, fx.manifest), true);
  } finally {
    fs.rmSync(fx.root, { recursive: true, force: true });
  }
});

test('publishStaging still refuses a directory that is not a valid cache', () => {
  const fx = makePackage();
  try {
    fs.mkdirSync(fx.cache, { recursive: true }); // exists, but empty: no marker/server
    const staging = `${fx.cache}.staging-3`;
    fs.mkdirSync(staging, { recursive: true });
    fs.writeFileSync(path.join(staging, 'puppet.exe'), SERVER_BYTES);
    assert.throws(
      () => launcher.publishStaging(staging, fx.cache, fx.manifest.runtime, fx.manifest),
      /not a valid cache yet/,
      'an unverifiable sibling directory must never be adopted'
    );
  } finally {
    fs.rmSync(fx.root, { recursive: true, force: true });
  }
});

// --- 2. end-to-end: parallel launchers on one version all succeed ----------

test('two launchers provisioning the same version in parallel both succeed', async () => {
  const fx = makePackage();
  try {
    for (let round = 0; round < 5; round++) {
      fs.rmSync(fx.cache, { recursive: true, force: true });
      const [a, b] = await Promise.all([
        runLauncher(fx.pkg, fx.bundle, fx.cache),
        runLauncher(fx.pkg, fx.bundle, fx.cache),
      ]);
      for (const [name, r] of [['A', a], ['B', b]]) {
        assert.ok(
          /runtime cache ready/.test(r.err),
          `round ${round} launcher ${name} must end with a ready cache; stderr:\n${r.err}`
        );
        assert.ok(
          !/provisioning failed/.test(r.err),
          `round ${round} launcher ${name} must never fail because a sibling won; stderr:\n${r.err}`
        );
      }
      assert.strictEqual(
        launcher.targetReady(fx.cache, fx.manifest.runtime, fx.manifest),
        true,
        `round ${round} must leave one valid cache`
      );
    }
  } finally {
    fs.rmSync(fx.root, { recursive: true, force: true });
  }
});
