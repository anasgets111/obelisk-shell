//! Rust-Lua marshalling boundary, enforcing strict, non-coercive rules for `f64`, `i64`/`u64`,
//! and `String`. `mlua` maps shape but does not reject NaN, unsafe integers, or oversized
//! strings. First caller: `signal::Signal::try_new_direct`.

/// `2^53 - 1`, the largest exact integer in Lua's IEEE-754-double `number`, even when its integer
/// subtype holds it.
const MAX_SAFE_INTEGER: i64 = (1i64 << 53) - 1;
const MIN_SAFE_INTEGER: i64 = -MAX_SAFE_INTEGER;

/// Maximum string size: 64KB.
const MAX_STRING_BYTES: usize = 64 * 1024;

#[derive(Debug, PartialEq, Eq, thiserror::Error)]
pub enum MarshalError {
    #[error("number must be finite, got NaN or Inf")]
    NotFinite,
    #[error("integer outside [{MIN_SAFE_INTEGER}, {MAX_SAFE_INTEGER}]")]
    IntegerOutOfRange,
    #[error("string is {len} bytes, over the {MAX_STRING_BYTES}-byte cap")]
    StringTooLong { len: usize },
}

pub fn check_number(value: f64) -> Result<f64, MarshalError> {
    if value.is_finite() { Ok(value) } else { Err(MarshalError::NotFinite) }
}

pub fn check_integer(value: i64) -> Result<i64, MarshalError> {
    if (MIN_SAFE_INTEGER..=MAX_SAFE_INTEGER).contains(&value) {
        Ok(value)
    } else {
        Err(MarshalError::IntegerOutOfRange)
    }
}

pub fn check_string(value: &str) -> Result<&str, MarshalError> {
    if value.len() <= MAX_STRING_BYTES { Ok(value) } else { Err(MarshalError::StringTooLong { len: value.len() }) }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn check_number_rejects_nan_and_infinity() {
        assert_eq!(check_number(f64::NAN), Err(MarshalError::NotFinite));
        assert_eq!(check_number(f64::INFINITY), Err(MarshalError::NotFinite));
        assert_eq!(check_number(f64::NEG_INFINITY), Err(MarshalError::NotFinite));
    }

    #[test]
    fn check_number_accepts_finite_values() {
        assert_eq!(check_number(0.75), Ok(0.75));
    }

    #[test]
    fn check_integer_rejects_outside_the_2_pow_53_safe_range() {
        assert_eq!(check_integer(MAX_SAFE_INTEGER + 1), Err(MarshalError::IntegerOutOfRange));
        assert_eq!(check_integer(MIN_SAFE_INTEGER - 1), Err(MarshalError::IntegerOutOfRange));
    }

    #[test]
    fn check_integer_accepts_the_safe_range_boundary() {
        assert_eq!(check_integer(MAX_SAFE_INTEGER), Ok(MAX_SAFE_INTEGER));
        assert_eq!(check_integer(MIN_SAFE_INTEGER), Ok(MIN_SAFE_INTEGER));
    }

    #[test]
    fn check_string_rejects_over_64kb() {
        let oversized = "a".repeat(MAX_STRING_BYTES + 1);
        assert_eq!(check_string(&oversized), Err(MarshalError::StringTooLong { len: MAX_STRING_BYTES + 1 }));
    }

    #[test]
    fn check_string_accepts_exactly_64kb() {
        let exact = "a".repeat(MAX_STRING_BYTES);
        assert!(check_string(&exact).is_ok());
    }
}
