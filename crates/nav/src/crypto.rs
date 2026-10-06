// SPDX-License-Identifier: MPL-2.0
//! NAV request crypto: the technical user's password hash, the two `requestSignature`
//! constructions, and exchange-token decryption.
//!
//! Nothing here formats timestamps: callers pass `ts` already rendered as `yyyyMMddHHmmss`,
//! UTC, no separators.

use aes::Aes128;
use aes::cipher::{Block, BlockCipherDecrypt, KeyInit};
use base64::{Engine as _, engine::general_purpose::STANDARD as B64};
use mintworks_core::error::{ClResult, Error, StatusCode};
use sha2::Sha512;
use sha3::{Digest, Sha3_512};

/// AES block size, and the length of the exchange key.
const BLOCK: usize = 16;

/// `user/passwordHash`: uppercase hex SHA-512 of the technical user's password.
pub fn password_hash(password: &str) -> String {
	hex::encode_upper(Sha512::digest(password.as_bytes()))
}

/// `user/requestSignature` for `tokenExchange` and the `query*` operations:
/// `UPPERHEX(SHA3-512(requestId || ts || signKey))`.
pub fn request_signature(request_id: &str, ts: &str, sign_key: &str) -> String {
	hex::encode_upper(Sha3_512::digest(format!("{request_id}{ts}{sign_key}").as_bytes()))
}

/// `user/requestSignature` for `manageInvoice`: each invoice appends
/// `UPPERHEX(SHA3-512(invoiceOperation || base64(invoiceData)))` to the base, in batch order.
///
/// `invoices` holds `(invoiceOperation, base64 invoiceData)`. The base64 must be the exact
/// bytes that go on the wire, never a re-serialisation of the same document.
pub fn request_signature_invoices(
	request_id: &str,
	ts: &str,
	sign_key: &str,
	invoices: &[(&str, &str)],
) -> String {
	let mut base = format!("{request_id}{ts}{sign_key}");
	for (op, data_b64) in invoices {
		base.push_str(&hex::encode_upper(Sha3_512::digest(format!("{op}{data_b64}").as_bytes())));
	}
	hex::encode_upper(Sha3_512::digest(base.as_bytes()))
}

/// Decrypt `encodedExchangeToken`: base64, then AES-128-ECB under the 16-byte exchange key.
pub fn decrypt_exchange_token(encoded: &str, exchange_key: &[u8]) -> ClResult<String> {
	let key: [u8; BLOCK] = exchange_key
		.try_into()
		.map_err(|_| err("nav.exchange_key must be exactly 16 bytes"))?;
	let mut buf = B64.decode(encoded).map_err(|_| err("encodedExchangeToken is not base64"))?;
	if buf.is_empty() || buf.len() % BLOCK != 0 {
		return Err(err("encodedExchangeToken is not a whole number of AES blocks"));
	}
	let cipher = Aes128::new(&key.into());
	for block in buf.chunks_exact_mut(BLOCK) {
		let block: &mut Block<Aes128> = block.try_into().map_err(|_| err("AES block size"))?;
		cipher.decrypt_block(block);
	}
	strip_padding(&mut buf);
	String::from_utf8(buf).map_err(|_| err("decrypted exchange token is not UTF-8"))
}

/// NAV's own samples use Java's `AES/ECB/PKCS5Padding`, but the token is itself block-sized,
/// so an unpadded response is decodable too. Strip only well-formed padding.
// Tolerant strip; tighten to a strict PKCS#7 check once a real sandbox response settles which.
fn strip_padding(buf: &mut Vec<u8>) {
	let Some(&last) = buf.last() else { return };
	let n = last as usize;
	if (1..=BLOCK).contains(&n) && n <= buf.len() && buf[buf.len() - n..].iter().all(|&b| b == last)
	{
		buf.truncate(buf.len() - n);
	}
}

fn err(msg: &str) -> Error {
	Error::coded(StatusCode::BAD_GATEWAY, "E-NAV-CREDENTIALS", msg)
}

#[cfg(test)]
mod tests {
	use super::*;

	// Expected digests computed independently (python3 hashlib / openssl enc), not by this code.
	const REQ: &str = "TSTKFT1222564";
	const TS: &str = "20240101120000";
	const KEY: &str = "TESTSIGNKEY";

	#[test]
	fn password_hash_is_uppercase_hex_sha512() {
		assert_eq!(
			password_hash("Test1234!"),
			"6C759141FEBD969896A49C013271968942DAE74B7F17516D03C3F34BB033D4BEBD59166DE124FD141A22066FCA287703C80C822B65EE09A75F5C2535FE8840AC"
		);
	}

	#[test]
	fn signature_hashes_request_id_then_ts_then_key() {
		assert_eq!(
			request_signature(REQ, TS, KEY),
			"A6562A0BE06D42C3942D0398531A647DFB3D9B73A3B148248B8D6DF0FD243DFDF708BBDB063CE87B629D77BB9CC60F75ECEF19952314A37FCF9D916AF487E7A1"
		);
	}

	#[test]
	fn invoice_signature_appends_the_per_invoice_chunk() {
		assert_eq!(
			request_signature_invoices(REQ, TS, KEY, &[("CREATE", "PGludm9pY2U+")]),
			"B48877FBE6041BF90B6E970C3B01D04180579F0E2E74853EB9DA3F610EBE2EF939D493A6ADA4B71C49E66604CFF1D028FC3C46A0EBD601B0339DA349B4E36FCC"
		);
		// No invoices degrades to the plain construction.
		assert_eq!(request_signature_invoices(REQ, TS, KEY, &[]), request_signature(REQ, TS, KEY));
	}

	#[test]
	fn exchange_token_decrypts_unpadded_and_padded() {
		let key = b"0123456789abcdef";
		assert!(matches!(
			decrypt_exchange_token("xkak2Cuc/UlJ/hd7+MMWKw==", key).as_deref(),
			Ok("TOKEN-ABCDEFGHI!")
		));
		assert!(matches!(
			decrypt_exchange_token("PNV9pzxii466BWprFsPtug==", key).as_deref(),
			Ok("TOKEN-ABC")
		));
		assert!(decrypt_exchange_token("xkak2Cuc/UlJ/hd7+MMWKw==", b"short").is_err());
		assert!(decrypt_exchange_token("bm90YWJsb2Nr", key).is_err());
	}
}

// vim: ts=4
