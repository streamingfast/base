## v1.2.0-fh

### Added

* Added Firehose tracing-regression coverage for Base transactions: a prestate-driven test (block
  invariants + JSON projection golden) plus an end-to-end `base-system-tests` integration test
  tracing a B-20 precompile transfer. The chain-agnostic capture / invariants / projection / golden
  framework lives in the shared `firehose-tracer-test` crate (`evm-firehose-tracer-rs` `5.3.0`).

### Changed

* Bumped base to `v1.2.0`.
* Kept `streamingfast/reth` at `tag = "v2.3.0-fh-5"` — upstream `v1.2.0` did not move off reth
  `v2.3.0`, revm `40.0.3` or alloy-evm `0.36.0`, so no new Firehose reth release is required.
* Added `reth-eth-wire-types` to the `[patch."https://github.com/paradigmxyz/reth"]` table.
  Upstream `v1.2.0` began depending on it directly; without the patch entry it would resolve to a
  second copy from `paradigmxyz/reth` alongside the Firehose fork's copy.
* Stopped tracking the `.dev/` scratch directory and added it to `.gitignore`.

### Fixed

* Replaced the stale `base-firehose-tests` `nop_transfer` full-protobuf golden with a JSON
  projection plus property invariants. The full-block golden recorded `gas_limit: 30000000` for the
  EIP-4788 beacon-roots system call while the current tracer reports `31566720`; the projection
  excludes volatile fields like gas, so it no longer rots on such changes.

### Notes

* Upstream `v1.2.0` lands EIP-8130 (account abstraction) with *phased* transaction execution and a
  Cobalt irregular state transition. Firehose tracing is unaffected on real networks for now:
  EIP-8130 is gated behind the Cobalt upgrade, and `cobalt_timestamp` is `None` for mainnet,
  sepolia, devnet and zeronet in `crates/common/chains/src/config.rs`. Note that the local Docker
  devnet *does* schedule it (`L2_BASE_COBALT_BLOCK=22` in `etc/docker/devnet-env`), so tracing a
  local devnet past block 22 will exercise the unsupported path. Tracing support for multi-phase
  transactions must be added before Cobalt activates anywhere real.

## v1.1.1-fh-1

* Bumped `streamingfast/reth` dependencies to `tag = "v2.3.0-fh-5"`, which fixes a call/receipt log-count mismatch panic (`N call logs but N+1 receipt logs`) when a native-precompile log (B-20 token event) is emitted at a journal index freed by a reverted opcode `LOG` — seen on Base mainnet block 48387796 (Uniswap V4 revert-based quote). Also pulls in fh-2 (post-tx balance resolver), fh-3 (keccak OOM cap), fh-4 (Docker release CI).
* Added a `FIREHOSE_TRACER_IGNORE_LOG_MISMATCH` env-var safety net (patched `firehose-tracer` to `v5.2.2`): when set, log-count / BlockIndex mismatches are logged and skipped instead of panicking.

## v1.1.1-fh

* Bumped base to `v1.1.1`
* Fixed issue seen on base-sepolia with new B20 native token: 
  logs and state changes are correctly output from precompiled contract calls

## v1.1.0-fh

* Bumped base to `v1.1.0`
* Bumped `streamingfast/reth` dependencies to `tag = "v2.3.0-fh"` (reth v2.3.0, revm 40, alloy-evm 0.36)
* Patched `alloy-evm` to the StreamingFast fork (`streamingfast/evm` branch `sf/v0.36.0`), which routes EVM system calls (EIP-4788, EIP-2935, etc.) through the Inspector so Firehose traces them — fixes the missing EIP-4788 beacon-roots pre-execution system call
* Added Beryl hardfork activation timestamps (Sepolia: 1_781_805_600, Zeronet: 1_780_678_800)
* Added Cobalt hardfork plumbing

## v1.0.1-fh

* Bumped base to `v1.0.1`
* Fixes on flash blocks: fetch fresh state on every block to avoid mismatches that cause UNDOs

## v1.0.0-fh

* Bumped base to `v1.0.0`

## v0.9.1-fh-1

* Fixed flash blocks to arrive in the right order and be 100% identical to the canonical blocks

## v0.9.1-fh

* bumped upstream to 0.9.1
* initial flashblocks wiring (not to be used yet)

## v0.9.0-fh

* bumped upstream to 0.9.0

## v0.8.0-fh

* Parity with geth implementation, except flashblocks

### Changes

* Update workspace version to `v0.8.0`.
* Update all `streamingfast/reth` dependencies from `branch = "release/reth-1.x"` to `tag = "v1.11.4-fh-1"`.

## v0.7.6-fh

### Operator Notes

This is the initial integration of the Firehose instrumentation into [base-reth](https://github.com/base/base-reth), enabling Firehose 3.0 block production from a Reth-based Base node.

Due to the block model changes which we couldn't record in a backward compatible way, this release uses block version 5 of the Firehose Ethereum Block protobuf model.

### Changes

* Initial integration of the Firehose instrumentation to `base-reth`.

  Coming from op-geth instrumentation, this release brings a new Ethereum Block version (`ver = 5` in the Firehose Ethereum Block protobuf model) that has the following semantic changes versus version 4:
  - `GasChanges` has been removed from the `Call` object and is not traced anymore, this was announced in January and is now effective in block version 5.
  - Bug fix where some `CodeChange` were emitted without a real code change (e.g. that `CodeChange.prev == CodeChange.new`), those are not emitted in block version 5.
  - Self destructs tracing is now handled drastically differently than in block version 4 fixing some bugs along the way.
    - While in block version 4 all self-destruct related changes (`CodeChange`, `BalanceChange`, etc) were all done at time of `SELFDESTRUCT` opcode, this is not true anymore in block version 5 where some changes are now deferred to when the transaction is finalized. This fixes some inconsistencies that could happened, for instances some self-destructs did not properly emit 'nonce changes' to 0, they now do.
  - State change hooks no longer record entries when old and new values are identical (no-op state changes are dropped).
  - System Calls may be emitted in a different order (the same final state is produced regardless of the order of those calls)

  Outside of `GasChanges`, there is no major differences in the actual output more around in which order some of the changes are emitted so everyone should be able to accept version 5 like if this was version 4.
