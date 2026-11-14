#!/usr/bin/env node
// Structural gate for .github/workflows/release.yml: the release lane's
// ordering invariants, asserted on the raw workflow so an edit cannot quietly
// re-open the drift the derivation closed.
//
// This is NOT a YAML parser and does not try to be one: GitHub itself is the
// YAML gate (an unparsable workflow never runs). What this proves is the SHAPE
// of a workflow that does parse — that the version is derived before anything
// asserts it, that no step writes a version, that the changelog section closes
// after the test and before any tag, and that the tag is created in the publish
// job immediately before the release is attached.
//
// It reads the file's actual structure (jobs at 2 spaces, steps as `      - `
// items), so any reordering, removal or rename that breaks an invariant fails
// here in milliseconds instead of in a 20-minute release run. It is called by
// the release lane itself, first step after checkout.
//
// Usage: node scripts/check-release-workflow.mjs [--file <path>]
// Exit 0 when every invariant holds, 1 otherwise. No dependencies.

import { readFileSync } from "node:fs";
import { dirname, join, resolve } from "node:path";
import { fileURLToPath } from "node:url";

const SELF_ROOT = resolve(dirname(fileURLToPath(import.meta.url)), "..");
const fileArg = process.argv.indexOf("--file");
const file = fileArg >= 0 ? resolve(process.argv[fileArg + 1]) : join(SELF_ROOT, ".github", "workflows", "release.yml");

let text;
try {
  text = readFileSync(file, "utf8");
} catch (err) {
  console.error(`cannot read ${file}: ${err.message}`);
  process.exit(1);
}

const lines = text.split(/\r?\n/);

// A job is `  <name>:` directly under `jobs:`; its steps are `      - ` items.
function jobText(name) {
  const start = lines.findIndex((l) => new RegExp(`^  ${name}:\\s*$`).test(l));
  if (start < 0) return null;
  let end = lines.length;
  for (let i = start + 1; i < lines.length; i += 1) {
    if (/^  [A-Za-z0-9_-]+:\s*$/.test(lines[i])) { end = i; break; }
    if (/^[A-Za-z]/.test(lines[i])) { end = i; break; }
  }
  return lines.slice(start, end).join("\n");
}

function steps(job) {
  if (job === null) return [];
  const out = [];
  const rows = job.split("\n");
  for (let i = 0; i < rows.length; i += 1) {
    const m = /^      - (.*)$/.exec(rows[i]);
    if (!m) continue;
    const body = [m[1]];
    for (let j = i + 1; j < rows.length; j += 1) {
      if (/^      - /.test(rows[j])) break;
      body.push(rows[j]);
    }
    const raw = body.join("\n");
    const name = /^name: (.+)$/m.exec(raw)?.[1]?.trim() ?? null;
    const uses = /^uses: (.+)$/m.exec(raw)?.[1]?.trim() ?? null;
    out.push({ name, uses, raw });
  }
  return out;
}

const build = jobText("build");
const publish = jobText("publish");
const buildSteps = steps(build);
const publishSteps = steps(publish);
const names = (list) => list.map((s) => s.name ?? s.uses ?? "(unnamed)");
const indexOf = (list, re) => list.findIndex((s) => (s.name !== null && re.test(s.name)) || (s.uses !== null && re.test(s.uses)));
const at = (list, re) => {
  const i = indexOf(list, re);
  if (i < 0) throw new Error(`build/publish step matching ${re} not found; found: ${names(list).join(" | ")}`);
  return i;
};

const rows = [];
const check = (name, fn) => {
  try {
    const detail = fn();
    rows.push([name, "PASS", detail]);
  } catch (err) {
    rows.push([name, "FAIL", String(err?.message ?? err).split("\n")[0]]);
  }
};
const need = (cond, msg) => { if (!cond) throw new Error(msg); };

// ---- trigger: the version is never an input -------------------------------
check("dispatch-inputs", () => {
  const start = lines.findIndex((l) => /^  workflow_dispatch:\s*$/.test(l));
  need(start >= 0, "workflow_dispatch trigger is missing");
  const inputsAt = lines.findIndex((l, i) => i > start && /^    inputs:\s*$/.test(l));
  need(inputsAt >= 0, "workflow_dispatch has no inputs block");
  const got = [];
  for (let i = inputsAt + 1; i < lines.length; i += 1) {
    if (/^  [A-Za-z0-9_-]+:\s*$/.test(lines[i]) || /^[A-Za-z]/.test(lines[i])) break;
    const m = /^      ([A-Za-z0-9_-]+):\s*$/.exec(lines[i]);
    if (m) got.push(m[1]);
  }
  const sorted = [...got].sort().join(",");
  need(sorted === "commit,dry_run", `dispatch inputs are ${sorted || "(none)"}; expected commit,dry_run (a typed version is the drift this lane removed)`);
  return "commit + dry_run only, no version input";
});

check("trigger-push-tags", () => {
  need(/^  push:\s*$/m.test(text) && /^\s+- 'v\*'\s*$/m.test(text), "push on v* tags is missing (a rerun of an existing release)");
  return "push v* kept for release reruns";
});

check("concurrency", () => {
  const m = /^concurrency:\s*\n\s+group: (.+)$/m.exec(text);
  need(m, "concurrency.group is missing");
  need(m[1].trim() === "release-${{ github.repository }}", `concurrency.group is '${m[1].trim()}'`);
  return "release-${{ github.repository }}";
});

// ---- checkout: the dispatched ref is the source, the commit input is an
// assertion, and the derivation needs the full history and tags -------------
check("checkout-full-history-no-ref", () => {
  for (const [label, list] of [["build", buildSteps], ["publish", publishSteps]]) {
    const i = at(list, /^actions\/checkout/);
    const raw = list[i].raw;
    need(/fetch-depth: 0/.test(raw), `${label} checkout lacks fetch-depth: 0 (the derivation needs tags and history)`);
    need(!/^\s+ref:/m.test(raw), `${label} checkout sets ref: (the dispatched ref must be the source)`);
  }
  return "both checkouts: fetch-depth 0, no ref:";
});

// ---- the derivation happens before anything asserts a version -------------
check("build-derive-first", () => {
  const derive = at(buildSteps, /^derive release version$/);
  const refAssert = at(buildSteps, /^assert the ref names the derived version$/);
  const versionAssert = at(buildSteps, /^assert versions/);
  const commitAssert = at(buildSteps, /^assert the commit input/);
  need(derive < refAssert, `derive (#${derive}) must precede the ref assert (#${refAssert})`);
  need(derive < versionAssert, `derive (#${derive}) must precede the version assert (#${versionAssert})`);
  need(commitAssert < derive, `the commit-input assertion (#${commitAssert}) must precede the derive (#${derive})`);
  return `commit-input #${commitAssert} < derive #${derive} < ref #${refAssert} < versions #${versionAssert}`;
});

check("publish-rederives", () => {
  const redrive = at(publishSteps, /^re-derive on the release commit$/);
  const tag = at(publishSteps, /^create the release tag$/);
  need(redrive < tag, `publish must re-derive (#${redrive}) before creating the tag (#${tag})`);
  return `re-derive #${redrive} < tag #${tag}`;
});

// ---- no writer: the lane asserts every manifest, it never repairs one -----
check("no-version-writer", () => {
  const all = [...buildSteps, ...publishSteps];
  need(indexOf(all, /^apply the derived version$/) < 0, "an 'apply the derived version' step exists; this lane asserts, never writes");
  for (const s of all) {
    need(!/derive-release\.mjs\s*$/m.test(s.raw), `step '${s.name}' runs derive-release.mjs with no mode: that is the writer path, and this lane has none`);
  }
  return "no apply step, no bare (writer) derive-release invocation";
});

check("manifest-asserts-present", () => {
  const versions = at(buildSteps, /^assert versions/);
  const raw = buildSteps[versions].raw;
  for (const f of ["Cargo.toml", "npm/package.json", "npm/engine-manifest.json"]) {
    need(raw.includes(f), `the version assert does not check ${f}`);
  }
  need(/\$ver\s*=\s*"\$\{\{ steps\.derive\.outputs\.version \}\}"/.test(raw), "the version assert does not read the derived version");
  return "Cargo.toml + npm/package.json + npm/engine-manifest.json asserted against the derivation";
});

// ---- changelog closes after the gate, and the tag exists only after that --
check("changelog-after-test", () => {
  const test = at(buildSteps, /^test$/);
  const close = at(buildSteps, /^close the changelog section$/);
  need(test < close, `the changelog closes (#${close}) before the test (#${test}); a red build must not close a section`);
  return `test #${test} < close #${close}`;
});

check("changelog-before-tag", () => {
  need(indexOf(buildSteps, /^create the release tag$/) < 0, "the build job creates a tag; the tag belongs to publish");
  need(!/^\s+git tag /m.test(build), "the build job runs `git tag`");
  const close = at(buildSteps, /^close the changelog section$/);
  need(close >= 0, "the changelog is never closed");
  return "closed in build after the test; no tag in build";
});

check("tag-before-attach", () => {
  const tag = at(publishSteps, /^create the release tag$/);
  const attach = at(publishSteps, /^attach archives to the GitHub Release$/);
  need(tag + 1 === attach, `the tag (#${tag}) must be created immediately before the attach (#${attach}), found #${tag} then #${attach}`);
  const raw = publishSteps[tag].raw;
  need(/release_mode\.outputs\.current/.test(raw), "the tag step is not guarded by the stale-tag check");
  need(/inputs\.dry_run == false/.test(raw), "the tag step is not guarded by dry_run");
  return `tag #${tag} immediately before attach #${attach}, guarded by stale check + dry_run`;
});

// ---- the publish assertions read the derived tag, not GITHUB_REF ----------
check("publish-reads-derived-tag", () => {
  const env = /^  publish:[\s\S]*?\n    env:\n      RELEASE_TAG: (.+)$/m.exec(text);
  need(env, "publish has no RELEASE_TAG env");
  need(env[1].trim() === "v${{ needs.build.outputs.version }}", `publish RELEASE_TAG is '${env[1].trim()}', not built from needs.build.outputs.version`);
  need(!/inputs\.version/.test(text), "the workflow still references inputs.version");
  return "publish tags v${{ needs.build.outputs.version }}";
});

check("retain-historical-releases", () => {
  const cleanup = publishSteps.find((s) => /delete outgoing releases/i.test(s.name ?? ""));
  need(!cleanup, "publish must retain historical releases and their runtime assets");
  need(!/gh release delete/.test(publish), "publish runs gh release delete");
  return "historical releases and runtime assets are retained";
});

const w = (s, n) => s.padEnd(n);
console.log(`${w("INVARIANT", 30)}${w("RESULT", 8)}DETAIL`);
for (const [name, ok, detail] of rows) console.log(`${w(name, 30)}${w(ok, 8)}${detail}`);
const failed = rows.filter(([, r]) => r === "FAIL").length;
console.log(`\n${rows.length - failed}/${rows.length} invariants hold (${file})`);
process.exitCode = failed === 0 ? 0 : 1;
