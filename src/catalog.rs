use std::fs::{self, File, OpenOptions};
use std::io::Write;
use std::path::{Path, PathBuf};

use zorg::account::AccountInfo;
use zorg::network::WalletNetwork;

pub struct Catalog {
    pub root: PathBuf,
    pub databases: Vec<PathBuf>,
}

pub struct LocatedAccount {
    pub db: String,
    pub network: WalletNetwork,
    pub account: AccountInfo,
}

impl Catalog {
    pub fn home() -> Result<PathBuf, String> {
        let home = std::env::var_os("HOME").ok_or("HOME is not set")?;
        let home = PathBuf::from(home);
        Ok(if cfg!(target_os = "macos") {
            home.join("Library/Application Support/zorg")
        } else {
            home.join(".zorg")
        })
    }

    pub fn lock(root: &Path) -> Result<File, String> {
        super::ensure_private_wallet_dir(root)?;
        let path = root.join("index.lock");
        let file = private_options()
            .create(true)
            .truncate(false)
            .open(&path)
            .map_err(|e| format!("Could not open database index lock: {e}"))?;
        super::secure_wallet_db_file(&path)?;
        file.lock()
            .map_err(|e| format!("Could not lock database index: {e}"))?;
        Ok(file)
    }

    pub fn load(root: PathBuf) -> Result<Self, String> {
        let index = root.join("index.toml");
        let mut databases = Vec::new();
        match fs::read_to_string(&index) {
            Ok(text) => {
                let table: toml::Table = text
                    .parse()
                    .map_err(|e| format!("Invalid database index {}: {e}", index.display()))?;
                if table.keys().any(|key| key != "databases") {
                    return Err("Unknown field in database index".into());
                }
                if let Some(entries) = table.get("databases") {
                    for entry in entries.as_array().ok_or("databases must be a list")? {
                        let entry = entry.as_table().ok_or("Database entry must be a table")?;
                        if entry.len() != 1 {
                            return Err("Each database entry must contain only path".into());
                        }
                        let path = PathBuf::from(
                            entry
                                .get("path")
                                .and_then(|p| p.as_str())
                                .ok_or("Each database entry needs a path string")?,
                        );
                        if !path.is_absolute() {
                            return Err("Database index paths must be absolute".into());
                        }
                        if !databases.contains(&path) {
                            databases.push(path);
                        }
                    }
                }
            }
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => {}
            Err(e) => return Err(format!("Could not read database index: {e}")),
        }
        Ok(Self { root, databases })
    }

    pub fn save(&self) -> Result<(), String> {
        let entries: Result<Vec<_>, String> = self
            .databases
            .iter()
            .map(|path| {
                let mut entry = toml::Table::new();
                entry.insert(
                    "path".into(),
                    toml::Value::String(
                        path.to_str()
                            .ok_or("Database path must be UTF-8")?
                            .to_string(),
                    ),
                );
                Ok(toml::Value::Table(entry))
            })
            .collect();
        let mut table = toml::Table::new();
        table.insert("databases".into(), toml::Value::Array(entries?));
        let text = toml::to_string(&table).map_err(|e| e.to_string())?;
        let pending = self
            .root
            .join(format!("index.{}.tmp", uuid::Uuid::new_v4()));
        let result = (|| {
            let mut file = private_options()
                .create_new(true)
                .open(&pending)
                .map_err(|e| format!("Could not create database index: {e}"))?;
            file.write_all(text.as_bytes()).map_err(|e| e.to_string())?;
            file.sync_all().map_err(|e| e.to_string())?;
            fs::rename(&pending, self.root.join("index.toml")).map_err(|e| e.to_string())?;
            File::open(&self.root)
                .and_then(|f| f.sync_all())
                .map_err(|e| e.to_string())
        })();
        if result.is_err() {
            let _ = fs::remove_file(&pending);
        }
        result
    }

    pub fn accounts(
        &self,
        network: Option<WalletNetwork>,
    ) -> Result<(Vec<LocatedAccount>, Vec<String>), String> {
        let mut result = Vec::new();
        let mut unavailable = Vec::new();
        let mut seen_paths = std::collections::HashSet::new();
        for path in &self.databases {
            let canonical = match path.canonicalize() {
                Ok(path) => path,
                Err(e) => {
                    unavailable.push(format!("Database unavailable: {} ({e})", path.display()));
                    continue;
                }
            };
            if !seen_paths.insert(canonical) {
                continue;
            }
            let path = path.to_str().ok_or("Database path must be UTF-8")?;
            let net = super::network_in_wallet_db(path)?;
            let conn = rusqlite::Connection::open_with_flags(
                path,
                rusqlite::OpenFlags::SQLITE_OPEN_READ_ONLY,
            )
            .map_err(|e| e.to_string())?;
            let wrong_seed: bool = conn.query_row(
                "SELECT EXISTS(SELECT 1 FROM accounts WHERE hd_seed_fingerprint IS NULL OR hd_seed_fingerprint != (SELECT seed_fingerprint FROM zorg_seed WHERE singleton = 1))",
                [], |row| row.get(0),
            ).map_err(|e| format!("Could not verify database seed: {e}"))?;
            if wrong_seed {
                return Err(format!(
                    "Database {path} must contain accounts from exactly one seed"
                ));
            }
            if network.is_some_and(|n| n != net) {
                continue;
            }
            super::ensure_custom_wallet_dir(
                Path::new(path).parent().ok_or("Database has no parent")?,
            )?;
            super::secure_wallet_db_file(Path::new(path))?;
            let accounts = zorg::account::list_accounts(path, net)?;
            for account in accounts {
                result.push(LocatedAccount {
                    db: path.to_string(),
                    network: net,
                    account,
                });
            }
        }
        Ok((result, unavailable))
    }

    pub fn ensure_new_seed(
        &self,
        seed: &secrecy::SecretVec<u8>,
        network: WalletNetwork,
    ) -> Result<(), String> {
        use secrecy::ExposeSecret;
        let fingerprint = zip32::fingerprint::SeedFingerprint::from_seed(seed.expose_secret())
            .ok_or("Invalid seed length")?;
        for path in &self.databases {
            let conn = rusqlite::Connection::open_with_flags(
                path,
                rusqlite::OpenFlags::SQLITE_OPEN_READ_ONLY,
            )
            .map_err(|e| e.to_string())?;
            let present: bool = conn.query_row(
                "SELECT seed_fingerprint = ?1 AND network = ?2 FROM zorg_seed WHERE singleton = 1",
                rusqlite::params![fingerprint.to_bytes().as_slice(), network.as_str()], |row| row.get(0),
            ).map_err(|e| e.to_string())?;
            if present {
                return Err("This recovery phrase is already registered on this network; run `zorg accounts`".into());
            }
        }
        Ok(())
    }

    pub fn register(&mut self, path: &Path) -> Result<(), String> {
        let path = path
            .canonicalize()
            .map_err(|e| format!("Could not locate database: {e}"))?;
        if !path.is_file() {
            return Err("Database path must point to a file".into());
        }
        if self
            .databases
            .iter()
            .any(|p| p.canonicalize().ok().as_ref() == Some(&path))
        {
            return Ok(());
        }
        let mut candidate = Self {
            root: self.root.clone(),
            databases: self.databases.clone(),
        };
        candidate.databases.push(path);
        let (accounts, unavailable) = candidate.accounts(None)?;
        require_available(&unavailable)?;
        let mut names = std::collections::HashSet::new();
        for account in accounts {
            validate_name(&account.account.name)?;
            if !names.insert(account.account.name.clone()) {
                return Err(format!("Account name {:?} already exists; give accounts unique names before registering the database", account.account.name));
            }
        }
        let mut seeds = std::collections::HashSet::new();
        for path in &candidate.databases {
            let conn = rusqlite::Connection::open_with_flags(
                path,
                rusqlite::OpenFlags::SQLITE_OPEN_READ_ONLY,
            )
            .map_err(|e| e.to_string())?;
            let identity: (Vec<u8>, String) = conn
                .query_row(
                    "SELECT seed_fingerprint, network FROM zorg_seed WHERE singleton = 1",
                    [],
                    |row| Ok((row.get(0)?, row.get(1)?)),
                )
                .map_err(|e| e.to_string())?;
            if !seeds.insert(identity) {
                return Err("This seed already has a registered database on this network".into());
            }
        }
        candidate.save()?;
        self.databases = candidate.databases;
        Ok(())
    }
}

pub fn require_available(unavailable: &[String]) -> Result<(), String> {
    if unavailable.is_empty() {
        Ok(())
    } else {
        Err(format!(
            "Reconnect unavailable databases before changing account names or adding accounts:\n{}",
            unavailable.join("\n")
        ))
    }
}

pub fn validate_name(name: &str) -> Result<(), String> {
    if name.is_empty() || name.trim() != name || name.chars().any(char::is_control) {
        Err(
            "Account name must be nonempty, with no surrounding whitespace or control characters"
                .into(),
        )
    } else {
        Ok(())
    }
}

pub fn select<'a>(
    accounts: &'a [LocatedAccount],
    name: Option<&str>,
) -> Result<&'a LocatedAccount, String> {
    match name {
        Some(name) => {
            let mut matches = accounts.iter().filter(|a| a.account.name == name);
            let account = matches
                .next()
                .ok_or_else(|| format!("No account named {name:?}; run `zorg accounts`"))?;
            if matches.next().is_some() {
                return Err(format!(
                    "Multiple accounts named {name:?}; give them unique names before selecting one"
                ));
            }
            Ok(account)
        }
        None if accounts.len() == 1 => Ok(&accounts[0]),
        None if accounts.is_empty() => {
            Err("No accounts found. Run `zorg create` or `zorg import`.".into())
        }
        None => Err("Choose an account with --account <name>; run `zorg accounts`".into()),
    }
}

fn private_options() -> OpenOptions {
    let mut options = OpenOptions::new();
    options.write(true).read(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;
        options.mode(0o600);
    }
    options
}

#[cfg(test)]
mod tests {
    use super::*;
    use zorg::{account, keys};

    fn fixture(root: &Path, filename: &str, name: &str) -> String {
        super::super::ensure_private_wallet_dir(root).unwrap();
        let path = root.join(filename);
        let db = path.to_str().unwrap();
        let seed = keys::mnemonic_to_seed(&keys::generate_mnemonic()).unwrap();
        account::init_db_and_create_account(db, WalletNetwork::Regtest, &seed, Some(1), name)
            .unwrap();
        super::super::secure_wallet_db_file(&path).unwrap();
        path.canonicalize().unwrap().to_str().unwrap().to_string()
    }

    #[test]
    fn missing_index_is_empty_and_does_not_adopt_old_databases() {
        let root = tempfile::tempdir().unwrap();
        fixture(root.path(), "wallet.db", "Old");
        let index = Catalog::load(root.path().to_path_buf()).unwrap();
        assert!(index.databases.is_empty());
        assert!(!root.path().join("index.toml").exists());
    }

    #[test]
    fn index_round_trips_paths_and_register_deduplicates_symlinks() {
        let root = tempfile::tempdir().unwrap();
        let db = fixture(root.path(), "a space.db", "Daily");
        let _lock = Catalog::lock(root.path()).unwrap();
        let mut index = Catalog::load(root.path().to_path_buf()).unwrap();
        index.register(Path::new(&db)).unwrap();
        index.register(Path::new(&db)).unwrap();
        #[cfg(unix)]
        {
            let alias = root.path().join("alias.db");
            std::os::unix::fs::symlink(&db, &alias).unwrap();
            index.register(&alias).unwrap();
        }
        let reloaded = Catalog::load(root.path().to_path_buf()).unwrap();
        assert_eq!(reloaded.databases.len(), 1);
        let text = fs::read_to_string(root.path().join("index.toml")).unwrap();
        assert!(text.contains("[[databases]]"));
        assert!(!text.contains("Daily"));
        assert!(!text.contains("selected"));
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            assert_eq!(
                fs::metadata(root.path().join("index.toml"))
                    .unwrap()
                    .permissions()
                    .mode()
                    & 0o777,
                0o600
            );
        }
    }

    #[test]
    fn malformed_indexes_fail_without_being_overwritten() {
        let root = tempfile::tempdir().unwrap();
        for text in [
            "not toml",
            "[[databases]]\npath = 'relative.db'",
            "databases = 7",
            "selected_database = 1",
            "[[databases]]\npath = '/tmp/a'\nname = 'Daily'",
        ] {
            fs::write(root.path().join("index.toml"), text).unwrap();
            assert!(Catalog::load(root.path().to_path_buf()).is_err(), "{text}");
            assert_eq!(
                fs::read_to_string(root.path().join("index.toml")).unwrap(),
                text
            );
        }
    }

    #[test]
    fn accounts_are_resolved_by_name_across_databases_and_survive_rename() {
        let root = tempfile::tempdir().unwrap();
        let first = fixture(root.path(), "first.db", "Daily");
        let second = fixture(root.path(), "second.db", "Savings");
        let mut index = Catalog::load(root.path().to_path_buf()).unwrap();
        let _lock = Catalog::lock(root.path()).unwrap();
        index.register(Path::new(&first)).unwrap();
        index.register(Path::new(&second)).unwrap();
        let (accounts, unavailable) = index.accounts(None).unwrap();
        assert!(unavailable.is_empty());
        assert_eq!(accounts.len(), 2);
        assert!(select(&accounts, None).is_err());
        let savings = select(&accounts, Some("Savings")).unwrap();
        assert_eq!(savings.db, second);
        assert_eq!(savings.account.account_index, 0);
        let uuid = savings.account.uuid.clone();
        let address = savings.account.unified_address.clone();
        account::rename_account(&second, WalletNetwork::Regtest, &uuid, "Travel").unwrap();
        let (accounts, _) = index.accounts(None).unwrap();
        assert!(select(&accounts, Some("Savings")).is_err());
        let renamed = select(&accounts, Some("Travel")).unwrap();
        assert_eq!(renamed.account.uuid, uuid);
        assert_eq!(renamed.account.unified_address, address);
        assert_eq!(renamed.account.account_index, 0);
    }

    #[test]
    fn register_rejects_duplicate_names_without_changing_index() {
        let root = tempfile::tempdir().unwrap();
        let first = fixture(root.path(), "first.db", "Daily");
        let second = fixture(root.path(), "second.db", "Daily");
        let _lock = Catalog::lock(root.path()).unwrap();
        let mut index = Catalog::load(root.path().to_path_buf()).unwrap();
        index.register(Path::new(&first)).unwrap();
        let before = fs::read(root.path().join("index.toml")).unwrap();
        assert!(index
            .register(Path::new(&second))
            .unwrap_err()
            .contains("already exists"));
        assert_eq!(before, fs::read(root.path().join("index.toml")).unwrap());
    }

    #[test]
    fn unavailable_databases_remain_indexed_and_are_never_recreated() {
        let root = tempfile::tempdir().unwrap();
        let db = fixture(root.path(), "offline.db", "Offline");
        let _lock = Catalog::lock(root.path()).unwrap();
        let mut index = Catalog::load(root.path().to_path_buf()).unwrap();
        index.register(Path::new(&db)).unwrap();
        fs::rename(&db, root.path().join("unmounted.db")).unwrap();
        let (accounts, unavailable) = index.accounts(None).unwrap();
        assert!(accounts.is_empty());
        assert_eq!(unavailable.len(), 1);
        assert!(require_available(&unavailable).is_err());
        assert!(!Path::new(&db).exists());
        assert_eq!(
            Catalog::load(root.path().to_path_buf())
                .unwrap()
                .databases
                .len(),
            1
        );
    }

    #[test]
    fn ambiguous_names_are_never_selected_by_order() {
        let root = tempfile::tempdir().unwrap();
        let first = fixture(root.path(), "first.db", "Daily");
        let second = fixture(root.path(), "second.db", "Daily");
        let index = Catalog {
            root: root.path().to_path_buf(),
            databases: vec![first.into(), second.into()],
        };
        let (accounts, _) = index.accounts(None).unwrap();
        assert!(select(&accounts, Some("Daily")).is_err());
        assert!(select(&accounts, Some("daily")).is_err());
    }
}
