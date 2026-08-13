//! The Base networks a prestate fixture can be generated for.

use std::sync::LazyLock;

use alloy_genesis::ChainConfig as GenesisChainConfig;
use base_common_chains::ChainConfig;
use base_common_consensus::{BaseTxEnvelope, Predeploys};
use base_common_evm::L1BlockInfo;
use base_common_rpc_types::{FeeInfo, GenesisInfo, UpgradeInfo};
use base_execution_chainspec::BaseChainSpec;
use eyre::{Context, eyre};
use firehose_tracer_prestate::{PrestateChain, SeededAccount};

/// A Base network the generator knows how to build a fixture for.
///
/// The Go generator this ports carries a hand-copied `params.ChainConfig` per network. Here the
/// fork schedule comes out of [`ChainConfig`], the same table the node configures itself from, so a
/// fixture can never drift from the schedule the node executes with.
#[derive(Debug, Clone, Copy, PartialEq, Eq, clap::ValueEnum)]
pub enum PrestateNetwork {
    /// Base Mainnet, chain id 8453.
    #[value(name = "base")]
    Mainnet,
    /// Base Sepolia, chain id 84532.
    #[value(name = "base-sepolia")]
    Sepolia,
}

impl PrestateNetwork {
    /// The node-side configuration for this network.
    pub const fn chain_config(self) -> &'static ChainConfig {
        match self {
            Self::Mainnet => ChainConfig::mainnet(),
            Self::Sepolia => ChainConfig::sepolia(),
        }
    }

    /// The chain spec this network's genesis is derived from.
    pub fn chain_spec(self) -> BaseChainSpec {
        match self {
            Self::Mainnet => BaseChainSpec::mainnet(),
            Self::Sepolia => BaseChainSpec::sepolia(),
        }
    }

    /// The L2 chain id.
    pub const fn chain_id(self) -> u64 {
        self.chain_config().chain_id
    }

    /// The genesis config a generated fixture embeds.
    ///
    /// [`BaseChainSpec`]'s own genesis config only carries the block-numbered forks: every
    /// timestamped OP and Base upgrade reaches the spec through [`ChainConfig`] instead, and so
    /// would be silently missing from a fixture built from the genesis alone — the replay would run
    /// a post-Isthmus transaction under pre-Canyon rules. This re-projects the timestamps, the
    /// Base-specific upgrades and the EIP-1559 parameters back into the genesis `config` extras that
    /// [`BaseChainSpec::from_genesis`] reads.
    pub fn genesis_config(self) -> eyre::Result<GenesisChainConfig> {
        let chain = self.chain_config();
        let mut config = self.chain_spec().genesis.config.clone();

        let genesis_info = GenesisInfo {
            bedrock_block: Some(chain.bedrock_block),
            regolith_time: Some(chain.regolith_timestamp),
            canyon_time: Some(chain.canyon_timestamp),
            ecotone_time: Some(chain.ecotone_timestamp),
            fjord_time: Some(chain.fjord_timestamp),
            granite_time: Some(chain.granite_timestamp),
            holocene_time: Some(chain.holocene_timestamp),
            isthmus_time: Some(chain.isthmus_timestamp),
            jovian_time: Some(chain.jovian_timestamp),
            base: UpgradeInfo {
                azul: chain.azul_timestamp,
                beryl: chain.beryl_timestamp,
                cobalt: chain.cobalt_timestamp,
            },
            activation_admin_address: ChainConfig::beryl_activation_admin_address_by_chain_id(
                chain.chain_id,
            ),
        };

        let fee_info = FeeInfo {
            eip1559_elasticity: Some(chain.eip1559_elasticity),
            eip1559_denominator: Some(chain.eip1559_denominator),
            eip1559_denominator_canyon: Some(chain.eip1559_denominator_canyon),
        };

        Self::merge(&mut config, &genesis_info).context("merging the Base genesis info")?;
        Self::merge(&mut config, &fee_info).context("merging the EIP-1559 parameters")?;

        Ok(config)
    }

    /// Flattens `extras` into a genesis config's extra fields.
    fn merge(config: &mut GenesisChainConfig, extras: &impl serde::Serialize) -> eyre::Result<()> {
        let serde_json::Value::Object(fields) = serde_json::to_value(extras)? else {
            return Err(eyre!("genesis config extras must serialise to an object"));
        };

        config.extra_fields.extend(fields);

        Ok(())
    }
}

impl PrestateChain for PrestateNetwork {
    type Transaction = BaseTxEnvelope;

    fn chain_id(&self) -> u64 {
        self.chain_config().chain_id
    }

    fn genesis_config(&self) -> eyre::Result<GenesisChainConfig> {
        (*self).genesis_config()
    }

    fn seeded_accounts(&self) -> &[SeededAccount] {
        static L1_BLOCK: LazyLock<[SeededAccount; 1]> = LazyLock::new(|| [l1_block_account()]);
        L1_BLOCK.as_slice()
    }
}

/// The `L1Block` predeploy plus the slots [`L1BlockInfo::try_fetch`] reads.
///
/// Kept here so the generator does not grow the `base-common-*` public API. Isthmus's
/// [`L1BlockInfo::OPERATOR_FEE_SCALARS_SLOT`] and Jovian's
/// [`L1BlockInfo::DA_FOOTPRINT_GAS_SCALAR_SLOT`] share slot 8, so the latter is omitted. A new
/// fork that adds a distinct slot needs an extra entry.
fn l1_block_account() -> SeededAccount {
    SeededAccount {
        address: Predeploys::L1_BLOCK_INFO,
        slots: vec![
            L1BlockInfo::L1_BASE_FEE_SLOT,
            L1BlockInfo::ECOTONE_L1_FEE_SCALARS_SLOT,
            L1BlockInfo::L1_OVERHEAD_SLOT,
            L1BlockInfo::L1_SCALAR_SLOT,
            L1BlockInfo::ECOTONE_L1_BLOB_BASE_FEE_SLOT,
            L1BlockInfo::OPERATOR_FEE_SCALARS_SLOT,
        ],
    }
}

#[cfg(test)]
mod tests {
    use base_common_consensus::Predeploys;
    use base_execution_chainspec::BaseChainSpec;
    use firehose_tracer_prestate::PrestateChain;

    use super::PrestateNetwork;

    /// The whole point of [`PrestateNetwork::genesis_config`]: a genesis carrying it must rebuild
    /// into the same fork schedule the node runs with. A missing timestamp here means a fixture
    /// replays a transaction under the wrong rules and quietly disagrees with production.
    #[test]
    fn genesis_config_round_trips_into_the_same_hardforks() {
        for (network, expected) in [
            (PrestateNetwork::Mainnet, BaseChainSpec::mainnet()),
            (PrestateNetwork::Sepolia, BaseChainSpec::sepolia()),
        ] {
            let mut genesis = expected.genesis.clone();
            genesis.config = network.genesis_config().expect("building the genesis config");

            let rebuilt = BaseChainSpec::from_genesis(genesis);

            assert_eq!(
                rebuilt.hardforks.forks_iter().collect::<Vec<_>>(),
                expected.hardforks.forks_iter().collect::<Vec<_>>(),
                "{network:?} genesis config must round trip into the same hardforks"
            );
        }
    }

    #[test]
    fn seeds_the_l1_block_predeploy_slots_the_l1_cost_function_reads() {
        let accounts = PrestateNetwork::Mainnet.seeded_accounts();
        assert_eq!(accounts.len(), 1);
        assert_eq!(accounts[0].address, Predeploys::L1_BLOCK_INFO);
        assert_eq!(accounts[0], super::l1_block_account());
    }
}
