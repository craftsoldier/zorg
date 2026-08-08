use secrecy::SecretVec;
use zeroize::Zeroizing;

use crate::wallet::{keys, network::WalletNetwork};

const ACCOUNT_MNEMONIC_KEY_PREFIX: &str = "zorg_wallet_";

pub fn seed_from_macos_stored_mnemonic(
    network: WalletNetwork,
    account_uuid: &str,
) -> Result<SecretVec<u8>, String> {
    let account_key = account_mnemonic_key(account_uuid);
    
    // The payload is now the raw utf-8 mnemonic string stored in the keychain
    let mnemonic_bytes =
        macos_read_secure_store_value(&mnemonic_store_service_for_network(network), &account_key)?
            .ok_or_else(|| "Mnemonic not found for account".to_string())?;

    let mnemonic_str = std::str::from_utf8(mnemonic_bytes.as_slice())
        .map_err(|_| "Mnemonic is not valid UTF-8".to_string())?;
    let seed = keys::mnemonic_to_seed(mnemonic_str)?;
    drop(mnemonic_bytes);
    Ok(seed)
}

pub fn store_mnemonic_in_macos_keychain(
    network: WalletNetwork,
    account_uuid: &str,
    mnemonic: &str,
) -> Result<(), String> {
    let account_key = account_mnemonic_key(account_uuid);
    macos_write_secure_store_value(
        &mnemonic_store_service_for_network(network),
        &account_key,
        mnemonic.as_bytes(),
    )
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

fn account_mnemonic_key(account_uuid: &str) -> String {
    format!("{ACCOUNT_MNEMONIC_KEY_PREFIX}{account_uuid}")
}

#[cfg(target_os = "macos")]
fn macos_read_secure_store_value(
    service: &str,
    key: &str,
) -> Result<Option<Zeroizing<Vec<u8>>>, String> {
    use security_framework::item::{ItemClass, ItemSearchOptions, SearchResult};

    const ERR_SEC_ITEM_NOT_FOUND: i32 = -25300;

    let mut search = ItemSearchOptions::new();
    search
        .class(ItemClass::generic_password())
        .service(service)
        .account(key)
        .ignore_legacy_keychains()
        .load_data(true);

    match search.search() {
        Ok(results) => {
            if results.is_empty() {
                return Ok(None);
            }
            match results.into_iter().next() {
                Some(SearchResult::Data(data)) => Ok(Some(Zeroizing::new(data))),
                Some(other) => Err(format!(
                    "Unexpected keychain search result for service={service} key={key}: {other:?}"
                )),
                None => Ok(None),
            }
        }
        Err(error) if error.code() == ERR_SEC_ITEM_NOT_FOUND => Ok(None),
        Err(error) => Err(format!(
            "Keychain read failed for service={service} key={key}: {error}"
        )),
    }
}

#[cfg(target_os = "macos")]
fn macos_write_secure_store_value(
    service: &str,
    key: &str,
    value: &[u8],
) -> Result<(), String> {
    use security_framework::passwords::set_generic_password;
    
    set_generic_password(service, key, value)
        .map_err(|e| format!("Failed to write to keychain for service={service} key={key}: {e}"))
}

#[cfg(not(target_os = "macos"))]
fn macos_read_secure_store_value(
    _service: &str,
    _key: &str,
) -> Result<Option<Zeroizing<Vec<u8>>>, String> {
    Err("macOS stored mnemonic path is unsupported on this platform".to_string())
}

#[cfg(not(target_os = "macos"))]
fn macos_write_secure_store_value(
    _service: &str,
    _key: &str,
    _value: &[u8],
) -> Result<(), String> {
    Err("macOS stored mnemonic path is unsupported on this platform".to_string())
}
