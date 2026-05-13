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

Now that the merge is done, I noticed we had to redefine constants because compilation was not working anymore.

It seems those were actually renamed, for example I found "0x4200000000000000000000000000000000000019" but under name `BASE_FEE_VAULT` in crates/common/consensus/src/predeploys.rs as well as `L1_FEE_VAULT` and `OPERATOR_FEE_VAULT`.

## Spec & Implementation

### Changes Made

1. Updated `[workspace.package] version` from `"0.7.6"` to `"0.8.0"` in `Cargo.toml`.
2. Replaced all 68 occurrences of `branch = "release/reth-1.x"` with `tag = "v1.11.4-fh"` in `Cargo.toml` (comments referencing the branch name were left as-is since they are informational).
3. Added `v0.8.0-fh` section to `CHANGELOG.sf.md`.

Note: `cargo check` could not be run in the sandbox (Rust toolchain not installed), but the changes are purely dependency reference updates with no logic changes.

## State Tracker

**Last Updated:** 2026-05-13
**Current Step:** Step 6 — Used proper Predeploys constants for fee vault addresses
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

### Step 4 — Merged v0.8.0 tag (Completed)
- Ran `git merge v0.8.0` and resolved all conflicts
- Non-firehose files took `--theirs` (upstream v0.8.0)
- Resolved Cargo.toml conflicts: alloy upgraded to 1.8, kept firehose patch sections
- Committed merge: `6eb9894f9`

### Step 5 — Post-merge compilation fixes (Completed)
- Removed duplicate `alloy-sol-types` key in `crates/client/metering/Cargo.toml`
- Updated `crates/execution/firehose/Cargo.toml`: replaced old deps with `base-common-evm` (reth feature) and `base-common-consensus` (reth feature)
- Updated `extras.rs`: replaced `base_alloy_evm::OpEvm`/`base_revm::*` with `base_common_evm::{BaseEvm, ...}`, inlined fee recipient addresses
- Updated `evm_config.rs`: replaced `OpEvmFactory`→`BaseEvmFactory`, `OpPrimitives`→`BasePrimitives`, added `OpTransaction<TxEnv>: TransactionEnv` where clauses
- `cargo check --workspace` passes (only unrelated `sp1-prover-types` build script failure, pre-existing)
- `cargo test -p base-execution-firehose` passes
- Committed: `db62c390f`

### Step 6 — Used proper Predeploys constants (Completed)
- Replaced inlined fee vault address constants (`BASE_FEE_RECIPIENT`, `L1_FEE_RECIPIENT`, `OPERATOR_FEE_RECIPIENT`) in `extras.rs` with `Predeploys::BASE_FEE_VAULT`, `Predeploys::L1_FEE_VAULT`, `Predeploys::OPERATOR_FEE_VAULT` from `base_common_consensus`
- `cargo test -p base-execution-firehose` passes
- Committed: `a4b88c30d`
