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


test('publishStaging adopts a cache a sibling already published', () => {
  const fx = makePackage();
  try {
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

test('post-publish adoption removes the owned staging copy (deterministic local fixture)', async () => {
  const fx = makePackage();
  const parent = path.dirname(fx.cache);
  const prefix = `${path.basename(fx.cache)}.staging-`;
  const ownedStaging = () =>
    fs.existsSync(parent) ? fs.readdirSync(parent).filter((name) => name.startsWith(prefix)) : [];
  try {
    fs.mkdirSync(parent, { recursive: true });
    for (const name of ownedStaging()) fs.rmSync(path.join(parent, name), { recursive: true, force: true });
    fs.rmSync(fx.cache, { recursive: true, force: true });

    // Publish a valid ready cache at the cache path only AFTER this launcher has
    // created its own staging dir. At that point its early reuse/adopt checks and
    // the cache removal at :439 are behind it, so publishStaging must take the
    // exact post-publish 'adopted' branch. The winner is built from the same
    // local fixture bytes; no network and no remote binary.
    const publishWinner = () => {
      fs.mkdirSync(fx.cache, { recursive: true });
      fs.writeFileSync(path.join(fx.cache, 'puppet.exe'), SERVER_BYTES);
      fs.writeFileSync(
        path.join(fx.cache, '.ready'),
        JSON.stringify({
          serverVersion: fx.manifest.serverVersion,
          bundleSha256: fx.manifest.runtime.archive.sha256,
        })
      );
    };
    const winner = new Promise((resolve, reject) => {
      const timer = setTimeout(() => {
        watcher.close();
        reject(new Error('staging dir never appeared; cannot force post-publish adoption'));
      }, 15000);
      let watcher;
      watcher = fs.watch(parent, (_event, filename) => {
        const base = typeof filename === 'string' ? path.basename(filename) : '';
        if (!base.startsWith(prefix)) return;
        watcher.close();
        clearTimeout(timer);
        try {
          publishWinner();
          resolve();
        } catch (err) {
          reject(err);
        }
      });
    });

    const [r] = await Promise.all([runLauncher(fx.pkg, fx.bundle, fx.cache), winner]);

    // Branch proof: an extraction in this process followed by the adopt log is
    // the post-publish path. The early-adopt log at :395 has no extraction line.
    assert.match(r.err, /extracted \d+ entries/, `launcher must extract into staging; stderr:\n${r.err}`);
    assert.match(
      r.err,
      /runtime cache ready \(adopted from a concurrent launcher\)/,
      `launcher must take the post-publish adopt branch; stderr:\n${r.err}`
    );
    assert.doesNotMatch(r.err, /provisioning failed/, `adoption must not be reported as failure; stderr:\n${r.err}`);

    // The winning ready dir, its server contents and its marker survive.
    assert.strictEqual(
      launcher.targetReady(fx.cache, fx.manifest.runtime, fx.manifest),
      true,
      'the winning cache stays valid'
    );
    assert.deepStrictEqual(fs.readFileSync(path.join(fx.cache, 'puppet.exe')), SERVER_BYTES, 'winning server contents survive');
    const marker = JSON.parse(fs.readFileSync(path.join(fx.cache, '.ready'), 'utf8'));
    assert.strictEqual(marker.serverVersion, fx.manifest.serverVersion, 'marker version survives');
    assert.strictEqual(
      String(marker.bundleSha256).toLowerCase(),
      fx.manifest.runtime.archive.sha256.toLowerCase(),
      'marker bundle hash survives'
    );

    // The owned staging copy must be gone on success.
    assert.deepStrictEqual(ownedStaging(), [], `no owned .staging-* copy may survive; found: ${ownedStaging().join(', ')}`);
  } finally {
    fs.rmSync(fx.root, { recursive: true, force: true });
  }
});
