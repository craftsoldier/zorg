use bip0039::{Count, English, Language, Mnemonic};
use secrecy::{ExposeSecret, SecretVec};

use zcash_keys::keys::{ReceiverRequirement, UnifiedAddressRequest, UnifiedSpendingKey};
use zeroize::Zeroizing;

use crate::network::WalletNetwork;

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

/// Convert an account index to the backend's ZIP-32 account id.
pub fn zip32_account_id(account_index: u32) -> Result<zip32::AccountId, String> {
    zip32::AccountId::try_from(account_index)
        .map_err(|_| format!("Invalid ZIP32 account index: {account_index}"))
}

/// Derive the Unified Spending Key for an account index from the seed.
fn unified_spending_key_for_account(
    network: WalletNetwork,
    seed: &SecretVec<u8>,
    account_index: u32,
) -> Result<UnifiedSpendingKey, String> {
    let account_id = zip32_account_id(account_index)?;
    UnifiedSpendingKey::from_seed(&network, seed.expose_secret(), account_id)
        .map_err(|e| format!("USK derivation failed for account {account_index}: {e:?}"))
}

/// Standard shielded address request: Orchard + Sapling, no transparent.
/// Matches the behavior of zodl/Zashi wallets.
pub fn shielded_address_request() -> UnifiedAddressRequest {
    UnifiedAddressRequest::custom(
        ReceiverRequirement::Require, // Orchard
        ReceiverRequirement::Require, // Sapling
        ReceiverRequirement::Omit,    // Transparent
    )
    .expect("valid receiver requirements")
}

/// The default unified address for account `index`, derived from the seed.
pub fn default_address_for_account(
    network: WalletNetwork,
    seed: &SecretVec<u8>,
    account_index: u32,
) -> Result<String, String> {
    let usk = unified_spending_key_for_account(network, seed, account_index)?;
    let ufvk = usk.to_unified_full_viewing_key();
    let (ua, _) = ufvk
        .default_address(shielded_address_request())
        .map_err(|e| format!("Failed to derive address: {e}"))?;
    Ok(ua.encode(&network))
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
}
