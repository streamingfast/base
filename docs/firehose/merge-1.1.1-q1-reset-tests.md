# Q1 — Tests for the flashblock reset behavior

## What changed and why a test was needed

v1.1.1 (backport of PR #3603) changed `FlashblockSequenceValidator::validate` to take a
fifth argument, `prev_flashblock_id: FlashblockId`, and added a new result variant,
`NonSequentialPredecessor { expected, actual }`.

Each delta flashblock now carries `metadata.prev_flashblock_id` — an explicit link naming
the flashblock it claims to follow. The validator checks it:

```rust
// crates/execution/flashblocks/src/validation.rs
if incoming_prev_flashblock_id != FlashblockId::default()
    && incoming_prev_flashblock_id != latest_flashblock_id
{
    return SequenceValidationResult::NonSequentialPredecessor { expected, actual };
}
```

The firehose processor consumes this validator, so it was adapted to pass
`flashblock.metadata.prev_flashblock_id` and to handle the new variant by **resetting**
its in-flight state and waiting for the next base flashblock (same recovery path as the
existing `NonSequentialGap`).

This reset path had no test coverage. Three were added.

## Tests added

File: `crates/firehose-flashblocks/tests/flashblock_sequence.rs`

1. **`delta_with_mismatched_prev_id_resets`** — the reset case.
   `base(1,0)` establishes the latest id `{block:1, index:0}`; then a `delta(1,1)` whose
   `prev_flashblock_id` is `{block:9, index:9}` (non-default, non-matching) arrives. The
   index is sequentially valid, so this isolates the *predecessor* check from the *gap*
   check. Asserts: no FIRE events emitted, and — reading processor state directly via
   `pending_state_for_test()` — `current_block == None`, `pending == false`,
   `stored_count == 0`, i.e. the sequence was fully reset.

2. **`delta_with_matching_prev_id_is_accepted`** — the happy path.
   Same setup but `prev_flashblock_id == {block:1, index:0}` (correctly links to the base).
   Asserts the delta is accepted and emits the consolidated flashblock at `flash_idx=1`.
   Guards against the new check rejecting valid links.

3. **`delta_with_default_prev_id_is_accepted`** — backward compatibility.
   `prev_flashblock_id == FlashblockId::default()` (all-zero), which is what a builder that
   does not yet emit the field produces. Asserts the check is skipped and the delta is
   accepted on index ordering alone, exactly as pre-v1.1.1.

## Supporting test helper

Added `flash_delta_with_prev_id(block_number, block_hash, index, prev_flashblock_id)` to
`crates/firehose-flashblocks/tests/framework/mod.rs` — builds a delta with an explicit
`metadata.prev_flashblock_id`. The pre-existing helpers default that field, so they could
not exercise the new check.

## Result

```
cargo test -p base-firehose-flashblocks --test flashblock_sequence
test result: ok. 38 passed; 0 failed
```

(35 pre-existing + 3 new.)
