//! Data Streams V3 report encoding and deterministic test signing.
//! Signer scalars 1..31 are public fixtures and must never secure funds.

extern crate alloc;

use alloc::vec::Vec as StdVec;
use k256::ecdsa::SigningKey;
use sha3::{Digest, Keccak256};
use soroban_sdk::{Bytes, BytesN, Env, Vec};

/// Config digest used by the shared test verifier.
pub const CONFIG: [u8; 32] = [0x42; 32];
/// Default f=1 verifier: four configured signers and two signatures per report.
pub const DEFAULT_FAULT_TOLERANCE: u32 = 1;

/// A report's shelf-life past its observation time (30 days, matching the
/// production DON's issuance).
pub const REPORT_VALIDITY_SECONDS: u64 = 2_592_000;

/// Timestamps of a report, for tests that steer them independently.
#[derive(Clone, Copy)]
pub struct ReportTimes {
    pub valid_from: u64,
    pub observations: u64,
    pub expires_at: u64,
}

impl ReportTimes {
    /// The latest-report shape: `valid_from == observations`, expiry 30 days out.
    pub fn at(observations: u64) -> Self {
        ReportTimes {
            valid_from: observations,
            observations,
            expires_at: observations + REPORT_VALIDITY_SECONDS,
        }
    }
}

fn push_u64_word(buf: &mut StdVec<u8>, value: u64) {
    let mut word = [0u8; 32];
    word[24..32].copy_from_slice(&value.to_be_bytes());
    buf.extend_from_slice(&word);
}

fn push_i128_word(buf: &mut StdVec<u8>, value: i128) {
    let mut word = if value < 0 { [0xff_u8; 32] } else { [0u8; 32] };
    word[16..32].copy_from_slice(&value.to_be_bytes());
    buf.extend_from_slice(&word);
}

/// Build a V3 report body for `feed` with an explicit bid/ask around `price`.
///
/// # Layout (nine 32-byte ABI words)
/// ```text
/// [feedId][validFrom u32][observations u32][nativeFee][linkFee]
/// [expiresAt u32][price int192][bid int192][ask int192]
/// ```
pub fn build_report(
    env: &Env,
    feed: &BytesN<32>,
    times: ReportTimes,
    price: i128,
    bid: i128,
    ask: i128,
) -> Bytes {
    let mut buf: StdVec<u8> = StdVec::with_capacity(288);
    buf.extend_from_slice(&feed.to_array());
    push_u64_word(&mut buf, times.valid_from);
    push_u64_word(&mut buf, times.observations);
    push_i128_word(&mut buf, 0); // nativeFee, inert
    push_i128_word(&mut buf, 0); // linkFee, inert
    push_u64_word(&mut buf, times.expires_at);
    push_i128_word(&mut buf, price);
    push_i128_word(&mut buf, bid);
    push_i128_word(&mut buf, ask);
    Bytes::from_slice(env, &buf)
}

fn signer(scalar: u32) -> SigningKey {
    let mut bytes = [0u8; 32];
    bytes[28..].copy_from_slice(&scalar.to_be_bytes());
    SigningKey::from_bytes((&bytes).into()).unwrap()
}

pub fn signer_addresses(env: &Env, first: u32, count: u32) -> Vec<BytesN<20>> {
    Vec::from_iter(
        env,
        (first..first + count).map(|scalar| {
            let point = signer(scalar).verifying_key().to_encoded_point(false);
            let hash = Keccak256::digest(&point.as_bytes()[1..]);
            BytesN::from_array(env, hash[12..].try_into().unwrap())
        }),
    )
}

/// ABI encodes `[context; 3]`, report bytes, `R[]`, `S[]`, and recovery bytes. Digest
/// formula follows the pinned verifier source, not the Zenex oracle decoder.
pub fn envelope(env: &Env, body: &Bytes, config: [u8; 32], first: u32, count: u32) -> Bytes {
    let body = body.to_alloc_vec();
    assert_eq!(body.len(), 288);
    assert!(count <= 31);
    let mut context = [0u8; 96];
    context[..32].copy_from_slice(&config);
    let mut digest_input = Keccak256::digest(&body).to_vec();
    digest_input.extend_from_slice(&context);
    let digest = Keccak256::digest(digest_input);
    let rs_offset = 224 + 32 + body.len();
    let ss_offset = rs_offset + 32 + count as usize * 32;
    let mut blob = std::vec![0u8; ss_offset + 32 + count as usize * 32];
    let put_word = |blob: &mut [u8], offset: usize, value: usize| {
        blob[offset + 24..offset + 32].copy_from_slice(&(value as u64).to_be_bytes());
    };
    blob[..96].copy_from_slice(&context);
    put_word(&mut blob, 96, 224);
    put_word(&mut blob, 128, rs_offset);
    put_word(&mut blob, 160, ss_offset);
    put_word(&mut blob, 224, body.len());
    blob[256..256 + body.len()].copy_from_slice(&body);
    put_word(&mut blob, rs_offset, count as usize);
    put_word(&mut blob, ss_offset, count as usize);
    for (i, scalar) in (first..first + count).enumerate() {
        let (signature, recovery) = signer(scalar).sign_prehash_recoverable(&digest).unwrap();
        // k256 emits low-S signatures and adjusts recovery parity itself.
        assert!(signature.normalize_s().is_none());
        blob[192 + i] = recovery.to_byte();
        blob[rs_offset + 32 + i * 32..rs_offset + 64 + i * 32]
            .copy_from_slice(&signature.r().to_bytes());
        blob[ss_offset + 32 + i * 32..ss_offset + 64 + i * 32]
            .copy_from_slice(&signature.s().to_bytes());
    }
    Bytes::from_slice(env, &blob)
}

pub fn hex(bytes: &[u8]) -> String {
    bytes.iter().map(|b| format!("{b:02x}")).collect()
}
