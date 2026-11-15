//! Data Streams V3 report decoding, nothing but the wire format. The layout
//! is ported from Chainlink's reference decoder (`data-streams-sdk`, MIT).
//! A report is nine 32-byte ABI words, each validated structurally here,
//! numeric width included. Meaning (freshness, price sanity, spread) lives
//! in [`crate::verify`].

use soroban_sdk::{panic_with_error, Bytes, BytesN, Env};

use crate::error::OracleError;

/// A V3 report body is exactly nine 32-byte ABI words.
const V3_REPORT_LEN: u32 = 288;

/// The decoded V3 report, field for field the reference layout.
///
/// The fee fields are inert under subscription billing. They are still
/// decoded and structurally validated like every other word, so nothing in
/// the body is accepted unparsed.
pub struct ReportDataV3 {
    /// Word 0. The stream id.
    pub feed_id: BytesN<32>,
    /// Word 1, `uint32` (seconds). The start of the validity window.
    pub valid_from_timestamp: u64,
    /// Word 2, `uint32` (seconds). The observation time.
    pub observations_timestamp: u64,
    /// Word 3, `uint192` big-endian. Inert.
    pub native_fee: [u8; 24],
    /// Word 4, `uint192` big-endian. Inert.
    pub link_fee: [u8; 24],
    /// Word 5, `uint32` (seconds). The end of the validity window.
    pub expires_at: u64,
    /// Word 6, `int192`, in the feed's native precision.
    pub benchmark_price: i128,
    /// Word 7, `int192`, in the feed's native precision.
    pub bid: i128,
    /// Word 8, `int192`, in the feed's native precision.
    pub ask: i128,
}

/// Decodes a verified V3 report body.
///
/// # Errors
/// - [`OracleError::InvalidData`] if the body is not exactly nine words or a
///   word carries non-zero padding.
/// - [`OracleError::InvalidPrice`] if an `int192` value overflows `i128`.
pub fn decode(env: &Env, body: &Bytes) -> ReportDataV3 {
    if body.len() != V3_REPORT_LEN {
        panic_with_error!(env, OracleError::InvalidData);
    }
    let mut raw = [0u8; V3_REPORT_LEN as usize];
    body.copy_into_slice(&mut raw);
    ReportDataV3 {
        feed_id: BytesN::from_array(env, word(&raw, 0)),
        valid_from_timestamp: read_u32(env, word(&raw, 1)),
        observations_timestamp: read_u32(env, word(&raw, 2)),
        native_fee: read_u192(env, word(&raw, 3)),
        link_fee: read_u192(env, word(&raw, 4)),
        expires_at: read_u32(env, word(&raw, 5)),
        benchmark_price: read_i192(env, word(&raw, 6)),
        bid: read_i192(env, word(&raw, 7)),
        ask: read_i192(env, word(&raw, 8)),
    }
}

/// Returns ABI word `index` of the copied body.
fn word(raw: &[u8; V3_REPORT_LEN as usize], index: usize) -> &[u8; 32] {
    raw[index * 32..(index + 1) * 32].try_into().unwrap()
}

/// Decodes a right-aligned `uint32` ABI word. Non-zero padding rejects.
fn read_u32(env: &Env, word: &[u8; 32]) -> u64 {
    if word[..28].iter().any(|b| *b != 0) {
        panic_with_error!(env, OracleError::InvalidData);
    }
    u32::from_be_bytes(word[28..32].try_into().unwrap()) as u64
}

/// Decodes a right-aligned `uint192` ABI word. The 8 padding bytes must be
/// zero. The value stays raw at its full 24-byte width.
fn read_u192(env: &Env, word: &[u8; 32]) -> [u8; 24] {
    if word[..8].iter().any(|b| *b != 0) {
        panic_with_error!(env, OracleError::InvalidData);
    }
    word[8..32].try_into().unwrap()
}

/// Decodes a sign-extended `int192` ABI word into `i128`. The top 16 bytes
/// must be the uniform extension of the low half's sign. Otherwise the value
/// overflows `i128` and rejects.
fn read_i192(env: &Env, word: &[u8; 32]) -> i128 {
    let low: [u8; 16] = word[16..32].try_into().unwrap();
    let extension = if low[0] & 0x80 != 0 { 0xff } else { 0x00 };
    if word[..16].iter().any(|b| *b != extension) {
        panic_with_error!(env, OracleError::InvalidPrice);
    }
    i128::from_be_bytes(low)
}

#[cfg(test)]
mod tests {
    use soroban_sdk::Env;

    use super::*;

    #[test]
    fn i192_decodes_negative_values_via_sign_extension() {
        let env = Env::default();
        let mut w = [0xff_u8; 32];
        w[16..32].copy_from_slice(&(-42_i128).to_be_bytes());
        assert_eq!(read_i192(&env, &w), -42);
        assert_eq!(
            read_i192(&env, &[0xff_u8; 32]),
            -1,
            "all-ones is minus one, not an overflow"
        );
    }

    #[test]
    fn i192_decodes_i128_extremes_exactly() {
        let env = Env::default();
        let mut max = [0u8; 32];
        max[16..32].copy_from_slice(&i128::MAX.to_be_bytes());
        assert_eq!(read_i192(&env, &max), i128::MAX);
        let mut min = [0xff_u8; 32];
        min[16..32].copy_from_slice(&i128::MIN.to_be_bytes());
        assert_eq!(read_i192(&env, &min), i128::MIN);
    }

    #[test]
    #[should_panic(expected = "#781")]
    fn i192_rejects_a_mixed_extension_as_overflow() {
        // The sign bit is set in the low half but the extension is zero: a
        // positive int192 too wide for i128.
        let env = Env::default();
        let mut w = [0u8; 32];
        w[16] = 0x80;
        read_i192(&env, &w);
    }

    #[test]
    fn u32_word_reads_the_low_four_bytes() {
        let env = Env::default();
        let mut w = [0u8; 32];
        w[28..32].copy_from_slice(&u32::MAX.to_be_bytes());
        assert_eq!(read_u32(&env, &w), u32::MAX as u64);
    }

    #[test]
    fn u192_fee_decodes_the_full_abi_range() {
        let env = Env::default();
        let mut w = [0u8; 32];
        w[8..32].copy_from_slice(&[0xff; 24]);
        assert_eq!(
            read_u192(&env, &w),
            [0xff; 24],
            "uint192::MAX is a valid fee"
        );
    }

    #[test]
    #[should_panic(expected = "#780")]
    fn u192_fee_with_nonzero_padding_rejects() {
        let env = Env::default();
        let mut w = [0u8; 32];
        w[7] = 1;
        read_u192(&env, &w);
    }
}
