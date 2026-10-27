//! Shared test utilities for `llm-proxy-core`.
//!
//! [`TEST_ENV_LOCK`] serialises all environment-variable-mutating tests
//! across modules.

use std::sync::Mutex;

/// Crate-level mutex serialising tests that mutate environment variables.
///
/// # Safety invariant
///
/// The mutex guarantees exclusive access, so the `unsafe`
/// `std::env::set_var` / `std::env::remove_var` calls within the lock scope
/// are safe: no other test thread can concurrently read or write the
/// environment.
pub static TEST_ENV_LOCK: Mutex<()> = Mutex::new(());

/// RAII wrapper that holds [`TEST_ENV_LOCK`]; the lock is released on drop.
///
/// Acquire via [`TestEnvLock::acquire`].
#[derive(Debug)]
pub struct TestEnvLock {
    _guard: std::sync::MutexGuard<'static, ()>,
}

impl TestEnvLock {
    /// Acquire the crate-level test env mutex, recovering from poison.
    pub fn acquire() -> Self {
        let guard = TEST_ENV_LOCK.lock().unwrap_or_else(|e| e.into_inner());
        Self { _guard: guard }
    }
}

/// RAII guard that saves an environment variable on creation and restores it
/// (or removes it) on drop.
///
/// # Lock discipline (load-bearing invariant)
///
/// [`EnvVarGuard::set`] and [`EnvVarGuard::remove`] perform `unsafe`
/// `std::env::set_var` / `remove_var` calls, and [`Drop`] restores the
/// original value with the same calls. These are only sound while no other
/// thread is touching the process environment, i.e. while a [`TestEnvLock`]
/// is held. The guard carries no lifetime tie to the lock, so this invariant
/// is enforced by convention: **callers must not let an [`EnvVarGuard`]
/// outlive the [`TestEnvLock`] that authorises it** (e.g. by returning it
/// from a helper or storing it in a struct) — bind both in the same scope so
/// the guard drops before the lock.
///
/// ```no_run
/// # use llm_proxy_core::test_support::{TestEnvLock, EnvVarGuard};
/// let _lock = TestEnvLock::acquire();      // acquired first ...
/// let _guard = EnvVarGuard::set("K", "v"); // ... so the guard drops first.
/// ```
#[derive(Debug)]
pub struct EnvVarGuard {
    key: String,
    original: Option<String>,
}

impl EnvVarGuard {
    /// Set an environment variable, saving the previous value for restoration.
    ///
    /// # Lock discipline
    ///
    /// Caller must ensure exclusive access to the process environment (e.g.
    /// by holding `TestEnvLock`), and must drop the returned guard before
    /// that lock is released (reverse drop order). See the type-level
    /// "Lock discipline" note.
    pub fn set(key: &str, value: &str) -> Self {
        let original = std::env::var(key).ok();
        #[expect(
            unsafe_code,
            reason = "std::env::set_var is unsafe since edition 2024; the fn-level Safety contract requires the caller to hold TestEnvLock"
        )]
        // SAFETY: `set_var` is sound only while no other thread reads or writes the
        // process environment. Every caller in this crate holds `TEST_ENV_LOCK`
        // (via `TestEnvLock::acquire`) for the whole lifetime of the returned
        // guard, so this thread has exclusive access to the environment for the
        // duration of the call.
        unsafe {
            std::env::set_var(key, value);
        }
        Self {
            key: key.to_owned(),
            original,
        }
    }

    /// Remove an environment variable, saving the previous value for restoration.
    ///
    /// # Lock discipline
    ///
    /// Caller must ensure exclusive access to the process environment (e.g.
    /// by holding `TestEnvLock`), and must drop the returned guard before
    /// that lock is released (reverse drop order). See the type-level
    /// "Lock discipline" note.
    pub fn remove(key: &str) -> Self {
        let original = std::env::var(key).ok();
        #[expect(
            unsafe_code,
            reason = "std::env::remove_var is unsafe since edition 2024; the fn-level Safety contract requires the caller to hold TestEnvLock"
        )]
        // SAFETY: `remove_var` is sound only while no other thread reads or
        // writes the process environment. Every caller in this crate holds
        // `TEST_ENV_LOCK` (via `TestEnvLock::acquire`) for the whole lifetime of
        // the returned guard, so this thread has exclusive access to the
        // environment for the duration of the call.
        unsafe {
            std::env::remove_var(key);
        }
        Self {
            key: key.to_owned(),
            original,
        }
    }
}

/// Restore the saved environment variable.
///
/// # Safety
///
/// `Drop` performs the same `unsafe` `set_var` / `remove_var` as the
/// constructors and therefore inherits their contract: it must run while a
/// [`TestEnvLock`] is still held. This is guaranteed today solely by Rust's
/// reverse field-drop order — the guard is always dropped before the lock
/// binding it shares a scope with — because [`EnvVarGuard`] carries no
/// lifetime tie to the lock.
impl Drop for EnvVarGuard {
    #[expect(
        unsafe_code,
        reason = "restores the env var via the same unsafe set_var/remove_var as the constructors; runs while TestEnvLock is still held per the Safety note above"
    )]
    fn drop(&mut self) {
        match &self.original {
            // SAFETY: restoring the saved value runs while the `TestEnvLock`
            // that authorised the corresponding `set`/`remove` is still held
            // (guaranteed by reverse drop order at every call site), so no
            // other thread can be reading or writing the environment here.
            Some(val) => unsafe {
                std::env::set_var(&self.key, val);
            },
            // SAFETY: same invariant as the `Some` arm above — the guard is
            // always dropped before the `TestEnvLock` it was created under, so
            // this thread has exclusive access to the environment.
            None => unsafe {
                std::env::remove_var(&self.key);
            },
        }
    }
}
