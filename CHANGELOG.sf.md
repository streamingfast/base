## v0.8.0-fh

### Changes

* Update workspace version to `v0.8.0`.
* Update all `streamingfast/reth` dependencies from `branch = "release/reth-1.x"` to `tag = "v1.11.4-fh"`.

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
