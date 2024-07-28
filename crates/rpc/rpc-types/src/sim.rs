use serde::{
    Deserialize, Serialize,
};

/// Block for `eth_simulateBlock`
#[derive(Default, Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct EthSimulateBlock{
    /// A list of hex-encoded signed transactions
    pub txs: Vec<Bytes>,
    /// hex encoded block number for which this bundle is valid on
    pub block_number: U64,
    /// Either a hex encoded number or a block tag for which state to base this simulation on
    pub state_block_number: BlockNumberOrTag,
    /// coinbase used for simulation
    pub coinbase: Address,
    /// base fee used for simulation
    pub base_fee: U64,
    /// builder address list
    pub builder_addresses: Vec<Address>,
    /// the timestamp to use for this bundle simulation, in seconds since the unix epoch
    #[serde(skip_serializing_if = "Option::is_none")]
    pub timestamp: Option<u64>,
}

/// Response for `eth_simulateBlock`
#[derive(Default, Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "camelCase")]
pub struct EthSimulateBlockResponse {
    /// The balance of the coinbase before the block
    #[serde(with = "u256_numeric_string")]
    pub coinbase_before: U256,
    /// The balance of the coinbase after the block
    #[serde(with = "u256_numeric_string")]
    pub coinbase_after: U256,
    /// The balance of the builders before the block
    pub builder_balances_before: Vec<U256>,
    /// The balance of the builders after the block
    pub builder_balances_after: Vec<U256>,
    /// The total gas fees paid for all transactions in the bundle
    #[serde(with = "u256_numeric_string")]
    pub gas_fees: U256,
    /// Results of individual transactions within the bundle
    pub results: Vec<EthCallBundleTransactionResult>,
    /// The block number used as a base for this simulation
    pub state_block_number: u64,
    /// The total gas used by all transactions in the bundle
    pub total_gas_used: u64,
}
