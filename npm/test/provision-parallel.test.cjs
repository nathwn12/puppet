'use strict';

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

function makePackage() {
  const root = makeRoot('puppet-provision');
  fs.mkdirSync(path.join(root, 'pkg', 'bin'), { recursive: true });
  fs.copyFileSync(LAUNCHER, path.join(root, 'pkg', 'bin', 'puppet-mcp.js'));
  fs.writeFileSync(
    path.join(root, 'pkg', 'package.json'),
    JSON.stringify({ name: 'puppet-mcp', version: VERSION }, null, 2)
  );
  const entries = [['puppet.exe', SERVER_BYTES]];
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

test('a peer claimed path without a ready marker fails closed and is left intact', async () => {
  const fx = makePackage();
  try {
    fs.mkdirSync(fx.cache, { recursive: true });
    fs.writeFileSync(path.join(fx.cache, 'puppet.exe'), SERVER_BYTES);
    const r = await runLauncherWithEnv(fx.pkg, {
      ...baseEnvFor(fx),
      PUPPET_CACHE_READY_TIMEOUT_MS: '200',
    });
    assert.match(r.err, /timed out waiting for .* to become ready/, `must fail closed; stderr:\n${r.err}`);
    assert.match(r.err, /remove that directory and retry/, `explicit recovery hint required; stderr:\n${r.err}`);
    assert.strictEqual(fs.existsSync(path.join(fx.cache, 'puppet.exe')), true, 'peer created bytes must survive');
    assert.strictEqual(fs.existsSync(path.join(fx.cache, '.ready')), false, 'no marker invented for a partial path');
  } finally {
    fs.rmSync(fx.root, { recursive: true, force: true });
  }
});

function runLauncherWithEnv(pkgDir, env) {
  return new Promise((resolve) => {
    const child = spawn(process.execPath, [path.join(pkgDir, 'bin', 'puppet-mcp.js')], {
      env, stdio: ['ignore', 'pipe', 'pipe'],
    });
    let out = '';
    let err = '';
    child.stdout.on('data', (d) => (out += d.toString()));
    child.stderr.on('data', (d) => (err += d.toString()));
    child.on('exit', (code) => resolve({ code, out, err }));
    child.on('error', (e) => resolve({ code: -1, out, err: `${err}\nspawn error: ${e.message}` }));
  });
}

test('a sibling that publishes mid-provision keeps its ready cache and marker', async () => {
  const fx = makePackage();
  const at = path.join(fx.root, 'at-read');
  const go = path.join(fx.root, 'go');
  const hook = path.join(fx.root, 'race-hook.cjs');
  // Test-only preload: pause the loser AFTER its early targetReady checks but
  // BEFORE it touches the cache, so a sibling can publish a ready cache in the
  // window. No production code is involved in the interleave.
  fs.writeFileSync(hook, `'use strict';
const fs = require('node:fs');
const path = require('node:path');
const real = fs.readFileSync;
const bundle = process.env.PUPPET_RACE_BUNDLE;
const dir = process.env.PUPPET_RACE_DIR;
if (!bundle || !dir) throw new Error('race hook: missing env');
fs.readFileSync = function (p) {
  if (typeof p === 'string' && path.resolve(p) === path.resolve(bundle)) {
    fs.writeFileSync(path.join(dir, 'at-read'), '1');
    const go = path.join(dir, 'go');
    const deadline = Date.now() + 20000;
    while (!fs.existsSync(go)) {
      if (Date.now() > deadline) throw new Error('race hook: go sentinel timeout');
      Atomics.wait(new Int32Array(new SharedArrayBuffer(4)), 0, 0, 25);
    }
  }
  return real.apply(this, arguments);
};
`);
  try {
    fs.rmSync(fx.cache, { recursive: true, force: true });
    const baseEnv = {
      ...process.env,
      PUPPET_ENGINE_BUNDLE: fx.bundle,
      PUPPET_ENGINE_CACHE: fx.cache,
      PUPPET_MCP_ALLOW_NON_WINDOWS: '1',
    };
    const loser = runLauncherWithEnv(fx.pkg, {
      ...baseEnv,
      NODE_OPTIONS: `--require=${hook}`,
      PUPPET_RACE_BUNDLE: fx.bundle,
      PUPPET_RACE_DIR: fx.root,
    });
    const deadline = Date.now() + 20000;
    while (!fs.existsSync(at)) {
      if (Date.now() > deadline) {
        const partial = await loser;
        assert.fail(`loser never reached the bundle read; stderr:\n${partial.err}`);
      }
      await new Promise((r) => setTimeout(r, 25));
    }
    const winner = await runLauncherWithEnv(fx.pkg, baseEnv);
    assert.strictEqual(
      launcher.targetReady(fx.cache, fx.manifest.runtime, fx.manifest), true,
      `winner must publish a ready cache first; stderr:\n${winner.err}`
    );
    // A fixture-owned marker: if the loser deletes the published cache, it vanishes.
    const token = path.join(fx.cache, 'owner-a.token');
    fs.writeFileSync(token, 'A');
    fs.writeFileSync(go, '1');
    const r = await loser;
    assert.ok(fs.existsSync(token), `loser must not delete a ready cache its sibling published; stderr:\n${r.err}`);
    assert.strictEqual(launcher.targetReady(fx.cache, fx.manifest.runtime, fx.manifest), true, 'ready cache survives');
    assert.match(r.err, /runtime cache ready/, `loser must end ready; stderr:\n${r.err}`);
    assert.doesNotMatch(r.err, /provisioning failed/, `loser must not fail; stderr:\n${r.err}`);
  } finally {
    fs.rmSync(fx.root, { recursive: true, force: true });
  }
});

function baseEnvFor(fx) {
  return {
    ...process.env,
    PUPPET_ENGINE_BUNDLE: fx.bundle,
    PUPPET_ENGINE_CACHE: fx.cache,
    PUPPET_MCP_ALLOW_NON_WINDOWS: '1',
  };
}

test('an invalid ready cache is refused, not cleared', async () => {
  const fx = makePackage();
  try {
    fs.mkdirSync(fx.cache, { recursive: true });
    fs.writeFileSync(path.join(fx.cache, '.ready'), JSON.stringify({ serverVersion: VERSION, bundleSha256: 'deadbeef' }));
    fs.writeFileSync(path.join(fx.cache, 'puppet.exe'), 'stale');
    assert.strictEqual(launcher.targetReady(fx.cache, fx.manifest.runtime, fx.manifest), false, 'fixture cache must start invalid');
    const r = await runLauncherWithEnv(fx.pkg, {
      ...baseEnvFor(fx),
      PUPPET_CACHE_READY_TIMEOUT_MS: '200',
    });
    assert.match(r.err, /timed out waiting for .* to become ready/, `invalid cache must fail closed; stderr:\n${r.err}`);
    assert.match(r.err, /remove that directory and retry/, `explicit recovery hint required; stderr:\n${r.err}`);
    assert.strictEqual(fs.readFileSync(path.join(fx.cache, 'puppet.exe'), 'utf8'), 'stale', 'invalid bytes must remain untouched');
  } finally {
    fs.rmSync(fx.root, { recursive: true, force: true });
  }
});

test('a ready cache is reused without reading the bundle', async () => {
  const fx = makePackage();
  try {
    const first = await runLauncherWithEnv(fx.pkg, baseEnvFor(fx));
    assert.strictEqual(launcher.targetReady(fx.cache, fx.manifest.runtime, fx.manifest), true, `first run must publish; stderr:\n${first.err}`);
    const second = await runLauncherWithEnv(fx.pkg, {
      ...baseEnvFor(fx),
      PUPPET_ENGINE_BUNDLE: path.join(fx.root, 'missing-bundle.zip'),
    });
    assert.match(second.err, /runtime cache ready \(reused\)/, `warm run must reuse; stderr:\n${second.err}`);
    assert.doesNotMatch(second.err, /reading |downloading |provisioning runtime cache/, `warm run must not provision; stderr:\n${second.err}`);
  } finally {
    fs.rmSync(fx.root, { recursive: true, force: true });
  }
});

test('a failed provision clears only the path it created', async () => {
  const fx = makePackage();
  try {
    const corrupt = Buffer.from('corrupt server bytes\n', 'utf8');
    const entries = [['puppet.exe', corrupt]];
    for (let i = 0; i < 8; i++) entries.push([`assets/f-${i}.bin`, Buffer.alloc(64, i)]);
    const zip = storedZip(entries);
    fs.writeFileSync(fx.bundle, zip);
    const manifest = {
      ...fx.manifest,
      runtime: {
        ...fx.manifest.runtime,
        archive: { ...fx.manifest.runtime.archive, sha256: sha256(zip), size: zip.length },
        server: { ...fx.manifest.runtime.server, sha256: sha256(SERVER_BYTES), size: SERVER_BYTES.length },
      },
    };
    fs.writeFileSync(path.join(fx.pkg, 'engine-manifest.json'), JSON.stringify(manifest, null, 2));
    // The override names a leaf; keep its sha8 in step with the new archive.
    fx.cache = path.join(path.dirname(fx.cache), `${VERSION}-${sha256(zip).slice(0, 8)}`);
    const r = await runLauncherWithEnv(fx.pkg, baseEnvFor(fx));
    assert.match(r.err, /extracted puppet\.exe invalid/, `server check must fail; stderr:\n${r.err}`);
    assert.match(r.err, /provisioning failed/, `failure must be named; stderr:\n${r.err}`);
    assert.strictEqual(fs.existsSync(fx.cache), false, 'only the creator clears its own claimed path');
    assert.strictEqual(launcher.targetReady(fx.cache, manifest.runtime, manifest), false, 'no ready cache on failure');
  } finally {
    fs.rmSync(fx.root, { recursive: true, force: true });
  }
});
