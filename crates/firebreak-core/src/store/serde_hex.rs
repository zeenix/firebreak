//! Lowercase hex strings for byte values, for `#[serde(with = "...")]` fields.
//!
//! Serde writes a 32-byte array, a `TxID`, a point or a scalar as a list of numbers. Every JSON
//! file Firebreak writes holds them as hex strings instead, so one attribute on the field gives
//! the file format its readable and exact form. The module works for any type that is
//! [`HexBytes`], and its [`mod@option`] and [`mod@vec`] submodules cover `Option<T>` and `Vec<T>`
//! fields.
//!
//! Decoding is strict about what the bytes mean, not only how many there are: a scalar must be in
//! canonical form and a point must be a valid encoding. Error messages never repeat the text they
//! refuse, because the text may be a key.

use curve25519_dalek::ristretto::CompressedRistretto;
use curve25519_dalek::scalar;
use flamevm::TxID;
use serde::de::{self, Deserialize, Deserializer};
use serde::{Serialize, Serializer};

/// A value that is written as hex bytes and read back from them.
pub trait HexBytes: Sized {
    /// The bytes the value is written as.
    fn to_raw(&self) -> Vec<u8>;

    /// The value that `bytes` stand for, or why they stand for none.
    fn from_raw(bytes: &[u8]) -> Result<Self, String>;
}

/// Writes `value` as a lowercase hex string.
pub fn serialize<T, S>(value: &T, serializer: S) -> Result<S::Ok, S::Error>
where
    T: HexBytes,
    S: Serializer,
{
    serializer.serialize_str(&encode(value))
}

/// Reads a value from a hex string.
pub fn deserialize<'de, T, D>(deserializer: D) -> Result<T, D::Error>
where
    T: HexBytes,
    D: Deserializer<'de>,
{
    // A `String`, never a `&str`: a value inside a buffered enum or flattened struct cannot lend
    // out a borrowed string.
    let text = String::deserialize(deserializer)?;
    decode(&text).map_err(de::Error::custom)
}

/// `Option<T>` fields, written as a hex string or `null`.
pub mod option {
    use super::{Deserialize, Deserializer, HexBytes, Serializer, de, decode, encode};

    /// Writes the value as a lowercase hex string, or `null` for none.
    pub fn serialize<T, S>(value: &Option<T>, serializer: S) -> Result<S::Ok, S::Error>
    where
        T: HexBytes,
        S: Serializer,
    {
        match value {
            Some(value) => serializer.serialize_str(&encode(value)),
            None => serializer.serialize_none(),
        }
    }

    /// Reads a hex string, or `null` for none.
    pub fn deserialize<'de, T, D>(deserializer: D) -> Result<Option<T>, D::Error>
    where
        T: HexBytes,
        D: Deserializer<'de>,
    {
        let text = Option::<String>::deserialize(deserializer)?;
        text.map(|text| decode(&text))
            .transpose()
            .map_err(de::Error::custom)
    }
}

/// `Vec<T>` fields, written as a list of hex strings.
pub mod vec {
    use super::{Deserialize, Deserializer, HexBytes, Serialize, Serializer, de, decode, encode};

    /// Writes every value as a lowercase hex string.
    pub fn serialize<T, S>(values: &[T], serializer: S) -> Result<S::Ok, S::Error>
    where
        T: HexBytes,
        S: Serializer,
    {
        let words: Vec<String> = values.iter().map(encode).collect();
        words.serialize(serializer)
    }

    /// Reads a list of hex strings.
    pub fn deserialize<'de, T, D>(deserializer: D) -> Result<Vec<T>, D::Error>
    where
        T: HexBytes,
        D: Deserializer<'de>,
    {
        let words = Vec::<String>::deserialize(deserializer)?;
        words
            .iter()
            .map(|text| decode(text))
            .collect::<Result<_, _>>()
            .map_err(de::Error::custom)
    }
}

impl<const N: usize> HexBytes for [u8; N] {
    fn to_raw(&self) -> Vec<u8> {
        self.to_vec()
    }

    fn from_raw(bytes: &[u8]) -> Result<Self, String> {
        bytes
            .try_into()
            .map_err(|_| format!("expected {N} bytes, got {}", bytes.len()))
    }
}

impl HexBytes for Vec<u8> {
    fn to_raw(&self) -> Vec<u8> {
        self.clone()
    }

    fn from_raw(bytes: &[u8]) -> Result<Self, String> {
        Ok(bytes.to_vec())
    }
}

impl HexBytes for TxID {
    fn to_raw(&self) -> Vec<u8> {
        self.0.to_vec()
    }

    fn from_raw(bytes: &[u8]) -> Result<Self, String> {
        <[u8; 32]>::from_raw(bytes).map(TxID)
    }
}

/// A compressed point, which must be a valid Ristretto encoding.
impl HexBytes for CompressedRistretto {
    fn to_raw(&self) -> Vec<u8> {
        self.to_bytes().to_vec()
    }

    fn from_raw(bytes: &[u8]) -> Result<Self, String> {
        let point = CompressedRistretto(<[u8; 32]>::from_raw(bytes)?);
        if point.decompress().is_none() {
            return Err("not a valid Ristretto point".to_owned());
        }
        Ok(point)
    }
}

/// A signing or blinding scalar, which must be in canonical form.
impl HexBytes for scalar::Scalar {
    fn to_raw(&self) -> Vec<u8> {
        self.to_bytes().to_vec()
    }

    fn from_raw(bytes: &[u8]) -> Result<Self, String> {
        let canonical = scalar::Scalar::from_canonical_bytes(<[u8; 32]>::from_raw(bytes)?);
        Option::from(canonical).ok_or_else(|| "not a canonical scalar".to_owned())
    }
}

/// The VM's scalar, which is canonical by construction and so is read back the same way.
impl HexBytes for flamevm::Scalar {
    fn to_raw(&self) -> Vec<u8> {
        self.to_bytes().to_vec()
    }

    fn from_raw(bytes: &[u8]) -> Result<Self, String> {
        flamevm::Scalar::from_bytes(<[u8; 32]>::from_raw(bytes)?)
            .ok_or_else(|| "not a canonical scalar".to_owned())
    }
}

/// The lowercase hex of `value`.
fn encode<T>(value: &T) -> String
where
    T: HexBytes,
{
    hex::encode(value.to_raw())
}

/// The value written as `text`.
fn decode<T>(text: &str) -> Result<T, String>
where
    T: HexBytes,
{
    let bytes = hex::decode(text).map_err(|_| "not a hex string".to_owned())?;
    T::from_raw(&bytes)
}

#[cfg(test)]
mod tests {
    use curve25519_dalek::constants::RISTRETTO_BASEPOINT_COMPRESSED;
    use rand::SeedableRng;
    use rand::rngs::StdRng;
    use serde::Deserialize;

    use super::*;
    use crate::keys;

    /// One field of every kind this module writes.
    #[derive(Debug, PartialEq, Serialize, Deserialize)]
    struct Sample {
        #[serde(with = "super")]
        id: [u8; 32],
        #[serde(with = "super")]
        seed: [u8; 64],
        #[serde(with = "super")]
        blob: Vec<u8>,
        #[serde(with = "super")]
        txid: TxID,
        #[serde(with = "super")]
        point: CompressedRistretto,
        #[serde(with = "super")]
        key: scalar::Scalar,
        #[serde(with = "super")]
        flavor: flamevm::Scalar,
        #[serde(default, with = "super::option")]
        maybe: Option<TxID>,
        #[serde(with = "super::vec")]
        ids: Vec<[u8; 32]>,
    }

    fn sample() -> Sample {
        Sample {
            id: [0xab; 32],
            seed: [0x07; 64],
            blob: vec![0, 1, 254, 255],
            txid: TxID([0x11; 32]),
            point: RISTRETTO_BASEPOINT_COMPRESSED,
            key: keys::generate(&mut StdRng::seed_from_u64(1)),
            flavor: flamevm::Scalar::from(7u64),
            maybe: Some(TxID([0x22; 32])),
            ids: vec![[1; 32], [2; 32]],
        }
    }

    /// The JSON of `sample()` with one field's value replaced by `text`.
    fn json_with(field: &str, text: &str) -> String {
        let mut value = serde_json::to_value(sample()).expect("the sample serializes");
        value[field] = serde_json::Value::String(text.to_owned());
        value.to_string()
    }

    #[test]
    fn every_kind_of_value_round_trips_as_a_hex_string() {
        let sample = sample();
        let json = serde_json::to_value(&sample).expect("serialize");
        assert_eq!(json["id"], "ab".repeat(32));
        assert_eq!(json["seed"], "07".repeat(64));
        assert_eq!(json["blob"], "0001feff");
        assert_eq!(json["txid"], "11".repeat(32));
        assert_eq!(json["maybe"], "22".repeat(32));
        assert_eq!(
            json["ids"],
            serde_json::json!(["01".repeat(32), "02".repeat(32)])
        );
        assert_eq!(json["flavor"], format!("07{}", "00".repeat(31)));
        let back: Sample = serde_json::from_value(json).expect("deserialize");
        assert_eq!(back, sample);
    }

    #[test]
    fn an_absent_option_is_null_and_reads_back_as_none() {
        let mut sample = sample();
        sample.maybe = None;
        let json = serde_json::to_value(&sample).expect("serialize");
        assert!(json["maybe"].is_null());
        let back: Sample = serde_json::from_value(json.clone()).expect("deserialize");
        assert_eq!(back.maybe, None);

        // A missing key reads as none too, which keeps older files readable.
        let mut object = json;
        object.as_object_mut().expect("an object").remove("maybe");
        let back: Sample = serde_json::from_value(object).expect("deserialize");
        assert_eq!(back.maybe, None);
    }

    #[test]
    fn the_wrong_length_is_refused() {
        let error = serde_json::from_str::<Sample>(&json_with("id", &"ab".repeat(31)))
            .expect_err("31 bytes is not an id");
        assert!(
            error.to_string().contains("expected 32 bytes, got 31"),
            "{error}"
        );
        let error = serde_json::from_str::<Sample>(&json_with("seed", &"ab".repeat(32)))
            .expect_err("32 bytes is not a seed");
        assert!(
            error.to_string().contains("expected 64 bytes, got 32"),
            "{error}"
        );
        assert!(serde_json::from_str::<Sample>(&json_with("txid", "")).is_err());
    }

    #[test]
    fn text_that_is_not_hex_is_refused_without_repeating_it() {
        for text in [
            "zz".repeat(32),
            "0x".to_owned() + &"ab".repeat(32),
            "abc".to_owned(),
        ] {
            let error =
                serde_json::from_str::<Sample>(&json_with("id", &text)).expect_err("not hex");
            assert!(error.to_string().contains("not a hex string"), "{error}");
            assert!(
                !error.to_string().contains(&text),
                "the error repeats the text"
            );
        }
    }

    #[test]
    fn a_non_canonical_scalar_is_refused() {
        // The group order, and the largest 256-bit value, both exceed every canonical scalar.
        let order = (-scalar::Scalar::ONE).to_bytes();
        let mut over = order;
        over[0] += 1;
        for bytes in [over, [0xff; 32]] {
            let error = serde_json::from_str::<Sample>(&json_with("key", &hex::encode(bytes)))
                .expect_err("non-canonical");
            assert!(
                error.to_string().contains("not a canonical scalar"),
                "{error}"
            );
            let error = serde_json::from_str::<Sample>(&json_with("flavor", &hex::encode(bytes)))
                .expect_err("non-canonical");
            assert!(
                error.to_string().contains("not a canonical scalar"),
                "{error}"
            );
        }
        // The largest canonical scalar is fine.
        let largest = hex::encode(order);
        assert!(serde_json::from_str::<Sample>(&json_with("key", &largest)).is_ok());
    }

    #[test]
    fn an_invalid_point_is_refused() {
        // All ones is not a canonical Ristretto encoding.
        let error = serde_json::from_str::<Sample>(&json_with("point", &"ff".repeat(32)))
            .expect_err("not a point");
        assert!(
            error.to_string().contains("not a valid Ristretto point"),
            "{error}"
        );
    }

    #[test]
    fn a_value_that_is_not_a_string_is_refused() {
        let mut value = serde_json::to_value(sample()).expect("serialize");
        value["id"] = serde_json::json!(vec![0u8; 32]);
        assert!(serde_json::from_value::<Sample>(value).is_err());
    }
}
