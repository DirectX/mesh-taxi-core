use blake3::Hasher;
use ed25519_dalek::SigningKey;
use h3o::CellIndex;
use sha2::{Digest, Sha256};
use veilid_core::{
    BareOpaqueRecordKey, BarePublicKey, BareRecordKey, BareSecretKey, CRYPTO_KIND_VLD0, DHTSchema,
    KeyPair, RecordKey,
};

use crate::error::CoreResult;

/// Number of writable subkeys (ride request slots) in a board record.
/// One request occupies one subkey, derived from its request id.
pub const BOARD_SCHEMA_O_CNT: u16 = 256;
/// Domain prefix for the per-cell board keypair seed.
const SEED_PREFIX: &str = "mesh_taxi_v1/board/";

/// The DHT schema shared by every board record.
pub fn board_schema() -> DHTSchema {
    DHTSchema::dflt(BOARD_SCHEMA_O_CNT).expect("board schema count is valid")
}

/// Deterministic "board" owner keypair for an H3 cell.
///
/// Every node derives the same key from the cell index, which lets any node
/// address the board record without a bootstrap. The secret is public by
/// design: boards are shared aggregators, ride requests carry their own E2E
/// privacy via the private route blob.
pub fn board_owner_keypair(hex: CellIndex) -> KeyPair {
    let seed = derive_seed(hex);

    let signing_key = SigningKey::from_bytes(&seed);
    let public_bytes = signing_key.verifying_key().to_bytes();

    let public_key =
        veilid_core::PublicKey::new(CRYPTO_KIND_VLD0, BarePublicKey::new(&public_bytes));
    let bare_secret = BareSecretKey::new(&seed);

    KeyPair::new_from_parts(public_key, bare_secret)
}

/// The `RecordKey` at which the board of an H3 cell lives.
///
/// Mirrors `StorageManager::make_record_key`: blake3(kind || owner || schema).
pub fn board_record_key(hex: CellIndex) -> RecordKey {
    let owner = board_owner_keypair(hex);
    let schema_data = board_schema().compile();

    let mut hasher = Hasher::new();
    hasher.update(CRYPTO_KIND_VLD0.bytes());
    hasher.update(owner.key().ref_value().as_ref());
    hasher.update(&schema_data);

    let opaque = BareOpaqueRecordKey::new(hasher.finalize().as_bytes());
    RecordKey::new(CRYPTO_KIND_VLD0, BareRecordKey::new(opaque, None))
}

/// Maps a request id onto a stable subkey inside the board schema.
pub fn subkey_for_request(request_id: &str) -> u32 {
    let hash = Sha256::digest(request_id.as_bytes());
    let n = u64::from_be_bytes(hash[..8].try_into().expect("8 bytes from sha256 digest"));
    (n % BOARD_SCHEMA_O_CNT as u64) as u32
}

/// Validates that a derived subkey fits the board schema (guard against drift).
pub fn validate_subkey(subkey: u32) -> CoreResult<()> {
    if subkey >= BOARD_SCHEMA_O_CNT as u32 {
        return Err(crate::error::CoreError::Encoding(format!(
            "subkey {} out of board schema range",
            subkey
        )));
    }
    Ok(())
}

fn derive_seed(hex: CellIndex) -> [u8; 32] {
    let mut hasher = Sha256::new();
    hasher.update(format!("{}{}", SEED_PREFIX, hex).as_bytes());
    let digest = hasher.finalize();
    let mut seed = [0u8; 32];
    seed.copy_from_slice(&digest);
    seed
}
