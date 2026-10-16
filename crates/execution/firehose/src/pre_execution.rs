use alloy_primitives::{Address, U256, address};
use base_common_consensus::Predeploys;
use base_common_evm::BaseTime;
use base_common_precompiles::NonceManagerStorage;
use reth_firehose::PreExecutionWrites;

/// State that Base's pre-execution changes write straight to the state DB, outside the EVM.
///
/// Each entry is an irregular state transition at an upgrade activation:
/// - Canyon force-deploys the `create2deployer` code (`ensure_create2_deployer`).
/// - Denim installs the BaseTime implementation code and points the BaseTime proxy's EIP-1967
///   implementation slot at it (`BaseTime::ensure_predeploy`).
/// - Everest plants a code stub on the EIP-8130 nonce manager (`ensure_eip8130_system_accounts`).
///
/// Must list every write of `BaseBlockExecutor::apply_pre_execution_changes` that does not go
/// through the EVM.
#[derive(Debug, Clone, Copy, Default)]
pub struct BasePreExecutionWrites;

impl BasePreExecutionWrites {
    /// Address of the `create2deployer` installed at Canyon. Must stay in sync with
    /// `crates/common/evm/src/canyon.rs`.
    pub const CREATE2_DEPLOYER: Address = address!("0x13b0D85CcB8bf860b6b79AF3029fCA081AE9beF2");

    /// Accounts whose code the pre-execution changes may set.
    pub const CODE_ADDRESSES: [Address; 3] =
        [Self::CREATE2_DEPLOYER, BaseTime::IMPLEMENTATION_ADDRESS, NonceManagerStorage::ADDRESS];

    /// Storage slots the pre-execution changes may write.
    pub const STORAGE_SLOTS: [(Address, U256); 1] =
        [(Predeploys::BASE_TIME, BaseTime::IMPLEMENTATION_SLOT)];
}

impl PreExecutionWrites for BasePreExecutionWrites {
    fn code_addresses(&self) -> &[Address] {
        &Self::CODE_ADDRESSES
    }

    fn storage_slots(&self) -> &[(Address, U256)] {
        &Self::STORAGE_SLOTS
    }
}
