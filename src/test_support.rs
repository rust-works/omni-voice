//! Shared test-only helpers.
//!
//! These utilities are consumed by unit tests across the crate and must
//! stay in sync between shim-writing sites — see issue #642.

#![allow(clippy::unwrap_used, clippy::expect_used)]

#[cfg(unix)]
pub(crate) mod shim {
    use std::path::Path;
    use std::sync::{Mutex, MutexGuard};

    /// Serializes every test that writes an executable shim and then
    /// `execve`s it. Belt-and-braces pairing with `write_exec_script`'s
    /// sync+close: even with each test's FD fully released before exec,
    /// high parallelism (cargo llvm-cov) could still land a `fork()` from
    /// one test while another thread's writable FD was live, letting the
    /// child inherit it and hit `ETXTBSY`. See issue #642.
    static SHIM_LOCK: Mutex<()> = Mutex::new(());

    /// Acquires the crate-wide shim lock, recovering from poisoning so
    /// intentional panics in one test don't cascade into the rest of the
    /// suite.
    pub(crate) fn shim_lock() -> MutexGuard<'static, ()> {
        SHIM_LOCK
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
    }

    /// Writes an executable script at `path`, flushes it to disk, and
    /// explicitly drops the writable FD before returning. Setting mode
    /// via `OpenOptions` avoids a second open-for-write that
    /// `chmod`-after-`fs::write` would cause.
    pub(crate) fn write_exec_script(path: &Path, script: &str) {
        use std::io::Write;
        use std::os::unix::fs::OpenOptionsExt;
        let mut file = std::fs::OpenOptions::new()
            .write(true)
            .create(true)
            .truncate(true)
            .mode(0o755)
            .open(path)
            .unwrap();
        file.write_all(script.as_bytes()).unwrap();
        file.sync_all().unwrap();
        drop(file);
    }
}

/// Crate-wide serialization for env-var-mutating tests.
///
/// Environment variables are process-global, so a module-local mutex can
/// only serialise tests *within* that module — tests in other modules
/// still race on the same var (or on the global env namespace). Every
/// env-var-touching test in the crate must therefore serialise on the one
/// [`env_lock`] below rather than its own private mutex. See issue #12.
pub(crate) mod env {
    use std::sync::{Mutex, MutexGuard, PoisonError};

    /// The single process-global lock every env-var-mutating test takes.
    static ENV_LOCK: Mutex<()> = Mutex::new(());

    /// Acquires the crate-wide env lock, recovering from poisoning so an
    /// intentional panic in one test doesn't cascade into the rest of the
    /// suite.
    pub(crate) fn env_lock() -> MutexGuard<'static, ()> {
        ENV_LOCK.lock().unwrap_or_else(PoisonError::into_inner)
    }

    /// Convenience guard for the common shape: acquire [`env_lock`],
    /// snapshot and clear `keys`, and restore their original values on
    /// drop. Mutate vars during the test with [`EnvGuard::set`] (or
    /// directly) — anything listed in `keys` is restored regardless.
    pub(crate) struct EnvGuard {
        _lock: MutexGuard<'static, ()>,
        saved: Vec<(String, Option<String>)>,
    }

    impl EnvGuard {
        /// Locks, then snapshots and clears every var in `keys`.
        pub(crate) fn clearing(keys: &[&str]) -> Self {
            let lock = env_lock();
            let saved = keys
                .iter()
                .map(|k| ((*k).to_string(), std::env::var(k).ok()))
                .collect();
            for k in keys {
                std::env::remove_var(k);
            }
            Self { _lock: lock, saved }
        }

        /// Sets `key` to `value` for the lifetime of the guard.
        pub(crate) fn set(&self, key: &str, value: &str) {
            std::env::set_var(key, value);
        }
    }

    impl Drop for EnvGuard {
        fn drop(&mut self) {
            for (k, v) in self.saved.drain(..) {
                match v {
                    Some(val) => std::env::set_var(&k, val),
                    None => std::env::remove_var(&k),
                }
            }
        }
    }

    #[cfg(test)]
    mod tests {
        use super::EnvGuard;

        // Unique to this test, so the pre-guard mutations below can't race
        // other env tests; `EnvGuard` serialises everything else.
        const PROBE: &str = "OMNI_VOICE_TEST_ENVGUARD_PROBE";

        #[test]
        fn envguard_snapshots_clears_and_restores() {
            // A var with a prior value is restored to it on drop.
            std::env::set_var(PROBE, "original");
            {
                let guard = EnvGuard::clearing(&[PROBE]);
                assert!(std::env::var(PROBE).is_err(), "clearing() clears the var");
                guard.set(PROBE, "temporary");
                assert_eq!(std::env::var(PROBE).unwrap(), "temporary");
            }
            assert_eq!(
                std::env::var(PROBE).unwrap(),
                "original",
                "drop restores the prior value"
            );

            // A var with no prior value is removed again on drop.
            std::env::remove_var(PROBE);
            {
                let guard = EnvGuard::clearing(&[PROBE]);
                guard.set(PROBE, "temporary");
                assert_eq!(std::env::var(PROBE).unwrap(), "temporary");
            }
            assert!(
                std::env::var(PROBE).is_err(),
                "drop removes a var that had no prior value"
            );
        }
    }
}
