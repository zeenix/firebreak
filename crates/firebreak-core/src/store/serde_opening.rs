//! Token openings as JSON objects, for `#[serde(with = "...")]` fields.
//!
//! An opening is what its holder needs to spend a confidential token: the quantity and flavor
//! and the blinding factors of their two commitments. It is written as
//!
//! ```json
//! {"qty": "50", "flv": "<hex>", "qty_blinding": "<hex>", "flv_blinding": "<hex>"}
//! ```
//!
//! with the quantity as a decimal string and the other three as 64 hex digits. Both blinding
//! factors and the flavor must be canonical scalars. Everything in this encoding is secret, so it
//! belongs in private stores only. The [`option`] submodule covers `Option<Opening>` fields.

use curve25519_dalek::scalar;
use flamepayments::Opening;
use serde::{Deserialize, Deserializer, Serialize, Serializer};

use super::{serde_amount, serde_hex};

/// Writes an opening as a JSON object.
pub fn serialize<S>(opening: &Opening, serializer: S) -> Result<S::Ok, S::Error>
where
    S: Serializer,
{
    Stored::from(opening).serialize(serializer)
}

/// Reads an opening from a JSON object.
pub fn deserialize<'de, D>(deserializer: D) -> Result<Opening, D::Error>
where
    D: Deserializer<'de>,
{
    Stored::deserialize(deserializer).map(Opening::from)
}

/// `Option<Opening>` fields, written as an opening object or `null`.
pub mod option {
    use flamepayments::Opening;
    use serde::{Deserialize, Deserializer, Serialize, Serializer};

    use super::Stored;

    /// Writes the opening as a JSON object, or `null` for none.
    pub fn serialize<S>(opening: &Option<Opening>, serializer: S) -> Result<S::Ok, S::Error>
    where
        S: Serializer,
    {
        opening.as_ref().map(Stored::from).serialize(serializer)
    }

    /// Reads an opening object, or `null` for none.
    pub fn deserialize<'de, D>(deserializer: D) -> Result<Option<Opening>, D::Error>
    where
        D: Deserializer<'de>,
    {
        Ok(Option::<Stored>::deserialize(deserializer)?.map(Opening::from))
    }
}

/// An opening as it is written. It has no `Debug`, since every field is secret.
#[derive(Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct Stored {
    #[serde(with = "serde_amount")]
    qty: u64,
    #[serde(with = "serde_hex")]
    flv: flamevm::Scalar,
    #[serde(with = "serde_hex")]
    qty_blinding: scalar::Scalar,
    #[serde(with = "serde_hex")]
    flv_blinding: scalar::Scalar,
}

impl From<&Opening> for Stored {
    fn from(opening: &Opening) -> Stored {
        Stored {
            qty: opening.qty,
            flv: opening.flv,
            qty_blinding: opening.qty_blinding,
            flv_blinding: opening.flv_blinding,
        }
    }
}

impl From<Stored> for Opening {
    fn from(stored: Stored) -> Opening {
        Opening {
            qty: stored.qty,
            flv: stored.flv,
            qty_blinding: stored.qty_blinding,
            flv_blinding: stored.flv_blinding,
        }
    }
}

#[cfg(test)]
mod tests {
    use flamekd::util;
    use flamepayments::{Account, OutputSpec, prepare_output};
    use flamevm::FLAME_FLAVOR;
    use rand::SeedableRng;
    use rand::rngs::StdRng;

    use super::*;
    use crate::NETWORK;

    #[derive(Serialize, Deserialize)]
    struct Sample {
        #[serde(with = "super")]
        opening: Opening,
        #[serde(default, with = "super::option")]
        maybe: Option<Opening>,
    }

    /// An opening from a real prepared output.
    fn opening(qty: u64) -> Opening {
        let address = Account::from_seed(&[9; 64], NETWORK, 0)
            .expect("an account")
            .address_at(util::RECEIVING, 0)
            .expect("an address");
        let spec = OutputSpec {
            address,
            qty,
            flv: FLAME_FLAVOR,
            memo: Vec::new(),
        };
        prepare_output(&spec, &mut StdRng::seed_from_u64(qty))
            .expect("prepare an output")
            .opening
    }

    fn same(a: &Opening, b: &Opening) -> bool {
        a.qty == b.qty
            && a.flv == b.flv
            && a.qty_blinding == b.qty_blinding
            && a.flv_blinding == b.flv_blinding
    }

    #[test]
    fn an_opening_round_trips_as_a_json_object() {
        let sample = Sample {
            opening: opening(50),
            maybe: Some(opening(20)),
        };
        let json = serde_json::to_value(&sample).expect("serialize");
        let object = json["opening"].as_object().expect("an object");
        assert_eq!(object["qty"], "50");
        let mut keys: Vec<&str> = object.keys().map(String::as_str).collect();
        keys.sort_unstable();
        assert_eq!(keys, ["flv", "flv_blinding", "qty", "qty_blinding"]);
        for key in ["flv", "qty_blinding", "flv_blinding"] {
            assert_eq!(object[key].as_str().expect("hex").len(), 64);
        }
        let back: Sample = serde_json::from_value(json).expect("deserialize");
        assert!(same(&back.opening, &sample.opening));
        assert!(same(
            back.maybe.as_ref().expect("some"),
            sample.maybe.as_ref().expect("some")
        ));
    }

    #[test]
    fn no_opening_is_null() {
        let sample = Sample {
            opening: opening(10),
            maybe: None,
        };
        let json = serde_json::to_value(&sample).expect("serialize");
        assert!(json["maybe"].is_null());
        let back: Sample = serde_json::from_value(json).expect("deserialize");
        assert!(back.maybe.is_none());
    }

    #[test]
    fn a_non_canonical_blinding_or_a_bad_amount_is_refused() {
        let good = serde_json::to_value(Sample {
            opening: opening(50),
            maybe: None,
        })
        .expect("serialize");

        for (field, text) in [
            ("qty_blinding", "ff".repeat(32)),
            ("flv_blinding", "ff".repeat(32)),
            ("flv", "ff".repeat(32)),
            ("qty", "-50".to_owned()),
            ("qty", "050".to_owned()),
            ("qty_blinding", "00".repeat(31)),
        ] {
            let mut json = good.clone();
            json["opening"][field] = serde_json::Value::String(text);
            assert!(serde_json::from_value::<Sample>(json).is_err(), "{field}");
        }

        let mut json = good.clone();
        json["opening"]["extra"] = serde_json::json!(1);
        assert!(
            serde_json::from_value::<Sample>(json).is_err(),
            "unknown field"
        );
        let mut json = good;
        json["opening"]
            .as_object_mut()
            .expect("an object")
            .remove("flv");
        assert!(
            serde_json::from_value::<Sample>(json).is_err(),
            "missing field"
        );
    }
}
