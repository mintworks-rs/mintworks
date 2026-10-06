// SPDX-License-Identifier: MPL-2.0
//! The HMAC and comparison primitives every signed token shares.

use hmac::{Hmac, Mac};
use sha2::Sha256;

use crate::error::{ClResult, Error};

/// HMAC-SHA256 of `msg` under `key`, hex-encoded.
pub fn hmac_hex(key: &[u8], msg: &str) -> ClResult<String> {
	let mut mac = <Hmac<Sha256> as Mac>::new_from_slice(key)
		.map_err(|e| Error::internal(format!("hmac key rejected: {e}")))?;
	mac.update(msg.as_bytes());
	Ok(hex::encode(mac.finalize().into_bytes()))
}

/// Length-checked, branch-free comparison, for anything an attacker can retry. `==`
/// short-circuits at the first differing byte.
pub fn ct_eq(a: &[u8], b: &[u8]) -> bool {
	a.len() == b.len() && a.iter().zip(b).fold(0u8, |acc, (x, y)| acc | (x ^ y)) == 0
}

#[cfg(test)]
mod tests {
	use super::*;

	#[test]
	fn ct_eq_is_exact() {
		assert!(ct_eq(b"abc", b"abc"));
		assert!(!ct_eq(b"abc", b"abd"));
		assert!(!ct_eq(b"abc", b"ab"));
	}

	#[test]
	fn hmac_hex_matches_rfc4231_case_2() {
		assert_eq!(
			hmac_hex(b"Jefe", "what do ya want for nothing?").unwrap(),
			"5bdcc146bf60754e6a042426089575c75a003f089d2739839dec58b964ec3843"
		);
	}
}

// vim: ts=4
