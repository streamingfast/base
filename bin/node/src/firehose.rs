//! Firehose tracer wiring for `base-reth-node`.
//!
//! Initializes the process-wide Firehose tracer and installs an ExEx that re-executes
//! committed chains with the Firehose inspector.

use base_node_runner::{BaseNodeExtension, FromExtensionConfig, NodeHooks};

/// Runner-level extension that installs the Firehose ExEx.
#[derive(Debug)]
pub struct FirehoseExtension;

impl BaseNodeExtension for FirehoseExtension {
    fn apply(self: Box<Self>, hooks: NodeHooks) -> NodeHooks {
        hooks.install_exex("firehose", |ctx| async move {
            Ok(async move { reth_firehose::run_exex(ctx).await })
        })
    }
}

impl FromExtensionConfig for FirehoseExtension {
    type Config = ();

    fn from_config(_: Self::Config) -> Self {
        Self
    }
}

/// Initializes the process-wide Firehose tracer with the default `reth` chain-client config.
pub fn init() {
    reth_firehose::init_tracer(firehose_tracer::Tracer::new(firehose_tracer::config::Config {
        chain_client: firehose_tracer::config::ChainClient::Reth,
        ..Default::default()
    }));
}
