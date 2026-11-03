use bip0039::{Count, English, Language, Mnemonic};
use secrecy::{ExposeSecret, SecretVec};

use zcash_keys::keys::{ReceiverRequirement, UnifiedAddressRequest, UnifiedSpendingKey};
use zeroize::Zeroizing;

use crate::network::WalletNetwork;

pub fn generate_mnemonic() -> Zeroizing<String> {
    Zeroizing::new(Mnemonic::<English>::generate(Count::Words24).into_phrase())
}

/// Convert a 12- or 24-word mnemonic phrase to a 64-byte seed.
/// The seed is zeroized from memory when the SecretVec is dropped.
/// Errors name a bad word by its position, never by its text.
pub fn mnemonic_to_seed(phrase: &str) -> Result<SecretVec<u8>, String> {
    let words: Vec<&str> = phrase.split_whitespace().collect();
    if !matches!(words.len(), 12 | 24) {
        return Err(format!(
            "Recovery phrase has {} words; expected 12 or 24",
            words.len()
        ));
    }
    if let Some(i) = words.iter().position(|w| !English::WORD_LIST.contains(w)) {
        return Err(format!("Word {} is not in the BIP 39 word list", i + 1));
    }

    let mnemonic = Mnemonic::<English>::from_phrase(phrase)
        .map_err(|_| "Recovery phrase checksum is invalid")?;
    let seed = Zeroizing::new(mnemonic.to_seed(""));
    drop(mnemonic);
    let secret_seed = SecretVec::new(seed.to_vec());
    drop(seed);
    Ok(secret_seed)
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
    fn test_mnemonic_to_seed_accepts_12_and_24_words() {
        for count in [Count::Words12, Count::Words24] {
            let phrase = Mnemonic::<English>::generate(count).phrase().to_string();
            assert_eq!(mnemonic_to_seed(&phrase).unwrap().expose_secret().len(), 64);
        }
    }

    #[test]
    fn test_mnemonic_to_seed_rejects_other_word_counts() {
        for count in [11, 13, 15, 18, 21, 25] {
            let phrase = vec!["abandon"; count].join(" ");
            let error = mnemonic_to_seed(&phrase).err().unwrap();
            assert!(error.contains("expected 12 or 24"), "{error}");
        }
    }

    #[test]
    fn test_mnemonic_to_seed_names_a_bad_word_by_position_only() {
        let mut words: Vec<String> = generate_mnemonic().split(' ').map(String::from).collect();
        words[6] = "zorgword".into();
        let error = mnemonic_to_seed(&words.join(" ")).err().unwrap();
        assert_eq!(error, "Word 7 is not in the BIP 39 word list");
    }

    #[test]
    fn test_mnemonic_to_seed_rejects_a_bad_checksum_without_echoing_words() {
        let phrase = vec!["abandon"; 24].join(" ");
        let error = mnemonic_to_seed(&phrase).err().unwrap();
        assert_eq!(error, "Recovery phrase checksum is invalid");
    }
}
