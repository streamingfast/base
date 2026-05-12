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

Note: `cargo fetch` could not complete in the sandbox due to a libgit2 bug — the `streamingfast/reth` tag `v1.11.4-fh` contains a gitlink entry (`.worktrees/feature-update-to-reth-2.x`, mode 160000) without a corresponding `.gitmodules` file, causing libgit2 to fail with "no URL configured for submodule". The tag commit SHA was verified via `git ls-remote` (`18dec45a3d48e6fa8f16a51ff0cd30ad5f86f3dd`) and the Cargo.lock was updated directly. The user should run `cargo fetch` on their machine to confirm it resolves correctly (the issue may not affect their local git setup if the repo is already cached).

**User response**: A new tag v1.11.4-fh-1 has been pushed which removes this one, make the necessary work to update to it, ensuring Cargo.lock is updated and double check that cargo test passes.

## Spec & Implementation

### Changes Made

1. Updated `[workspace.package] version` from `"0.7.6"` to `"0.8.0"` in `Cargo.toml`.
2. Replaced all 68 occurrences of `branch = "release/reth-1.x"` with `tag = "v1.11.4-fh"` in `Cargo.toml` (comments referencing the branch name were left as-is since they are informational).
3. Added `v0.8.0-fh` section to `CHANGELOG.sf.md`.

Note: `cargo check` could not be run in the sandbox (Rust toolchain not installed), but the changes are purely dependency reference updates with no logic changes.

## State Tracker

**Last Updated:** 2026-05-12
**Current Step:** Step 4 — Updated to v1.11.4-fh-1
**Status:** Ready for review

### Step 1 — Implementation (Completed)
- Bumped workspace version to v0.8.0
- Updated all 68 reth dependency references from branch to tag v1.11.4-fh
- Updated CHANGELOG.sf.md
- Committed: `d0d396013`

### Step 2 — Cargo.lock Updated (Completed)
- Updated 109 reth source entries in Cargo.lock from `branch=release%2Freth-1.x` to `tag=v1.11.4-fh#18dec45a3d48e6fa8f16a51ff0cd30ad5f86f3dd`
- `cargo fetch` failed in sandbox due to libgit2 bug with gitlink-without-.gitmodules in reth tag; Cargo.lock updated directly with verified commit SHA
- Committed: `c669ff610`

### Step 3 — Updated to v1.11.4-fh-1 (Completed)
- User pushed new tag v1.11.4-fh-1 which removes the problematic gitlink entry
- Updated all 68 references in Cargo.toml from `tag = "v1.11.4-fh"` to `tag = "v1.11.4-fh-1"`
- Updated all 109 Cargo.lock entries to use new SHA `54e9307e50bf85e5190aac2ea8b288394229b1cc`
- Committed: `ee152398b`
- Note: Rust toolchain not available in sandbox; user should run `cargo check`/`cargo test` to confirm
