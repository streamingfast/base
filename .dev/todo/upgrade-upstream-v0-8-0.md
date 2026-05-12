# Upgrade to upstream v0.8.0

mode: bug
state: review
root_git: .worktrees/fix/upgrade-upstream-v0-8-0
worktree: .worktrees/fix/upgrade-upstream-v0-8-0
branch: fix/upgrade-upstream-v0-8-0
target_branch: firehose/0.x

> **Resume protocol:** read **Dev Feedback** and the **State Tracker** below first, then jump to the
> step marked `Current`. Ensure that you are in the correct worktree and branch according to preamble here. Update current with Developer feedback and update the tracker after every meaningful change.
> Do not mutate completed steps; append a new entry instead.

---

## Initial Description

Upgrade to upstream v0.8.0 version which we are now based on v0.7.6. We have bumped our own reth fork to v1.11.4 using a tag (https://github.com/streamingfast/reth/tree/v1.11.4-fh). Update to use tag when referencing the repository.

Concretely this means:
1. Update workspace version from v0.7.6 to v0.8.0 in Cargo.toml
2. Update all streamingfast/reth dependencies from `branch = "release/reth-1.x"` to `tag = "v1.11.4-fh"` in Cargo.toml

## Dev Feedback

## Spec & Implementation

### Changes Made

1. Updated `[workspace.package] version` from `"0.7.6"` to `"0.8.0"` in `Cargo.toml`.
2. Replaced all 68 occurrences of `branch = "release/reth-1.x"` with `tag = "v1.11.4-fh"` in `Cargo.toml` (comments referencing the branch name were left as-is since they are informational).
3. Added `v0.8.0-fh` section to `CHANGELOG.sf.md`.

Note: `cargo check` could not be run in the sandbox (Rust toolchain not installed), but the changes are purely dependency reference updates with no logic changes.

## State Tracker

**Last Updated:** 2026-05-12
**Current Step:** Step 2 — Ready for review
**Status:** Implementation complete, awaiting review

### Step 1 — Implementation (Completed)
- Bumped workspace version to v0.8.0
- Updated all 68 reth dependency references from branch to tag v1.11.4-fh
- Updated CHANGELOG.sf.md
- Committed: `d0d396013`
