//! Receiving addresses as Bech32 strings, for `#[serde(with = "...")]` fields.
//!
//! Every address in a Firebreak file is a testnet `tf1...` address. Reading one checks its network
//! prefix, its checksum and both of its points, so a field of this type always holds a valid
//! address.

use flamekd::ReceivingAddress;
use serde::Serializer;
use serde::de::{self, Deserialize, Deserializer};

use crate::NETWORK;

/// Writes an address as its Bech32 string.
pub fn serialize<S>(address: &ReceivingAddress, serializer: S) -> Result<S::Ok, S::Error>
where
    S: Serializer,
{
    serializer.serialize_str(&address.to_bech32(NETWORK))
}

/// Reads an address from its Bech32 string.
pub fn deserialize<'de, D>(deserializer: D) -> Result<ReceivingAddress, D::Error>
where
    D: Deserializer<'de>,
{
    let text = String::deserialize(deserializer)?;
    ReceivingAddress::from_bech32(&text, NETWORK)
        .map_err(|error| de::Error::custom(format!("not a testnet address: {error}")))
}

#[cfg(test)]
mod tests {
    use flamekd::{Network, util};
    use flamepayments::Account;
    use serde::{Deserialize, Serialize};

    use super::*;

    #[derive(Debug, PartialEq, Serialize, Deserialize)]
    struct Sample {
        #[serde(with = "super")]
        address: ReceivingAddress,
    }

    fn address() -> ReceivingAddress {
        Account::from_seed(&[5; 64], NETWORK, 0)
            .expect("an account")
            .address_at(util::RECEIVING, 0)
            .expect("an address")
    }

    #[test]
    fn an_address_is_a_testnet_bech32_string() {
        let sample = Sample { address: address() };
        let json = serde_json::to_value(&sample).expect("serialize");
        let text = json["address"].as_str().expect("a string");
        assert!(text.starts_with("tf1"), "{text}");
        let back: Sample = serde_json::from_value(json).expect("deserialize");
        assert_eq!(back, sample);
    }

    #[test]
    fn another_network_or_a_corrupt_address_is_refused() {
        let mainnet = address().to_bech32(Network::Mainnet);
        let testnet = address().to_bech32(Network::Testnet);
        let mut corrupt = testnet.clone();
        corrupt.pop();
        corrupt.push(if testnet.ends_with('q') { 'p' } else { 'q' });
        for text in [
            mainnet,
            corrupt,
            String::new(),
            "tf1".to_owned(),
            "hello".to_owned(),
        ] {
            let json = serde_json::json!({ "address": text });
            assert!(serde_json::from_value::<Sample>(json).is_err(), "{text}");
        }
    }
}
