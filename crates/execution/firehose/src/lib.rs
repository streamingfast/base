#![doc = include_str!("../README.md")]
#![cfg_attr(docsrs, feature(doc_cfg))]
#![cfg_attr(not(test), warn(unused_crate_dependencies))]

mod extras;
pub use extras::{OpPostTxExtras, OpPreTxAdjust};

mod evm_config;
pub use evm_config::{OpChainHooks, OpFirehoseEvmConfig};
