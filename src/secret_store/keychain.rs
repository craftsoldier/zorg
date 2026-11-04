use zeroize::Zeroizing;

const ERR_SEC_ITEM_NOT_FOUND: i32 = -25300;
const ERR_SEC_MISSING_ENTITLEMENT: i32 = -34018;

pub(super) fn read(
    _root: &std::path::Path,
    service: &str,
    key: &str,
) -> Result<Option<Zeroizing<Vec<u8>>>, String> {
    use security_framework::passwords::{generic_password, PasswordOptions};

    let mut options = PasswordOptions::new_generic_password(service, key);
    options.use_protected_keychain();
    match generic_password(options) {
        Ok(value) => Ok(Some(Zeroizing::new(value))),
        Err(error) if error.code() == ERR_SEC_ITEM_NOT_FOUND => Ok(None),
        Err(error) => Err(keychain_error("Keychain read failed", error)),
    }
}

pub(super) fn write(
    _root: &std::path::Path,
    service: &str,
    key: &str,
    value: &[u8],
) -> Result<(), String> {
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
        Err(error) => return Err(keychain_error("Failed to replace Keychain entry", error)),
    }
    let mut options = PasswordOptions::new_generic_password(service, key);
    options.use_protected_keychain();
    options.set_access_control(access);
    set_generic_password_options(value, options)
        .map_err(|error| keychain_error("Failed to write protected mnemonic to Keychain", error))
}

/// The message for a failed Keychain call: what to do when the binary lacks
/// zorg's Keychain entitlement, otherwise `context` and the error.
fn keychain_error(context: &str, error: security_framework::base::Error) -> String {
    if error.code() == ERR_SEC_MISSING_ENTITLEMENT {
        return "This zorg binary isn't the signed zorg app, so macOS won't let it use the \
                Keychain for recovery phrases. Install and run the signed zorg app."
            .to_string();
    }
    format!("{context}: {error}")
}

pub(super) fn delete(_root: &std::path::Path, service: &str, key: &str) -> Result<(), String> {
    use security_framework::passwords::{delete_generic_password_options, PasswordOptions};

    let mut options = PasswordOptions::new_generic_password(service, key);
    options.use_protected_keychain();
    match delete_generic_password_options(options) {
        Ok(()) => Ok(()),
        Err(error) if error.code() == ERR_SEC_ITEM_NOT_FOUND => Ok(()),
        Err(error) => Err(keychain_error("Failed to delete Keychain entry", error)),
    }
}
