#!/usr/bin/env node
// Clean-room gate: install the package the way a stranger does — scrubbed
// profile, empty caches — then prove the engine provisions and the server boots.
//
//   node scripts/cleanroom-check.mjs --tarball <path.tgz> [--bundle <path.zip>] [--keep]
//   node scripts/cleanroom-check.mjs --registry --version <semver> [--keep]
//
// --bundle points the launcher (PUPPET_ENGINE_BUNDLE) at a locally built
// runtime archive instead of the Release download, so the gate can run BEFORE
// that Release exists. It applies to --tarball mode only; the launcher still
// checks the archive's size and sha256 against the installed manifest, so a
// wrong zip fails this gate exactly like a bad download would. --registry mode
// scrubs the override, so the public gate can never be bypassed.
//
// Windows-only; Node stdlib only. Prints a PASS/FAIL table, exits 1 on any
// FAIL. Cleans its temp dir in a finally unless --keep is passed. Never
// downloads outside <tmp>: every writeable profile/cache path is redirected
// under <tmp> in the scrubbed child env.
import { spawn, spawnSync } from "node:child_process";
import {
  copyFileSync, existsSync, mkdirSync, mkdtempSync, readdirSync, readFileSync, rmSync, statSync,
} from "node:fs";
import { tmpdir } from "node:os";
import { basename, dirname, join, resolve } from "node:path";
import { fileURLToPath } from "node:url";

const BOOT_TIMEOUT_MS = 300_000;
const STEP_TIMEOUT_MS = 300_000;

const root = resolve(dirname(fileURLToPath(import.meta.url)), "..");
const repoPkg = JSON.parse(readFileSync(join(root, "npm", "package.json"), "utf8"));
const PKG = repoPkg.name;
const REPO_BIN = Object.keys(repoPkg.bin ?? {})[0];
if (!PKG || !REPO_BIN) {
  console.error("cleanroom: cannot read name/bin from npm/package.json");
  process.exit(1);
}
if (process.platform !== "win32") {
  console.error(`cleanroom: Windows-only gate (this host: ${process.platform})`);
  process.exit(1);
}

// ---- args ---------------------------------------------------------------
const argv = process.argv.slice(2);
const keep = argv.includes("--keep");
const ti = argv.indexOf("--tarball");
const registry = argv.includes("--registry");
const vi = argv.indexOf("--version");
const tarballArg = ti !== -1 ? argv[ti + 1] : null;
const versionArg = vi !== -1 ? argv[vi + 1] : null;
const bi = argv.indexOf("--bundle");
const bundleArg = bi !== -1 ? argv[bi + 1] : null;
const usage = "usage: node scripts/cleanroom-check.mjs --tarball <path.tgz> [--bundle <path.zip>] [--keep] | --registry --version <semver> [--keep]";
function dieUsage(msg) {
  console.error(`cleanroom: ${msg}\n${usage}`);
  process.exit(1);
}
if (tarballArg && registry) dieUsage("pick one mode: --tarball or --registry");
if (!tarballArg && !registry) dieUsage("pick one mode: --tarball or --registry");
if (registry && !/^\d+\.\d+\.\d+$/.test(versionArg ?? "")) dieUsage("--registry needs --version <semver>");
if (tarballArg && versionArg) dieUsage("--tarball takes a <path.tgz>, not --version");
if (tarballArg && tarballArg.startsWith("--")) dieUsage("--tarball needs a <path.tgz> value");
if (tarballArg && (!existsSync(resolve(tarballArg)) || statSync(resolve(tarballArg)).isDirectory())) {
  dieUsage(`tarball not found: ${tarballArg}`);
}
// --bundle is a tarball-mode input only: in --registry mode the launcher must
// provision from the public Release, so an override there would fake the gate.
if (bi !== -1 && !bundleArg) dieUsage("--bundle needs a <path.zip> value");
if (bundleArg && bundleArg.startsWith("--")) dieUsage("--bundle needs a <path.zip> value");
if (bundleArg && registry) dieUsage("--bundle applies to --tarball mode only; --registry must download from the public Release");
if (bundleArg && (!existsSync(resolve(bundleArg)) || statSync(resolve(bundleArg)).isDirectory())) {
  dieUsage(`bundle not found: ${bundleArg}`);
}

const rows = [];
const pass = (check, detail) => rows.push([check, "PASS", detail]);
const fail = (check, detail) => rows.push([check, "FAIL", detail]);
// Quote one argv element for cmd.exe. Paths here never contain `"`, so plain
// wrapping suffices; the command travels as ONE string with shell:true (Node
// passes it verbatim), never as an args array (libuv would backslash-escape
// the quotes and npm would receive them literally).
const q = (s) => `"${s}"`;

// ---- scrubbed world -----------------------------------------------------
const tmp = mkdtempSync(join(tmpdir(), "puppet-cleanroom-"));
const home = join(tmp, "home");
const localAppData = join(home, "AppData", "Local");
const roaming = join(home, "AppData", "Roaming");
const scratchTmp = join(tmp, "tmp");
const npmCache = join(tmp, "npm-cache");
const npmPrefix = join(tmp, "npm-prefix");
const appDir = join(tmp, "app");

const realEngineDir = process.env.LOCALAPPDATA
  ? join(process.env.LOCALAPPDATA, "puppet", "engine")
  : null;
const listNames = (dir) => {
  try {
    return readdirSync(dir).filter((n) => {
      try { return statSync(join(dir, n)).isDirectory(); } catch { return false; }
    }).sort();
  } catch { return []; }
};
const realBefore = realEngineDir ? listNames(realEngineDir) : [];

const childEnv = { ...process.env };
childEnv.USERPROFILE = home;
childEnv.HOME = home;
childEnv.LOCALAPPDATA = localAppData;
childEnv.APPDATA = roaming;
childEnv.TEMP = scratchTmp;
childEnv.TMP = scratchTmp;
childEnv.npm_config_cache = npmCache;
childEnv.npm_config_prefix = npmPrefix;
// A stale engine pin in the parent env would bypass the clean room; scrub it.
delete childEnv.PUPPET_ENGINE_CACHE;
// A local-bundle override is only ever supplied by this script, and only for
// the pre-publish tarball gate. Scrubbing it here means --registry can never
// inherit one and skip the real download.
delete childEnv.PUPPET_ENGINE_BUNDLE;
// Tarball mode + --bundle: hand the child a copy inside <tmp> so the gate still
// touches nothing outside its scratch dir. The launcher verifies the copy's
// size and sha256 against the installed manifest, so a wrong zip is refused.
if (tarballArg && bundleArg) {
  const stagedBundle = join(tmp, "bundle", basename(resolve(bundleArg)));
  mkdirSync(dirname(stagedBundle), { recursive: true });
  copyFileSync(resolve(bundleArg), stagedBundle);
  childEnv.PUPPET_ENGINE_BUNDLE = stagedBundle;
}

function sh(command, { timeout = STEP_TIMEOUT_MS } = {}) {
  // Single command string + shell:true: Node hands it to cmd.exe verbatim,
  // so our own quoting above is what cmd parses. (An args array here would
  // either warn DEP0190 or get libuv-escaped into literal quotes.)
  const r = spawnSync(command, {
    env: childEnv, cwd: tmp, encoding: "utf8", timeout, shell: true, windowsHide: true,
  });
  const tail = (s) => String(s ?? "").trim().split(/\r?\n/).slice(-5).join(" | ");
  return { code: r.status, out: String(r.stdout ?? ""), err: String(r.stderr ?? ""), tail: tail(r.stderr || r.stdout), timedOut: r.signal != null && r.error?.code === "ETIMEDOUT" };
}

function treeHasFile(dir) {
  let entries;
  try { entries = readdirSync(dir, { withFileTypes: true }); } catch { return false; }
  for (const e of entries) {
    if (e.isFile()) return true;
    if (e.isDirectory() && treeHasFile(join(dir, e.name))) return true;
  }
  return false;
}

function boot(command) {
  return new Promise((resolveBoot) => {
    const start = Date.now();
    const child = spawn(command, {
      env: childEnv, cwd: tmp, stdio: ["ignore", "pipe", "pipe"], shell: true, windowsHide: true,
    });
    let stderr = "";
    child.stderr.on("data", (d) => { stderr += d.toString(); });
    const timer = setTimeout(() => {
      try { child.kill("SIGKILL"); } catch { /* ignore */ }
      resolveBoot({ code: null, stderr, ms: Date.now() - start, timedOut: true });
    }, BOOT_TIMEOUT_MS);
    child.on("error", (err) => {
      clearTimeout(timer);
      resolveBoot({ code: null, stderr: `${stderr}\nspawn error: ${err.message}`, ms: Date.now() - start, timedOut: false });
    });
    child.on("close", (code) => {
      clearTimeout(timer);
      resolveBoot({ code, stderr, ms: Date.now() - start, timedOut: false });
    });
  });
}

function printTable() {
  const w = (s, n) => String(s).padEnd(n);
  console.log(`${w("CHECK", 12)}${w("RESULT", 8)}DETAIL`);
  for (const [c, r, d] of rows) console.log(`${w(c, 12)}${w(r, 8)}${d}`);
}

async function main() {
let exitCode = 0;
let engineDir = null;
let exeSize = null;
let readyPresent = false;
let bootMs = null;
let bootCode = null;
try {
  let binCommand; // command string that runs the bin (no args)
  let expected; // expected version string
  let manifest; // { runtime: { archive, server: { name, sha256, size } }, serverVersion }

  if (tarballArg) {
    const tgz = resolve(tarballArg);
    const inst = sh(`npm install --prefix ${q(appDir)} --no-audit --no-fund ${q(tgz)}`);
    if (inst.code !== 0) {
      fail("install", `npm install --prefix app <tarball> exit ${inst.code}: ${inst.tail}`);
      printTable();
      exitCode = 1;
      return;
    }
    // Resolve the installed package dir (normally node_modules/<PKG>).
    let pkgDir = join(appDir, "node_modules", PKG);
    if (!existsSync(join(pkgDir, "package.json"))) {
      pkgDir = null;
      try {
        for (const e of readdirSync(join(appDir, "node_modules"), { withFileTypes: true })) {
          if (e.isDirectory() && existsSync(join(appDir, "node_modules", e.name, "package.json"))) {
            const cand = JSON.parse(readFileSync(join(appDir, "node_modules", e.name, "package.json"), "utf8"));
            if (cand && typeof cand.version === "string" && cand.bin) { pkgDir = join(appDir, "node_modules", e.name); break; }
          }
        }
      } catch { /* ignore */ }
    }
    if (!pkgDir || !existsSync(join(pkgDir, "package.json"))) {
      fail("install", "installed package dir not found under app/node_modules");
      printTable();
      exitCode = 1;
      return;
    }
    const ipkg = JSON.parse(readFileSync(join(pkgDir, "package.json"), "utf8"));
    const binName = Object.keys(ipkg.bin ?? {})[0] ?? REPO_BIN;
    const binCmd = join(appDir, "node_modules", ".bin", `${binName}.cmd`);
    if (!existsSync(binCmd)) {
      fail("install", `.bin shim missing: ${binCmd}`);
      printTable();
      exitCode = 1;
      return;
    }
    expected = ipkg.version;
    manifest = JSON.parse(readFileSync(join(pkgDir, "engine-manifest.json"), "utf8"));
    binCommand = `${q(binCmd)}`;
    pass("install", `npm install --prefix app ${PKG}@${expected}`);
  } else {
    expected = versionArg;
    const probe = sh(`npx -y ${PKG}@${expected} --version`);
    if (probe.code !== 0) {
      fail("install", `npx -y ${PKG}@${expected} --version exit ${probe.code}: ${probe.tail}`);
      printTable();
      exitCode = 1;
      return;
    }
    if (!probe.out.includes(expected)) {
      fail("install", `npx probe output missing ${expected}: ${probe.out.trim().split(/\r?\n/).slice(-2).join(" | ")}`);
      printTable();
      exitCode = 1;
      return;
    }
    binCommand = `npx -y ${PKG}@${expected}`;
    pass("install", `npx -y ${PKG}@${expected} resolves`);
    try {
      const pm = sh(`${binCommand} --print-manifest`);
      const m = pm.out.match(/\{[\s\S]*\}/);
      const report = JSON.parse(m[0]);
      manifest = { ...report, runtime: report.runtime };
      if (!manifest.runtime || !manifest.runtime.server) throw new Error("no runtime in report");
    } catch (err) {
      fail("install", `cannot parse --print-manifest JSON: ${String(err.message ?? err).split("\n")[0]}`);
      printTable();
      exitCode = 1;
      return;
    }
  }

  // 1. --version output contains the expected version.
  const ver = sh(`${binCommand} --version`);
  if (ver.code === 0 && ver.out.includes(expected)) pass("version", `--version reports ${ver.out.trim().split(/\r?\n/)[0]}`);
  else fail("version", `exit ${ver.code}, output missing ${expected}: ${(ver.out || ver.err).trim().split(/\r?\n/).slice(-2).join(" | ")}`);

  // 2. --doctor exits 0.
  const doc = sh(`${binCommand} --doctor`);
  if (doc.code === 0) pass("doctor", "--doctor exit 0");
  else fail("doctor", `--doctor exit ${doc.code}: ${doc.tail}`);

  // 3. Real boot: spawn the bin, stdin closed, timeout 300 s.
  const b = await boot(binCommand);
  bootMs = b.ms;
  bootCode = b.code;
  const dirty = /error:/i.test(b.stderr) || /sha256 mismatch/i.test(b.stderr);
  if (!b.timedOut && b.code === 0 && !dirty) {
    pass("boot", `exit 0 in ${(b.ms / 1000).toFixed(1)}s, stderr clean`);
  } else if (b.timedOut) {
    fail("boot", `timeout after ${(b.ms / 1000).toFixed(1)}s; stderr tail: ${b.stderr.trim().split(/\r?\n/).slice(-3).join(" | ")}`);
  } else {
    fail("boot", `exit ${b.code}, stderr tail: ${b.stderr.trim().split(/\r?\n/).slice(-3).join(" | ")}`);
  }

  // 4. Temp engine path holds the manifest-sized exe plus .ready.
  const engRoot = join(localAppData, "puppet", "engine");
  const leaves = listNames(engRoot).filter((n) => n.startsWith(`${expected}-`));
  const wantSize = manifest?.runtime?.server?.size;
  const serverName = manifest?.runtime?.server?.name ?? "puppet.exe";
  if (leaves.length === 0) {
    fail("engine", `no ${expected}-* leaf under ${engRoot}`);
  } else {
    engineDir = join(engRoot, leaves[0]);
    const exe = join(engineDir, serverName);
    try {
      exeSize = statSync(exe).size;
    } catch { exeSize = null; }
    readyPresent = existsSync(join(engineDir, ".ready"));
    if (exeSize === null) fail("engine", `missing ${exe}`);
    else if (exeSize !== wantSize) fail("engine", `size ${exeSize} != manifest ${wantSize}: ${exe}`);
    else if (!readyPresent) fail("engine", `${exe} size OK but .ready missing`);
    else pass("engine", `${exe} size ${exeSize} == manifest, .ready present`);
  }

  // 5. The real %LOCALAPPDATA%\puppet\engine listing is identical before/after.
  const realAfter = realEngineDir ? listNames(realEngineDir) : [];
  const same = realBefore.join("\0") === realAfter.join("\0");
  if (same) pass("real-cache", `unchanged (${realAfter.length} dirs)`);
  else fail("real-cache", `before [${realBefore.join(",")}] != after [${realAfter.join(",")}]`);

  // 6. The npm cache was written under <tmp>.
  if (treeHasFile(npmCache)) pass("npm-cache", `written under ${npmCache}`);
  else fail("npm-cache", `empty: ${npmCache}`);

  printTable();
  console.log(`temp engine: ${engineDir ?? "(none)"}`);
  console.log(`exe size: ${exeSize ?? "(missing)"} (manifest ${wantSize ?? "(unknown)"})`);
  console.log(`.ready: ${readyPresent ? "present" : "missing"}`);
  console.log(`engine source: ${childEnv.PUPPET_ENGINE_BUNDLE ? `local bundle ${childEnv.PUPPET_ENGINE_BUNDLE}` : "Release download"}`);
  console.log(`boot: ${bootMs == null ? "(n/a)" : `${(bootMs / 1000).toFixed(1)}s`} exit ${bootCode}`);
  console.log(`real cache unchanged: ${same ? "yes" : "NO"} (${realBefore.length} -> ${realAfter.length} dirs)`);
  exitCode = rows.some((r) => r[1] === "FAIL") ? 1 : 0;
} finally {
  if (keep) {
    console.log(`kept: ${tmp}`);
  } else {
    try { rmSync(tmp, { recursive: true, force: true }); } catch { /* ignore */ }
  }
  process.exitCode = exitCode;
}
}
await main();
