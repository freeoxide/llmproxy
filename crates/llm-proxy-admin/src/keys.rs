//! Managed client keys (plan 09 step 2): create, list, revoke — persisted
//! in the admin surface's own SQLite store, issued as
//! `<key-id>.<secret>` tokens (the plan 05 lookup shape: one argon2 run
//! per presented token), and exportable as
//! [`ClientKeySpec`](llm_proxy_core::ClientKeySpec)s so the daemon's
//! `ClientAuth` accepts them alongside the `[auth.clients]` file keys.
//!
//! The plaintext secret is shown exactly once, at creation; only the
//! argon2id verifier is stored.

use std::path::Path;

use argon2::password_hash::try_generate_salt;
use argon2::{Algorithm, Argon2, Params, PasswordHasher, Version};
use llm_proxy_core::{ClientKeySpec, KeyBudgetConfig, KeyWindowLimits, UnestimatableCostPolicy};
use rusqlite::Connection;
use secrecy::SecretString;
use serde::{Deserialize, Serialize};
use time::OffsetDateTime;

/// Errors from the managed-key store.
#[derive(Debug, thiserror::Error)]
pub enum ManagedKeyError {
    /// The store's database could not be opened or initialized.
    #[error("opening the managed-key store: {0}")]
    Open(#[from] rusqlite::Error),
    /// The issued token could not be hashed (parameters or OS entropy).
    #[error("hashing the issued token: {0}")]
    Hash(String),
    /// A key id does not exist.
    #[error("no managed key with id {0}")]
    UnknownKey(String),
    /// A key is already revoked.
    #[error("key {0} is already revoked")]
    AlreadyRevoked(String),
}

/// What the operator may pin on a key at creation (the plan 05 policy
/// fields; everything is optional).
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct KeyPolicyInput {
    /// Model globs this key may run; empty allows all.
    #[serde(default)]
    pub allow_models: Vec<String>,
    /// Provider or pool globs this key may address; empty allows all.
    #[serde(default)]
    pub allow_providers: Vec<String>,
    /// Requests per minute.
    #[serde(default)]
    pub rpm: Option<u32>,
    /// Tokens per minute.
    #[serde(default)]
    pub tpm: Option<u64>,
    /// Requests per day.
    #[serde(default)]
    pub rpd: Option<u64>,
    /// Daily spend budget in USD (fixed-point string, e.g. `"5.00"`).
    #[serde(default)]
    pub usd_per_day: Option<String>,
}

/// A key record as listed by the admin API: never the verifier, never a
/// secret.
#[derive(Debug, Clone, Serialize)]
#[serde(deny_unknown_fields)]
pub struct KeyRecord {
    /// The key id tokens are prefixed with.
    pub id: String,
    /// When the key was created (RFC 3339).
    pub created_at: String,
    /// When the key was revoked (RFC 3339), if it was.
    pub revoked_at: Option<String>,
    /// The policy pinned at creation.
    pub policy: KeyPolicyInput,
}

/// The one-time answer to key creation: the plaintext token, shown once.
#[derive(Clone)]
pub struct IssuedKey {
    /// The key id (also the token prefix).
    pub key_id: String,
    /// The full `<key-id>.<secret>` token. Shown once; only the verifier
    /// is stored.
    pub token: SecretString,
    /// When the key was created (RFC 3339).
    pub created_at: String,
}

impl std::fmt::Debug for IssuedKey {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("IssuedKey")
            .field("key_id", &self.key_id)
            .field("token", &"[REDACTED]")
            .field("created_at", &self.created_at)
            .finish()
    }
}

/// The managed-key store: one SQLite table.
#[derive(Debug)]
pub struct ManagedKeys {
    conn: std::sync::Mutex<Connection>,
}

const SCHEMA: &str = "CREATE TABLE IF NOT EXISTS managed_keys (
    id TEXT PRIMARY KEY,
    phc TEXT NOT NULL,
    created_ms INTEGER NOT NULL,
    revoked_ms INTEGER,
    policy TEXT NOT NULL
);";

impl ManagedKeys {
    /// Open (or create) the store at `path`. Call at startup.
    ///
    /// # Errors
    ///
    /// [`ManagedKeyError::Open`] when the database cannot be opened.
    pub fn open(path: impl AsRef<Path>) -> Result<Self, ManagedKeyError> {
        let conn = Connection::open(path)?;
        conn.execute_batch(SCHEMA)?;
        Ok(Self {
            conn: std::sync::Mutex::new(conn),
        })
    }

    /// An in-memory store (tests).
    ///
    /// # Errors
    ///
    /// [`ManagedKeyError::Open`] — never for `:memory:`.
    pub fn in_memory() -> Result<Self, ManagedKeyError> {
        Self::open(":memory:")
    }

    /// Issues one key under `policy`, returning the plaintext token once.
    ///
    /// # Errors
    ///
    /// [`ManagedKeyError::Hash`] when the verifier cannot be produced.
    pub fn create(&self, policy: &KeyPolicyInput) -> Result<IssuedKey, ManagedKeyError> {
        let key_id = format!("k{}", hex(&random_bytes(6)?));
        let secret = hex(&random_bytes(32)?);
        let token = format!("{key_id}.{secret}");
        let phc = hash_token(&token)?;
        let created = OffsetDateTime::now_utc();
        let conn = self
            .conn
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        conn.execute(
            "INSERT INTO managed_keys (id, phc, created_ms, revoked_ms, policy) \
             VALUES (?1, ?2, ?3, NULL, ?4)",
            rusqlite::params![
                key_id,
                phc,
                created.unix_timestamp().saturating_mul(1000),
                serde_json::to_string(policy).map_err(|error| {
                    ManagedKeyError::Hash(format!("serializing the policy: {error}"))
                })?,
            ],
        )?;
        Ok(IssuedKey {
            key_id,
            token: SecretString::from(token),
            created_at: rfc3339(created),
        })
    }

    /// Lists every key, newest first. No verifiers, no secrets.
    ///
    /// # Errors
    ///
    /// [`ManagedKeyError::Open`] on a query failure.
    pub fn list(&self) -> Result<Vec<KeyRecord>, ManagedKeyError> {
        let conn = self
            .conn
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        let mut statement = conn.prepare(
            "SELECT id, created_ms, revoked_ms, policy FROM managed_keys \
             ORDER BY created_ms DESC, id",
        )?;
        let rows = statement.query_map([], |row| {
            let id: String = row.get(0)?;
            let created_ms: i64 = row.get(1)?;
            let revoked_ms: Option<i64> = row.get(2)?;
            let policy: String = row.get(3)?;
            Ok((id, created_ms, revoked_ms, policy))
        })?;
        let mut records = Vec::new();
        for row in rows {
            let (id, created_ms, revoked_ms, policy) = row?;
            let policy: KeyPolicyInput = serde_json::from_str(&policy).map_err(|error| {
                ManagedKeyError::Hash(format!("stored policy is corrupt: {error}"))
            })?;
            records.push(KeyRecord {
                id,
                created_at: rfc3339(from_unix_millis(created_ms)),
                revoked_at: revoked_ms.map(|ms| rfc3339(from_unix_millis(ms))),
                policy,
            });
        }
        Ok(records)
    }

    /// Revokes `key_id`; revoked keys stop authenticating once the daemon
    /// rebuilds its client auth from this store.
    ///
    /// # Errors
    ///
    /// [`ManagedKeyError::UnknownKey`] when the id does not exist,
    /// [`ManagedKeyError::AlreadyRevoked`] when it is revoked already.
    pub fn revoke(&self, key_id: &str) -> Result<(), ManagedKeyError> {
        let conn = self
            .conn
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        let revoked_ms: Option<i64> = conn
            .query_row(
                "SELECT revoked_ms FROM managed_keys WHERE id = ?1",
                rusqlite::params![key_id],
                |row| row.get(0),
            )
            .map_err(|error| {
                if error == rusqlite::Error::QueryReturnedNoRows {
                    ManagedKeyError::UnknownKey(key_id.to_owned())
                } else {
                    ManagedKeyError::Open(error)
                }
            })?;
        if revoked_ms.is_some() {
            return Err(ManagedKeyError::AlreadyRevoked(key_id.to_owned()));
        }
        conn.execute(
            "UPDATE managed_keys SET revoked_ms = ?1 WHERE id = ?2",
            rusqlite::params![
                OffsetDateTime::now_utc()
                    .unix_timestamp()
                    .saturating_mul(1000),
                key_id
            ],
        )?;
        Ok(())
    }

    /// The live (unrevoked) keys as `[auth.clients]`-shaped specs: what
    /// the daemon merges into `ClientAuth` at startup.
    ///
    /// # Errors
    ///
    /// [`ManagedKeyError::Open`] on a query failure.
    pub fn client_specs(&self) -> Result<Vec<(String, ClientKeySpec)>, ManagedKeyError> {
        let conn = self
            .conn
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        let mut statement =
            conn.prepare("SELECT id, phc, policy FROM managed_keys WHERE revoked_ms IS NULL")?;
        let rows = statement.query_map([], |row| {
            let id: String = row.get(0)?;
            let phc: String = row.get(1)?;
            let policy: String = row.get(2)?;
            Ok((id, phc, policy))
        })?;
        let mut specs = Vec::new();
        for row in rows {
            let (id, phc, policy) = row?;
            let policy: KeyPolicyInput = serde_json::from_str(&policy).map_err(|error| {
                ManagedKeyError::Hash(format!("stored policy is corrupt: {error}"))
            })?;
            specs.push((id, policy.into_spec(SecretString::from(phc))));
        }
        Ok(specs)
    }
}

impl KeyPolicyInput {
    /// Converts the wire policy to the engine's client-key spec.
    ///
    /// # Panics
    ///
    /// Never in practice: `usd_per_day` strings were parsed once at
    /// creation; a stored garbage value surfaces as `rust_decimal`'s
    /// error instead.
    pub fn into_spec(self, verifier: SecretString) -> ClientKeySpec {
        let budget = self.usd_per_day.map(|usd| KeyBudgetConfig {
            usd_per_day: usd.parse().unwrap_or_default(),
            unestimatable: UnestimatableCostPolicy::Reject,
            assumed_max_output_tokens: 4096,
        });
        let restricted = budget.is_some()
            || self.rpm.is_some()
            || self.tpm.is_some()
            || self.rpd.is_some()
            || !self.allow_models.is_empty()
            || !self.allow_providers.is_empty();
        if restricted {
            ClientKeySpec::Policy {
                verifier,
                allow_models: self.allow_models,
                allow_providers: self.allow_providers,
                limits: Some(KeyWindowLimits {
                    rpm: self.rpm,
                    tpm: self.tpm,
                    rpd: self.rpd,
                }),
                budget,
            }
        } else {
            ClientKeySpec::Verifier(verifier)
        }
    }
}

fn hash_token(token: &str) -> Result<String, ManagedKeyError> {
    let argon = Argon2::new(Algorithm::Argon2id, Version::V0x13, argon2_params());
    argon
        .hash_password(token.as_bytes())
        .map(|hash| hash.to_string())
        .map_err(|error| ManagedKeyError::Hash(error.to_string()))
}

/// Argon2 parameters for issued verifiers: the OWASP-recommended 19 MiB /
/// t=2 / p=1; creation is rare, so the cost is paid once per key.
fn argon2_params() -> Params {
    Params::new(19_456, 2, 1, None).unwrap_or_default()
}

fn random_bytes(count: usize) -> Result<Vec<u8>, ManagedKeyError> {
    // try_generate_salt yields 16 fresh OS-random bytes; stack calls for
    // longer needs (the token secret).
    let mut bytes = Vec::with_capacity(count);
    while bytes.len() < count {
        bytes.extend_from_slice(
            &try_generate_salt().map_err(|error| {
                ManagedKeyError::Hash(format!("OS entropy unavailable: {error}"))
            })?,
        );
    }
    bytes.truncate(count);
    Ok(bytes)
}

fn hex(bytes: &[u8]) -> String {
    let mut out = String::with_capacity(bytes.len() * 2);
    use std::fmt::Write as _;
    for byte in bytes {
        if write!(out, "{byte:02x}").is_err() {
            // A String's fmt writer cannot fail; if it ever did, the id is
            // truncated rather than wrong.
            break;
        }
    }
    out
}

fn from_unix_millis(ms: i64) -> OffsetDateTime {
    OffsetDateTime::from_unix_timestamp_nanos(i128::from(ms) * 1_000_000)
        .unwrap_or_else(|_| OffsetDateTime::now_utc())
}

fn rfc3339(at: OffsetDateTime) -> String {
    at.format(&time::format_description::well_known::Rfc3339)
        .unwrap_or_else(|_| at.to_string())
}

#[cfg(test)]
mod tests {
    use super::*;
    use secrecy::ExposeSecret;

    #[test]
    fn create_lists_and_rejects_double_revoke() {
        let store = ManagedKeys::in_memory().expect("store");
        let issued = store.create(&KeyPolicyInput::default()).expect("issued");
        assert!(
            issued.token.expose_secret().starts_with(&issued.key_id),
            "the token is key-id-prefixed (the plan 05 lookup shape)"
        );
        let listed = store.list().expect("listed");
        assert_eq!(listed.len(), 1);
        assert_eq!(listed[0].id, issued.key_id);
        assert!(listed[0].revoked_at.is_none());

        store.revoke(&issued.key_id).expect("revoked");
        assert!(
            matches!(
                store.revoke(&issued.key_id),
                Err(ManagedKeyError::AlreadyRevoked(_))
            ),
            "a revoked key cannot be revoked twice"
        );
        let listed = store.list().expect("listed");
        assert!(
            listed[0].revoked_at.is_some(),
            "the record shows the revoke"
        );
    }

    #[test]
    fn revoked_keys_leave_client_specs_live_keys_stay() {
        let store = ManagedKeys::in_memory().expect("store");
        let live = store.create(&KeyPolicyInput::default()).expect("live");
        let dead = store.create(&KeyPolicyInput::default()).expect("dead");
        store.revoke(&dead.key_id).expect("revoked");

        let specs = store.client_specs().expect("specs");
        assert_eq!(specs.len(), 1, "only the live key authenticates");
        assert_eq!(specs[0].0, live.key_id);
    }

    #[test]
    fn restricted_policies_become_policy_specs() {
        let store = ManagedKeys::in_memory().expect("store");
        store
            .create(&KeyPolicyInput {
                allow_models: vec!["claude-*".to_owned()],
                allow_providers: vec![],
                rpm: Some(60),
                tpm: None,
                rpd: None,
                usd_per_day: Some("5.00".to_owned()),
            })
            .expect("issued");
        let specs = store.client_specs().expect("specs");
        match &specs[0].1 {
            ClientKeySpec::Policy {
                allow_models,
                limits,
                budget,
                ..
            } => {
                assert_eq!(allow_models, &["claude-*".to_owned()]);
                assert_eq!(limits.as_ref().and_then(|l| l.rpm), Some(60));
                assert_eq!(
                    budget
                        .as_ref()
                        .map(|b| b.usd_per_day.normalize().to_string()),
                    Some("5".to_owned())
                );
            }
            other => panic!("a restricted policy must stay a Policy spec, got {other:?}"),
        }
    }

    #[test]
    fn an_issued_token_verifies_with_one_argon2_run() {
        // The store only persists the verifier; the issued token must
        // verify against it exactly as ClientAuth would.
        let store = ManagedKeys::in_memory().expect("store");
        let issued = store.create(&KeyPolicyInput::default()).expect("issued");
        let conn = store
            .conn
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        let phc: String = conn
            .query_row(
                "SELECT phc FROM managed_keys WHERE id = ?1",
                rusqlite::params![issued.key_id],
                |row| row.get(0),
            )
            .expect("stored verifier");
        use argon2::PasswordVerifier;
        assert!(
            Argon2::default()
                .verify_password(issued.token.expose_secret().as_bytes(), phc.as_str())
                .is_ok(),
            "the once-shown token verifies against the stored verifier"
        );
    }

    #[test]
    fn issued_tokens_are_unique() {
        let store = ManagedKeys::in_memory().expect("store");
        let first = store.create(&KeyPolicyInput::default()).expect("first");
        let second = store.create(&KeyPolicyInput::default()).expect("second");
        assert_ne!(
            first.token.expose_secret(),
            second.token.expose_secret(),
            "two issuances never share a token"
        );
        assert_ne!(first.key_id, second.key_id);
    }
}
