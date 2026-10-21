#!/usr/bin/env node
// Local release preflight: pack the npm package and assert it is publishable.
// Usage: node scripts/release-check.mjs [--allow-placeholders] [--compare <release-manifest.json>]
// --compare asserts the tarball's runtime.archive.sha256 and
// runtime.server.sha256 equal the given release manifest's (the class of
// bug where a stale manifest ships inside a good-looking tarball).
// No dependencies beyond Node stdlib.
import { execSync } from "node:child_process";
import { mkdtempSync, readFileSync, rmSync } from "node:fs";
import { tmpdir } from "node:os";
import { dirname, join, resolve } from "node:path";
import { gunzipSync } from "node:zlib";
import { fileURLToPath } from "node:url";

const root = resolve(dirname(fileURLToPath(import.meta.url)), "..");
const npmDir = join(root, "npm");
const allowPlaceholders = process.argv.includes("--allow-placeholders");

function parseTar(buf) {
  const entries = [];
  let off = 0;
  while (off + 512 <= buf.length) {
    const head = buf.subarray(off, off + 512);
    let allZero = true;
    for (let i = 0; i < 512; i++) {
      if (head[i] !== 0) { allZero = false; break; }
    }
    if (allZero) break;
    const name = head.subarray(0, 100).toString("utf8").replace(/\0.*$/s, "");
    const sizeOct = head.subarray(124, 136).toString("utf8").replace(/\0.*$/s, "").trim();
    const size = sizeOct === "" ? 0 : parseInt(sizeOct, 8);
    if (!Number.isFinite(size) || size < 0) throw new Error(`bad tar size for ${name}: ${sizeOct}`);
    entries.push({ name, size, data: Buffer.from(buf.subarray(off + 512, off + 512 + size)) });
    off += 512 + Math.ceil(size / 512) * 512;
  }
  return entries;
}

const rows = [];
const fail = (check, detail) => rows.push([check, "FAIL", detail]);
const pass = (check, detail) => rows.push([check, "PASS", detail]);

let tarballPath = null;
const tmp = mkdtempSync(join(tmpdir(), "release-check-"));
try {
  const out = execSync(`npm pack --json --pack-destination "${tmp}"`, {
    cwd: npmDir, encoding: "utf8",
  });
  const parsed = JSON.parse(out);
  const entry = Array.isArray(parsed) ? parsed[0] : Object.values(parsed)[0];
  const filename = entry?.filename;
  if (!filename) throw new Error("npm pack --json returned no filename");
  tarballPath = join(tmp, filename);
} catch (err) {
  fail("npm-pack", String(err?.message ?? err).split("\n")[0]);
  printAndExit(1);
}

let names;
let manifestText = null;
try {
  const gz = readFileSync(tarballPath);
  const tar = gunzipSync(gz);
  const entries = parseTar(tar);
  names = entries.map((e) => e.name.replace(/^package\//, "")).filter(Boolean);
  const man = entries.find((e) => e.name === "package/engine-manifest.json");
  if (man) manifestText = man.data.toString("utf8");
} catch (err) {
  fail("tarball-extract", String(err?.message ?? err).split("\n")[0]);
  printAndExit(1);
}

const pkg = JSON.parse(readFileSync(join(npmDir, "package.json"), "utf8"));
let tarballManifest = null;

if (manifestText === null) {
  fail("manifest-present", "engine-manifest.json missing from tarball");
} else {
  pass("manifest-present", "engine-manifest.json in tarball");
  let man;
  try {
    man = JSON.parse(manifestText);
  } catch {
    fail("manifest-json", "engine-manifest.json is not valid JSON");
  }
  if (man) {
    tarballManifest = man;
    if (man.serverVersion === pkg.version) pass("serverVersion", `manifest == package == ${pkg.version}`);
    else fail("serverVersion", `manifest ${man.serverVersion} != package ${pkg.version}`);
    const count = manifestText.split("RELEASE_WORKFLOW_FILLS_THIS").length - 1;
    if (count === 0) pass("placeholders", "0 placeholders");
    else if (allowPlaceholders) pass("placeholders", `${count} placeholder(s) allowed by flag`);
    else fail("placeholders", `${count} RELEASE_WORKFLOW_FILLS_THIS (workflow fills at release)`);
  }
}

const hit = (pred) => names.filter(pred);
const exe = hit((n) => n.toLowerCase().endsWith(".exe"));
const env = hit((n) => n.split("/").some((s) => s === ".env" || s.toLowerCase().endsWith(".env")));
const secret = hit((n) => n.toLowerCase().includes("secret"));
const modules = hit((n) => n === "node_modules" || n.startsWith("node_modules/"));

if (exe.length === 0) pass("no-exe", "no .exe in tarball");
else fail("no-exe", exe.slice(0, 5).join(", "));
if (env.length === 0) pass("no-env", "no .env in tarball");
else fail("no-env", env.slice(0, 5).join(", "));
if (secret.length === 0) pass("no-secret", "no *secret* in tarball");
else fail("no-secret", secret.slice(0, 5).join(", "));
if (modules.length === 0) pass("no-node_modules", "no node_modules in tarball");
else fail("no-node_modules", modules.slice(0, 5).join(", "));

const compareIdx = process.argv.indexOf("--compare");
if (compareIdx !== -1) {
  const refArg = process.argv[compareIdx + 1];
  if (!refArg || refArg.startsWith("--")) {
    fail("compare-arg", "--compare requires a <release-manifest.json> path");
  } else {
    let ref = null;
    try {
      ref = JSON.parse(readFileSync(resolve(root, refArg), "utf8"));
    } catch {
      fail("compare-ref", `cannot read ${refArg}`);
    }
    if (ref) {
      if (!tarballManifest) {
        fail("compare-hashes", "no tarball manifest to compare");
      } else {
        const get = (o, p) => p.split(".").reduce((x, k) => (x == null ? x : x[k]), o);
        for (const field of ["runtime.archive.sha256", "runtime.server.sha256"]) {
          const a = get(tarballManifest, field);
          const b = get(ref, field);
          const short = (v) => (typeof v === "string" ? v.slice(0, 12) : String(v));
          const name = `compare-${field.split(".")[1]}`;
          if (typeof a === "string" && a === b) pass(name, `${field} ${short(a)}… matches ${refArg}`);
          else fail(name, `${field} tarball ${short(a)} != reference ${short(b)}`);
        }
      }
    }
  }
}

printAndExit(rows.some((r) => r[1] === "FAIL") ? 1 : 0);

function printAndExit(code) {
  const w = (s, n) => s.padEnd(n);
  console.log(`${w("CHECK", 18)}${w("RESULT", 8)}DETAIL`);
  for (const [c, r, d] of rows) console.log(`${w(c, 18)}${w(r, 8)}${d}`);
  try { rmSync(tmp, { recursive: true, force: true }); } catch { /* ignore */ }
  process.exit(code);
}
