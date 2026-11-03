use bip0039::{Count, English, Language, Mnemonic};
use secrecy::SecretVec;

use zcash_keys::keys::{ReceiverRequirement, UnifiedAddressRequest};
use zeroize::Zeroizing;

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

#[cfg(test)]
mod tests {
    use secrecy::ExposeSecret;

    use super::*;

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
