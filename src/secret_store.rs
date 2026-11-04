use crate::{keys, network::WalletNetwork};
use secrecy::SecretVec;
use std::path::Path;
use zeroize::Zeroizing;

#[cfg(not(feature = "packaged"))]
mod age;
#[cfg(feature = "packaged")]
mod keychain;

#[cfg(not(feature = "packaged"))]
use age as backend;
#[cfg(feature = "packaged")]
use keychain as backend;

pub fn seed_from_stored_mnemonic(
    db: &str,
    network: WalletNetwork,
    account_uuid: &str,
) -> Result<SecretVec<u8>, String> {
    keys::mnemonic_to_seed(&mnemonic(db, network, account_uuid)?)
}

/// The account's recovery phrase, read with macOS authentication.
pub fn mnemonic(
    db: &str,
    network: WalletNetwork,
    account_uuid: &str,
) -> Result<Zeroizing<String>, String> {
    mnemonic_if_present(db, network, account_uuid)?
        .ok_or_else(|| "Mnemonic not found for account".to_string())
}

/// The account's recovery phrase, or `None` when the selected store has no entry.
pub fn mnemonic_if_present(
    db: &str,
    network: WalletNetwork,
    account_uuid: &str,
) -> Result<Option<Zeroizing<String>>, String> {
    let Some(bytes) = backend::read(
        root(db)?,
        &mnemonic_store_service_for_network(network),
        &account_mnemonic_key(account_uuid)?,
    )?
    else {
        return Ok(None);
    };
    let text =
        std::str::from_utf8(&bytes).map_err(|_| "Mnemonic is not valid UTF-8".to_string())?;
    Ok(Some(Zeroizing::new(text.to_string())))
}

pub fn store_mnemonic(
    db: &str,
    network: WalletNetwork,
    account_uuid: &str,
    mnemonic: &str,
) -> Result<(), String> {
    backend::write(
        root(db)?,
        &mnemonic_store_service_for_network(network),
        &account_mnemonic_key(account_uuid)?,
        mnemonic.as_bytes(),
    )
}

/// Removes the account's stored phrase. A missing entry is not an error.
pub fn delete_mnemonic(db: &str, network: WalletNetwork, account_uuid: &str) -> Result<(), String> {
    backend::delete(
        root(db)?,
        &mnemonic_store_service_for_network(network),
        &account_mnemonic_key(account_uuid)?,
    )
}

fn root(db: &str) -> Result<&Path, String> {
    Path::new(db)
        .parent()
        .filter(|p| !p.as_os_str().is_empty())
        .ok_or_else(|| "Wallet database must have a parent directory".to_string())
}

fn secure_store_service_for_network(network: WalletNetwork) -> String {
    match network {
        WalletNetwork::Main => "org.zorg.cli".to_string(),
        WalletNetwork::Test => "org.zorg.cli.testnet".to_string(),
        WalletNetwork::Regtest => "org.zorg.cli.regtest".to_string(),
    }
}

fn mnemonic_store_service_for_network(network: WalletNetwork) -> String {
    format!("{}.mnemonic", secure_store_service_for_network(network))
}

fn account_mnemonic_key(account_uuid: &str) -> Result<String, String> {
    uuid::Uuid::parse_str(account_uuid).map_err(|_| "Invalid account UUID".to_string())?;
    Ok(format!("zorg_wallet_{account_uuid}"))
}

#[cfg(test)]
mod tests {
    use super::*;
    use secrecy::ExposeSecret;

    #[test]
    fn invalid_account_identifier_never_accesses_the_store() {
        let dir = tempfile::tempdir().unwrap();
        let db = dir.path().join("regtest.db");
        for uuid in ["", "../other-account", "not-a-uuid"] {
            assert!(
                store_mnemonic(db.to_str().unwrap(), WalletNetwork::Regtest, uuid, "unused")
                    .is_err()
            );
            assert!(
                mnemonic_if_present(db.to_str().unwrap(), WalletNetwork::Regtest, uuid).is_err()
            );
            assert!(delete_mnemonic(db.to_str().unwrap(), WalletNetwork::Regtest, uuid).is_err());
        }
        assert_eq!(std::fs::read_dir(dir.path()).unwrap().count(), 0);
    }

    #[test]
    #[ignore = "requires macOS authentication and the selected store's runtime setup"]
    fn stored_phrase_round_trips_through_production_read_path() {
        let dir = tempfile::tempdir().unwrap();
        let db = dir.path().join("regtest.db");
        let db = db.to_str().unwrap();
        let uuid = uuid::Uuid::new_v4().to_string();
        let phrase = keys::generate_mnemonic();
        let expected = keys::mnemonic_to_seed(&phrase).unwrap();
        store_mnemonic(db, WalletNetwork::Regtest, &uuid, &phrase).unwrap();
        let result = seed_from_stored_mnemonic(db, WalletNetwork::Regtest, &uuid);
        delete_mnemonic(db, WalletNetwork::Regtest, &uuid).unwrap();
        assert_eq!(result.unwrap().expose_secret(), expected.expose_secret());
    }
}
