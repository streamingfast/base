use std::path::PathBuf;

use base_firehose_tests::{assert_block_equals_golden, run_prestate};

#[test]
fn nop_transfer() {
    let folder = case_dir("nop_transfer");
    let outcome = run_prestate(&folder).expect("nop_transfer prestate must succeed");
    let golden = folder.join("block.2099.binpb");

    if std::env::var("UPDATE_GOLDENS").is_ok() {
        use prost::Message as _;
        std::fs::write(&golden, outcome.block.encode_to_vec())
            .expect("writing golden file must succeed");
        return;
    }

    assert_block_equals_golden(&outcome.block, &golden).expect("captured block must match golden");
}

fn case_dir(name: &str) -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("tests").join("cases").join(name)
}
