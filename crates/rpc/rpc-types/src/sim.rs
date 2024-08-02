use alloy_eips::BlockNumberOrTag;
use alloy_primitives::{Address, Bytes, U256};
use alloy_rpc_types_mev::EthCallBundleTransactionResult;
use serde::{
    Deserialize, Serialize,
};

mod u256_numeric_string {
    use alloy_primitives::U256;
    use serde::{de, Deserialize, Serializer};
    use std::str::FromStr;

    pub(crate) fn deserialize<'de, D>(deserializer: D) -> Result<U256, D::Error>
    where
        D: de::Deserializer<'de>,
    {
        let val = serde_json::Value::deserialize(deserializer)?;
        match val {
            serde_json::Value::String(s) => {
                if let Ok(val) = s.parse::<u128>() {
                    return Ok(U256::from(val));
                }
                U256::from_str(&s).map_err(de::Error::custom)
            }
            serde_json::Value::Number(num) => {
                num.as_u64().map(U256::from).ok_or_else(|| de::Error::custom("invalid u256"))
            }
            _ => Err(de::Error::custom("invalid u256")),
        }
    }

    pub(crate) fn serialize<S>(val: &U256, serializer: S) -> Result<S::Ok, S::Error>
    where
        S: Serializer,
    {
        let val: u128 = (*val).try_into().map_err(serde::ser::Error::custom)?;
        serializer.serialize_str(&val.to_string())
    }

}

/// Block for `eth_simulateBlock`
#[derive(Default, Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct EthSimulateBlock{
    /// A list of hex-encoded signed transactions
    pub txs: Vec<Bytes>,
    /// hex encoded block number for which this bundle is valid on
    pub block_number: u64,
    /// Either a hex encoded number or a block tag for which state to base this simulation on
    pub state_block_number: BlockNumberOrTag,
    /// coinbase used for simulation
    pub coinbase: Address,
    /// base fee used for simulation
    pub base_fee: Option<u128>,
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
    #[serde(with = "alloy_serde::quantity")]
    pub state_block_number: u64,
    /// The total gas used by all transactions in the bundle
    #[serde(with = "alloy_serde::quantity")]
    pub total_gas_used: u64,
}
