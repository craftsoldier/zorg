use std::{
    fs::{self, File, OpenOptions},
    io::Write,
    os::unix::fs::{DirBuilderExt, OpenOptionsExt},
    path::{Path, PathBuf},
    process::{Command, Stdio},
};
use zeroize::Zeroizing;

fn directory(root: &Path, service: &str, key: &str) -> PathBuf {
    root.join("secrets").join(service).join(key)
}

fn capture(command: &mut Command, name: &str) -> Result<Zeroizing<Vec<u8>>, String> {
    let output = command
        .stdin(Stdio::null())
        .stderr(Stdio::inherit())
        .output()
        .map_err(|e| {
            format!("Could not run {name}: {e}. Install age and age-plugin-se with Homebrew.")
        })?;
    let bytes = Zeroizing::new(output.stdout);
    if !output.status.success() {
        return Err(format!(
            "{name} failed; authentication or decryption was not completed"
        ));
    }
    Ok(bytes)
}

fn private_file(path: &Path) -> Result<File, String> {
    OpenOptions::new()
        .write(true)
        .create_new(true)
        .mode(0o600)
        .open(path)
        .map_err(|e| format!("Could not create secret file: {e}"))
}

pub(super) fn write(root: &Path, service: &str, key: &str, value: &[u8]) -> Result<(), String> {
    let dir = directory(root, service, key);
    crate::account::ensure_private_wallet_dir(dir.parent().unwrap())?;
    fs::DirBuilder::new()
        .mode(0o700)
        .create(&dir)
        .map_err(|e| {
            format!("Could not create secret store (existing entries are never replaced): {e}")
        })?;
    let result = (|| {
        // Capture keygen output and persist it ourselves: the plugin's -o path
        // neither refuses replacement nor checks its file-write result.
        let identity = capture(
            Command::new("age-plugin-se").args([
                "keygen",
                "--access-control",
                "any-biometry-or-passcode",
            ]),
            "age-plugin-se keygen",
        )?;
        let mut file = private_file(&dir.join("identity.txt"))?;
        file.write_all(&identity)
            .and_then(|()| file.sync_all())
            .map_err(|e| format!("Could not save Secure Enclave identity: {e}"))?;
        let recipient = capture(
            Command::new("age-plugin-se")
                .args(["recipients", "-i"])
                .arg(dir.join("identity.txt")),
            "age-plugin-se recipients",
        )?;
        let recipient = std::str::from_utf8(&recipient)
            .map_err(|_| "Invalid Secure Enclave recipient".to_string())?
            .trim();
        if recipient.is_empty() || recipient.lines().count() != 1 {
            return Err("Expected one Secure Enclave recipient".into());
        }
        let encrypted = dir.join("mnemonic.age");
        let mut child = Command::new("age")
            .args(["--encrypt", "--recipient", recipient])
            .stdin(Stdio::piped())
            .stdout(Stdio::from(private_file(&encrypted)?))
            .stderr(Stdio::inherit())
            .spawn()
            .map_err(|e| format!("Could not run age: {e}. Install age with Homebrew."))?;
        let sent = child.stdin.take().unwrap().write_all(value);
        let status = child
            .wait()
            .map_err(|e| format!("Could not wait for age: {e}"))?;
        sent.map_err(|e| format!("Could not encrypt phrase: {e}"))?;
        if !status.success() {
            return Err("age encryption failed".into());
        }
        File::open(encrypted)
            .and_then(|file| file.sync_all())
            .map_err(|e| format!("Could not save encrypted phrase: {e}"))
    })();
    if result.is_err() {
        let _ = fs::remove_dir_all(&dir);
    }
    result
}

pub(super) fn read(
    root: &Path,
    service: &str,
    key: &str,
) -> Result<Option<Zeroizing<Vec<u8>>>, String> {
    let dir = directory(root, service, key);
    match fs::symlink_metadata(&dir) {
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(None),
        Err(e) => return Err(format!("Could not read secret store: {e}")),
        Ok(_) => {}
    }
    capture(
        Command::new("age")
            .args(["--decrypt", "--identity"])
            .arg(dir.join("identity.txt"))
            .arg(dir.join("mnemonic.age")),
        "age decryption",
    )
    .map(Some)
}

pub(super) fn delete(root: &Path, service: &str, key: &str) -> Result<(), String> {
    match fs::remove_dir_all(directory(root, service, key)) {
        Ok(()) => Ok(()),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(()),
        Err(e) => Err(format!("Could not delete secret store: {e}")),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn refuses_to_replace_existing_identity() {
        let root = tempfile::tempdir().unwrap();
        let dir = directory(root.path(), "regtest", "account");
        fs::create_dir_all(&dir).unwrap();
        fs::write(dir.join("identity.txt"), b"existing identity").unwrap();
        assert!(write(root.path(), "regtest", "account", b"new phrase").is_err());
        assert_eq!(
            fs::read(dir.join("identity.txt")).unwrap(),
            b"existing identity"
        );
    }

    #[test]
    fn absent_store_can_be_read_and_deleted_without_tools() {
        let root = tempfile::tempdir().unwrap();
        assert!(read(root.path(), "regtest", "account").unwrap().is_none());
        delete(root.path(), "regtest", "account").unwrap();
        assert_eq!(fs::read_dir(root.path()).unwrap().count(), 0);
    }
}
