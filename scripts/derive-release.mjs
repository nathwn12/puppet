#!/usr/bin/env node
// Derive the release version from history, so the release run produces the
// version and the tag instead of trusting a hand-typed number.
//
// The old lane took its version FROM a dispatch input: `version: 6.1.4`
// decided everything and the workflow only checked that the tree matched it.
// A typed version that disagreed with the manifests, or a tag cut before the
// gate was green, could only be caught by a human reading the log. Here the
// history is the input: the commits since the last release decide the hop, the
// manifests and the changelog are checked against it, and the tag is created
// from it at the end of the publish job (see .github/workflows/release.yml).
//
// MANIFESTS ARE ASSERTED, NEVER WRITTEN. This script reads Cargo.toml,
// Cargo.lock, npm/package.json and npm/engine-manifest.json and refuses unless
// every one of them ALREADY carries the derived version, so the release either
// matches its history or stops in seconds. The release lane never repairs a
// version: a tree that disagrees with its own commits is a defect to land, not
// a number for CI to guess. The only file written here is CHANGELOG.md, in
// --changelog mode, after the gate is green.
//
// LEVEL
//   major  a commit with `!` before the `:` in its subject, or a
//          `BREAKING CHANGE:` / `BREAKING-CHANGE:` trailer in its body
//   minor  any `feat:`
//   patch  any `fix:` or `perf:`
//   refuse everything else: a release with no user-visible change in it is
//          not a release, it is a rerun waiting to happen.
//   On 0.x a major hop moves the MINOR, so 0.6.2 + breaking -> 0.7.0.
//
// MODES
//   (default)          derive + validate, print the derivation, write nothing.
//   --dry-run          same, spelled out, and said so in the output.
//   --json             derive + validate, print machine JSON, write nothing.
//   --changelog        close `## [Unreleased]` into `## [X.Y.Z] - <UTC date>`.
//                      Idempotent: a second run over an already-closed section
//                      changes nothing.
//   --check            run the fixture cases; exit 0 only if every one passes.
//   --repo <dir>       operate on <dir> instead of this script's parent.
//   --date YYYY-MM-DD  heading date for --changelog (default: UTC today).
//   --notes-out <path> also write the closed section as release-body markdown.
//
// No dependencies beyond Node stdlib and git.

import { execFileSync } from "node:child_process";
import { existsSync, mkdtempSync, mkdirSync, readFileSync, rmSync, writeFileSync } from "node:fs";
import { tmpdir } from "node:os";
import { dirname, join, resolve } from "node:path";
import { fileURLToPath } from "node:url";

const SELF_ROOT = resolve(dirname(fileURLToPath(import.meta.url)), "..");

const CONVENTIONAL = /^([a-z]+)(?:\(([^()]*)\))?(!)?: (.+)$/;
// Git writes its own revert subject as `Revert "<original subject>"`, which is
// not `type(scope): description`. The Conventional Commits spec has a type for
// exactly this intent (`revert: <original subject>`), so the shape is accepted
// here - and ONLY when the quoted original is itself a known conventional
// subject. `Revert the diff change` and `Revert "Fixed the thing"` stay
// refusals: a revert commit is exempt from the shape only because it names a
// commit that was not.
const GIT_REVERT = /^Revert "(.+)"$/;
const TYPES = new Set([
  "feat", "fix", "perf", "docs", "chore", "test", "build", "ci", "style", "refactor", "revert",
]);
const BUMP = { feat: "minor", fix: "patch", perf: "patch" };
const CRATE = "puppet";
const REPO_URL = "https://github.com/nathwn12/puppet";
const FILES = {
  cargo: "Cargo.toml",
  lock: "Cargo.lock",
  pkg: "npm/package.json",
  man: "npm/engine-manifest.json",
  changelog: "CHANGELOG.md",
};


class Refusal extends Error {
  constructor(code, message) {
    super(message);
    this.code = code;
  }
}

const refuse = (code, message) => {
  throw new Refusal(code, message);
};


const SEMVER = /^(\d+)\.(\d+)\.(\d+)$/;

function cmpSemver(a, b) {
  const [x, y] = [SEMVER.exec(a), SEMVER.exec(b)];
  for (let i = 1; i <= 3; i += 1) {
    const d = Number(x[i]) - Number(y[i]);
    if (d !== 0) return d;
  }
  return 0;
}

// 0.x: a major hop moves the minor (the brief's rule), and so does a minor
// hop; the two are indistinguishable before 1.0 and that is deliberate.
function bumpVersion(version, level) {
  const [maj, min, pat] = SEMVER.exec(version).slice(1).map(Number);
  if (maj === 0) {
    if (level === "major" || level === "minor") return `0.${min + 1}.0`;
    return `0.${min}.${pat + 1}`;
  }
  if (level === "major") return `${maj + 1}.0.0`;
  if (level === "minor") return `${maj}.${min + 1}.0`;
  return `${maj}.${min}.${pat + 1}`;
}


function git(root, args) {
  return execFileSync("git", ["-C", root, ...args], {
    encoding: "utf8",
    maxBuffer: 64 * 1024 * 1024,
    stdio: ["ignore", "pipe", "pipe"],
  });
}

function headSha(root) {
  try {
    return git(root, ["rev-parse", "--verify", "HEAD"]).trim();
  } catch {
    refuse("no-git", `${root} is not a git work tree with a HEAD commit`);
  }
}

// Highest `vX.Y.Z` tag reachable from HEAD, plus the tag sitting exactly on
// HEAD (a rerun of an existing release, which must resolve to its own version
// rather than derive a new one from an empty range).
function readTags(root) {
  const head = headSha(root);
  let names;
  try {
    names = git(root, ["tag", "--merged", "HEAD", "--list", "v*"])
      .split(/\r?\n/)
      .map((s) => s.trim())
      .filter((s) => SEMVER.test(s.replace(/^v/, "")));
  } catch {
    names = [];
  }
  const tags = names
    .map((name) => {
      let commit = "";
      try {
        commit = git(root, ["rev-parse", "--verify", "-q", `refs/tags/${name}^{commit}`]).trim();
      } catch {
        commit = "";
      }
      return { name, version: name.replace(/^v/, ""), commit };
    })
    .filter((t) => t.commit !== "");
  tags.sort((a, b) => cmpSemver(a.version, b.version));
  const atHead = tags.filter((t) => t.commit === head);
  const below = tags.filter((t) => t.commit !== head);
  return {
    head,
    headTag: atHead.length ? atHead[atHead.length - 1] : null,
    base: below.length ? below[below.length - 1] : null,
    all: tags,
  };
}

function readCommits(root, base) {
  const range = base ? `${base.name}..HEAD` : "HEAD";
  const out = git(root, ["log", "--no-merges", "--format=%H%x09%s%x09%b%x1e", range]);
  return out
    .split("\x1e")
    .map((chunk) => chunk.replace(/^\r?\n/, ""))
    .filter((chunk) => chunk.trim() !== "")
    .map((chunk) => {
      const [sha, subject, ...rest] = chunk.split("\t");
      return { sha: sha.trim(), subject: (subject ?? "").trim(), body: rest.join("\t").trim() };
    })
    .filter((c) => /^[0-9a-f]{40}$/.test(c.sha));
}


function classify(commit) {
  const body = commit.body ?? "";
  const breakingBody = /^BREAKING[ -]CHANGE:/m.test(body);
  const record = { ...commit, breaking: breakingBody, bump: null, ignored: false };
  // A git revert names a conventional subject it undoes. The revert carries no
  // hop of its own: the hop belongs to the change being reverted, which is in
  // the same range but already counted (or not) on its own merits.
  const revert = GIT_REVERT.exec(commit.subject);
  if (revert) {
    const original = CONVENTIONAL.exec(revert[1]);
    if (!original || !TYPES.has(original[1])) {
      record.bad = true;
      return record;
    }
    record.type = "revert";
    record.scope = null;
    record.bang = false;
    record.reverts = revert[1];
    if (record.breaking) record.bump = "major";
    else record.ignored = true;
    return record;
  }
  const m = CONVENTIONAL.exec(commit.subject);
  if (!m) {
    record.bad = true;
    return record;
  }
  record.type = m[1];
  record.scope = m[2] ?? null;
  record.bang = Boolean(m[3]);
  if (!TYPES.has(record.type)) {
    record.bad = true;
    return record;
  }
  record.breaking = record.breaking || record.bang;
  if (record.breaking) record.bump = "major";
  else if (BUMP[record.type]) record.bump = BUMP[record.type];
  else record.ignored = true;
  // `revert: <original subject>` names the same change the git shape names.
  if (record.type === "revert") record.reverts = m[4];
  return record;
}

// A revert names a change it undoes. When that change is an EARLIER non-merge
// commit in the same range and carried a hop, the pair cancels: the tree after
// the revert is byte-identical to the tree before the original, so a release
// derived from the pair would ship nothing while burning a version. Both
// records become ignored (no bump on either side). The match is the exact
// subject string, never approximate; a revert of a docs/chore/test subject
// changes nothing because that subject had no hop to cancel; a revert naming a
// subject outside the range (or a merge subject, which is not in the range)
// stays the plain ignored revert it is today. A revert with a bump OF ITS OWN
// (`revert!:` or a BREAKING CHANGE trailer) is exempt: it changes behavior by
// itself, so it keeps its major hop and cancels nothing - a cancellation that
// swallowed a breaking change would ship a breaking revert as no release.
function cancelRevertedHops(records) {
  for (let i = 0; i < records.length; i += 1) {
    const revert = records[i];
    if (!revert.reverts) continue;
    if (revert.bump !== null) continue;
    // `git log` lists newest first, so the reverted commit, if it is in the
    // range, is OLDER than the revert: a higher index. A revert that predates
    // the change it names cancels nothing.
    const original = records
      .slice(i + 1)
      .find((r) => !r.reverts && r.subject === revert.reverts && r.bump !== null);
    if (!original) continue;
    original.bump = null;
    original.ignored = true;
    original.cancelledBy = revert.sha;
    revert.bump = null;
    revert.ignored = true;
    revert.cancels = original.sha;
  }
}

function deriveLevel(records) {
  if (records.length === 0) {
    refuse("empty-range", "no non-merge commits since the base: there is no release to derive");
  }
  const bad = records.filter((r) => r.bad);
  if (bad.length) {
    refuse(
      "non-conventional",
      `${bad.length} commit subject(s) are not Conventional Commits:\n` +
        bad.map((r) => `  ${r.sha.slice(0, 8)} ${JSON.stringify(r.subject)}`).join("\n"),
    );
  }
  if (records.some((r) => r.bump === "major")) return "major";
  if (records.some((r) => r.bump === "minor")) return "minor";
  if (records.some((r) => r.bump === "patch")) return "patch";
  refuse(
    "no-release-type",
    "every commit since the base is docs/chore/test/build/ci/style/refactor/revert; " +
      "a release needs at least one feat, fix, perf or breaking change:\n" +
      records.map((r) => `  ${r.sha.slice(0, 8)} ${r.type}: ${r.subject}`).join("\n"),
  );
}


function readManifestVersions(root) {
  const read = (rel) => {
    const p = join(root, rel);
    if (!existsSync(p)) refuse("missing-file", `${rel} not found under ${root}`);
    return readFileSync(p, "utf8");
  };
  const cargoText = read(FILES.cargo);
  const lockText = read(FILES.lock);
  const pkgText = read(FILES.pkg);
  let man;
  try {
    man = JSON.parse(read(FILES.man));
  } catch (err) {
    refuse("bad-manifest", `${FILES.man} is not valid JSON: ${err.message}`);
  }
  const cargoVersion = /^version\s*=\s*"([^"]+)"/m.exec(cargoText)?.[1];
  if (!cargoVersion || !SEMVER.test(cargoVersion)) {
    refuse("bad-manifest", `${FILES.cargo} has no X.Y.Z [package] version`);
  }
  const lockVersion = /name = "puppet"\r?\nversion = "([^"]+)"/.exec(lockText)?.[1] ?? null;
  let pkg;
  try {
    pkg = JSON.parse(pkgText);
  } catch (err) {
    refuse("bad-manifest", `${FILES.pkg} is not valid JSON: ${err.message}`);
  }
  const pkgVersion = pkg?.version;
  if (!pkgVersion || !SEMVER.test(pkgVersion)) {
    refuse("bad-manifest", `${FILES.pkg} has no X.Y.Z version`);
  }
  return {
    versions: {
      cargo: cargoVersion,
      lock: lockVersion,
      pkg: pkgVersion,
      man: man?.serverVersion ?? null,
    },
    manifest: man,
  };
}

// Every manifest must ALREADY name the derived version. There is no repair
// path on purpose: the release run is not the writer of the version, so a
// disagreement is a tree defect that fails the run before it tags anything.
function checkManifests(versions, manifest, derived) {
  const rows = [
    [FILES.cargo, versions.cargo],
    [FILES.pkg, versions.pkg],
    [FILES.man, versions.man],
  ];
  const disagree = rows.filter(([, v]) => v !== versions.cargo);
  if (disagree.length) {
    refuse(
      "manifests-disagree",
      "committed manifest versions do not agree with each other:\n" +
        rows.map(([f, v]) => `  ${f}: ${v ?? "(missing)"}`).join("\n"),
    );
  }
  if (versions.lock !== null && versions.lock !== versions.cargo) {
    refuse(
      "lock-drift",
      `Cargo.lock says ${versions.lock} for the crate but Cargo.toml says ${versions.cargo}; ` +
        "a --locked build would fail (or ship a stale version)",
    );
  }
  if (versions.cargo !== derived) {
    refuse(
      "version-mismatch",
      `history derives ${derived} but the committed manifests say ${versions.cargo} ` +
        `(${FILES.cargo}, ${FILES.pkg}, ${FILES.man} agree): land the version bump in the same ` +
        "commit range as the change it releases; this lane asserts the version, it never writes it",
    );
  }
  // The launcher resolves the archive from runtime.source/archive.name, so a
  // bumped serverVersion with a stale URL fails the publish assert instead of
  // shipping a release that 404s on first run. Catch it here, at derive time.
  const source = manifest?.runtime?.source ?? "";
  const archive = manifest?.runtime?.archive?.name ?? "";
  if (!source.endsWith(`/v${derived}`) || !archive.includes(`v${derived}`)) {
    refuse(
      "manifest-paths",
      `${FILES.man} runtime.source/archive do not name v${derived}:\n` +
        `  runtime.source: ${source || "(missing)"}\n` +
        `  runtime.archive.name: ${archive || "(missing)"}`,
    );
  }
}


function parseChangelog(text) {
  const lines = text.split("\n");
  const heading = /^##\s+\[([^\]]+)\](?:\s*-\s*(.+))?\s*$/;
  const unreleasedAt = lines.findIndex((l) => {
    const m = heading.exec(l);
    return m && m[1].trim().toLowerCase() === "unreleased";
  });
  const sections = [];
  for (let i = 0; i < lines.length; i += 1) {
    const m = heading.exec(lines[i]);
    if (m) sections.push({ version: m[1].trim(), date: m[2]?.trim() ?? null, line: i });
  }
  let body = [];
  if (unreleasedAt >= 0) {
    const next = sections.find((s) => s.line > unreleasedAt);
    body = lines.slice(unreleasedAt + 1, next ? next.line : lines.length);
  }
  const entries = body.filter((l) => /^\s*[-*]\s+\S/.test(l));
  return {
    lines,
    unreleasedAt,
    bodyText: body.join("\n").trim(),
    entries,
    hasEntries: entries.length > 0,
    has: (version) => sections.some((s) => s.version === version),
    sections,
  };
}

function closeChangelog(text, version, date) {
  const parsed = parseChangelog(text);
  if (parsed.has(version)) return { changed: false, reason: `## [${version}] already present`, text, parsed };
  if (parsed.unreleasedAt < 0) {
    refuse("changelog-empty", `CHANGELOG.md has no ## [Unreleased] section and no ## [${version}] section`);
  }
  if (!parsed.hasEntries) {
    refuse(
      "changelog-empty",
      `CHANGELOG.md ## [Unreleased] is empty and there is no ## [${version}] section: ` +
        "a release needs a written entry (a code lane lands under ## [Unreleased]; it never adds a version heading)",
    );
  }
  const lines = [...parsed.lines];
  lines.splice(parsed.unreleasedAt + 1, 0, "", `## [${version}] - ${date}`);
  return { changed: true, reason: `closed ## [Unreleased] into ## [${version}] - ${date}`, text: lines.join("\n"), parsed };
}

// Body of one closed changelog section, for the release notes.
function sectionBody(text, version) {
  const lines = text.split("\n");
  const start = lines.findIndex((l) => new RegExp(`^##\\s+\\[${version.replace(/\./g, "\\.")}\\]`).test(l));
  if (start < 0) return "";
  const end = lines.findIndex((l, i) => i > start && /^##\s+\[/.test(l));
  return lines.slice(start + 1, end < 0 ? lines.length : end).join("\n").trim();
}


function deriveAll(opts = {}) {
  const root = opts.root ?? SELF_ROOT;
  const tags = readTags(root);
  const files = readManifestVersions(root);
  const committed = files.versions.cargo;

  if (tags.headTag && cmpSemver(committed, tags.headTag.version) === 0) {
    // A rerun of an existing release resolves to the version that tag names;
    // deriving anew would read an empty range and disagree with the tag. When
    // the tree says something else, fall through and check the tag against the
    // derivation instead.
    const version = tags.headTag.version;
    checkManifests(files.versions, files.manifest, version);
    const parsed = parseChangelog(readFileSync(join(root, FILES.changelog), "utf8"));
    return {
      root,
      version,
      base: tags.headTag.name,
      baseSource: "tag at HEAD",
      level: "none",
      alreadyReleased: true,
      current: committed,
      commits: [],
      ignored: [],
      changelog: {
        hasUnreleased: parsed.unreleasedAt >= 0,
        unreleasedEntries: parsed.entries.length,
        alreadyClosed: parsed.has(version),
      },
      versions: files.versions,
    };
  }

  const base = tags.base ?? null;
  const baseSource = base
    ? "highest vX.Y.Z tag reachable from HEAD"
    : `no vX.Y.Z tag: base is the manifest version ${committed}`;

  const raw = readCommits(root, base);
  if (raw.length === 0) {
    refuse(
      "empty-range",
      `no non-merge commits in ${base ? `${base.name}..HEAD` : "HEAD"}: there is no release to derive`,
    );
  }
  const records = raw.map(classify);
  cancelRevertedHops(records);
  const level = deriveLevel(records);
  const derived = bumpVersion(base ? base.version : committed, level);
  // A tag already sitting on this commit is a claim, not a source of truth: it
  // must name the version the history derives, and the manifests must too.
  if (tags.headTag && tags.headTag.version !== derived) {
    refuse(
      "tag-mismatch",
      `tag ${tags.headTag.name} sits on HEAD but the history derives ${derived}; ` +
        "the tag is produced from the derivation, never the other way round",
    );
  }
  checkManifests(files.versions, files.manifest, derived);

  const parsed = parseChangelog(readFileSync(join(root, FILES.changelog), "utf8"));
  const alreadyClosed = parsed.has(derived);
  if (!parsed.hasEntries && !alreadyClosed) {
    refuse(
      "changelog-empty",
      `CHANGELOG.md ## [Unreleased] is empty and there is no ## [${derived}] section: ` +
        "a release needs a written entry",
    );
  }

  return {
    root,
    version: derived,
    base: base ? base.name : committed,
    baseSource,
    level,
    alreadyReleased: false,
    current: committed,
    commits: records.map((r) => ({
      sha: r.sha,
      subject: r.subject,
      type: r.type ?? null,
      bump: r.bump,
      ignored: r.ignored,
      breaking: r.breaking,
      cancels: r.cancels ?? null,
    })),
    ignored: records.filter((r) => r.ignored).map((r) => ({ sha: r.sha, subject: r.subject, type: r.type })),
    changelog: {
      hasUnreleased: parsed.unreleasedAt >= 0,
      unreleasedEntries: parsed.entries.length,
      alreadyClosed,
    },
    versions: files.versions,
  };
}


function report(result) {
  const out = [];
  const line = (k, v) => out.push(`  ${k.padEnd(12)}${v}`);
  out.push("release derivation");
  line("base", `${result.base}  (${result.baseSource})`);
  if (result.alreadyReleased) {
    line("state", `HEAD is already the released ${result.base}; nothing to derive`);
    line("version", result.version);
    line("manifests", `cargo ${result.versions.cargo} = pkg ${result.versions.pkg} = manifest ${result.versions.man}`);
    return out.join("\n");
  }
  line("range", `${result.base}..HEAD  (${result.commits.length} non-merge commit${result.commits.length === 1 ? "" : "s"})`);
  line("level", `${result.level}  (${result.level === "patch" ? "fix/perf" : result.level === "minor" ? "feat" : "breaking change"})`);
  line("version", `${result.version}  (committed ${result.current})`);
  line("changelog", result.changelog.alreadyClosed
    ? `## [${result.version}] already present`
    : `## [Unreleased] has ${result.changelog.unreleasedEntries} entr${result.changelog.unreleasedEntries === 1 ? "y" : "ies"} to close`);
  out.push("  commits");
  for (const c of result.commits) {
    const what = c.bump ? `-> ${c.bump}` : `ignored (${c.type})`;
    out.push(`    ${c.sha.slice(0, 8)}  ${c.subject}  ${what}`);
  }
  if (result.ignored.length) {
    out.push(`  ignored      ${result.ignored.length} commit(s) with no release hop:`);
    for (const c of result.ignored) out.push(`    ${c.sha.slice(0, 8)}  ${c.type}: ${c.subject}`);
  }
  return out.join("\n");
}


const CHECK_CHANGELOG = "# Changelog\n\n## [Unreleased]\n\n### Fixed\n\n- something real.\n";

function fixtureCargo(v) {
  return `[package]\nname = "puppet"\nversion = "${v}"\nedition = "2021"\n`;
}

function fixtureLock(v) {
  return `version = 4\n\n[[package]]\nname = "puppet"\nversion = "${v}"\ndependencies = []\n`;
}

function fixturePkg(v) {
  const hint = `puppet-mcp: global install refused - use npx puppet-mcp@${v}`;
  return JSON.stringify({
    name: "puppet-mcp",
    version: v,
    scripts: {
      preinstall: `node -e "if(process.env.npm_config_global==='true'){console.error('${hint}');process.exit(1)}"`,
      postinstall: `node -e "if(process.env.npm_config_global==='true'){console.error('${hint}');process.exit(1)}"`,
    },
  }, null, 2) + "\n";
}

function fixtureManifest(v, pathVersion = v) {
  return JSON.stringify({
    package: "puppet-mcp",
    serverVersion: v,
    runtime: {
      source: `${REPO_URL}/releases/download/v${pathVersion}`,
      archive: { name: `puppet-v${pathVersion}-windows-x86_64.zip`, sha256: "RELEASE_WORKFLOW_FILLS_THIS", size: 0 },
      server: { name: "puppet.exe", sha256: "RELEASE_WORKFLOW_FILLS_THIS", size: 0 },
    },
  }, null, 2) + "\n";
}

function writeFixtureManifests(dir, version, { pathVersion = version } = {}) {
  const write = (rel, text) => {
    const p = join(dir, rel);
    mkdirSync(dirname(p), { recursive: true });
    writeFileSync(p, text);
  };
  write(FILES.cargo, fixtureCargo(version));
  write(FILES.lock, fixtureLock(version));
  write(FILES.pkg, fixturePkg(version));
  write(FILES.man, fixtureManifest(version, pathVersion));
}

// A fixture repo: manifests at <manifestAt> (a `chore:` bump commit, so the
// bump itself carries no release hop), a base tag at <base>, then the commits
// under test.
function makeFixture({ base, manifestAt = base, commits = [], changelog = null, pathVersion = null } = {}) {
  const dir = mkdtempSync(join(tmpdir(), "derive-release-"));
  const run = (args) => execFileSync("git", ["-C", dir, ...args], { encoding: "utf8", stdio: ["ignore", "pipe", "pipe"] });
  run(["init", "-q"]);
  run(["config", "user.email", "fixture@localhost"]);
  run(["config", "user.name", "fixture"]);
  run(["config", "commit.gpgsign", "false"]);
  run(["config", "tag.gpgsign", "false"]);
  run(["config", "core.autocrlf", "false"]);
  writeFixtureManifests(dir, base, { pathVersion: pathVersion ?? base });
  writeFileSync(join(dir, FILES.changelog), changelog ?? CHECK_CHANGELOG);
  run(["add", "-A"]);
  run(["commit", "-q", "-m", "chore: fixture base"]);
  run(["tag", `v${base}`]);
  if (manifestAt !== base) {
    writeFixtureManifests(dir, manifestAt, { pathVersion: pathVersion ?? manifestAt });
    run(["add", "-A"]);
    run(["commit", "-q", "-m", `chore: bump manifests to ${manifestAt}`]);
  }
  for (const c of commits) {
    run(["commit", "-q", "--allow-empty", "-m", c.subject, ...(c.body ? ["-m", c.body] : [])]);
  }
  return dir;
}

function tryDerive(root) {
  try {
    return { ok: true, result: deriveAll({ root }) };
  } catch (err) {
    if (err instanceof Refusal) return { ok: false, code: err.code, message: err.message };
    throw err;
  }
}

function runCheck() {
  const rows = [];
  const record = (name, ok, detail) => rows.push([name, ok ? "PASS" : "FAIL", detail]);
  const fixtures = [];

  // want.version implies the manifests must already carry it, which is the
  // lane's contract: derive AND assert in one call.
  const expect = (name, base, commits, want, opts = {}) => {
    const dir = makeFixture({
      base,
      commits,
      ...opts,
      manifestAt: opts.manifestAt ?? want.version ?? base,
    });
    fixtures.push(dir);
    const got = tryDerive(dir);
    const brief = (r) => (r.ok ? `${r.result.version} (${r.result.level})` : `${r.code}: ${r.message.split("\n")[0]}`);
    if (want.code) {
      const ok = !got.ok && got.code === want.code;
      record(name, ok, ok ? `refused ${want.code}` : `wanted refusal ${want.code}, got ${brief(got)}`);
    } else {
      const ok = got.ok && got.result.version === want.version && got.result.level === want.level;
      record(name, ok, ok
        ? `${want.version} (${want.level})`
        : `wanted ${want.version} (${want.level}), got ${brief(got)}`);
    }
    return dir;
  };

  try {
    expect("patch-hop", "1.0.0", [{ subject: "fix: a real bug" }], { version: "1.0.1", level: "patch" });
    expect("minor-hop", "1.0.0", [{ subject: "feat(api): add a tool" }], { version: "1.1.0", level: "minor" });
    expect("major-hop", "1.0.0", [{ subject: "feat!: drop the old handshake" }], { version: "2.0.0", level: "major" });
    expect("major-body-trailer", "1.0.0", [{ subject: "feat: reshape", body: "BREAKING CHANGE: the wire format moved" }], { version: "2.0.0", level: "major" });
    expect("zero-x-major-moves-minor", "0.6.2", [{ subject: "feat!: reshape" }], { version: "0.7.0", level: "major" });
    expect("zero-x-feat-moves-minor", "0.6.2", [{ subject: "feat: reshape" }], { version: "0.7.0", level: "minor" });
    expect("zero-x-patch", "0.6.2", [{ subject: "fix(core): patch it" }], { version: "0.6.3", level: "patch" });
    expect("mixed-takes-highest", "1.4.2", [{ subject: "docs: notes" }, { subject: "feat: thing" }, { subject: "fix: other" }], { version: "1.5.0", level: "minor" });

    // A real merge commit is excluded from the range, so its non-conventional
    // subject ("Merge branch ...") is never parsed.
    {
      const dir = makeFixture({ base: "1.0.0", manifestAt: "1.0.1", commits: [{ subject: "fix: one" }] });
      fixtures.push(dir);
      const run = (args, input) => execFileSync("git", ["-C", dir, ...args], { encoding: "utf8", input, stdio: ["pipe", "pipe", "pipe"] });
      const tree = run(["rev-parse", "HEAD^{tree}"]).trim();
      const head = run(["rev-parse", "HEAD"]).trim();
      const other = run(["commit-tree", tree, "-p", head], "docs: side branch\n").trim();
      const merge = run(["commit-tree", tree, "-p", head, "-p", other], "Merge branch 'side'\n").trim();
      run(["reset", "-q", "--hard", merge]);
      const got = tryDerive(dir);
      const ok = got.ok && got.result.version === "1.0.1" && got.result.level === "patch" && !got.result.alreadyReleased;
      record("merge-commit-not-parsed", ok, ok ? "1.0.1 (patch), merge subject ignored" : `got ${got.ok ? `${got.result.version} (${got.result.level})` : got.code}`);
    }

    // A real empty range: the base tag sits on an ancestor and HEAD is only a
    // merge, so `--no-merges base..HEAD` yields nothing to derive from.
    {
      const dir = makeFixture({ base: "1.0.0", commits: [{ subject: "docs: release note" }] });
      fixtures.push(dir);
      const run = (args, input) => execFileSync("git", ["-C", dir, ...args], { encoding: "utf8", input, stdio: ["pipe", "pipe", "pipe"] });
      run(["tag", "-f", "v1.0.0", "HEAD"]);
      const tree = run(["rev-parse", "HEAD^{tree}"]).trim();
      const parent = run(["rev-parse", "HEAD~1"]).trim();
      const sha = run(["commit-tree", tree, "-p", "HEAD", "-p", parent], "merge\n").trim();
      run(["reset", "-q", "--hard", sha]);
      const got = tryDerive(dir);
      const ok = !got.ok && got.code === "empty-range";
      record("refuse-empty-range-merge-only", ok, ok ? "refused empty-range" : `got ${got.ok ? got.result.version : got.code}`);
    }

    expect("refuse-only-chore", "1.0.0", [{ subject: "chore: deps" }, { subject: "docs: readme" }], { code: "no-release-type" });
    expect("refuse-non-conventional", "1.0.0", [{ subject: "Fixed the thing" }], { code: "non-conventional" });
    expect("refuse-unknown-type", "1.0.0", [{ subject: "wip: half a feature" }], { code: "non-conventional" });

    // A revert is a legitimate commit, not a malformed subject. Git writes its
    // own reverts as `Revert "<original subject>"` and the Conventional
    // Commits spec writes the same intent as `revert: <original subject>`;
    // both are accepted. When the named change is in the same range and
    // carried a hop, the pair CANCELS: the tree after the revert has no net
    // change, so history must derive no release from it. (Before this rule,
    // the pair still derived 1.0.1 / 1.1.0 - a released hop for a reverted
    // change, which is how v7.0.1..main derived 7.0.2 for a zero-delta tree.)
    expect(
      "revert-git-shape",
      "1.0.0",
      [{ subject: "fix(diff): emit the diff" }, { subject: 'Revert "fix(diff): emit the diff"' }],
      { code: "no-release-type" },
    );
    expect(
      "revert-colon-shape",
      "1.0.0",
      [{ subject: "feat: a tool" }, { subject: "revert: feat: a tool" }],
      { code: "no-release-type" },
    );
    expect("refuse-revert-only", "1.0.0", [{ subject: 'Revert "fix(diff): emit the diff"' }], { code: "no-release-type" });
    // A revert that carries its OWN breaking marker is not a cancellation: the
    // revert changes behavior by itself (`revert!:` / a BREAKING CHANGE
    // trailer), so it keeps its major hop and the original keeps its own. The
    // dangerous shape was a cancellation that swallowed the breaking change.
    expect(
      "breaking-revert-keeps-major",
      "1.0.0",
      [{ subject: "fix: x" }, { subject: "revert!: fix: x" }],
      { version: "2.0.0", level: "major" },
    );
    expect(
      "breaking-revert-body-keeps-major",
      "1.0.0",
      [{ subject: "fix: x" }, { subject: 'Revert "fix: x"', body: "BREAKING CHANGE: the revert is the break" }],
      { version: "2.0.0", level: "major" },
    );
    // The narrowness is the point: a subject that only LOOKS like a revert, or
    // one that reverts a non-conventional subject, is still refused.
    expect("refuse-revert-of-unconventional", "1.0.0", [{ subject: 'Revert "Fixed the thing"' }], { code: "non-conventional" });
    expect("refuse-revert-prose", "1.0.0", [{ subject: "Revert the diff change" }], { code: "non-conventional" });
    // A revert of a MERGE subject names no conventional change and is not in
    // the range at all (merges are filtered out): still refused.
    expect("refuse-revert-of-merge-subject", "1.0.0", [{ subject: "Revert \"Merge branch 'side'\"" }], { code: "non-conventional" });
    // Cancellation is exact-subject and in-range only: a revert naming a
    // subject that is not in the range cancels nothing, so the surviving fix
    // keeps its patch hop (this is the proof against blanket ignoring).
    expect(
      "revert-unrelated-subject",
      "1.0.0",
      [{ subject: "fix: a real bug" }, { subject: 'Revert "fix: some other bug"' }],
      { version: "1.0.1", level: "patch" },
    );
    // ...and a revert older than the change it names cannot cancel it: the
    // original still lands after the revert and still needs its release.
    expect(
      "revert-before-original",
      "1.0.0",
      [{ subject: 'Revert "fix: later bug"' }, { subject: "fix: later bug" }],
      { version: "1.0.1", level: "patch" },
    );
    // A reverted docs commit had no hop to cancel; the fix beside it stands.
    expect(
      "revert-of-docs-changes-nothing",
      "1.0.0",
      [{ subject: "docs: notes" }, { subject: 'Revert "docs: notes"' }, { subject: "fix: a real bug" }],
      { version: "1.0.1", level: "patch" },
    );

    // The lane asserts the version, it never writes it: a tree whose manifests
    // were not bumped with its commits is refused, not repaired.
    expect("refuse-manifest-not-bumped", "1.0.0", [{ subject: "feat: thing" }], { code: "version-mismatch" }, { manifestAt: "1.0.0" });

    // Empty `## [Unreleased]` refuses...
    {
      const dir = makeFixture({
        base: "1.0.0",
        manifestAt: "1.1.0",
        commits: [{ subject: "feat: thing" }],
        changelog: "# Changelog\n\n## [Unreleased]\n",
      });
      fixtures.push(dir);
      const got = tryDerive(dir);
      const ok = !got.ok && got.code === "changelog-empty";
      record("refuse-empty-unreleased", ok, ok ? "refused changelog-empty" : `got ${got.ok ? got.result.version : got.code}`);
    }
    // ...unless the derived version's section already exists (a rerun over a
    // closed section, or a section written by the release run).
    {
      const dir = makeFixture({
        base: "1.0.0",
        manifestAt: "1.1.0",
        commits: [{ subject: "feat: thing" }],
        changelog: "# Changelog\n\n## [Unreleased]\n\n## [1.1.0] - 2026-11-02\n\n### Added\n- already closed.\n",
      });
      fixtures.push(dir);
      const got = tryDerive(dir);
      const ok = got.ok && got.result.version === "1.1.0" && got.result.changelog.alreadyClosed;
      record("empty-unreleased-with-section", ok, ok ? "1.1.0 derivable, section present" : `got ${got.ok ? got.result.version : got.code}`);
    }

    // Idempotent changelog close: second run over the closed section is a no-op.
    {
      const dir = makeFixture({ base: "1.0.0", manifestAt: "1.1.0", commits: [{ subject: "feat: thing" }] });
      fixtures.push(dir);
      const text = readFileSync(join(dir, FILES.changelog), "utf8");
      const first = closeChangelog(text, "1.1.0", "2026-11-02");
      const second = closeChangelog(first.text, "1.1.0", "2026-11-02");
      const ok = first.changed && !second.changed && first.text.includes("## [1.1.0] - 2026-11-02") && second.text === first.text;
      record("changelog-close-idempotent", ok, ok ? "closed once, then a no-op" : "second close changed bytes");
      writeFileSync(join(dir, FILES.changelog), first.text);
      const again = tryDerive(dir);
      const ok2 = again.ok && again.result.version === "1.1.0" && again.result.changelog.alreadyClosed;
      record("changelog-closed-still-derives", ok2, ok2 ? "1.1.0 derivable with the section already closed" : `got ${again.ok ? again.result.version : again.code}`);
      const body = sectionBody(first.text, "1.1.0");
      const ok3 = body.includes("- something real.") && !body.includes("## [1.1.0]");
      record("changelog-section-body", ok3, ok3 ? "notes body carries the entries, not the heading" : "section body wrong");
    }

    // Refuse manifests that disagree with each other.
    {
      const dir = makeFixture({ base: "1.0.0", manifestAt: "1.0.1", commits: [{ subject: "fix: small" }] });
      fixtures.push(dir);
      writeFileSync(join(dir, FILES.pkg), fixturePkg("1.0.9"));
      const got = tryDerive(dir);
      const ok = !got.ok && got.code === "manifests-disagree";
      record("refuse-manifests-disagree", ok, ok ? "refused manifests-disagree" : `got ${got.ok ? got.result.version : got.code}`);
    }
    // Refuse a Cargo.lock left behind (a --locked build ships a stale version).
    {
      const dir = makeFixture({ base: "1.0.0", manifestAt: "1.0.1", commits: [{ subject: "fix: small" }] });
      fixtures.push(dir);
      writeFileSync(join(dir, FILES.lock), fixtureLock("1.0.0"));
      const got = tryDerive(dir);
      const ok = !got.ok && got.code === "lock-drift";
      record("refuse-lock-drift", ok, ok ? "refused lock-drift" : `got ${got.ok ? got.result.version : got.code}`);
    }
    // Refuse a manifest whose runtime paths still name the old tag: the
    // launcher would 404 on first run of the new release.
    {
      const dir = makeFixture({ base: "1.0.0", manifestAt: "1.0.1", commits: [{ subject: "fix: small" }], pathVersion: "1.0.0" });
      fixtures.push(dir);
      const got = tryDerive(dir);
      const ok = !got.ok && got.code === "manifest-paths";
      record("refuse-manifest-path-drift", ok, ok ? "refused manifest-paths" : `got ${got.ok ? got.result.version : got.code}`);
    }

    // A tag sitting on HEAD reruns as its own version instead of deriving anew.
    {
      const dir = makeFixture({ base: "1.0.0", commits: [] });
      fixtures.push(dir);
      const got = tryDerive(dir);
      const ok = got.ok && got.result.alreadyReleased && got.result.version === "1.0.0" && got.result.level === "none";
      record("rerun-at-released-tag", ok, ok ? "HEAD tag 1.0.0 resolved" : `got ${got.ok ? `${got.result.version}/${got.result.level}` : got.code}`);
    }

    // A hand-cut tag at HEAD is a claim, and the manifests still have to back
    // it: a tag naming another version is refused, and so is a tag that names
    // the derivation while the tree behind it was never bumped.
    {
      const dir = makeFixture({ base: "1.0.0", manifestAt: "1.1.0", commits: [{ subject: "feat: thing" }] });
      fixtures.push(dir);
      execFileSync("git", ["-C", dir, "tag", "v1.4.0"], { encoding: "utf8", stdio: ["ignore", "pipe", "pipe"] });
      const bad = tryDerive(dir);
      const ok = !bad.ok && bad.code === "tag-mismatch";
      record("refuse-hand-cut-tag-mismatch", ok, ok ? "refused tag-mismatch" : `got ${bad.ok ? bad.result.version : bad.code}`);
    }
    {
      const dir = makeFixture({ base: "1.0.0", commits: [{ subject: "feat: thing" }] });
      fixtures.push(dir);
      execFileSync("git", ["-C", dir, "tag", "v1.1.0"], { encoding: "utf8", stdio: ["ignore", "pipe", "pipe"] });
      const got = tryDerive(dir);
      const ok = !got.ok && got.code === "version-mismatch";
      record("refuse-tag-ahead-of-manifests", ok, ok ? "refused version-mismatch" : `got ${got.ok ? got.result.version : got.code}`);
    }
  } finally {
    for (const dir of fixtures) {
      try { rmSync(dir, { recursive: true, force: true }); } catch { /* ignore */ }
    }
  }

  const w = (s, n) => s.padEnd(n);
  console.log(`${w("CASE", 32)}${w("RESULT", 8)}DETAIL`);
  for (const [name, ok, detail] of rows) console.log(`${w(name, 32)}${w(ok, 8)}${detail}`);
  const failed = rows.filter(([, r]) => r === "FAIL").length;
  console.log(`\n${rows.length - failed}/${rows.length} cases pass`);
  return failed === 0 ? 0 : 1;
}

// -------------------------------------------------------------------- main

function main(argv) {
  const has = (f) => argv.includes(f);
  if (has("--check")) return runCheck();
  if (has("--help") || has("-h")) {
    console.log(readFileSync(fileURLToPath(import.meta.url), "utf8").split("\n").slice(1, 45).join("\n"));
    return 0;
  }

  const valueOf = (flag) => {
    const i = argv.indexOf(flag);
    return i >= 0 ? argv[i + 1] : null;
  };
  const root = valueOf("--repo") ? resolve(valueOf("--repo")) : SELF_ROOT;
  const date = valueOf("--date") ?? new Date().toISOString().slice(0, 10);
  const notesOut = valueOf("--notes-out");

  let result;
  try {
    result = deriveAll({ root });
  } catch (err) {
    if (err instanceof Refusal) {
      console.error(`refuse: ${err.code}: ${err.message}`);
      return 1;
    }
    throw err;
  }

  const json = {
    version: result.version,
    level: result.level,
    base: result.base,
    baseSource: result.baseSource,
    current: result.current,
    alreadyReleased: result.alreadyReleased,
    changelog: result.changelog,
    commits: result.commits,
    ignored: result.ignored,
  };

  if (has("--json")) {
    // stdout stays machine-parseable; the human derivation goes to the log.
    console.error(report(result));
    console.log(JSON.stringify(json, null, 2));
    return 0;
  }

  console.log(report(result));

  if (has("--dry-run")) {
    console.log("  dry-run      nothing written");
    return 0;
  }

  if (has("--changelog")) {
    const text = readFileSync(join(root, FILES.changelog), "utf8");
    let closed;
    try {
      closed = closeChangelog(text, result.version, date);
    } catch (err) {
      if (err instanceof Refusal) {
        console.error(`refuse: ${err.code}: ${err.message}`);
        return 1;
      }
      throw err;
    }
    if (closed.changed) writeFileSync(join(root, FILES.changelog), closed.text);
    console.log(`  changelog    ${closed.changed ? closed.reason : `unchanged (${closed.reason})`}`);
    if (notesOut) {
      const section = sectionBody(closed.text, result.version);
      mkdirSync(dirname(resolve(notesOut)), { recursive: true });
      writeFileSync(resolve(notesOut), `${section}\n`);
      console.log(`  notes        ${notesOut}`);
    }
    return 0;
  }

  console.log("  written      nothing: this lane asserts versions, it never writes them");
  return 0;
}

process.exitCode = main(process.argv.slice(2));
