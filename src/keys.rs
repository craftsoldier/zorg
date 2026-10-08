use bip0039::{Count, English, Language, Mnemonic};
use secrecy::SecretVec;

use zcash_client_sqlite::AccountUuid;
use zcash_keys::keys::{ReceiverRequirement, UnifiedAddressRequest};
use zeroize::Zeroizing;

use crate::network::WalletNetwork;

use crate::account::*;

pub fn generate_mnemonic() -> String {
    let mnemonic = Mnemonic::<English>::generate(Count::Words24);
    mnemonic.phrase().to_string()
}

/// Return the BIP-39 English word list used for mnemonic validation.
pub fn mnemonic_word_list() -> Vec<String> {
    English::WORD_LIST
        .iter()
        .map(|word| (*word).to_string())
        .collect()
}

fn is_supported_mnemonic_word_count(count: usize) -> bool {
    count == 24
}

/// Convert a mnemonic phrase and optional BIP39 passphrase to a 64-byte seed.
/// The seed is zeroized from memory when the SecretVec is dropped.
pub fn mnemonic_to_seed_with_passphrase(
    phrase: &str,
    bip39_passphrase: &str,
) -> Result<SecretVec<u8>, String> {
    let word_count = phrase.split_whitespace().count();
    if !is_supported_mnemonic_word_count(word_count) {
        return Err("Invalid mnemonic word count: expected 24 words".to_string());
    }

    let mnemonic =
        Mnemonic::<English>::from_phrase(phrase).map_err(|e| format!("Invalid mnemonic: {e}"))?;
    let seed = Zeroizing::new(mnemonic.to_seed(bip39_passphrase));
    drop(mnemonic);
    let secret_seed = SecretVec::new(seed.to_vec());
    drop(seed);
    Ok(secret_seed)
}

/// Convert a mnemonic without a BIP39 passphrase.
pub fn mnemonic_to_seed(phrase: &str) -> Result<SecretVec<u8>, String> {
    mnemonic_to_seed_with_passphrase(phrase, "")
}

/// Parse network string to wallet network enum.
pub fn parse_network(network: &str) -> Result<WalletNetwork, String> {
    network.parse()
}

/// Initialize the wallet database schema. Idempotent — safe to call multiple times.
/// Called without seed to avoid SeedNotRelevant errors when only Imported accounts exist.
pub fn parse_account_uuid(s: &str) -> Result<AccountUuid, String> {
    let uuid = uuid::Uuid::parse_str(s).map_err(|e| format!("Invalid account UUID: {e}"))?;
    Ok(AccountUuid::from_uuid(uuid))
}

/// Resolve account_id: if uuid provided, parse it; otherwise take first account.
pub fn shielded_address_request() -> UnifiedAddressRequest {
    UnifiedAddressRequest::custom(
        ReceiverRequirement::Require, // Orchard
        ReceiverRequirement::Require, // Sapling
        ReceiverRequirement::Omit,    // Transparent
    )
    .expect("valid receiver requirements")
}

// ======================== Public API Structs ========================

pub struct WalletCreationResult {
    pub mnemonic: String,
    pub unified_address: String,
    pub account_number: u32,
}

pub struct WalletImportResult {
    pub unified_address: String,
    pub account_number: u32,
}

pub struct AccountCreationResult {
    pub unified_address: String,
    pub account_uuid: String,
}

// ======================== Convenience Functions ========================

/// Create a new wallet: generate mnemonic, derive seed, create first account.
/// The bootstrap account is always "Account 1" (index 0).
pub fn create_wallet(
    network_str: &str,
    db_path: &str,
    birthday_height: Option<u64>,
) -> Result<WalletCreationResult, String> {
    let network = parse_network(network_str)?;
    let mnemonic = generate_mnemonic();
    let seed = mnemonic_to_seed(&mnemonic)?;
    let created = init_db_and_create_account(db_path, network, &seed, birthday_height)?;
    #[cfg(target_os = "macos")]
    if let Err(e) =
        crate::secret_store::store_mnemonic_in_macos_keychain(network, &created.uuid, &mnemonic)
    {
        log::warn!("Failed to store mnemonic in keychain: {e}");
    }
    Ok(WalletCreationResult {
        mnemonic,
        unified_address: created.unified_address,
        account_number: created.number,
    })
}

/// Import a wallet from a mnemonic phrase.
/// The bootstrap account is always "Account 1" (index 0).
pub fn import_wallet(
    mnemonic: &str,
    bip39_passphrase: &str,
    birthday_height: Option<u64>,
    network_str: &str,
    db_path: &str,
) -> Result<WalletImportResult, String> {
    let network = parse_network(network_str)?;
    let seed = mnemonic_to_seed_with_passphrase(mnemonic, bip39_passphrase)?;
    let created = init_db_and_create_account(db_path, network, &seed, birthday_height)?;
    #[cfg(target_os = "macos")]
    if let Err(e) =
        crate::secret_store::store_mnemonic_in_macos_keychain(network, &created.uuid, mnemonic)
    {
        log::warn!("Failed to store mnemonic in keychain: {e}");
    }
    Ok(WalletImportResult {
        unified_address: created.unified_address,
        account_number: created.number,
    })
}

/// Validate a mnemonic phrase.
pub fn validate_mnemonic(mnemonic: &str) -> bool {
    mnemonic_to_seed(mnemonic).is_ok()
}

#[cfg(test)]
mod tests {
    use secrecy::ExposeSecret;

    use super::*;

    #[test]
    fn test_generate_mnemonic_is_24_words() {
        let phrase = generate_mnemonic();
        let words: Vec<&str> = phrase.split_whitespace().collect();
        assert_eq!(words.len(), 24);
    }

    #[test]
    fn test_mnemonic_to_seed_roundtrip() {
        let phrase = generate_mnemonic();
        let seed = mnemonic_to_seed(&phrase).unwrap();
        assert_eq!(seed.expose_secret().len(), 64);
    }

    #[test]
    fn test_mnemonic_to_seed_accepts_supported_word_counts() {
        let count = Count::Words24;
        let phrase = Mnemonic::<English>::generate(count).phrase().to_string();
        let seed = mnemonic_to_seed(&phrase).unwrap();
        assert_eq!(seed.expose_secret().len(), 64);
    }

    #[test]
    fn test_mnemonic_to_seed_rejects_unsupported_word_counts() {
        for count in [11, 12, 13, 25] {
            let phrase = std::iter::repeat_n("abandon", count)
                .collect::<Vec<_>>()
                .join(" ");
            let error = match mnemonic_to_seed(&phrase) {
                Ok(_) => panic!("unsupported mnemonic word count should be rejected"),
                Err(error) => error,
            };
            assert!(error.contains("expected 24 words"));
        }
    }

    #[test]
    fn test_invalid_mnemonic() {
        let result = mnemonic_to_seed("invalid words here");
        assert!(result.is_err());
    }

    #[test]
    fn test_parse_network() {
        assert!(matches!(parse_network("main"), Ok(WalletNetwork::Main)));
        assert!(matches!(parse_network("test"), Ok(WalletNetwork::Test)));
        assert!(matches!(
            parse_network("regtest"),
            Ok(WalletNetwork::Regtest)
        ));
        assert!(parse_network("invalid").is_err());
    }

    #[test]
    fn test_create_testnet_wallet() {
        let temp_dir = tempfile::tempdir().unwrap();
        let db_path = temp_dir.path().join("wallet.db");
        let db_path_str = db_path.to_str().unwrap();

        let phrase = generate_mnemonic();
        let seed = mnemonic_to_seed(&phrase).unwrap();

        let address = init_db_and_create_account(db_path_str, WalletNetwork::Test, &seed, None)
            .unwrap()
            .unified_address;

        assert!(
            address.starts_with("utest1"),
            "Expected utest1 prefix, got: {address}"
        );
    }

    #[test]
    fn test_deterministic_address_from_same_seed() {
        let phrase = "abandon abandon abandon abandon abandon abandon abandon abandon abandon abandon abandon abandon abandon abandon abandon abandon abandon abandon abandon abandon abandon abandon abandon art";
        let seed = mnemonic_to_seed(phrase).unwrap();

        let temp1 = tempfile::tempdir().unwrap();
        let db1 = temp1.path().join("wallet.db");
        let addr1 =
            init_db_and_create_account(db1.to_str().unwrap(), WalletNetwork::Main, &seed, None)
                .unwrap()
                .unified_address;

        let temp2 = tempfile::tempdir().unwrap();
        let db2 = temp2.path().join("wallet.db");
        let addr2 =
            init_db_and_create_account(db2.to_str().unwrap(), WalletNetwork::Main, &seed, None)
                .unwrap()
                .unified_address;

        assert_eq!(addr1, addr2, "Same seed should produce same address");
    }

    #[test]
    fn test_shielded_address_has_sapling_and_orchard_only() {
        // Verify our address uses Sapling+Orchard receivers (no transparent),
        // matching zodl/Zashi wallet behavior.
        let temp_dir = tempfile::tempdir().unwrap();
        let db_path = temp_dir.path().join("wallet.db");
        let db_path_str = db_path.to_str().unwrap();

        let phrase = generate_mnemonic();
        let seed = mnemonic_to_seed(&phrase).unwrap();

        let address = init_db_and_create_account(db_path_str, WalletNetwork::Main, &seed, None)
            .unwrap()
            .unified_address;
        // Decode and verify receiver types
        let za = zcash_address::ZcashAddress::try_from_encoded(&address).unwrap();
        let debug = format!("{:?}", za);
        assert!(
            debug.contains("Sapling"),
            "UA should contain Sapling receiver"
        );
        assert!(
            debug.contains("Orchard"),
            "UA should contain Orchard receiver"
        );
        assert!(
            !debug.contains("P2pkh"),
            "UA should NOT contain transparent receiver"
        );
    }
}
