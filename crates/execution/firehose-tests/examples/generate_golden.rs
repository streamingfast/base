//! Helper binary to generate `prestate.json` and `block.<N>.binpb` golden files for the
//! `base-firehose-tests` integration test suite.
//!
//! Run with:
//! ```sh
//! cargo run -p base-firehose-tests --example generate_golden
//! ```
//!
//! This will write `tests/cases/nop_transfer/prestate.json` and
//! `tests/cases/nop_transfer/block.2099.binpb`.

use std::path::PathBuf;

use alloy_consensus::{TxEip1559, transaction::SignableTransaction};
use alloy_eips::eip2718::Encodable2718;
use alloy_primitives::{Signature, TxKind, U256, hex};
use base_firehose_tests::run_prestate;
use k256::ecdsa::SigningKey;
use prost::Message as _;

fn main() {
    let case_dir = PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .join("tests")
        .join("cases")
        .join("nop_transfer");

    std::fs::create_dir_all(&case_dir).expect("creating case directory");

    // Well-known Anvil/Hardhat account 0.
    // Private key: 0xac0974bec39a17e36ba4a6b4d238ff944bacb478cbed5efcae784d7bf4f2ff80
    // Address:     0xf39Fd6e51aad88F6F4ce6aB8827279cffFb92266
    let private_key_bytes =
        hex!("ac0974bec39a17e36ba4a6b4d238ff944bacb478cbed5efcae784d7bf4f2ff80");
    let signing_key = SigningKey::from_bytes((&private_key_bytes).into()).unwrap();

    // Derive sender address from public key.
    let sender_addr = {
        let vk = signing_key.verifying_key();
        let point = vk.to_encoded_point(false);
        let hash = alloy_primitives::keccak256(&point.as_bytes()[1..]);
        alloy_primitives::Address::from_slice(&hash[12..])
    };    println!("sender = {sender_addr}");

    let recipient: alloy_primitives::Address =
        "0xa714ac97f3418798620b4486f93f849693f99264".parse().unwrap();

    // chain_id = 8453 (Base mainnet)
    let chain_id: u64 = 8453;

    let tx = TxEip1559 {
        chain_id,
        nonce: 0,
        gas_limit: 21000,
        max_fee_per_gas: 16_000_000_007,
        max_priority_fee_per_gas: 1_500_000_000,
        to: TxKind::Call(recipient),
        value: U256::from(1_000_000_000_000_000_000u128), // 1 ETH
        ..Default::default()
    };

    let sig_hash = tx.signature_hash();
    let (sig, recid) = signing_key.sign_prehash_recoverable(sig_hash.as_ref()).unwrap();
    let signature = Signature::from_signature_and_parity(sig, recid.is_y_odd());

    let signed = alloy_consensus::Signed::new_unchecked(tx, signature, Default::default());

    let mut encoded = Vec::new();
    signed.encode_2718(&mut encoded);
    let input_hex = format!("0x{}", hex::encode(&encoded));
    println!("input tx bytes: {input_hex}");

    // Build genesis prestate.json
    let prestate_path = case_dir.join("prestate.json");
    let sender_hex = format!("{sender_addr:#x}");
    let prestate_json = format!(
        r#"{{
  "context": {{
    "baseFeePerGas": "7",
    "difficulty": "0",
    "gasLimit": "30000000",
    "miner": "0xf97e180c050e5ab072211ad2c213eb5aee4df134",
    "number": "2099",
    "timestamp": "1742240844"
  }},
  "genesis": {{
    "alloc": {{
      "{sender_hex}": {{
        "balance": "0x3635c9adc5dea00000"
      }},
      "0xf97e180c050e5ab072211ad2c213eb5aee4df134": {{
        "balance": "0x3a7bcde14030be1956"
      }}
    }},
    "baseFeePerGas": "7",
    "config": {{
      "chainId": {chain_id},
      "homesteadBlock": 0,
      "eip150Block": 0,
      "eip155Block": 0,
      "eip158Block": 0,
      "byzantiumBlock": 0,
      "constantinopleBlock": 0,
      "petersburgBlock": 0,
      "istanbulBlock": 0,
      "berlinBlock": 0,
      "londonBlock": 0,
      "mergeNetsplitBlock": 0,
      "terminalTotalDifficulty": 0,
      "bedrockBlock": 0,
      "regolithTime": 0,
      "canyonTime": 0,
      "ecotoneTime": 0,
      "fjordTime": 0,
      "graniteTime": 0,
      "holoceneTime": 0,
      "isthmusTime": 9999999999,
      "eip1559Elasticity": 6,
      "eip1559Denominator": 50,
      "eip1559DenominatorCanyon": 250
    }},
    "difficulty": "0",
    "gasLimit": "30000000",
    "miner": "0xf97e180c050e5ab072211ad2c213eb5aee4df134",
    "mixHash": "0x0000000000000000000000000000000000000000000000000000000000000000",
    "nonce": "0x0000000000000000",
    "number": "2098",
    "timestamp": "1742240832"
  }},
  "input": "{input_hex}"
}}
"#
    );

    std::fs::write(&prestate_path, &prestate_json).expect("writing prestate.json");
    println!("wrote {}", prestate_path.display());

    // Run the prestate harness and write the golden.
    let outcome = run_prestate(&case_dir).expect("run_prestate must succeed");

    let golden_path = case_dir.join("block.2099.binpb");
    std::fs::write(&golden_path, outcome.block.encode_to_vec()).expect("writing golden .binpb");
    println!("wrote {}", golden_path.display());

    println!("Done! Golden files are ready.");
}
