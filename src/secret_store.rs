use secrecy::SecretVec;
use zeroize::Zeroizing;

use crate::{keys, network::WalletNetwork};

const ACCOUNT_MNEMONIC_KEY_PREFIX: &str = "zorg_wallet_";

#[cfg(target_os = "macos")]
const ERR_SEC_ITEM_NOT_FOUND: i32 = -25300;

pub fn seed_from_macos_stored_mnemonic(
    network: WalletNetwork,
    seed_reference: &str,
) -> Result<SecretVec<u8>, String> {
    let mnemonic = mnemonic_from_macos_keychain(network, seed_reference)?;
    keys::mnemonic_to_seed(&mnemonic)
}

/// The seed's recovery phrase from the protected Keychain. Reading it asks
/// for Touch ID or the login password.
pub fn mnemonic_from_macos_keychain(
    network: WalletNetwork,
    seed_reference: &str,
) -> Result<Zeroizing<String>, String> {
    let account_key = account_mnemonic_key(seed_reference);

    // The payload is now the raw utf-8 mnemonic string stored in the keychain
    let service = mnemonic_store_service_for_network(network);
    let mnemonic_bytes = macos_read_secure_store_value(&service, &account_key)?
        .ok_or_else(|| "Mnemonic not found for account".to_string())?;

    let mnemonic_str = std::str::from_utf8(mnemonic_bytes.as_slice())
        .map_err(|_| "Mnemonic is not valid UTF-8".to_string())?;
    Ok(Zeroizing::new(mnemonic_str.to_string()))
}

pub fn store_mnemonic_in_macos_keychain(
    network: WalletNetwork,
    seed_reference: &str,
    mnemonic: &str,
) -> Result<(), String> {
    let account_key = account_mnemonic_key(seed_reference);
    macos_write_secure_store_value(
        &mnemonic_store_service_for_network(network),
        &account_key,
        mnemonic.as_bytes(),
    )
}

/// Removes the seed's mnemonic from the protected Keychain. A missing entry
/// is not an error.
#[cfg(target_os = "macos")]
pub fn delete_mnemonic_from_macos_keychain(
    network: WalletNetwork,
    seed_reference: &str,
) -> Result<(), String> {
    use security_framework::passwords::{delete_generic_password_options, PasswordOptions};

    let mut options = PasswordOptions::new_generic_password(
        &mnemonic_store_service_for_network(network),
        &account_mnemonic_key(seed_reference),
    );
    options.use_protected_keychain();
    match delete_generic_password_options(options) {
        Ok(()) => Ok(()),
        Err(error) if error.code() == ERR_SEC_ITEM_NOT_FOUND => Ok(()),
        Err(error) => Err(format!("Failed to delete Keychain entry: {error}")),
    }
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

fn account_mnemonic_key(seed_reference: &str) -> String {
    format!("{ACCOUNT_MNEMONIC_KEY_PREFIX}{seed_reference}")
}

#[cfg(target_os = "macos")]
fn macos_read_secure_store_value(
    service: &str,
    key: &str,
) -> Result<Option<Zeroizing<Vec<u8>>>, String> {
    use security_framework::passwords::{generic_password, PasswordOptions};

    let mut options = PasswordOptions::new_generic_password(service, key);
    options.use_protected_keychain();
    match generic_password(options) {
        Ok(value) => Ok(Some(Zeroizing::new(value))),
        Err(error) if error.code() == ERR_SEC_ITEM_NOT_FOUND => Ok(None),
        Err(error) => Err(format!("Keychain read failed: {error}")),
    }
}

#[cfg(target_os = "macos")]
fn macos_write_secure_store_value(service: &str, key: &str, value: &[u8]) -> Result<(), String> {
    use security_framework::{
        access_control::{ProtectionMode, SecAccessControl},
        passwords::{
            delete_generic_password_options, set_generic_password_options, AccessControlOptions,
            PasswordOptions,
        },
    };

    let access = SecAccessControl::create_with_protection(
        Some(ProtectionMode::AccessibleWhenUnlockedThisDeviceOnly),
        AccessControlOptions::USER_PRESENCE.bits(),
    )
    .map_err(|error| format!("Create Keychain access control: {error}"))?;
    let mut existing = PasswordOptions::new_generic_password(service, key);
    existing.use_protected_keychain();
    match delete_generic_password_options(existing) {
        Ok(()) => {}
        Err(error) if error.code() == ERR_SEC_ITEM_NOT_FOUND => {}
        Err(error) => return Err(format!("Failed to replace Keychain entry: {error}")),
    }
    let mut options = PasswordOptions::new_generic_password(service, key);
    options.use_protected_keychain();
    options.set_access_control(access);
    set_generic_password_options(value, options)
        .map_err(|error| format!("Failed to write protected mnemonic to Keychain: {error}"))
}

#[cfg(not(target_os = "macos"))]
fn macos_read_secure_store_value(
    _service: &str,
    _key: &str,
) -> Result<Option<Zeroizing<Vec<u8>>>, String> {
    Err("macOS stored mnemonic path is unsupported on this platform".to_string())
}

#[cfg(not(target_os = "macos"))]
fn macos_write_secure_store_value(_service: &str, _key: &str, _value: &[u8]) -> Result<(), String> {
    Err("macOS stored mnemonic path is unsupported on this platform".to_string())
}

#[cfg(all(test, target_os = "macos"))]
mod tests {
    use super::*;

    #[test]
    #[ignore = "requires a signed test bundle and interactive Keychain authentication"]
    fn keychain_round_trips_through_production_read_path() {
        use secrecy::ExposeSecret;
        use security_framework::passwords::{delete_generic_password_options, PasswordOptions};

        let seed_reference = uuid::Uuid::new_v4().to_string();
        let mnemonic = keys::generate_mnemonic();
        let expected = keys::mnemonic_to_seed(&mnemonic).unwrap();
        let service = mnemonic_store_service_for_network(WalletNetwork::Regtest);
        let key = account_mnemonic_key(&seed_reference);

        store_mnemonic_in_macos_keychain(WalletNetwork::Regtest, &seed_reference, &mnemonic)
            .unwrap();
        let actual =
            seed_from_macos_stored_mnemonic(WalletNetwork::Regtest, &seed_reference).unwrap();

        let mut options = PasswordOptions::new_generic_password(&service, &key);
        options.use_protected_keychain();
        delete_generic_password_options(options).unwrap();
        assert_eq!(actual.expose_secret(), expected.expose_secret());
    }
}
