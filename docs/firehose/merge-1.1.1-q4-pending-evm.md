# Q4 — What is the "pending EVM" thing (PR #3603)?

## The feature: a pending block built from flashblocks

Base's sequencer streams **flashblocks** — partial block updates (a base + a series of
deltas) emitted ~200 ms apart, before the full block is sealed and committed. To let RPC
clients query the not-yet-canonical tip (`eth_call`, `eth_getBalance`, `eth_call` against
the `pending` block tag, etc.), the node maintains a **pending EVM state**: an in-memory
EVM/`State` built by replaying the in-flight flashblocks on top of the latest canonical
state.

In `crates/execution/flashblocks/src/processor.rs` this is the `LivePendingState`
(`db` = a revm `State`/`CacheDB`, plus `state_overrides`) that the processor keeps swapping
as new flashblocks arrive. "Pending EVM" = that in-memory EVM state used to answer queries
about the block currently being built.

## The bug PR #3603 fixed

The pending block is built on top of state that is **not yet committed to the database**.
revm's `BLOCKHASH` opcode (and anything reading the parent block hash) resolves the
immediate parent's hash via the in-memory `block_hashes` map. For the pending block, that
map had **no entry for `block_number - 1`**, so `blockhash(block.number - 1)` returned the
wrong value (zero) inside the pending EVM — diverging from how the same call would behave
once the block is canonical.

The fix (the part the new `ParentBlockhashGuard.sol` test pins down):

```rust
// crates/execution/flashblocks/src/processor.rs
db.block_hashes.insert(base.block_number - 1, base.parent_hash);
// and the assembled pending header now carries:
header.parent_hash = base.parent_hash;
```

The new test contract `ParentBlockhashGuard.succeedsOnlyWhenParentBlockHashIsCanonical`
calls `blockhash(block.number - 1)` and asserts it equals the canonical parent hash —
exactly the value that was previously missing in the pending EVM.

The rest of PR #3603 (typed `FlashblockId` errors, the `prev_flashblock_id` predecessor
link, atomic counters) is unrelated plumbing bundled in the same backport.

## Relevance to firehose

**The fix lives in the RPC pending-block pipeline, which firehose does not run.** Firehose
has its **own** processor (`crates/firehose-flashblocks/src/processor.rs`) that re-executes
flashblocks to emit FIRE BLOCK trace lines. The upstream `LivePendingState` change does
**not** propagate to it, and it was **not** ported — this is the pre-existing divergence
flagged in the merge summary.

Whether the firehose execute path needs an equivalent parent-hash insertion is a
**pre-existing** question, not something v1.1.1 changes:

- Firehose only executes a block once its **parent is available from the provider**. The
  execute path looks up `header_by_number(block_number - 1)` and, on a miss, buffers the
  sequence as *pending* instead of executing (see the test
  `parent_header_missing_buffers_instead_of_resetting`). So for the immediate parent,
  `provider.block_hash(n-1)` has a resolution path that the upstream pending pipeline
  lacked.
- Firehose runs `apply_pre_execution_changes()` per block, which performs the EIP-2935
  history-storage system call (writing the parent hash into the history contract). Beryl is
  well past Isthmus/Prague where EIP-2935 is active, so `blockhash` reads of recent blocks
  resolve through contract state that firehose populates.

**Recommended follow-up (not a merge blocker):** add a targeted trace-correctness check
that traces a transaction calling `blockhash(n-1)` (e.g. a `ParentBlockhashGuard`-style
contract) through the firehose processor and confirms the traced result matches canonical.
That validates the firehose path has the same parent-hash guarantee the upstream RPC path
just gained — but it exercises pre-existing firehose code, independent of this merge.
