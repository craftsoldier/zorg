use secrecy::{ExposeSecret, SecretVec};
use security_framework::item::{ItemClass, ItemSearchOptions, SearchResult};
use security_framework::passwords::set_generic_password;
use zip32::fingerprint::SeedFingerprint;

use crate::network::WalletNetwork;

/// Reads the seed with the given fingerprint from the macOS Keychain.
pub fn seed_from_macos_keychain(
    network: WalletNetwork,
    seed_fingerprint: &SeedFingerprint,
) -> Result<SecretVec<u8>, String> {
    const ERR_SEC_ITEM_NOT_FOUND: i32 = -25300;

    let service = seed_service_for_network(network);
    let key = seed_key(seed_fingerprint);
    let mut search = ItemSearchOptions::new();
    search
        .class(ItemClass::generic_password())
        .service(&service)
        .account(&key)
        .load_data(true);

    match search.search() {
        Ok(results) => match results.into_iter().next() {
            Some(SearchResult::Data(seed)) => Ok(SecretVec::new(seed)),
            Some(other) => Err(format!(
                "Unexpected keychain search result for service={service} key={key}: {other:?}"
            )),
            None => Err("Seed not found in keychain".to_string()),
        },
        Err(error) if error.code() == ERR_SEC_ITEM_NOT_FOUND => {
            Err("Seed not found in keychain".to_string())
        }
        Err(error) => Err(format!(
            "Keychain read failed for service={service} key={key}: {error}"
        )),
    }
}

/// Stores the seed in the macOS Keychain under its fingerprint, one entry per seed.
pub fn store_seed_in_macos_keychain(
    network: WalletNetwork,
    seed: &SecretVec<u8>,
) -> Result<(), String> {
    let seed_fingerprint = SeedFingerprint::from_seed(seed.expose_secret())
        .ok_or("Seed length is invalid for ZIP-32")?;
    let service = seed_service_for_network(network);
    let key = seed_key(&seed_fingerprint);
    set_generic_password(&service, &key, seed.expose_secret())
        .map_err(|e| format!("Failed to write to keychain for service={service} key={key}: {e}"))
}

fn seed_service_for_network(network: WalletNetwork) -> String {
    match network {
        WalletNetwork::Main => "org.zorg.cli.seed".to_string(),
        WalletNetwork::Test => "org.zorg.cli.testnet.seed".to_string(),
        WalletNetwork::Regtest => "org.zorg.cli.regtest.seed".to_string(),
    }
}

fn seed_key(seed_fingerprint: &SeedFingerprint) -> String {
    format!("seed_{}", hex::encode(seed_fingerprint.to_bytes()))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn keychain_round_trips_through_production_read_path() {
        use security_framework::passwords::delete_generic_password;

        let seed = crate::keys::mnemonic_to_seed(&crate::keys::generate_mnemonic()).unwrap();
        let seed_fingerprint = SeedFingerprint::from_seed(seed.expose_secret()).unwrap();

        if let Err(error) = store_seed_in_macos_keychain(WalletNetwork::Regtest, &seed) {
            if keychain_is_unavailable(&error) && std::env::var_os("CI").is_none() {
                eprintln!("Skipping keychain round-trip: {error}");
                return;
            }
            panic!("failed to write test seed: {error}");
        }
        let actual = seed_from_macos_keychain(WalletNetwork::Regtest, &seed_fingerprint).unwrap();
        assert_eq!(actual.expose_secret(), seed.expose_secret());

        delete_generic_password(
            &seed_service_for_network(WalletNetwork::Regtest),
            &seed_key(&seed_fingerprint),
        )
        .unwrap();
    }

    fn keychain_is_unavailable(error: &str) -> bool {
        error.contains("No keychain is available")
            || error.contains("One or more parameters passed to a function were not valid")
    }
}
