//! This module handles the locking logic during compilation.

use crate::util::flock;
use crate::{
    CargoResult,
    compiler::{BuildRunner, Unit},
    util::{FileLock, Filesystem},
};

use crate::util::data_structures::HashMap;
use anyhow::bail;
use std::{
    fmt::{Display, Formatter},
    fs::TryLockError,
    path::PathBuf,
    sync::RwLock,
    time::{Duration, Instant},
};
use tracing::instrument;

const LAST_USED_UPDATE_INTERVAL: Duration = Duration::from_secs(24 * 60 * 60);

/// A struct to store the lock handles for build units during compilation.
pub struct LockManager {
    locks: RwLock<HashMap<LockKey, FileLock>>,
}

impl LockManager {
    pub fn new() -> Self {
        Self {
            locks: RwLock::new(HashMap::default()),
        }
    }

    /// Takes a shared lock on a given [`Unit`]
    /// This prevents other Cargo instances from compiling (writing) to
    /// this build unit.
    ///
    /// This function returns a [`LockKey`] which can be used to
    /// upgrade/unlock the lock.
    #[instrument(skip_all, fields(key))]
    pub fn lock_shared(
        &self,
        build_runner: &BuildRunner<'_, '_>,
        unit: &Unit,
    ) -> CargoResult<LockKey> {
        let key = LockKey::from_unit(build_runner, unit);
        tracing::Span::current().record("key", key.0.to_str());

        let mut locks = self.locks.write().unwrap();
        if let Some(lock) = locks.get_mut(&key) {
            flock::lock_shared(lock.file())?;
            update_last_used(lock);
        } else {
            let fs = Filesystem::new(key.0.clone());
            let lock_msg = format!(
                "{} ({})",
                unit.pkg.name(),
                build_runner.files().unit_hash(unit)
            );
            let lock = fs.open_ro_shared_create(&key.0, build_runner.bcx.gctx, &lock_msg)?;
            update_last_used(&lock);
            locks.insert(key.clone(), lock);
        }

        Ok(key)
    }

    /// Locks a unit that this build found dirty, before compiling it. Returns `true` with the
    /// unit locked exclusively, for us to compile it, or `false` with the unit locked shared, when
    /// `fresh` says that another cargo has built it as we would have.
    ///
    /// It never blocks on the exclusive lock, which could wait forever: another cargo holds a unit
    /// that it built or found fresh shared until its build ends, and may be waiting for a unit
    /// that we hold shared. A shared lock waits only for a cargo compiling the unit, which waits
    /// for nothing else. So it waits for a shared lock and checks the fingerprint, then tries
    /// for the exclusive lock, and so on: a unit that nobody has built is held shared only while
    /// another cargo plans its build, and one that another cargo built as we would is fresh. A
    /// unit that stays dirty for us is read by another cargo with other settings: we fail at a
    /// deadline, naming the unit.
    #[instrument(skip(self, fresh))]
    pub fn lock_to_build(
        &self,
        key: &LockKey,
        fresh: impl Fn() -> CargoResult<bool>,
        unit: &str,
    ) -> CargoResult<bool> {
        let locks = self.locks.read().unwrap();
        let Some(lock) = locks.get(key) else {
            bail!("lock was not found in lock manager: {key}");
        };
        let file = lock.file();
        let timeout = lock_timeout();
        let start = Instant::now();
        let mut wait = Duration::from_millis(10);
        loop {
            if try_lock(file, flock::try_lock_exclusive)? {
                return Ok(true);
            }
            // Wait only on a cargo that is compiling the unit, then see what it made.
            flock::lock_shared(file)?;
            if fresh()? {
                return Ok(false);
            }
            flock::unlock(file)?;
            if start.elapsed() >= timeout {
                bail!(
                    "could not lock {unit} in the build directory for {}s: another cargo process uses \
                     it, built with settings that differ from this build's (lock {key})",
                    timeout.as_secs()
                );
            }
            // Jittered: cargos that wait for the same unit with the same backoff would otherwise
            // try at the same moments, each failing on the shared lock of the other's check.
            let jittered = wait / 2 + wait.mul_f64(jitter());
            std::thread::sleep(jittered.min(timeout.saturating_sub(start.elapsed())));
            wait = (wait * 2).min(Duration::from_millis(500));
        }
    }

    /// Upgrades an existing exclusive lock into a shared lock.
    #[instrument(skip(self))]
    pub fn downgrade_to_shared(&self, key: &LockKey) -> CargoResult<()> {
        let locks = self.locks.read().unwrap();
        let Some(lock) = locks.get(key) else {
            bail!("lock was not found in lock manager: {key}");
        };
        flock::lock_shared(lock.file())?;
        Ok(())
    }

    #[instrument(skip(self))]
    pub fn unlock(&self, key: &LockKey) -> CargoResult<()> {
        let locks = self.locks.read().unwrap();
        if let Some(lock) = locks.get(key) {
            flock::unlock(lock.file())?;
        };

        Ok(())
    }
}

fn try_lock(
    file: &std::fs::File,
    lock: fn(&std::fs::File) -> Result<(), TryLockError>,
) -> CargoResult<bool> {
    match lock(file) {
        Ok(()) => Ok(true),
        Err(TryLockError::WouldBlock) => Ok(false),
        Err(TryLockError::Error(e)) => Err(e.into()),
    }
}

/// A number in [0, 1), different in each call and process.
fn jitter() -> f64 {
    use std::hash::{BuildHasher, Hasher};
    let mut hasher = std::collections::hash_map::RandomState::new().build_hasher();
    hasher.write_u32(std::process::id());
    (hasher.finish() >> 11) as f64 / (1u64 << 53) as f64
}

/// How long [`LockManager::lock_to_build`] tries for a unit that another cargo uses with other
/// settings: 60 s, or `__CARGO_TEST_FINE_GRAIN_LOCK_TIMEOUT_MS` for the testsuite.
#[expect(
    clippy::disallowed_methods,
    reason = "testing only, no reason for config support"
)]
fn lock_timeout() -> Duration {
    std::env::var("__CARGO_TEST_FINE_GRAIN_LOCK_TIMEOUT_MS")
        .ok()
        .and_then(|ms| ms.parse().ok())
        .map(Duration::from_millis)
        .unwrap_or(Duration::from_secs(60))
}

fn update_last_used(lock: &FileLock) {
    let Ok(modified) = lock
        .file()
        .metadata()
        .and_then(|metadata| metadata.modified())
    else {
        return;
    };
    if modified
        .elapsed()
        .is_ok_and(|elapsed| elapsed >= LAST_USED_UPDATE_INTERVAL)
    {
        // The lock is visited for every unit in the active graph, including
        // fresh dependencies, making its mtime a cheap approximate last-use time.
        cargo_util::paths::set_file_time_no_err(lock.path(), filetime::FileTime::now());
    }
}

#[derive(Debug, Clone, Hash, Eq, PartialEq)]
pub struct LockKey(PathBuf);

impl LockKey {
    fn from_unit(build_runner: &BuildRunner<'_, '_>, unit: &Unit) -> Self {
        Self(build_runner.files().build_unit_lock(unit))
    }
}

impl Display for LockKey {
    fn fmt(&self, f: &mut Formatter<'_>) -> std::fmt::Result {
        write!(f, "{}", self.0.display())
    }
}
