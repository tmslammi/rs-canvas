//! Fractional indexing: string keys ordered lexicographically that always
//! allow a new key between any two neighbours.
//!
//! Port of David Greenspan's reference implementation
//! (<https://observablehq.com/@dgreensp/implementing-fractional-indexing>),
//! base-62, with the same `a0` starting key. Keys never end in `0` (that
//! rule is what keeps a midpoint available between any pair) and are only
//! ever compared as strings.

use std::fmt;

const DIGITS: &[u8] = b"0123456789ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz";
const INTEGER_ZERO: &str = "a0";
/// `A` followed by 26 zeroes; nothing may sort before it.
const SMALLEST_INTEGER: &str = "A00000000000000000000000000";

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum IndexError {
    /// The key is not a valid order key.
    InvalidKey,
    /// No key exists beyond this point (keyspace exhausted).
    OutOfRange,
    /// `a` does not sort before `b`.
    OutOfOrder,
}

impl fmt::Display for IndexError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            IndexError::InvalidKey => write!(f, "invalid order key"),
            IndexError::OutOfRange => write!(f, "order key out of range"),
            IndexError::OutOfOrder => write!(f, "order keys out of order"),
        }
    }
}

impl std::error::Error for IndexError {}

fn digit_index(c: u8) -> Option<usize> {
    DIGITS.iter().position(|&d| d == c)
}

fn integer_length(head: u8) -> Result<usize, IndexError> {
    match head {
        b'a'..=b'z' => Ok((head - b'a') as usize + 2),
        b'A'..=b'Z' => Ok((b'Z' - head) as usize + 2),
        _ => Err(IndexError::InvalidKey),
    }
}

fn integer_part(key: &str) -> Result<&str, IndexError> {
    let head = *key.as_bytes().first().ok_or(IndexError::InvalidKey)?;
    let len = integer_length(head)?;
    if len > key.len() {
        return Err(IndexError::InvalidKey);
    }
    Ok(&key[..len])
}

/// Check a key obeys the invariants (non-empty, no trailing zero).
pub fn validate_order_key(key: &str) -> Result<(), IndexError> {
    if key.is_empty() || key == SMALLEST_INTEGER {
        return Err(IndexError::InvalidKey);
    }
    let int = integer_part(key)?;
    if key[int.len()..].ends_with('0') {
        return Err(IndexError::InvalidKey);
    }
    Ok(())
}

fn validate_integer(int: &str) -> Result<(), IndexError> {
    let head = *int.as_bytes().first().ok_or(IndexError::InvalidKey)?;
    if integer_length(head)? != int.len() {
        return Err(IndexError::InvalidKey);
    }
    Ok(())
}

fn increment_integer(x: &str) -> Result<Option<String>, IndexError> {
    validate_integer(x)?;
    let mut chars = x.as_bytes().to_vec();
    let head = chars[0];
    let mut carry = true;
    let mut i = chars.len();
    while carry && i > 1 {
        i -= 1;
        let d = digit_index(chars[i]).ok_or(IndexError::InvalidKey)?;
        if d + 1 == DIGITS.len() {
            chars[i] = DIGITS[0];
        } else {
            chars[i] = DIGITS[d + 1];
            carry = false;
        }
    }
    if carry {
        if head == b'Z' {
            return Ok(Some(INTEGER_ZERO.to_string()));
        }
        if head == b'z' {
            return Ok(None);
        }
        let h = head + 1;
        if h > b'a' {
            chars.push(DIGITS[0]);
        } else {
            chars.pop();
        }
        chars[0] = h;
    }
    Ok(Some(String::from_utf8(chars).expect("ascii keys")))
}

fn decrement_integer(x: &str) -> Result<Option<String>, IndexError> {
    validate_integer(x)?;
    let mut chars = x.as_bytes().to_vec();
    let head = chars[0];
    let mut borrow = true;
    let mut i = chars.len();
    while borrow && i > 1 {
        i -= 1;
        let d = digit_index(chars[i]).ok_or(IndexError::InvalidKey)?;
        if d == 0 {
            chars[i] = DIGITS[DIGITS.len() - 1];
        } else {
            chars[i] = DIGITS[d - 1];
            borrow = false;
        }
    }
    if borrow {
        if head == b'a' {
            return Ok(Some(
                String::from_utf8(vec![b'Z', DIGITS[DIGITS.len() - 1]]).unwrap(),
            ));
        }
        if head == b'A' {
            return Ok(None);
        }
        let h = head - 1;
        if h < b'Z' {
            chars.push(DIGITS[DIGITS.len() - 1]);
        } else {
            chars.pop();
        }
        chars[0] = h;
    }
    Ok(Some(String::from_utf8(chars).expect("ascii keys")))
}

/// A string strictly between `a` and `b` (empty `a` means before everything,
/// `None` `b` means after everything).
fn midpoint(a: &str, b: Option<&str>) -> Result<String, IndexError> {
    if let Some(b) = b {
        if a >= b {
            return Err(IndexError::OutOfOrder);
        }
    }
    if a.ends_with('0') || b.is_some_and(|b| b.ends_with('0')) {
        return Err(IndexError::InvalidKey);
    }
    if let Some(b) = b {
        let mut n = 0;
        while n < b.len() && a.as_bytes().get(n).copied().unwrap_or(b'0') == b.as_bytes()[n] {
            n += 1;
        }
        if n > 0 {
            let rest_a = a.get(n..).unwrap_or("");
            let rest_b = &b[n..];
            return Ok(format!("{}{}", &b[..n], midpoint(rest_a, Some(rest_b))?));
        }
    }
    let digit_a: i32 = match a.as_bytes().first() {
        Some(&c) => digit_index(c).ok_or(IndexError::InvalidKey)? as i32,
        None => 0,
    };
    let digit_b: i32 = match b.and_then(|b| b.as_bytes().first()) {
        Some(&c) => digit_index(c).ok_or(IndexError::InvalidKey)? as i32,
        None => DIGITS.len() as i32,
    };
    if digit_b - digit_a > 1 {
        let mid = ((digit_a + digit_b) as f64 * 0.5).round() as usize;
        return Ok((DIGITS[mid] as char).to_string());
    }
    if let Some(b) = b {
        if b.len() > 1 {
            return Ok(b[..1].to_string());
        }
    }
    let rest = a.get(1..).unwrap_or("");
    Ok(format!(
        "{}{}",
        DIGITS[digit_a as usize] as char,
        midpoint(rest, None)?
    ))
}

/// A key strictly between `a` and `b`.
///
/// `None` means "no neighbour on that side": `key_between(None, None)` is the
/// first key, `key_between(Some(k), None)` appends after `k`.
pub fn key_between(a: Option<&str>, b: Option<&str>) -> Result<String, IndexError> {
    if let Some(a) = a {
        validate_order_key(a)?;
    }
    if let Some(b) = b {
        validate_order_key(b)?;
    }
    if let (Some(a), Some(b)) = (a, b) {
        if a >= b {
            return Err(IndexError::OutOfOrder);
        }
    }
    match (a, b) {
        (None, None) => Ok(INTEGER_ZERO.to_string()),
        (None, Some(b)) => {
            let ib = integer_part(b)?;
            let fb = &b[ib.len()..];
            if ib == SMALLEST_INTEGER {
                return Ok(format!("{}{}", ib, midpoint("", Some(fb))?));
            }
            if ib < b {
                return Ok(ib.to_string());
            }
            decrement_integer(ib)?.ok_or(IndexError::OutOfRange)
        }
        (Some(a), None) => {
            let ia = integer_part(a)?;
            let fa = &a[ia.len()..];
            match increment_integer(ia)? {
                None => Ok(format!("{}{}", ia, midpoint(fa, None)?)),
                Some(i) => Ok(i),
            }
        }
        (Some(a), Some(b)) => {
            let ia = integer_part(a)?;
            let fa = &a[ia.len()..];
            let ib = integer_part(b)?;
            let fb = &b[ib.len()..];
            if ia == ib {
                return Ok(format!("{}{}", ia, midpoint(fa, Some(fb))?));
            }
            match increment_integer(ia)? {
                None => Err(IndexError::OutOfRange),
                Some(i) => {
                    if i.as_str() < b {
                        Ok(i)
                    } else {
                        Ok(format!("{}{}", ia, midpoint(fa, None)?))
                    }
                }
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn first_key() {
        assert_eq!(key_between(None, None).unwrap(), "a0");
    }

    #[test]
    fn append_sequence_is_ordered() {
        let mut keys = vec![key_between(None, None).unwrap()];
        for _ in 0..100 {
            let next = key_between(keys.last().map(String::as_str), None).unwrap();
            assert!(next > *keys.last().unwrap());
            keys.push(next);
        }
        let mut sorted = keys.clone();
        sorted.sort();
        assert_eq!(keys, sorted);
    }

    #[test]
    fn prepend_sequence_is_ordered() {
        let mut keys = vec![key_between(None, None).unwrap()];
        for _ in 0..50 {
            let prev = key_between(None, Some(keys.first().unwrap())).unwrap();
            assert!(prev < *keys.first().unwrap());
            keys.insert(0, prev);
        }
        let mut sorted = keys.clone();
        sorted.sort();
        assert_eq!(keys, sorted);
    }

    #[test]
    fn insert_between_neighbours() {
        let a = "a0";
        let b = "a1";
        let mid = key_between(Some(a), Some(b)).unwrap();
        assert!(a < mid.as_str() && mid.as_str() < b, "got {mid}");
        let mid2 = key_between(Some(a), Some(&mid)).unwrap();
        assert!(a < mid2.as_str() && mid2.as_str() < mid.as_str());
        let mid3 = key_between(Some(&mid), Some(b)).unwrap();
        assert!(mid.as_str() < mid3.as_str() && mid3.as_str() < b);
    }

    #[test]
    fn repeated_bisection_stays_ordered() {
        let mut lo = key_between(None, None).unwrap();
        let mut hi = key_between(Some(&lo), None).unwrap();
        for _ in 0..200 {
            let mid = key_between(Some(&lo), Some(&hi)).unwrap();
            assert!(lo < mid && mid < hi, "{lo} < {mid} < {hi}");
            if mid.len().is_multiple_of(2) {
                lo = mid;
            } else {
                hi = mid;
            }
        }
    }

    #[test]
    fn rejects_bad_arguments() {
        assert_eq!(
            key_between(Some("a1"), Some("a1")),
            Err(IndexError::OutOfOrder)
        );
        assert_eq!(key_between(Some("b0"), None), Err(IndexError::InvalidKey));
        assert_eq!(key_between(Some(""), None), Err(IndexError::InvalidKey));
    }

    #[test]
    fn cannot_use_smallest_integer_as_a_key() {
        assert_eq!(
            key_between(None, Some(SMALLEST_INTEGER)),
            Err(IndexError::InvalidKey)
        );
    }

    #[test]
    fn keys_sort_below_and_above_a0() {
        let below = key_between(None, Some("a0")).unwrap();
        assert!(below.as_str() < "a0");
        let above = key_between(Some("a0"), None).unwrap();
        assert!(above.as_str() > "a0");
    }
}
