# Documentation structure

This is the agreed shape. It applies to every repository that vendors the gate, with the two variants described below.

## Root and package shape

- The repository root carries exactly two markdown entry points: a `README.md` and an `AGENTS.md`.
- A package carries its own `README.md` only when it differs from the root — a distinct purpose, interface, or local procedure. A package README that only restates the root is a duplicate fact and does not belong.
- A package carries `docs/ARCHITECTURE.md` only where the package has structure worth explaining: boundaries, dependencies, or invariants that the README cannot carry.
- A package carries its own `AGENTS.md` only for deltas from root policy. It states additional or overridden policy, never a fact.

## Facts, rules, decisions, and how-tos

Facts are generated or cited, never retyped. If a tool, schema, or source file already states a fact, documentation points at it or renders it. A human-written second copy is drift waiting to happen.

`AGENTS.md` holds policy only: what must and must not be done. It does not hold versions, absolute paths, installation steps, or current-state descriptions. Those are facts or procedure, and they belong in a config, a generated document, or a how-to.

## Identifiers

`[ID]` tags appear only in `ARCHITECTURE.md`. Each tag is paired with a test in both directions:

- a test that the tagged thing exists and behaves as the architecture claims;
- a test that a violation of the tagged claim is detected.

A tag with only one direction is an assertion, not a contract. Tags do not appear in README files, policy files, or changelogs.

## Solo repository and monorepo

- A **solo repository** has the same shape without a `packages/` layer: root `README.md` and `AGENTS.md`, plus architecture documentation only if the structure needs it.
- A **monorepo** has root policy plus cross-cutting contracts only at the root. Anything that is true of a single package lives with that package. Root documentation does not enumerate package-local facts.

## What goes where

| Kind | Home | Lifetime |
| --- | --- | --- |
| Fact | the owning source, generated or cited | lives as long as the source |
| Rule | `AGENTS.md` at the scope where it binds | lives until policy changes |
| Decision | decision record under the owning scope | permanent, marked when superseded |
| Invariant | `docs/ARCHITECTURE.md` with an `[ID]` and a two-direction test | lives with the structure it constrains |
| How-to | a README or a dedicated guide at the scope that needs it | lives until the procedure changes |
| Plan | the active plan, never in a README | closed when its tasks land |

## Rollout

Phase 1 is report-only across adopting repositories: the Node gate covers `forbidden`, `versions`, `generated`, and `checker`, while links are checked by a pinned `lychee` action where CI exists and by review elsewhere. Phase 2 turns strict mode on per repository after that repository's report is clean, making lychee and the three policy checks blocking. Phase 3 pilots repowise doc-drift and parks docsync. Frozen archives keep their existing documentation and are never migrated.

The mechanics of the gate are in [checker.md](checker.md).
