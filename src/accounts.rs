//! Persistent multi-account credentials and migration from the single-account format.
use crate::api::{self, Account, Config};
use anyhow::{bail, Context, Result};
use serde::{Deserialize, Serialize};
use std::{
    collections::HashSet,
    fs,
    io::Write,
    path::{Path, PathBuf},
    sync::atomic::{AtomicU64, Ordering},
};

static TEMP_SEQUENCE: AtomicU64 = AtomicU64::new(0);

#[derive(Clone, Serialize, Deserialize)]
pub struct StoredAccount {
    /// Server URL plus the account ID, unique even when different servers reuse IDs.
    pub id: String,
    pub label: String,
    pub config: Config,
}

#[derive(Clone, Default, Serialize, Deserialize)]
pub struct AccountStore {
    pub accounts: Vec<StoredAccount>,
    pub active_id: Option<String>,
    #[serde(skip)]
    removed: Vec<Config>,
    /// Exact on-disk version read by this process; compared while holding the lock.
    #[serde(skip)]
    baseline: Option<Vec<u8>>,
}

impl AccountStore {
    pub fn path() -> Result<PathBuf> {
        Ok(Config::path()?.with_file_name("accounts.json"))
    }

    /// The old credentials are retained untouched if migration fails or succeeds.
    pub fn load() -> Result<Self> {
        let path = Self::path()?;
        if let Some(store) = Self::read_from(&path)? {
            return Ok(store);
        }
        let mut store = Self::default();
        if let Some(config) = Config::load()? {
            store
                .add_legacy(config)
                .context("既存アカウントの移行に失敗しました")?;
            store.write_to(&path)?;
        }
        Ok(store)
    }

    fn read_from(path: &Path) -> Result<Option<Self>> {
        let bytes = match fs::read(path) {
            Ok(bytes) => bytes,
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(None),
            Err(e) => {
                return Err(e)
                    .with_context(|| format!("アカウントを読み込めません: {}", path.display()))
            }
        };
        let mut store: Self =
            serde_json::from_slice(&bytes).context("アカウントファイルの読み込みに失敗しました")?;
        store.validate()?;
        store.baseline = Some(bytes);
        Ok(Some(store))
    }

    fn validate(&self) -> Result<()> {
        let mut ids = HashSet::new();
        for account in &self.accounts {
            if account.id.is_empty() || !ids.insert(&account.id) {
                bail!("アカウントファイルに重複または空の ID があります");
            }
            if account.config.access_token.trim().is_empty() {
                bail!("アカウントのアクセストークンが空です");
            }
            api::normalize_instance(&account.config.instance)?;
        }
        if let Some(id) = &self.active_id {
            if !ids.contains(id) {
                bail!("選択中のアカウントが見つかりません");
            }
        } else if !self.accounts.is_empty() {
            bail!("選択中のアカウントが設定されていません");
        }
        Ok(())
    }

    #[cfg(test)]
    pub fn active(&self) -> Option<&StoredAccount> {
        let id = self.active_id.as_deref()?;
        self.accounts.iter().find(|a| a.id == id)
    }

    fn add_legacy(&mut self, mut config: Config) -> Result<()> {
        config.instance = api::normalize_instance(&config.instance)?;
        if config.access_token.trim().is_empty() {
            bail!("アクセストークンが空です");
        }
        let id = format!("{}#legacy", config.instance);
        let label = format!("{} (legacy)", config.instance);
        self.accounts.push(StoredAccount {
            id: id.clone(),
            label,
            config,
        });
        self.active_id = Some(id);
        Ok(())
    }

    /// Record an identity already verified against the corresponding server.
    pub fn add_with_identity(&mut self, mut config: Config, account: Account) -> Result<String> {
        config.instance = api::normalize_instance(&config.instance)?;
        if config.access_token.trim().is_empty()
            || account.id.trim().is_empty()
            || account.acct.trim().is_empty()
        {
            bail!("サーバーからアカウント情報を取得できませんでした");
        }
        let id = format!("{}#account:{}", config.instance, account.id);
        let host = reqwest::Url::parse(&config.instance)?
            .host_str()
            .context("サーバー名が不正です")?
            .to_owned();
        let label = if account.acct.contains('@') {
            format!("@{}", account.acct)
        } else {
            format!("@{}@{}", account.acct, host)
        };
        let legacy_id = format!("{}#legacy", config.instance);
        let legacy_position = self
            .accounts
            .iter()
            .position(|a| a.id == legacy_id && a.config.access_token == config.access_token);
        let entry = StoredAccount {
            id: id.clone(),
            label,
            config,
        };
        if let Some(index) = self.accounts.iter().position(|a| a.id == id) {
            if self.accounts[index].config.access_token != entry.config.access_token {
                self.removed.push(self.accounts[index].config.clone());
            }
            self.accounts[index] = entry;
            if let Some(legacy) = legacy_position {
                self.accounts.remove(legacy);
            }
        } else if let Some(index) = legacy_position {
            self.accounts[index] = entry;
        } else {
            self.accounts.push(entry);
        }
        self.active_id = Some(id.clone());
        Ok(id)
    }

    pub fn set_active(&mut self, id: &str) -> Result<()> {
        if !self.accounts.iter().any(|a| a.id == id) {
            bail!("アカウントが見つかりません");
        }
        self.active_id = Some(id.to_owned());
        Ok(())
    }

    pub fn remove(&mut self, id: &str) -> Result<()> {
        let index = self
            .accounts
            .iter()
            .position(|a| a.id == id)
            .context("アカウントが見つかりません")?;
        self.removed.push(self.accounts.remove(index).config);
        if self.active_id.as_deref() == Some(id) {
            self.active_id = self
                .accounts
                .get(index.min(self.accounts.len().saturating_sub(1)))
                .map(|a| a.id.clone());
        }
        Ok(())
    }

    pub fn save(&mut self) -> Result<()> {
        self.write_to(&Self::path()?)
    }

    /// Call after a successful save; failure only means obsolete legacy credentials remain.
    pub fn cleanup_removed_legacy(&self) -> Result<()> {
        let config_path = Config::path()?;
        let legacy_path = config_path
            .parent()
            .and_then(Path::parent)
            .context("設定パスが不正です")?
            .join("tootui")
            .join("config.json");
        for config in &self.removed {
            remove_matching_legacy(&config_path, config)?;
            remove_matching_legacy(&legacy_path, config)?;
        }
        Ok(())
    }

    fn write_to(&mut self, path: &Path) -> Result<()> {
        self.validate()?;
        let parent = path.parent().context("アカウントパスが不正です")?;
        fs::create_dir_all(parent)?;
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            fs::set_permissions(parent, fs::Permissions::from_mode(0o700))?;
        }
        // Lock a stable sidecar inode: the data file itself is atomically
        // replaced, so locking that inode would not protect later writers.
        let _lock = AccountLock::acquire(parent)?;
        let current = match fs::read(path) {
            Ok(bytes) => Some(bytes),
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => None,
            Err(e) => return Err(e).context("アカウントファイルを確認できません"),
        };
        if current != self.baseline {
            bail!("アカウントファイルが別のプロセスで変更されました。LazyDon を再起動してから再試行してください");
        }
        let mut bytes = serde_json::to_vec_pretty(self)?;
        bytes.push(b'\n');
        let temp = parent.join(format!(
            ".accounts.{}.{}.tmp",
            std::process::id(),
            TEMP_SEQUENCE.fetch_add(1, Ordering::Relaxed)
        ));
        let mut created = false;
        let result = (|| -> Result<()> {
            let mut options = fs::OpenOptions::new();
            options.write(true).create_new(true);
            #[cfg(unix)]
            {
                use std::os::unix::fs::OpenOptionsExt;
                options.mode(0o600);
            }
            let mut file = options.open(&temp)?;
            created = true;
            file.write_all(&bytes)?;
            file.sync_all()?;
            fs::rename(&temp, path)?;
            Ok(())
        })();
        if result.is_err() && created {
            let _ = fs::remove_file(&temp);
        }
        result.context("アカウントを保存できませんでした")?;
        self.baseline = Some(bytes);
        Ok(())
    }
}

// flock is released automatically when the file closes, even on a crash. The
// lock file remains in place; removing it would let writers lock different inodes.
#[cfg(unix)]
struct AccountLock(std::fs::File);

#[cfg(unix)]
impl AccountLock {
    fn acquire(parent: &Path) -> Result<Self> {
        use std::os::fd::AsRawFd;
        use std::os::unix::fs::OpenOptionsExt;
        extern "C" {
            fn flock(
                fd: std::os::raw::c_int,
                operation: std::os::raw::c_int,
            ) -> std::os::raw::c_int;
        }
        const LOCK_EX: i32 = 2;
        const LOCK_NB: i32 = 4;
        let file = fs::OpenOptions::new()
            .read(true)
            .write(true)
            .create(true)
            .mode(0o600)
            .open(parent.join(".accounts.lock"))
            .context("アカウントロックを開けません")?;
        // SAFETY: file is an open descriptor for the lifetime of the guard.
        if unsafe { flock(file.as_raw_fd(), LOCK_EX | LOCK_NB) } != 0 {
            return Err(std::io::Error::last_os_error())
                .context("別の LazyDon がアカウントを更新中です。再試行してください");
        }
        Ok(Self(file))
    }
}

// Unlock explicitly before closing: a concurrent fork may briefly inherit the
// open file description even though the descriptor is close-on-exec.
#[cfg(unix)]
impl Drop for AccountLock {
    fn drop(&mut self) {
        use std::os::fd::AsRawFd;
        extern "C" {
            fn flock(
                fd: std::os::raw::c_int,
                operation: std::os::raw::c_int,
            ) -> std::os::raw::c_int;
        }
        const LOCK_UN: i32 = 8;
        // SAFETY: the descriptor remains open until after this drop method.
        let _ = unsafe { flock(self.0.as_raw_fd(), LOCK_UN) };
    }
}

#[cfg(not(unix))]
struct AccountLock;
#[cfg(not(unix))]
impl AccountLock {
    fn acquire(_parent: &Path) -> Result<Self> {
        bail!("この OS ではアカウントの安全な同時保存に対応していません")
    }
}

/// Never remove a legacy file belonging to another account.
fn remove_matching_legacy(path: &Path, removed: &Config) -> Result<()> {
    let bytes = match fs::read(path) {
        Ok(bytes) => bytes,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(()),
        Err(e) => {
            return Err(e).with_context(|| {
                format!("旧アカウントファイルを確認できません: {}", path.display())
            })
        }
    };
    let legacy: Config = serde_json::from_slice(&bytes)
        .with_context(|| format!("旧アカウントファイルを確認できません: {}", path.display()))?;
    if api::normalize_instance(&legacy.instance)? == api::normalize_instance(&removed.instance)?
        && legacy.access_token == removed.access_token
    {
        fs::remove_file(path)
            .with_context(|| format!("旧アカウントファイルを削除できません: {}", path.display()))?;
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    fn config(host: &str, token: &str) -> Config {
        Config {
            instance: host.into(),
            access_token: token.into(),
        }
    }
    fn account(id: &str, acct: &str) -> Account {
        Account {
            id: id.into(),
            acct: acct.into(),
            ..Default::default()
        }
    }
    fn path() -> PathBuf {
        let dir = std::env::temp_dir().join(format!(
            "lazydon-accounts-{}-{}",
            std::process::id(),
            TEMP_SEQUENCE.fetch_add(1, Ordering::Relaxed)
        ));
        dir.join("accounts.json")
    }
    #[test]
    fn distinct_servers_upsert_and_selection_removal() {
        let mut store = AccountStore::default();
        let first = store
            .add_with_identity(config("https://one.example", "one"), account("42", "alice"))
            .unwrap();
        let second = store
            .add_with_identity(config("https://two.example", "two"), account("42", "alice"))
            .unwrap();
        assert_ne!(first, second);
        assert_eq!(store.accounts.len(), 2);
        assert_eq!(store.active().unwrap().label, "@alice@two.example");
        assert_eq!(
            store
                .add_with_identity(
                    config("https://one.example/", "new"),
                    account("42", "alice")
                )
                .unwrap(),
            first
        );
        assert_eq!(store.accounts.len(), 2);
        assert_eq!(store.active().unwrap().config.access_token, "new");
        store.set_active(&second).unwrap();
        assert!(store.remove("unknown").is_err());
        assert_eq!(store.accounts.len(), 2);
        store.remove(&second).unwrap();
        assert_eq!(store.active().unwrap().id, first);
        store.remove(&first).unwrap();
        assert!(store.active().is_none());
    }
    #[test]
    fn store_is_private_atomic_and_rejects_corruption() {
        let path = path();
        let mut store = AccountStore::default();
        store
            .add_with_identity(config("https://one.example", "secret"), account("1", "a"))
            .unwrap();
        store.write_to(&path).unwrap();
        let loaded = AccountStore::read_from(&path).unwrap().unwrap();
        assert_eq!(loaded.active().unwrap().config.access_token, "secret");
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            assert_eq!(
                fs::metadata(&path).unwrap().permissions().mode() & 0o777,
                0o600
            );
        }
        store.active_id = Some("missing".into());
        assert!(store.write_to(&path).is_err());
        assert_eq!(
            AccountStore::read_from(&path)
                .unwrap()
                .unwrap()
                .active()
                .unwrap()
                .config
                .access_token,
            "secret"
        );
        fs::write(&path, b"broken").unwrap();
        assert!(AccountStore::read_from(&path).is_err());
        fs::remove_dir_all(path.parent().unwrap()).unwrap();
    }
    #[test]
    fn stale_writer_cannot_overwrite_another_process_change() {
        let path = path();
        let mut initial = AccountStore::default();
        initial
            .add_with_identity(config("https://one.example", "one"), account("1", "a"))
            .unwrap();
        initial.write_to(&path).unwrap();
        let mut a = AccountStore::read_from(&path).unwrap().unwrap();
        let mut b = AccountStore::read_from(&path).unwrap().unwrap();
        a.add_with_identity(config("https://two.example", "two"), account("2", "b"))
            .unwrap();
        a.write_to(&path).unwrap();
        b.add_with_identity(config("https://three.example", "three"), account("3", "c"))
            .unwrap();
        let error = b.write_to(&path).unwrap_err().to_string();
        assert!(error.contains("別のプロセス"));
        let actual = AccountStore::read_from(&path).unwrap().unwrap();
        assert_eq!(actual.accounts.len(), 2);
        assert!(actual
            .accounts
            .iter()
            .any(|a| a.config.access_token == "two"));
        assert!(!actual
            .accounts
            .iter()
            .any(|a| a.config.access_token == "three"));
        fs::remove_dir_all(path.parent().unwrap()).unwrap();
    }

    #[test]
    fn parallel_writers_never_both_commit_the_same_baseline() {
        use std::sync::{Arc, Barrier};
        let path = path();
        let mut initial = AccountStore::default();
        initial
            .add_with_identity(config("https://one.example", "one"), account("1", "a"))
            .unwrap();
        initial.write_to(&path).unwrap();
        let mut a = AccountStore::read_from(&path).unwrap().unwrap();
        let mut b = AccountStore::read_from(&path).unwrap().unwrap();
        a.add_with_identity(config("https://two.example", "two"), account("2", "b"))
            .unwrap();
        b.add_with_identity(config("https://three.example", "three"), account("3", "c"))
            .unwrap();
        let barrier = Arc::new(Barrier::new(2));
        let child_path = path.clone();
        let child_barrier = barrier.clone();
        let child = std::thread::spawn(move || {
            child_barrier.wait();
            a.write_to(&child_path)
        });
        barrier.wait();
        let first = b.write_to(&path).is_ok();
        let second = child.join().unwrap().is_ok();
        assert_ne!(first, second, "exactly one writer must commit");
        let actual = AccountStore::read_from(&path).unwrap().unwrap();
        assert_eq!(actual.accounts.len(), 2);
        fs::remove_dir_all(path.parent().unwrap()).unwrap();
    }

    #[test]
    #[cfg(unix)]
    fn lock_is_released_without_removing_the_sidecar() {
        let path = path();
        let mut store = AccountStore::default();
        store
            .add_with_identity(config("https://one.example", "one"), account("1", "a"))
            .unwrap();
        store.write_to(&path).unwrap();
        let lock_path = path.parent().unwrap().join(".accounts.lock");
        let held = AccountLock::acquire(path.parent().unwrap()).unwrap();
        assert!(store.write_to(&path).is_err());
        drop(held);
        assert!(lock_path.exists());
        store.write_to(&path).unwrap();
        fs::remove_dir_all(path.parent().unwrap()).unwrap();
    }

    #[test]
    fn invalid_account_never_replaces_credentials() {
        let mut store = AccountStore::default();
        let id = store
            .add_with_identity(config("https://one.example", "old"), account("1", "a"))
            .unwrap();
        assert!(store
            .add_with_identity(config("https://one.example", "new"), account("", "a"))
            .is_err());
        assert_eq!(store.active().unwrap().id, id);
        assert_eq!(store.active().unwrap().config.access_token, "old");
    }

    #[test]
    fn legacy_migrates_without_network_and_upgrades_in_place() {
        let mut store = AccountStore::default();
        store
            .add_legacy(config("https://one.example/", "old"))
            .unwrap();
        assert!(store.active().unwrap().id.ends_with("#legacy"));
        let id = store
            .add_with_identity(config("https://one.example", "old"), account("1", "alice"))
            .unwrap();
        assert_eq!(store.accounts.len(), 1);
        assert_eq!(store.active_id.as_deref(), Some(id.as_str()));
        assert_eq!(store.active().unwrap().label, "@alice@one.example");
    }

    #[test]
    fn cleanup_deletes_only_matching_legacy_credentials() {
        let path = path();
        fs::create_dir_all(path.parent().unwrap()).unwrap();
        let legacy = config("https://one.example/", "old");
        fs::write(&path, serde_json::to_vec(&legacy).unwrap()).unwrap();
        remove_matching_legacy(&path, &config("https://other.example", "old")).unwrap();
        remove_matching_legacy(&path, &config("https://one.example", "other")).unwrap();
        assert!(path.exists());
        remove_matching_legacy(&path, &config("https://one.example", "old")).unwrap();
        assert!(!path.exists());
        fs::remove_dir_all(path.parent().unwrap()).unwrap();
    }
}
