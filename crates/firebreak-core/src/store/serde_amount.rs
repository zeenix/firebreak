//! Amounts of sparks as decimal strings, for `#[serde(with = "...")]` fields.
//!
//! JSON numbers lose precision in many readers, so every amount in a Firebreak file is a string
//! such as `"50"`. Reading is strict: only plain digits make an amount, with no sign, no spaces, no
//! fraction or exponent, and no leading zeros, so each amount has exactly one spelling.

use serde::Serializer;
use serde::de::{self, Deserialize, Deserializer};

/// Writes an amount as a decimal string.
pub fn serialize<S>(amount: &u64, serializer: S) -> Result<S::Ok, S::Error>
where
    S: Serializer,
{
    serializer.collect_str(amount)
}

/// Reads an amount from a decimal string.
pub fn deserialize<'de, D>(deserializer: D) -> Result<u64, D::Error>
where
    D: Deserializer<'de>,
{
    let text = String::deserialize(deserializer)?;
    parse(&text).map_err(de::Error::custom)
}

/// The amount a decimal string spells, such as a command-line argument or a JSON string.
///
/// Only the digits `0` to `9` are accepted, without a leading zero unless the amount is `0`, and
/// the amount must fit in 64 bits.
pub fn parse(text: &str) -> Result<u64, AmountError> {
    let plain = !text.is_empty()
        && text.bytes().all(|byte| byte.is_ascii_digit())
        && (text == "0" || !text.starts_with('0'));
    if !plain {
        return Err(AmountError::NotAnInteger(text.to_owned()));
    }
    text.parse()
        .map_err(|_| AmountError::TooLarge(text.to_owned()))
}

/// Why text is not an amount.
#[derive(Clone, Debug, PartialEq, Eq, thiserror::Error)]
pub enum AmountError {
    /// The text is not a plain non-negative integer.
    #[error("{0:?} is not a plain non-negative integer")]
    NotAnInteger(String),

    /// The text is an integer that does not fit in 64 bits.
    #[error("{0} does not fit in 64 bits")]
    TooLarge(String),
}

#[cfg(test)]
mod tests {
    use serde::{Deserialize, Serialize};

    use super::*;

    #[derive(Debug, PartialEq, Serialize, Deserialize)]
    struct Sample {
        #[serde(with = "super")]
        qty: u64,
    }

    fn read(text: &str) -> Result<Sample, serde_json::Error> {
        serde_json::from_str(&format!(r#"{{"qty": {text}}}"#))
    }

    #[test]
    fn an_amount_is_a_decimal_string() {
        for amount in [0, 1, 50, 1_000, u64::MAX] {
            let json = serde_json::to_string(&Sample { qty: amount }).expect("serialize");
            assert_eq!(json, format!(r#"{{"qty":"{amount}"}}"#));
            let back: Sample = serde_json::from_str(&json).expect("deserialize");
            assert_eq!(back.qty, amount);
        }
    }

    #[test]
    fn anything_but_a_plain_non_negative_integer_is_refused() {
        for text in [
            "", " 5", "5 ", "+5", "-5", "-0", "5.0", "5.", ".5", "1e3", "0x10", "٥", "5_000",
            "five", "00", "05", "050",
        ] {
            assert_eq!(
                parse(text),
                Err(AmountError::NotAnInteger(text.to_owned())),
                "{text:?}"
            );
            assert!(read(&format!("\"{text}\"")).is_err(), "{text:?}");
        }
    }

    #[test]
    fn an_amount_beyond_sixty_four_bits_is_refused() {
        assert_eq!(parse("18446744073709551615"), Ok(u64::MAX));
        assert_eq!(
            parse("18446744073709551616"),
            Err(AmountError::TooLarge("18446744073709551616".to_owned()))
        );
        assert!(read(r#""99999999999999999999999""#).is_err());
    }

    #[test]
    fn a_json_number_is_not_an_amount() {
        for text in ["50", "5.5", "-1", "true", "null", "[]"] {
            assert!(read(text).is_err(), "{text}");
        }
        assert_eq!(read(r#""50""#).expect("a string").qty, 50);
    }
}
