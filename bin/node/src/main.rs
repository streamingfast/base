#![doc = include_str!("../README.md")]
#![doc(issue_tracker_base_url = "https://github.com/base/base/issues/")]
#![cfg_attr(docsrs, feature(doc_cfg, doc_auto_cfg))]
#![cfg_attr(not(test), warn(unused_crate_dependencies))]

pub mod firehose;

use base_execution_cli::{Cli, StandardBaseRethNode, StandardNodeArgs};
use url::Url;

use crate::firehose::{FirehoseExtension, FirehoseFlashblocksExtension};

/// CLI arguments: upstream's standard set extended with Firehose-specific options.
#[derive(Debug, Clone, PartialEq, Eq, clap::Args)]
pub struct Args {
    /// Standard upstream node arguments.
    #[command(flatten)]
    pub standard: StandardNodeArgs,

    /// WebSocket URL of a flashblocks feed to re-execute through a dedicated Firehose tracer.
    ///
    /// When set (and the Firehose tracer is initialized), each incoming flashblock is
    /// re-executed and emitted as a partial-block FIRE event so the downstream Firehose
    /// consumer can observe state ahead of canonical engine-API confirmation. Independent of
    /// `--flashblocks-url` (which drives RPC pending state, not Firehose). Disabled (no-op)
    /// when unset.
    #[arg(long = "firehose-flashblocks-url")]
    pub firehose_flashblocks_url: Option<Url>,
}

type NodeCli = Cli<Args>;

#[global_allocator]
static ALLOC: reth_cli_util::allocator::Allocator = reth_cli_util::allocator::new_allocator();

fn main() {
    base_cli_utils::init_common!();
    base_reth_cli::init_reth!();

    firehose::init();

    let cli = base_cli_utils::parse_cli!(NodeCli);

    cli.run(|builder, args| async move {
        let mut runner = StandardBaseRethNode::runner_with_version_metrics(args.standard)?;
        runner.install_ext::<FirehoseExtension>(());
        if let Some(url) = args.firehose_flashblocks_url {
            runner.install_ext::<FirehoseFlashblocksExtension>(Some(url));
        }
        runner.run(builder).await
    })
    .unwrap();
}
