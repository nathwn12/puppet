#!/usr/bin/env node
// Payload-vs-schema conformance gate for the BUILT artifact: spawn the
// binary this job just built, run one tools/call per listed tool, and
// validate every structuredContent against that tool's OWN declared
// outputSchema from tools/list — the same rule as
// tests/envelope_truth.rs (required keys on success; per-key JSON types
// always; integer satisfies number; unlisted keys pass only because every
// schema sets additionalProperties: true; props declaring no type pass
// unconstrained). The diff row uses two identical PNGs (one screenshot
// written with out, diffed against itself), so the bbox:null /
// diff_out:null shape that once shipped in violation of the schema is
// proven on the release binary before publish can run.
//
// Usage: node scripts/conformance-check.mjs --exe <path-to-puppet.exe>
// Windows-only; Node stdlib only. Prints a CHECK/RESULT table, exits 1 on
// any FAIL. Cleans its temp dir.
import { spawn } from "node:child_process";
import { existsSync, mkdirSync, mkdtempSync, rmSync, writeFileSync } from "node:fs";
import { tmpdir } from "node:os";
import { join, resolve } from "node:path";

if (process.platform !== "win32") {
  console.error(`conformance: Windows-only gate (this host: ${process.platform})`);
  process.exit(1);
}
const ei = process.argv.indexOf("--exe");
const exeArg = ei !== -1 ? process.argv[ei + 1] : null;
if (!exeArg || exeArg.startsWith("--") || !existsSync(resolve(exeArg))) {
  console.error(`usage: node scripts/conformance-check.mjs --exe <path-to-puppet.exe> (not found: ${exeArg})`);
  process.exit(1);
}
const exe = resolve(exeArg);

const rows = [];
const pass = (check, detail) => rows.push([check, "PASS", detail]);
const fail = (check, detail) => rows.push([check, "FAIL", detail]);

const work = mkdtempSync(join(tmpdir(), "puppet-conform-"));
const png = join(work, "shot.png");

function start() {
  return spawn(exe, [], { stdio: ["pipe", "pipe", "ignore"], windowsHide: true });
}

function session() {
  const child = start();
  let buf = "";
  const waiters = [];
  child.stdout.on("data", (d) => {
    buf += d.toString();
    let idx;
    while ((idx = buf.indexOf("\n")) >= 0) {
      const line = buf.slice(0, idx).trim();
      buf = buf.slice(idx + 1);
      if (!line) continue;
      const w = waiters.shift();
      if (w) w(JSON.parse(line));
    }
  });
  let id = 0;
  const send = (method, params, timeoutMs = 60000) => new Promise((res, rej) => {
    const timer = setTimeout(() => rej(new Error(`${method} timed out after ${timeoutMs}ms`)), timeoutMs);
    waiters.push((msg) => { clearTimeout(timer); res(msg); });
    child.stdin.write(JSON.stringify({ jsonrpc: "2.0", id: ++id, method, params }) + "\n", (err) => {
      if (err) { clearTimeout(timer); rej(err); }
    });
  });
  return {
    list: () => send("tools/list", {}),
    call: (name, args) => send("tools/call", { name, arguments: args }),
    kill() { try { child.kill(); } catch { /* ignore */ } },
  };
}

function allowedTypes(prop) {
  const out = [];
  const pushT = (t) => {
    if (typeof t === "string") out.push(t);
    else if (Array.isArray(t)) for (const x of t) if (typeof x === "string") out.push(x);
  };
  if (prop.type !== undefined) pushT(prop.type);
  for (const key of ["anyOf", "oneOf"]) {
    if (Array.isArray(prop[key])) for (const v of prop[key]) if (v && v.type !== undefined) pushT(v.type);
  }
  return out;
}
function jsonType(v) {
  if (v === null) return "null";
  if (Array.isArray(v)) return "array";
  switch (typeof v) {
    case "boolean": return "boolean";
    case "string": return "string";
    case "number": return Number.isInteger(v) ? "integer" : "number";
    case "object": return "object";
    default: return typeof v;
  }
}
function conforms(tool, schema, envelope, what) {
  const sc = envelope?.structuredContent;
  if (!sc || typeof sc !== "object" || Array.isArray(sc)) {
    return `${what}: no structuredContent object`;
  }
  const props = schema?.properties;
  if (!props || typeof props !== "object") return `${what}: outputSchema needs properties`;
  const isErr = envelope.isError === true;
  if (!isErr) {
    const required = Array.isArray(schema.required) ? schema.required : [];
    for (const name of required) {
      if (!(name in sc)) return `${what}: success payload missing required \`${name}\``;
    }
  } else if (sc.ok !== false) {
    return `${what}: error envelope must carry ok:false`;
  }
  for (const [k, v] of Object.entries(sc)) {
    const prop = props[k];
    if (!prop) continue;
    const allowed = allowedTypes(prop);
    if (allowed.length === 0) continue;
    const actual = jsonType(v);
    if (!allowed.includes(actual) && !(actual === "integer" && allowed.includes("number"))) {
      return `${what}: field \`${k}\` value ${JSON.stringify(v)} (type ${actual}) not allowed`;
    }
  }
  return null;
}

async function main() {
  const s = session();
  let exitCode = 0;
  try {
    const listed = await s.list();
    const tools = listed?.result?.tools;
    if (!Array.isArray(tools) || tools.length === 0) {
      fail("tools/list", "no tools listed");
      printTable();
      process.exitCode = 1;
      return;
    }
    const schemas = new Map();
    for (const t of tools) {
      if (t?.name && t?.outputSchema) schemas.set(t.name, t.outputSchema);
    }
    const covered = new Set();
    const row = async (tool, args, what, expectErr) => {
      let env;
      try {
        env = (await s.call(tool, args)).result;
      } catch (err) {
        fail(tool, `${what}: ${err.message}`);
        return;
      }
      if (!schemas.has(tool)) {
        fail(tool, `${what}: ${tool} declares no outputSchema`);
        return;
      }
      const err = conforms(tool, schemas.get(tool), env, what);
      const wantErr = expectErr === true;
      const gotErr = env?.isError === true;
      if (err !== null) {
        fail(tool, err);
      } else if (wantErr && !gotErr) {
        fail(tool, `${what}: expected a tool error, got success`);
      } else {
        covered.add(tool);
        pass(tool, `${what}${gotErr ? " (typed refusal)" : ""}`);
      }
    };

    await row("cursor", {}, "cursor empty args");
    const lw = await s.call("list_windows", {});
    await row("list_windows", {}, "list_windows empty args");
    const wins = lw?.result?.structuredContent?.windows ?? [];

    await row("screenshot", { display: -1, region: [0, 0, 64, 64], out: png }, "screenshot out set");
    if (!existsSync(png)) fail("screenshot", `out file missing: ${png}`);

    await row("diff", { base: { path: png }, next: { path: png } }, "diff identical PNGs");

    await row("window", { action: "focus", title: "NoSuchWindowXYZ12345" }, "window unknown target", true);

    await row("do",
      { actions: [{ type: "click", x: 5, y: 5 }], dry_run: true, allow_foreground: true },
      "do posted-key refusal", true);

    if (wins.length > 0 && Number.isInteger(wins[0]?.hwnd)) {
      await row("elements", { hwnd: wins[0].hwnd }, `elements on hwnd ${wins[0].hwnd}`);
    } else {
      await row("elements", { title: "NoSuchWindowXYZ12345" }, "elements unknown target", true);
    }

    for (const name of schemas.keys()) {
      if (!covered.has(name)) fail("coverage", `tool ${name} has no passing conformance row`);
    }
    if (!rows.some((r) => r[0] === "coverage" && r[1] === "FAIL")) {
      pass("coverage", `${covered.size}/${schemas.size} tools with a passing row`);
    }
    printTable();
    exitCode = rows.some((r) => r[1] === "FAIL") ? 1 : 0;
  } catch (err) {
    fail("conformance", String(err?.message ?? err).split("\n")[0]);
    printTable();
    exitCode = 1;
  } finally {
    s.kill();
    try { rmSync(work, { recursive: true, force: true }); } catch { /* ignore */ }
    process.exitCode = exitCode;
  }
}

function printTable() {
  const w = (s, n) => String(s).padEnd(n);
  console.log(`${w("CHECK", 14)}${w("RESULT", 8)}DETAIL`);
  for (const [c, r, d] of rows) console.log(`${w(c, 14)}${w(r, 8)}${d}`);
}

await main();
