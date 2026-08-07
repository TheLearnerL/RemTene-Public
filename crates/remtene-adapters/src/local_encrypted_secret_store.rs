//! Local authenticated-encryption storage for third-party API keys.
//!
//! The database contains only versioned AES-256-GCM ciphertext. Its
//! installation-scoped master key lives in a separate application-private
//! file. This is intentionally a lightweight local protection boundary: it
//! protects a database copied on its own, but not an attacker who can read the
//! complete application data directory or the running process.

use std::{
    collections::HashMap,
    fs::{self, OpenOptions},
    io::{self, Read, Write},
    path::{Path, PathBuf},
    sync::Mutex,
    time::Duration,
};

use aes_gcm::{
    Aes256Gcm, KeyInit, Nonce,
    aead::{Aead, Payload},
};
use remtene_application::ports::{
    PortError, PortFuture, SecretMaterialState, SecretStore, SecretValue,
};
use rusqlite::{Connection, OpenFlags, OptionalExtension, TransactionBehavior, params};
use zeroize::Zeroizing;

const DATABASE_FILE_NAME: &str = "secrets.sqlite3";
const MASTER_KEY_FILE_NAME: &str = "master-key.bin";
const DATABASE_SCHEMA_VERSION: i64 = 1;
const ENVELOPE_VERSION: i64 = 1;
const ALGORITHM: &str = "AES-256-GCM";
const NONCE_LENGTH: usize = 12;
const AUTHENTICATION_TAG_LENGTH: usize = 16;
const MASTER_KEY_LENGTH: usize = 32;
// ADR-0008: these values authenticate stores created before the RemTene
// rename. They are a persistent encrypted-storage ABI, not a display brand.
// Changing either value would make every existing third-party API key
// unreadable, so new and upgraded installations intentionally share it.
const LEGACY_MASTER_KEY_MAGIC: &[u8; 8] = b"BARDKEY\0";
const MASTER_KEY_FORMAT_VERSION: u32 = 1;
const MASTER_KEY_FILE_LENGTH: usize = LEGACY_MASTER_KEY_MAGIC.len() + 4 + MASTER_KEY_LENGTH;
const MAX_SECRET_ID_LENGTH: usize = 128;
const MAX_SECRET_VALUE_LENGTH: usize = 64 * 1024;
const MAX_CIPHERTEXT_LENGTH: usize = MAX_SECRET_VALUE_LENGTH + AUTHENTICATION_TAG_LENGTH;
const MAX_SECRET_RECORDS: usize = 256;
const LEGACY_AAD_PREFIX: &[u8] = b"io.github.TheLearnerL.bard/secret-store";

/// SQLite-backed API-key store described by ADR-0007.
pub struct LocalEncryptedSecretStore {
    state: Mutex<StoreState>,
}

struct StoreState {
    connection: Connection,
    master_key_path: PathBuf,
    master_key: MasterKeyState,
    cache: HashMap<String, CachedSecret>,
}

enum MasterKeyState {
    Missing,
    Available(Zeroizing<[u8; MASTER_KEY_LENGTH]>),
    Unavailable {
        error: PortError,
        recoverable_when_empty: bool,
    },
}

#[derive(Clone, Debug, Eq, PartialEq)]
struct EncryptedRecord {
    envelope_version: i64,
    algorithm: String,
    nonce: Vec<u8>,
    ciphertext: Vec<u8>,
}

struct CachedSecret {
    plaintext: Zeroizing<String>,
    record: EncryptedRecord,
}

impl LocalEncryptedSecretStore {
    /// Opens (or creates) a secret store rooted at `root`.
    ///
    /// Creating the database does not create a master key. The key is generated
    /// only by the first successful [`SecretStore::replace`] operation.
    pub fn new(root: impl Into<PathBuf>) -> Result<Self, PortError> {
        let root = root.into();
        prepare_private_directory(&root)?;

        let database_path = root.join(DATABASE_FILE_NAME);
        let master_key_path = root.join(MASTER_KEY_FILE_NAME);
        prepare_database_file(&database_path)?;

        let flags = OpenFlags::SQLITE_OPEN_READ_WRITE
            | OpenFlags::SQLITE_OPEN_CREATE
            | OpenFlags::SQLITE_OPEN_NO_MUTEX
            | OpenFlags::SQLITE_OPEN_NOFOLLOW
            | OpenFlags::SQLITE_OPEN_EXRESCODE;
        let connection = Connection::open_with_flags(&database_path, flags)
            .map_err(|_| secret_error("secret.database_open_failed"))?;
        connection
            .busy_timeout(Duration::from_secs(2))
            .map_err(|_| secret_error("secret.database_configuration_failed"))?;
        initialize_schema(&connection)?;
        harden_regular_file(&database_path, 0o600, "secret.database_file_invalid")?;

        let master_key = load_master_key_state(&master_key_path);

        Ok(Self {
            state: Mutex::new(StoreState {
                connection,
                master_key_path,
                master_key,
                cache: HashMap::new(),
            }),
        })
    }
}

impl SecretStore for LocalEncryptedSecretStore {
    fn is_configured(&self, secret_id: &str) -> PortFuture<'_, Result<bool, PortError>> {
        let secret_id = secret_id.to_owned();
        Box::pin(async move {
            let mut state = self.state.lock().map_err(|_| lock_error())?;
            if let Err(error) = validate_secret_id(&secret_id) {
                state.cache.clear();
                return Err(error);
            }

            match record_exists(&state.connection, &secret_id) {
                Ok(configured) => Ok(configured),
                Err(error) => {
                    state.cache.clear();
                    Err(error)
                }
            }
        })
    }

    fn inspect(&self, secret_id: &str) -> PortFuture<'_, Result<SecretMaterialState, PortError>> {
        let secret_id = secret_id.to_owned();
        Box::pin(async move {
            let mut state = self.state.lock().map_err(|_| lock_error())?;
            if let Err(error) = validate_secret_id(&secret_id) {
                state.cache.clear();
                return Err(error);
            }

            let StoreState {
                connection,
                master_key_path,
                master_key,
                cache,
            } = &mut *state;
            match inspect_secret_material(connection, master_key_path, master_key, &secret_id) {
                Ok(SecretMaterialState::NotConfigured) => {
                    cache.remove(&secret_id);
                    Ok(SecretMaterialState::NotConfigured)
                }
                Ok(SecretMaterialState::Configured) => Ok(SecretMaterialState::Configured),
                Ok(SecretMaterialState::RecoveryRequired) => {
                    cache.clear();
                    Ok(SecretMaterialState::RecoveryRequired)
                }
                Err(error) => {
                    cache.clear();
                    Err(error)
                }
            }
        })
    }

    fn inspect_store(&self) -> PortFuture<'_, Result<SecretMaterialState, PortError>> {
        Box::pin(async move {
            let mut state = self.state.lock().map_err(|_| lock_error())?;
            let StoreState {
                connection,
                master_key_path,
                master_key,
                cache,
            } = &mut *state;
            match inspect_store_material(connection, master_key_path, master_key) {
                Ok(SecretMaterialState::Configured) => Ok(SecretMaterialState::Configured),
                Ok(state) => {
                    cache.clear();
                    Ok(state)
                }
                Err(error) => {
                    cache.clear();
                    Err(error)
                }
            }
        })
    }

    fn read(&self, secret_id: &str) -> PortFuture<'_, Result<Option<SecretValue>, PortError>> {
        let secret_id = secret_id.to_owned();
        Box::pin(async move {
            let mut state = self.state.lock().map_err(|_| lock_error())?;
            if let Err(error) = validate_secret_id(&secret_id) {
                state.cache.clear();
                return Err(error);
            }

            let record = match read_record(&state.connection, &secret_id) {
                Ok(record) => record,
                Err(error) => {
                    state.cache.clear();
                    return Err(error);
                }
            };
            let Some(record) = record else {
                state.cache.remove(&secret_id);
                return Ok(None);
            };

            if let Err(error) = verify_current_master_key(&state.master_key_path, &state.master_key)
            {
                state.cache.clear();
                reload_master_key_state(&mut state);
                return Err(error);
            }

            if let Some(cached) = state.cache.get(&secret_id) {
                if cached.record == record {
                    return Ok(Some(SecretValue::new(cached.plaintext.as_str().to_owned())));
                }
                state.cache.remove(&secret_id);
            }

            let plaintext = match decrypt_record(&state.master_key, &secret_id, &record) {
                Ok(plaintext) => plaintext,
                Err(error) => {
                    state.cache.clear();
                    return Err(error);
                }
            };
            let exposed = plaintext.as_str().to_owned();
            state.cache.insert(
                secret_id,
                CachedSecret {
                    plaintext: Zeroizing::new(exposed.clone()),
                    record,
                },
            );
            Ok(Some(SecretValue::new(exposed)))
        })
    }

    fn replace(
        &self,
        secret_id: &str,
        value: SecretValue,
    ) -> PortFuture<'_, Result<(), PortError>> {
        let secret_id = secret_id.to_owned();
        Box::pin(async move {
            let mut state = self.state.lock().map_err(|_| lock_error())?;
            // Invalidate before every attempted mutation. A failed write must
            // never leave an older plaintext available to a later request.
            state.cache.clear();
            validate_secret_id(&secret_id)?;
            validate_secret_value(value.expose())?;

            let StoreState {
                connection,
                master_key_path,
                master_key,
                cache,
            } = &mut *state;
            // Acquire SQLite's cross-process writer reservation before
            // inspecting or recovering the key. This serializes every process
            // that follows the store contract and prevents two empty-database
            // recovery attempts from installing different keys.
            let transaction =
                rusqlite::Transaction::new_unchecked(connection, TransactionBehavior::Immediate)
                    .map_err(|_| secret_error("secret.database_write_failed"))?;

            ensure_master_key_for_write(&transaction, master_key_path, master_key, &secret_id)?;
            if let Err(error) = verify_current_master_key(master_key_path, master_key) {
                cache.clear();
                reload_master_key(master_key_path, master_key);
                return Err(error);
            }

            let mut nonce_bytes = [0_u8; NONCE_LENGTH];
            getrandom::fill(&mut nonce_bytes)
                .map_err(|_| secret_error("secret.random_unavailable"))?;
            let aad = build_aad(&secret_id);
            let ciphertext = match encrypt_value(master_key, &nonce_bytes, &aad, value.expose()) {
                Ok(ciphertext) => ciphertext,
                Err(error) => {
                    cache.clear();
                    return Err(error);
                }
            };

            transaction
                .execute(
                    "INSERT INTO secrets (
                         secret_id, envelope_version, algorithm, nonce, ciphertext
                     ) VALUES (?1, ?2, ?3, ?4, ?5)
                     ON CONFLICT(secret_id) DO UPDATE SET
                         envelope_version = excluded.envelope_version,
                         algorithm = excluded.algorithm,
                         nonce = excluded.nonce,
                         ciphertext = excluded.ciphertext",
                    params![
                        secret_id,
                        ENVELOPE_VERSION,
                        ALGORITHM,
                        nonce_bytes.as_slice(),
                        ciphertext
                    ],
                )
                .map_err(|_| secret_error("secret.database_write_failed"))?;
            transaction
                .commit()
                .map_err(|_| secret_error("secret.database_commit_failed"))?;

            cache.insert(
                secret_id,
                CachedSecret {
                    plaintext: Zeroizing::new(value.expose().to_owned()),
                    record: EncryptedRecord {
                        envelope_version: ENVELOPE_VERSION,
                        algorithm: ALGORITHM.to_owned(),
                        nonce: nonce_bytes.to_vec(),
                        ciphertext,
                    },
                },
            );
            Ok(())
        })
    }

    fn replace_namespace(
        &self,
        namespace: &str,
        secret_id: &str,
        value: SecretValue,
    ) -> PortFuture<'_, Result<(), PortError>> {
        let namespace = namespace.to_owned();
        let secret_id = secret_id.to_owned();
        Box::pin(async move {
            let mut state = self.state.lock().map_err(|_| lock_error())?;
            // Every attempted namespace mutation invalidates all exposed
            // plaintext immediately. Failure keeps the cache empty; success
            // repopulates only the newly committed target.
            state.cache.clear();
            validate_secret_id(&namespace)?;
            validate_secret_id(&secret_id)?;
            validate_secret_value(value.expose())?;
            if !secret_id.starts_with(&namespace) {
                return Err(secret_error("secret.namespace_mismatch"));
            }
            let namespace_pattern = format!("{}%", escape_like_pattern(&namespace));

            let StoreState {
                connection,
                master_key_path,
                master_key,
                cache,
            } = &mut *state;
            let transaction =
                rusqlite::Transaction::new_unchecked(connection, TransactionBehavior::Immediate)
                    .map_err(|_| secret_error("secret.database_write_failed"))?;

            match inspect_store_material(&transaction, master_key_path, master_key)? {
                SecretMaterialState::RecoveryRequired => {
                    return Err(secret_error("secret.reset_required"));
                }
                SecretMaterialState::NotConfigured | SecretMaterialState::Configured => {}
            }

            ensure_master_key_for_write(&transaction, master_key_path, master_key, &secret_id)?;
            if let Err(error) = verify_current_master_key(master_key_path, master_key) {
                reload_master_key(master_key_path, master_key);
                return Err(error);
            }

            let mut nonce_bytes = [0_u8; NONCE_LENGTH];
            getrandom::fill(&mut nonce_bytes)
                .map_err(|_| secret_error("secret.random_unavailable"))?;
            let aad = build_aad(&secret_id);
            let ciphertext = encrypt_value(master_key, &nonce_bytes, &aad, value.expose())?;

            transaction
                .execute(
                    "DELETE FROM secrets
                     WHERE secret_id LIKE ?1 ESCAPE '\\' AND secret_id <> ?2",
                    params![namespace_pattern, secret_id],
                )
                .map_err(|_| secret_error("secret.database_write_failed"))?;
            transaction
                .execute(
                    "INSERT INTO secrets (
                         secret_id, envelope_version, algorithm, nonce, ciphertext
                     ) VALUES (?1, ?2, ?3, ?4, ?5)
                     ON CONFLICT(secret_id) DO UPDATE SET
                         envelope_version = excluded.envelope_version,
                         algorithm = excluded.algorithm,
                         nonce = excluded.nonce,
                         ciphertext = excluded.ciphertext",
                    params![
                        secret_id,
                        ENVELOPE_VERSION,
                        ALGORITHM,
                        nonce_bytes.as_slice(),
                        ciphertext
                    ],
                )
                .map_err(|_| secret_error("secret.database_write_failed"))?;
            transaction
                .commit()
                .map_err(|_| secret_error("secret.database_commit_failed"))?;

            cache.insert(
                secret_id,
                CachedSecret {
                    plaintext: Zeroizing::new(value.expose().to_owned()),
                    record: EncryptedRecord {
                        envelope_version: ENVELOPE_VERSION,
                        algorithm: ALGORITHM.to_owned(),
                        nonce: nonce_bytes.to_vec(),
                        ciphertext,
                    },
                },
            );
            Ok(())
        })
    }

    fn reset_unrecoverable(&self, secret_id: &str) -> PortFuture<'_, Result<(), PortError>> {
        let secret_id = secret_id.to_owned();
        Box::pin(async move {
            let mut state = self.state.lock().map_err(|_| lock_error())?;
            // Reset is the widest destructive operation in this store. Clear
            // every cached plaintext before validation or lock acquisition so
            // no rejected or failed attempt can leave a stale value available.
            state.cache.clear();
            validate_secret_id(&secret_id)?;

            let StoreState {
                connection,
                master_key_path,
                master_key,
                ..
            } = &mut *state;
            // The IMMEDIATE transaction establishes the cross-connection
            // writer boundary before rechecking recovery eligibility. A stale
            // UI observation can therefore never clear material that became
            // healthy or was already removed before this commit point.
            let transaction =
                rusqlite::Transaction::new_unchecked(connection, TransactionBehavior::Immediate)
                    .map_err(|_| secret_error("secret.database_reset_failed"))?;

            match inspect_store_material(&transaction, master_key_path, master_key)? {
                SecretMaterialState::RecoveryRequired => {}
                SecretMaterialState::NotConfigured | SecretMaterialState::Configured => {
                    return Err(secret_error("secret.reset_not_required"));
                }
            }

            // A missing or invalid installation key affects the complete
            // encrypted store, not just the record used to prove recovery.
            // Clearing all rows prevents a later write from creating a
            // mixed-key database. The key is deliberately not generated here;
            // the next explicit `replace` owns that action.
            transaction
                .execute("DELETE FROM secrets", [])
                .map_err(|_| secret_error("secret.database_reset_failed"))?;
            transaction
                .commit()
                .map_err(|_| secret_error("secret.database_commit_failed"))?;
            Ok(())
        })
    }

    fn reset_unrecoverable_store(&self) -> PortFuture<'_, Result<(), PortError>> {
        Box::pin(async move {
            let mut state = self.state.lock().map_err(|_| lock_error())?;
            state.cache.clear();
            let StoreState {
                connection,
                master_key_path,
                master_key,
                ..
            } = &mut *state;
            let transaction =
                rusqlite::Transaction::new_unchecked(connection, TransactionBehavior::Immediate)
                    .map_err(|_| secret_error("secret.database_reset_failed"))?;
            if inspect_store_material(&transaction, master_key_path, master_key)?
                != SecretMaterialState::RecoveryRequired
            {
                return Err(secret_error("secret.reset_not_required"));
            }
            transaction
                .execute("DELETE FROM secrets", [])
                .map_err(|_| secret_error("secret.database_reset_failed"))?;
            transaction
                .commit()
                .map_err(|_| secret_error("secret.database_commit_failed"))?;
            Ok(())
        })
    }

    fn delete(&self, secret_id: &str) -> PortFuture<'_, Result<(), PortError>> {
        let secret_id = secret_id.to_owned();
        Box::pin(async move {
            let mut state = self.state.lock().map_err(|_| lock_error())?;
            state.cache.clear();
            validate_secret_id(&secret_id)?;

            let StoreState {
                connection,
                master_key_path,
                master_key,
                ..
            } = &mut *state;
            let transaction = connection
                .transaction_with_behavior(TransactionBehavior::Immediate)
                .map_err(|_| secret_error("secret.database_delete_failed"))?;
            if inspect_secret_material(&transaction, master_key_path, master_key, &secret_id)?
                == SecretMaterialState::RecoveryRequired
            {
                return Err(secret_error("secret.reset_required"));
            }
            transaction
                .execute("DELETE FROM secrets WHERE secret_id = ?1", [&secret_id])
                .map_err(|_| secret_error("secret.database_delete_failed"))?;
            transaction
                .commit()
                .map_err(|_| secret_error("secret.database_commit_failed"))?;
            Ok(())
        })
    }

    fn delete_namespace(&self, namespace: &str) -> PortFuture<'_, Result<u64, PortError>> {
        let namespace = namespace.to_owned();
        Box::pin(async move {
            let mut state = self.state.lock().map_err(|_| lock_error())?;
            state.cache.clear();
            validate_secret_id(&namespace)?;
            let pattern = format!("{}%", escape_like_pattern(&namespace));
            let StoreState {
                connection,
                master_key_path,
                master_key,
                ..
            } = &mut *state;
            let transaction = connection
                .transaction_with_behavior(TransactionBehavior::Immediate)
                .map_err(|_| secret_error("secret.database_delete_failed"))?;
            match inspect_store_material(&transaction, master_key_path, master_key)? {
                SecretMaterialState::RecoveryRequired => {
                    return Err(secret_error("secret.reset_required"));
                }
                SecretMaterialState::NotConfigured | SecretMaterialState::Configured => {}
            }
            let deleted = transaction
                .execute(
                    "DELETE FROM secrets WHERE secret_id LIKE ?1 ESCAPE '\\'",
                    [&pattern],
                )
                .map_err(|_| secret_error("secret.database_delete_failed"))?;
            transaction
                .commit()
                .map_err(|_| secret_error("secret.database_commit_failed"))?;
            u64::try_from(deleted).map_err(|_| secret_error("secret.database_delete_failed"))
        })
    }
}

/// Fail-closed store used when the encrypted store cannot initialize.
///
/// Composition can inject this adapter without turning an LLM storage failure
/// into an application, microphone, or local-ASR startup failure.
pub struct UnavailableSecretStore {
    error: PortError,
}

impl UnavailableSecretStore {
    #[must_use]
    pub fn new() -> Self {
        Self {
            error: secret_error("secret.store_unavailable"),
        }
    }

    #[must_use]
    pub fn from_error(error: PortError) -> Self {
        Self { error }
    }
}

impl Default for UnavailableSecretStore {
    fn default() -> Self {
        Self::new()
    }
}

impl SecretStore for UnavailableSecretStore {
    fn is_configured(&self, _secret_id: &str) -> PortFuture<'_, Result<bool, PortError>> {
        let error = self.error.clone();
        Box::pin(async move { Err(error) })
    }

    fn inspect(&self, _secret_id: &str) -> PortFuture<'_, Result<SecretMaterialState, PortError>> {
        let error = self.error.clone();
        Box::pin(async move { Err(error) })
    }

    fn inspect_store(&self) -> PortFuture<'_, Result<SecretMaterialState, PortError>> {
        let error = self.error.clone();
        Box::pin(async move { Err(error) })
    }

    fn read(&self, _secret_id: &str) -> PortFuture<'_, Result<Option<SecretValue>, PortError>> {
        let error = self.error.clone();
        Box::pin(async move { Err(error) })
    }

    fn replace(
        &self,
        _secret_id: &str,
        _value: SecretValue,
    ) -> PortFuture<'_, Result<(), PortError>> {
        let error = self.error.clone();
        Box::pin(async move { Err(error) })
    }

    fn replace_namespace(
        &self,
        _namespace: &str,
        _secret_id: &str,
        _value: SecretValue,
    ) -> PortFuture<'_, Result<(), PortError>> {
        let error = self.error.clone();
        Box::pin(async move { Err(error) })
    }

    fn delete(&self, _secret_id: &str) -> PortFuture<'_, Result<(), PortError>> {
        let error = self.error.clone();
        Box::pin(async move { Err(error) })
    }

    fn delete_namespace(&self, _namespace: &str) -> PortFuture<'_, Result<u64, PortError>> {
        let error = self.error.clone();
        Box::pin(async move { Err(error) })
    }

    fn reset_unrecoverable(&self, _secret_id: &str) -> PortFuture<'_, Result<(), PortError>> {
        let error = self.error.clone();
        Box::pin(async move { Err(error) })
    }

    fn reset_unrecoverable_store(&self) -> PortFuture<'_, Result<(), PortError>> {
        let error = self.error.clone();
        Box::pin(async move { Err(error) })
    }
}

fn initialize_schema(connection: &Connection) -> Result<(), PortError> {
    let version: i64 = connection
        .pragma_query_value(None, "user_version", |row| row.get(0))
        .map_err(|_| secret_error("secret.schema_read_failed"))?;

    match version {
        DATABASE_SCHEMA_VERSION => {
            connection
                .prepare(
                    "SELECT secret_id, envelope_version, algorithm, nonce, ciphertext
                     FROM secrets LIMIT 0",
                )
                .map_err(|_| secret_error("secret.schema_invalid"))?;
            Ok(())
        }
        0 => {
            let user_table_count: i64 = connection
                .query_row(
                    "SELECT COUNT(*) FROM sqlite_schema
                     WHERE type = 'table' AND name NOT LIKE 'sqlite_%'",
                    [],
                    |row| row.get(0),
                )
                .map_err(|_| secret_error("secret.schema_read_failed"))?;
            if user_table_count != 0 {
                return Err(secret_error("secret.schema_unsupported"));
            }

            let transaction = connection
                .unchecked_transaction()
                .map_err(|_| secret_error("secret.schema_initialize_failed"))?;
            transaction
                .execute_batch(
                    "CREATE TABLE secrets (
                        secret_id TEXT PRIMARY KEY NOT NULL,
                        envelope_version INTEGER NOT NULL,
                        algorithm TEXT NOT NULL,
                        nonce BLOB NOT NULL CHECK(length(nonce) = 12),
                        ciphertext BLOB NOT NULL
                            CHECK(length(ciphertext) BETWEEN 16 AND 65552),
                        CHECK(length(secret_id) BETWEEN 1 AND 128),
                        CHECK(length(algorithm) BETWEEN 1 AND 64)
                    ) STRICT;",
                )
                .map_err(|_| secret_error("secret.schema_initialize_failed"))?;
            transaction
                .pragma_update(None, "user_version", DATABASE_SCHEMA_VERSION)
                .map_err(|_| secret_error("secret.schema_initialize_failed"))?;
            transaction
                .commit()
                .map_err(|_| secret_error("secret.schema_initialize_failed"))
        }
        _ => Err(secret_error("secret.schema_unsupported")),
    }
}

fn record_exists(connection: &Connection, secret_id: &str) -> Result<bool, PortError> {
    connection
        .query_row(
            "SELECT 1 FROM secrets WHERE secret_id = ?1",
            [secret_id],
            |_| Ok(()),
        )
        .optional()
        .map(|value| value.is_some())
        .map_err(|_| secret_error("secret.database_read_failed"))
}

fn read_record(
    connection: &Connection,
    secret_id: &str,
) -> Result<Option<EncryptedRecord>, PortError> {
    let row = connection
        .query_row(
            "SELECT
                 envelope_version,
                 CASE WHEN length(algorithm) BETWEEN 1 AND ?2
                      THEN algorithm END,
                 CASE WHEN length(nonce) = ?3 THEN nonce END,
                 CASE WHEN length(ciphertext) BETWEEN ?4 AND ?5
                      THEN ciphertext END
             FROM secrets WHERE secret_id = ?1",
            params![
                secret_id,
                64_i64,
                NONCE_LENGTH as i64,
                AUTHENTICATION_TAG_LENGTH as i64,
                MAX_CIPHERTEXT_LENGTH as i64
            ],
            |row| {
                Ok((
                    row.get::<_, i64>(0)?,
                    row.get::<_, Option<String>>(1)?,
                    row.get::<_, Option<Vec<u8>>>(2)?,
                    row.get::<_, Option<Vec<u8>>>(3)?,
                ))
            },
        )
        .optional()
        .map_err(|_| secret_error("secret.database_read_failed"))?;

    match row {
        None => Ok(None),
        Some((envelope_version, Some(algorithm), Some(nonce), Some(ciphertext))) => {
            Ok(Some(EncryptedRecord {
                envelope_version,
                algorithm,
                nonce,
                ciphertext,
            }))
        }
        Some(_) => Err(secret_error("secret.envelope_unsupported")),
    }
}

/// Authenticates one record without returning or caching its plaintext.
///
/// A changed on-disk key is reloaded once before classification. This lets an
/// external repair restore a healthy store while still treating a genuinely
/// missing, corrupt, or mismatched key as deterministic data loss. Only errors
/// that can be repaired by clearing every encrypted row are mapped to
/// `RecoveryRequired`; infrastructure failures remain `PortError`.
fn inspect_secret_material(
    connection: &Connection,
    master_key_path: &Path,
    master_key: &mut MasterKeyState,
    secret_id: &str,
) -> Result<SecretMaterialState, PortError> {
    let record = match read_record(connection, secret_id) {
        Ok(record) => record,
        Err(error) => return classify_material_error(error),
    };
    let Some(record) = record else {
        return Ok(SecretMaterialState::NotConfigured);
    };

    if verify_current_master_key(master_key_path, master_key).is_err() {
        reload_master_key(master_key_path, master_key);
        if let Err(current_error) = verify_current_master_key(master_key_path, master_key) {
            return classify_material_error(current_error);
        }
    }

    match decrypt_record(master_key, secret_id, &record) {
        Ok(plaintext) => {
            drop(plaintext);
            Ok(SecretMaterialState::Configured)
        }
        Err(error) => classify_material_error(error),
    }
}

fn inspect_store_material(
    connection: &Connection,
    master_key_path: &Path,
    master_key: &mut MasterKeyState,
) -> Result<SecretMaterialState, PortError> {
    let mut statement = connection
        .prepare("SELECT secret_id FROM secrets ORDER BY secret_id")
        .map_err(|_| secret_error("secret.database_read_failed"))?;
    let rows = statement
        .query_map([], |row| row.get::<_, String>(0))
        .map_err(|_| secret_error("secret.database_read_failed"))?;
    let mut secret_ids = Vec::new();
    for row in rows {
        secret_ids.push(row.map_err(|_| secret_error("secret.database_read_failed"))?);
    }
    drop(statement);
    if secret_ids.is_empty() {
        return Ok(SecretMaterialState::NotConfigured);
    }

    let mut recovery_required = false;
    for secret_id in secret_ids {
        match inspect_secret_material(connection, master_key_path, master_key, &secret_id)? {
            SecretMaterialState::NotConfigured => {}
            SecretMaterialState::Configured => {}
            SecretMaterialState::RecoveryRequired => recovery_required = true,
        }
    }
    Ok(if recovery_required {
        SecretMaterialState::RecoveryRequired
    } else {
        SecretMaterialState::Configured
    })
}

fn classify_material_error(error: PortError) -> Result<SecretMaterialState, PortError> {
    match error.code.as_str() {
        // With existing ciphertext, these states cannot recover the old value.
        // Once all rows are explicitly removed, `replace` can create, repair,
        // or adopt a usable installation key without forming a mixed-key DB.
        "secret.master_key_missing"
        | "secret.master_key_format_invalid"
        | "secret.authentication_failed"
        | "secret.plaintext_invalid"
        | "secret.envelope_invalid" => Ok(SecretMaterialState::RecoveryRequired),
        // Path type, database, locking, and ordinary I/O failures are not proof
        // of permanent data loss and must never enable destructive reset.
        // Permission problems can be repaired, while unknown key/envelope
        // versions may belong to a newer application and must survive a
        // downgrade without being erased.
        _ => Err(error),
    }
}

fn escape_like_pattern(value: &str) -> String {
    value
        .replace('\\', "\\\\")
        .replace('%', "\\%")
        .replace('_', "\\_")
}

fn decrypt_record(
    master_key: &MasterKeyState,
    secret_id: &str,
    record: &EncryptedRecord,
) -> Result<Zeroizing<String>, PortError> {
    if record.envelope_version != ENVELOPE_VERSION
        || record.algorithm != ALGORITHM
        || record.nonce.len() != NONCE_LENGTH
        || !(AUTHENTICATION_TAG_LENGTH..=MAX_CIPHERTEXT_LENGTH).contains(&record.ciphertext.len())
    {
        return Err(secret_error("secret.envelope_unsupported"));
    }

    let key = available_key(master_key)?;
    let cipher = Aes256Gcm::new_from_slice(key.as_ref())
        .map_err(|_| secret_error("secret.master_key_invalid"))?;
    let nonce_bytes: [u8; NONCE_LENGTH] = record
        .nonce
        .as_slice()
        .try_into()
        .map_err(|_| secret_error("secret.envelope_invalid"))?;
    let nonce = Nonce::from(nonce_bytes);
    let aad = build_aad(secret_id);
    let plaintext = cipher
        .decrypt(
            &nonce,
            Payload {
                msg: record.ciphertext.as_slice(),
                aad: &aad,
            },
        )
        .map_err(|_| secret_error("secret.authentication_failed"))?;
    let plaintext = Zeroizing::new(plaintext);
    let text = std::str::from_utf8(plaintext.as_slice())
        .map_err(|_| secret_error("secret.plaintext_invalid"))?
        .to_owned();
    Ok(Zeroizing::new(text))
}

fn encrypt_value(
    master_key: &MasterKeyState,
    nonce_bytes: &[u8; NONCE_LENGTH],
    aad: &[u8],
    value: &str,
) -> Result<Vec<u8>, PortError> {
    let key = available_key(master_key)?;
    let cipher = Aes256Gcm::new_from_slice(key.as_ref())
        .map_err(|_| secret_error("secret.master_key_invalid"))?;
    let nonce = Nonce::from(*nonce_bytes);
    cipher
        .encrypt(
            &nonce,
            Payload {
                msg: value.as_bytes(),
                aad,
            },
        )
        .map_err(|_| secret_error("secret.encryption_failed"))
}

fn available_key(
    master_key: &MasterKeyState,
) -> Result<&Zeroizing<[u8; MASTER_KEY_LENGTH]>, PortError> {
    match master_key {
        MasterKeyState::Available(key) => Ok(key),
        MasterKeyState::Missing => Err(secret_error("secret.master_key_missing")),
        MasterKeyState::Unavailable { error, .. } => Err(error.clone()),
    }
}

fn verify_current_master_key(
    master_key_path: &Path,
    master_key: &MasterKeyState,
) -> Result<(), PortError> {
    let expected = available_key(master_key)?;
    let actual = read_master_key_file(master_key_path)?;
    if actual.as_ref() != expected.as_ref() {
        return Err(secret_error("secret.master_key_changed"));
    }
    Ok(())
}

fn ensure_master_key_for_write(
    connection: &Connection,
    master_key_path: &Path,
    master_key: &mut MasterKeyState,
    secret_id: &str,
) -> Result<(), PortError> {
    if matches!(master_key, MasterKeyState::Available(_)) {
        if verify_current_master_key(master_key_path, master_key).is_err() {
            reload_master_key(master_key_path, master_key);
        }
    } else {
        // A prior interrupted create, another process, or an external repair
        // may have installed a valid key since this store was opened.
        reload_master_key(master_key_path, master_key);
    }
    let record_count: i64 = connection
        .query_row("SELECT COUNT(*) FROM secrets", [], |row| row.get(0))
        .map_err(|_| secret_error("secret.database_read_failed"))?;
    if record_count != 0 {
        if !matches!(master_key, MasterKeyState::Available(_)) {
            return Err(secret_error("secret.master_key_unavailable"));
        }
        verify_master_key_against_database(connection, master_key)?;
        if record_count >= MAX_SECRET_RECORDS as i64 && !record_exists(connection, secret_id)? {
            return Err(secret_error("secret.record_limit_exceeded"));
        }
        return Ok(());
    }
    if matches!(master_key, MasterKeyState::Available(_)) {
        return Ok(());
    }

    let (can_recover, replace_existing) = match master_key {
        MasterKeyState::Missing => (true, false),
        MasterKeyState::Unavailable {
            recoverable_when_empty,
            ..
        } => (*recoverable_when_empty, true),
        MasterKeyState::Available(_) => unreachable!("handled above"),
    };
    if !can_recover {
        return Err(secret_error("secret.master_key_unavailable"));
    }

    let mut key = Zeroizing::new([0_u8; MASTER_KEY_LENGTH]);
    getrandom::fill(key.as_mut()).map_err(|_| secret_error("secret.random_unavailable"))?;
    if let Err(error) = write_master_key_file(master_key_path, &key, replace_existing) {
        // The file may already have been durably created before a later
        // permission/directory-sync step failed, or another process may have
        // won the first-create race. Reload so a retry does not remain stuck
        // in the stale `Missing` state.
        reload_master_key(master_key_path, master_key);
        return Err(error);
    }
    *master_key = MasterKeyState::Available(key);
    Ok(())
}

fn reload_master_key_state(state: &mut StoreState) {
    reload_master_key(&state.master_key_path, &mut state.master_key);
}

fn reload_master_key(path: &Path, master_key: &mut MasterKeyState) {
    *master_key = load_master_key_state(path);
}

fn verify_master_key_against_database(
    connection: &Connection,
    master_key: &MasterKeyState,
) -> Result<(), PortError> {
    let mut statement = connection
        .prepare(
            "SELECT
                 CASE WHEN length(secret_id) BETWEEN 1 AND ?1
                      THEN secret_id END,
                 envelope_version,
                 CASE WHEN length(algorithm) BETWEEN 1 AND ?2
                      THEN algorithm END,
                 CASE WHEN length(nonce) = ?3 THEN nonce END,
                 CASE WHEN length(ciphertext) BETWEEN ?4 AND ?5
                      THEN ciphertext END
             FROM secrets
             LIMIT ?6",
        )
        .map_err(|_| secret_error("secret.database_read_failed"))?;
    let mut rows = statement
        .query(params![
            MAX_SECRET_ID_LENGTH as i64,
            64_i64,
            NONCE_LENGTH as i64,
            AUTHENTICATION_TAG_LENGTH as i64,
            MAX_CIPHERTEXT_LENGTH as i64,
            (MAX_SECRET_RECORDS + 1) as i64
        ])
        .map_err(|_| secret_error("secret.database_read_failed"))?;
    let mut verified = 0_usize;

    while let Some(row) = rows
        .next()
        .map_err(|_| secret_error("secret.database_read_failed"))?
    {
        verified += 1;
        if verified > MAX_SECRET_RECORDS {
            return Err(secret_error("secret.record_limit_exceeded"));
        }

        let secret_id = row
            .get::<_, Option<String>>(0)
            .map_err(|_| secret_error("secret.database_read_failed"))?
            .ok_or_else(|| secret_error("secret.envelope_unsupported"))?;
        let record = EncryptedRecord {
            envelope_version: row
                .get(1)
                .map_err(|_| secret_error("secret.database_read_failed"))?,
            algorithm: row
                .get::<_, Option<String>>(2)
                .map_err(|_| secret_error("secret.database_read_failed"))?
                .ok_or_else(|| secret_error("secret.envelope_unsupported"))?,
            nonce: row
                .get::<_, Option<Vec<u8>>>(3)
                .map_err(|_| secret_error("secret.database_read_failed"))?
                .ok_or_else(|| secret_error("secret.envelope_unsupported"))?,
            ciphertext: row
                .get::<_, Option<Vec<u8>>>(4)
                .map_err(|_| secret_error("secret.database_read_failed"))?
                .ok_or_else(|| secret_error("secret.envelope_unsupported"))?,
        };
        validate_secret_id(&secret_id)?;
        // The plaintext is held only in a zeroizing temporary and dropped
        // immediately. Authenticating every small, bounded record prevents a
        // valid-looking but wrong replacement key from creating a mixed-key
        // database on the next write.
        let _plaintext = decrypt_record(master_key, &secret_id, &record)?;
    }

    Ok(())
}

fn build_aad(secret_id: &str) -> Vec<u8> {
    let mut aad =
        Vec::with_capacity(LEGACY_AAD_PREFIX.len() + ALGORITHM.len() + secret_id.len() + 24);
    aad.extend_from_slice(LEGACY_AAD_PREFIX);
    aad.push(0);
    aad.extend_from_slice(&DATABASE_SCHEMA_VERSION.to_be_bytes());
    aad.extend_from_slice(&ENVELOPE_VERSION.to_be_bytes());
    aad.extend_from_slice(&(ALGORITHM.len() as u32).to_be_bytes());
    aad.extend_from_slice(ALGORITHM.as_bytes());
    aad.extend_from_slice(&(secret_id.len() as u32).to_be_bytes());
    aad.extend_from_slice(secret_id.as_bytes());
    aad
}

fn validate_secret_id(secret_id: &str) -> Result<(), PortError> {
    if secret_id.is_empty()
        || secret_id.len() > MAX_SECRET_ID_LENGTH
        || secret_id
            .chars()
            .any(|character| character.is_control() || character.is_whitespace())
    {
        return Err(secret_error("secret.id_invalid"));
    }
    Ok(())
}

fn validate_secret_value(value: &str) -> Result<(), PortError> {
    if value.is_empty() || value.len() > MAX_SECRET_VALUE_LENGTH || value.contains('\0') {
        return Err(secret_error("secret.value_invalid"));
    }
    Ok(())
}

fn prepare_private_directory(root: &Path) -> Result<(), PortError> {
    match fs::symlink_metadata(root) {
        Ok(metadata) if metadata.file_type().is_symlink() || !metadata.is_dir() => {
            return Err(secret_error("secret.directory_invalid"));
        }
        Ok(_) => {}
        Err(error) if error.kind() == io::ErrorKind::NotFound => {
            fs::create_dir_all(root).map_err(|_| secret_error("secret.directory_create_failed"))?;
        }
        Err(_) => return Err(secret_error("secret.directory_open_failed")),
    }

    let metadata =
        fs::symlink_metadata(root).map_err(|_| secret_error("secret.directory_open_failed"))?;
    if metadata.file_type().is_symlink() || !metadata.is_dir() {
        return Err(secret_error("secret.directory_invalid"));
    }
    set_unix_mode(root, 0o700, "secret.directory_permission_failed")
}

fn prepare_database_file(path: &Path) -> Result<(), PortError> {
    match fs::symlink_metadata(path) {
        Ok(metadata) if metadata.file_type().is_symlink() || !metadata.is_file() => {
            return Err(secret_error("secret.database_file_invalid"));
        }
        Ok(_) => {}
        Err(error) if error.kind() == io::ErrorKind::NotFound => {
            create_private_file(path, &[], false, "secret.database_create_failed")?;
        }
        Err(_) => return Err(secret_error("secret.database_file_invalid")),
    }
    harden_regular_file(path, 0o600, "secret.database_file_invalid")
}

fn load_master_key_state(path: &Path) -> MasterKeyState {
    match fs::symlink_metadata(path) {
        Err(error) if error.kind() == io::ErrorKind::NotFound => MasterKeyState::Missing,
        Ok(metadata) if metadata.is_file() && !metadata.file_type().is_symlink() => {
            match read_master_key_file(path) {
                Ok(key) => MasterKeyState::Available(key),
                Err(error) => MasterKeyState::Unavailable {
                    error,
                    recoverable_when_empty: true,
                },
            }
        }
        Ok(_) => MasterKeyState::Unavailable {
            error: secret_error("secret.master_key_file_invalid"),
            recoverable_when_empty: false,
        },
        Err(_) => MasterKeyState::Unavailable {
            error: secret_error("secret.master_key_read_failed"),
            recoverable_when_empty: false,
        },
    }
}

fn read_master_key_file(path: &Path) -> Result<Zeroizing<[u8; MASTER_KEY_LENGTH]>, PortError> {
    let mut file = open_master_key_for_read(path)?;
    let metadata = file
        .metadata()
        .map_err(|_| secret_error("secret.master_key_read_failed"))?;
    if !metadata.is_file() || metadata.len() != MASTER_KEY_FILE_LENGTH as u64 {
        return Err(secret_error("secret.master_key_format_invalid"));
    }
    verify_unix_mode(&metadata, 0o600, "secret.master_key_permission_invalid")?;

    let mut bytes = Zeroizing::new([0_u8; MASTER_KEY_FILE_LENGTH]);
    file.read_exact(bytes.as_mut())
        .map_err(|_| secret_error("secret.master_key_read_failed"))?;
    let mut trailing = [0_u8; 1];
    if file
        .read(&mut trailing)
        .map_err(|_| secret_error("secret.master_key_read_failed"))?
        != 0
        || &bytes[..LEGACY_MASTER_KEY_MAGIC.len()] != LEGACY_MASTER_KEY_MAGIC
    {
        return Err(secret_error("secret.master_key_format_invalid"));
    }
    let version_offset = LEGACY_MASTER_KEY_MAGIC.len();
    let version = u32::from_be_bytes(
        bytes[version_offset..version_offset + 4]
            .try_into()
            .map_err(|_| secret_error("secret.master_key_format_invalid"))?,
    );
    if version != MASTER_KEY_FORMAT_VERSION {
        return Err(secret_error("secret.master_key_version_unsupported"));
    }
    let key_offset = version_offset + 4;
    let mut key = Zeroizing::new([0_u8; MASTER_KEY_LENGTH]);
    key.copy_from_slice(&bytes[key_offset..]);
    Ok(key)
}

#[cfg(unix)]
fn open_master_key_for_read(path: &Path) -> Result<fs::File, PortError> {
    use rustix::fs::{Mode, OFlags};

    rustix::fs::open(
        path,
        OFlags::RDONLY | OFlags::NOFOLLOW | OFlags::NONBLOCK | OFlags::CLOEXEC,
        Mode::empty(),
    )
    .map(fs::File::from)
    .map_err(|error| {
        let error = io::Error::from(error);
        if error.kind() == io::ErrorKind::NotFound {
            secret_error("secret.master_key_missing")
        } else {
            secret_error("secret.master_key_read_failed")
        }
    })
}

#[cfg(not(unix))]
fn open_master_key_for_read(path: &Path) -> Result<fs::File, PortError> {
    let metadata = fs::symlink_metadata(path).map_err(|error| {
        if error.kind() == io::ErrorKind::NotFound {
            secret_error("secret.master_key_missing")
        } else {
            secret_error("secret.master_key_read_failed")
        }
    })?;
    if metadata.file_type().is_symlink() || !metadata.is_file() {
        return Err(secret_error("secret.master_key_file_invalid"));
    }
    OpenOptions::new()
        .read(true)
        .open(path)
        .map_err(|_| secret_error("secret.master_key_read_failed"))
}

fn write_master_key_file(
    path: &Path,
    key: &[u8; MASTER_KEY_LENGTH],
    replace_existing: bool,
) -> Result<(), PortError> {
    let mut bytes = Zeroizing::new(Vec::with_capacity(MASTER_KEY_FILE_LENGTH));
    bytes.extend_from_slice(LEGACY_MASTER_KEY_MAGIC);
    bytes.extend_from_slice(&MASTER_KEY_FORMAT_VERSION.to_be_bytes());
    bytes.extend_from_slice(key);

    if !replace_existing {
        create_private_file(
            path,
            bytes.as_slice(),
            true,
            "secret.master_key_write_failed",
        )?;
        harden_regular_file(path, 0o600, "secret.master_key_permission_invalid")?;
        return sync_parent_directory(path, "secret.master_key_write_failed");
    }

    let existing =
        fs::symlink_metadata(path).map_err(|_| secret_error("secret.master_key_file_invalid"))?;
    if existing.file_type().is_symlink() || !existing.is_file() {
        return Err(secret_error("secret.master_key_file_invalid"));
    }

    let mut random_suffix = [0_u8; 8];
    getrandom::fill(&mut random_suffix).map_err(|_| secret_error("secret.random_unavailable"))?;
    let suffix = random_suffix
        .iter()
        .map(|byte| format!("{byte:02x}"))
        .collect::<String>();
    let temporary_path = path.with_file_name(format!(".{MASTER_KEY_FILE_NAME}.{suffix}.tmp"));
    create_private_file(
        &temporary_path,
        bytes.as_slice(),
        true,
        "secret.master_key_write_failed",
    )?;

    let replace_result = fs::rename(&temporary_path, path);
    if replace_result.is_err() {
        let can_replace_regular_file = fs::symlink_metadata(path)
            .is_ok_and(|metadata| metadata.is_file() && !metadata.file_type().is_symlink());
        if !can_replace_regular_file
            || fs::remove_file(path).is_err()
            || fs::rename(&temporary_path, path).is_err()
        {
            let _ = fs::remove_file(&temporary_path);
            return Err(secret_error("secret.master_key_write_failed"));
        }
    }
    harden_regular_file(path, 0o600, "secret.master_key_permission_invalid")?;
    sync_parent_directory(path, "secret.master_key_write_failed")
}

fn create_private_file(
    path: &Path,
    contents: &[u8],
    sync: bool,
    error_code: &str,
) -> Result<(), PortError> {
    let mut options = OpenOptions::new();
    options.write(true).create_new(true);
    configure_private_create_mode(&mut options);
    let mut file = options.open(path).map_err(|_| secret_error(error_code))?;
    let write_result = file
        .write_all(contents)
        .and_then(|()| if sync { file.sync_all() } else { Ok(()) });
    if write_result.is_err() {
        drop(file);
        let _ = fs::remove_file(path);
        return Err(secret_error(error_code));
    }
    Ok(())
}

fn harden_regular_file(path: &Path, mode: u32, error_code: &str) -> Result<(), PortError> {
    let metadata = fs::symlink_metadata(path).map_err(|_| secret_error(error_code))?;
    if metadata.file_type().is_symlink() || !metadata.is_file() {
        return Err(secret_error(error_code));
    }
    set_unix_mode(path, mode, error_code)
}

#[cfg(unix)]
fn sync_parent_directory(path: &Path, error_code: &str) -> Result<(), PortError> {
    let parent = path.parent().ok_or_else(|| secret_error(error_code))?;
    fs::File::open(parent)
        .and_then(|directory| directory.sync_all())
        .map_err(|_| secret_error(error_code))
}

#[cfg(not(unix))]
fn sync_parent_directory(_path: &Path, _error_code: &str) -> Result<(), PortError> {
    Ok(())
}

#[cfg(unix)]
fn configure_private_create_mode(options: &mut OpenOptions) {
    use std::os::unix::fs::OpenOptionsExt;
    options.mode(0o600);
}

#[cfg(not(unix))]
fn configure_private_create_mode(_options: &mut OpenOptions) {}

#[cfg(unix)]
fn set_unix_mode(path: &Path, mode: u32, error_code: &str) -> Result<(), PortError> {
    use std::os::unix::fs::PermissionsExt;
    let permissions = fs::Permissions::from_mode(mode);
    fs::set_permissions(path, permissions).map_err(|_| secret_error(error_code))
}

#[cfg(not(unix))]
fn set_unix_mode(_path: &Path, _mode: u32, _error_code: &str) -> Result<(), PortError> {
    Ok(())
}

#[cfg(unix)]
fn verify_unix_mode(
    metadata: &fs::Metadata,
    expected: u32,
    error_code: &str,
) -> Result<(), PortError> {
    use std::os::unix::fs::PermissionsExt;
    if metadata.permissions().mode() & 0o777 != expected {
        return Err(secret_error(error_code));
    }
    Ok(())
}

#[cfg(not(unix))]
fn verify_unix_mode(
    _metadata: &fs::Metadata,
    _expected: u32,
    _error_code: &str,
) -> Result<(), PortError> {
    Ok(())
}

fn secret_error(code: &str) -> PortError {
    PortError {
        code: code.to_owned(),
        safe_message_key: "errors.secret.unavailable".to_owned(),
        retryable: false,
    }
}

fn lock_error() -> PortError {
    secret_error("secret.lock_poisoned")
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::{
        sync::atomic::{AtomicU64, Ordering},
        time::{SystemTime, UNIX_EPOCH},
    };

    static NEXT_TEMP_ID: AtomicU64 = AtomicU64::new(1);

    struct TestRoot(PathBuf);

    impl TestRoot {
        fn new(tag: &str) -> Self {
            let nanos = SystemTime::now()
                .duration_since(UNIX_EPOCH)
                .expect("system time should follow the Unix epoch")
                .as_nanos();
            let sequence = NEXT_TEMP_ID.fetch_add(1, Ordering::Relaxed);
            let temporary_root = std::env::temp_dir()
                .canonicalize()
                .expect("temporary directory should be canonicalizable");
            Self(temporary_root.join(format!(
                "remtene-secret-store-{tag}-{}-{nanos}-{sequence}",
                std::process::id()
            )))
        }

        fn path(&self) -> &Path {
            &self.0
        }

        fn database_path(&self) -> PathBuf {
            self.0.join(DATABASE_FILE_NAME)
        }

        fn master_key_path(&self) -> PathBuf {
            self.0.join(MASTER_KEY_FILE_NAME)
        }
    }

    impl Drop for TestRoot {
        fn drop(&mut self) {
            let _ = fs::remove_dir_all(&self.0);
        }
    }

    fn replace(store: &dyn SecretStore, secret_id: &str, value: &str) -> Result<(), PortError> {
        futures::executor::block_on(store.replace(secret_id, SecretValue::new(value)))
    }

    fn replace_namespace(
        store: &dyn SecretStore,
        namespace: &str,
        secret_id: &str,
        value: &str,
    ) -> Result<(), PortError> {
        futures::executor::block_on(store.replace_namespace(
            namespace,
            secret_id,
            SecretValue::new(value),
        ))
    }

    fn read(store: &dyn SecretStore, secret_id: &str) -> Result<Option<String>, PortError> {
        futures::executor::block_on(store.read(secret_id))
            .map(|value| value.map(|value| value.expose().to_owned()))
    }

    fn configured(store: &dyn SecretStore, secret_id: &str) -> Result<bool, PortError> {
        futures::executor::block_on(store.is_configured(secret_id))
    }

    fn inspect(store: &dyn SecretStore, secret_id: &str) -> Result<SecretMaterialState, PortError> {
        futures::executor::block_on(store.inspect(secret_id))
    }

    fn inspect_store(store: &dyn SecretStore) -> Result<SecretMaterialState, PortError> {
        futures::executor::block_on(store.inspect_store())
    }

    fn delete(store: &dyn SecretStore, secret_id: &str) -> Result<(), PortError> {
        futures::executor::block_on(store.delete(secret_id))
    }

    fn delete_namespace(store: &dyn SecretStore, namespace: &str) -> Result<u64, PortError> {
        futures::executor::block_on(store.delete_namespace(namespace))
    }

    fn reset_unrecoverable(store: &dyn SecretStore, secret_id: &str) -> Result<(), PortError> {
        futures::executor::block_on(store.reset_unrecoverable(secret_id))
    }

    fn reset_unrecoverable_store(store: &dyn SecretStore) -> Result<(), PortError> {
        futures::executor::block_on(store.reset_unrecoverable_store())
    }

    fn record_count(root: &TestRoot) -> i64 {
        let connection = Connection::open(root.database_path()).unwrap();
        connection
            .query_row("SELECT COUNT(*) FROM secrets", [], |row| row.get(0))
            .unwrap()
    }

    fn raw_record(root: &TestRoot, secret_id: &str) -> EncryptedRecord {
        let connection = Connection::open(root.database_path()).unwrap();
        connection
            .query_row(
                "SELECT envelope_version, algorithm, nonce, ciphertext
                 FROM secrets WHERE secret_id = ?1",
                [secret_id],
                |row| {
                    Ok(EncryptedRecord {
                        envelope_version: row.get(0)?,
                        algorithm: row.get(1)?,
                        nonce: row.get(2)?,
                        ciphertext: row.get(3)?,
                    })
                },
            )
            .unwrap()
    }

    fn cache_snapshot(store: &LocalEncryptedSecretStore) -> Vec<(String, String, EncryptedRecord)> {
        let state = store.state.lock().unwrap();
        let mut snapshot = state
            .cache
            .iter()
            .map(|(secret_id, cached)| {
                (
                    secret_id.clone(),
                    cached.plaintext.as_str().to_owned(),
                    cached.record.clone(),
                )
            })
            .collect::<Vec<_>>();
        snapshot.sort_by(|left, right| left.0.cmp(&right.0));
        snapshot
    }

    fn assert_directory_does_not_contain(root: &Path, needle: &[u8]) {
        for entry in fs::read_dir(root).unwrap() {
            let entry = entry.unwrap();
            if entry.file_type().unwrap().is_file() {
                let bytes = fs::read(entry.path()).unwrap();
                assert!(
                    !bytes.windows(needle.len()).any(|window| window == needle),
                    "plaintext appeared in {}",
                    entry.file_name().to_string_lossy()
                );
            }
        }
    }

    struct DefaultNamespaceReplaceStore;

    impl SecretStore for DefaultNamespaceReplaceStore {
        fn is_configured(&self, _secret_id: &str) -> PortFuture<'_, Result<bool, PortError>> {
            Box::pin(async { Ok(false) })
        }

        fn read(&self, _secret_id: &str) -> PortFuture<'_, Result<Option<SecretValue>, PortError>> {
            Box::pin(async { Ok(None) })
        }

        fn replace(
            &self,
            _secret_id: &str,
            _value: SecretValue,
        ) -> PortFuture<'_, Result<(), PortError>> {
            Box::pin(async { Ok(()) })
        }

        fn delete(&self, _secret_id: &str) -> PortFuture<'_, Result<(), PortError>> {
            Box::pin(async { Ok(()) })
        }
    }

    #[test]
    fn fresh_store_round_trips_across_restart_without_plaintext_on_disk() {
        let root = TestRoot::new("roundtrip");
        let secret_id = "provider.primary";
        let plaintext = "test-api-key-unique-secret-material-0123456789";

        {
            let store = LocalEncryptedSecretStore::new(root.path()).unwrap();
            assert!(!root.master_key_path().exists());
            assert!(!configured(&store, secret_id).unwrap());
            assert_eq!(read(&store, secret_id).unwrap(), None);

            replace(&store, secret_id, plaintext).unwrap();
            assert!(configured(&store, secret_id).unwrap());
            assert_eq!(read(&store, secret_id).unwrap().as_deref(), Some(plaintext));
            assert!(root.master_key_path().is_file());
            assert_directory_does_not_contain(root.path(), plaintext.as_bytes());
        }

        let reopened = LocalEncryptedSecretStore::new(root.path()).unwrap();
        assert_eq!(
            read(&reopened, secret_id).unwrap().as_deref(),
            Some(plaintext)
        );
    }

    #[test]
    fn authenticated_inspection_distinguishes_absent_healthy_and_unrecoverable_material() {
        let root = TestRoot::new("inspect");
        let store = LocalEncryptedSecretStore::new(root.path()).unwrap();
        let secret_id = "provider.inspect";

        assert_eq!(
            inspect(&store, secret_id).unwrap(),
            SecretMaterialState::NotConfigured
        );
        replace(&store, secret_id, "authenticated-secret").unwrap();
        assert_eq!(
            inspect(&store, secret_id).unwrap(),
            SecretMaterialState::Configured
        );

        let connection = Connection::open(root.database_path()).unwrap();
        connection
            .execute(
                "UPDATE secrets
                 SET ciphertext = zeroblob(length(ciphertext))
                 WHERE secret_id = ?1",
                [secret_id],
            )
            .unwrap();

        // The legacy existence probe remains true, but authenticated state must
        // never promote corrupted material to configured readiness.
        assert!(configured(&store, secret_id).unwrap());
        assert_eq!(
            inspect(&store, secret_id).unwrap(),
            SecretMaterialState::RecoveryRequired
        );
        assert!(store.state.lock().unwrap().cache.is_empty());
    }

    #[test]
    fn store_inspection_detects_unrecoverable_orphan_when_current_secret_is_absent() {
        let root = TestRoot::new("inspect-orphan");
        let store = LocalEncryptedSecretStore::new(root.path()).unwrap();
        let orphan_id = "llm.openai_compatible.retired-endpoint";
        let current_id = "llm.openai_compatible.current-endpoint";

        replace(&store, orphan_id, "orphaned-secret").unwrap();
        let connection = Connection::open(root.database_path()).unwrap();
        connection
            .execute(
                "UPDATE secrets
                 SET ciphertext = zeroblob(length(ciphertext))
                 WHERE secret_id = ?1",
                [orphan_id],
            )
            .unwrap();

        assert_eq!(
            inspect(&store, current_id).unwrap(),
            SecretMaterialState::NotConfigured
        );
        assert_eq!(
            inspect_store(&store).unwrap(),
            SecretMaterialState::RecoveryRequired
        );
        assert!(store.state.lock().unwrap().cache.is_empty());
    }

    #[test]
    fn store_reset_recovers_unrecoverable_orphan_without_a_current_route_record() {
        let root = TestRoot::new("reset-orphan");
        let store = LocalEncryptedSecretStore::new(root.path()).unwrap();
        let orphan_id = "llm.openai_compatible.retired-endpoint";
        let current_id = "llm.openai_compatible.current-endpoint";

        replace(&store, orphan_id, "orphaned-secret").unwrap();
        let connection = Connection::open(root.database_path()).unwrap();
        connection
            .execute(
                "UPDATE secrets
                 SET ciphertext = zeroblob(length(ciphertext))
                 WHERE secret_id = ?1",
                [orphan_id],
            )
            .unwrap();
        drop(connection);

        assert_eq!(
            inspect(&store, current_id).unwrap(),
            SecretMaterialState::NotConfigured
        );
        reset_unrecoverable_store(&store).unwrap();
        assert_eq!(record_count(&root), 0);
        assert_eq!(
            inspect_store(&store).unwrap(),
            SecretMaterialState::NotConfigured
        );

        replace(&store, current_id, "replacement-secret").unwrap();
        assert_eq!(
            read(&store, current_id).unwrap().as_deref(),
            Some("replacement-secret")
        );
    }

    #[test]
    fn reset_refuses_absent_and_healthy_material_without_persistent_mutation() {
        let root = TestRoot::new("reset-refusal");
        let store = LocalEncryptedSecretStore::new(root.path()).unwrap();
        let secret_id = "provider.reset-refusal";

        assert_eq!(
            reset_unrecoverable(&store, secret_id).unwrap_err().code,
            "secret.reset_not_required"
        );
        assert_eq!(record_count(&root), 0);
        assert!(!root.master_key_path().exists());

        replace(&store, secret_id, "healthy-secret").unwrap();
        let record_before = raw_record(&root, secret_id);
        let key_before = fs::read(root.master_key_path()).unwrap();
        assert!(read(&store, secret_id).unwrap().is_some());

        assert_eq!(
            reset_unrecoverable(&store, secret_id).unwrap_err().code,
            "secret.reset_not_required"
        );
        assert_eq!(raw_record(&root, secret_id), record_before);
        assert_eq!(fs::read(root.master_key_path()).unwrap(), key_before);
        assert_eq!(
            inspect(&store, secret_id).unwrap(),
            SecretMaterialState::Configured
        );
    }

    #[test]
    fn reset_rechecks_and_clears_all_unrecoverable_records_without_generating_a_key() {
        let root = TestRoot::new("reset-all");
        let store = LocalEncryptedSecretStore::new(root.path()).unwrap();
        let primary_id = "provider.reset-primary";
        let secondary_id = "provider.reset-secondary";

        replace(&store, primary_id, "primary-secret").unwrap();
        replace(&store, secondary_id, "secondary-secret").unwrap();
        assert!(read(&store, primary_id).unwrap().is_some());
        assert!(read(&store, secondary_id).unwrap().is_some());

        fs::write(root.master_key_path(), b"corrupt-key-file").unwrap();
        let corrupt_key = fs::read(root.master_key_path()).unwrap();
        assert_eq!(
            inspect(&store, primary_id).unwrap(),
            SecretMaterialState::RecoveryRequired
        );

        reset_unrecoverable(&store, primary_id).unwrap();

        assert_eq!(record_count(&root), 0);
        assert!(store.state.lock().unwrap().cache.is_empty());
        assert_eq!(fs::read(root.master_key_path()).unwrap(), corrupt_key);
        assert_eq!(
            inspect(&store, primary_id).unwrap(),
            SecretMaterialState::NotConfigured
        );
        assert_eq!(
            inspect(&store, secondary_id).unwrap(),
            SecretMaterialState::NotConfigured
        );

        // Reset itself never creates a key. The next explicit save repairs the
        // now-empty store and becomes the only source of new secret material.
        replace(&store, primary_id, "replacement-secret").unwrap();
        assert_ne!(fs::read(root.master_key_path()).unwrap(), corrupt_key);
        assert_eq!(
            read(&store, primary_id).unwrap().as_deref(),
            Some("replacement-secret")
        );
    }

    #[test]
    fn reset_revalidates_inside_the_write_transaction_before_clearing_other_records() {
        let root = TestRoot::new("reset-revalidate");
        let store = LocalEncryptedSecretStore::new(root.path()).unwrap();
        let damaged_id = "provider.reset-damaged";
        let healthy_id = "provider.reset-healthy";

        replace(&store, damaged_id, "damaged-secret").unwrap();
        replace(&store, healthy_id, "healthy-secret").unwrap();
        let connection = Connection::open(root.database_path()).unwrap();
        connection
            .execute(
                "UPDATE secrets
                 SET ciphertext = zeroblob(length(ciphertext))
                 WHERE secret_id = ?1",
                [damaged_id],
            )
            .unwrap();
        assert_eq!(
            inspect(&store, damaged_id).unwrap(),
            SecretMaterialState::RecoveryRequired
        );

        // Simulate another actor removing the damaged material after the UI
        // observed RecoveryRequired but before the destructive command.
        connection
            .execute("DELETE FROM secrets WHERE secret_id = ?1", [damaged_id])
            .unwrap();
        drop(connection);

        assert_eq!(
            reset_unrecoverable(&store, damaged_id).unwrap_err().code,
            "secret.reset_not_required"
        );
        assert_eq!(record_count(&root), 1);
        assert_eq!(
            read(&store, healthy_id).unwrap().as_deref(),
            Some("healthy-secret")
        );
    }

    #[test]
    fn repeated_plaintext_uses_a_fresh_nonce_and_ciphertext() {
        let root = TestRoot::new("fresh-nonce");
        let store = LocalEncryptedSecretStore::new(root.path()).unwrap();
        let secret_id = "provider.same-value";
        let plaintext = "same-secret-value";

        replace(&store, secret_id, plaintext).unwrap();
        let first = raw_record(&root, secret_id);
        replace(&store, secret_id, plaintext).unwrap();
        let second = raw_record(&root, secret_id);

        assert_ne!(first.nonce, second.nonce);
        assert_ne!(first.ciphertext, second.ciphertext);
        assert_eq!(read(&store, secret_id).unwrap().as_deref(), Some(plaintext));
    }

    #[test]
    fn replace_and_idempotent_delete_invalidate_cached_plaintext() {
        let root = TestRoot::new("cache-invalidation");
        let store = LocalEncryptedSecretStore::new(root.path()).unwrap();
        let secret_id = "provider.cache";

        replace(&store, secret_id, "old-value").unwrap();
        assert_eq!(
            read(&store, secret_id).unwrap().as_deref(),
            Some("old-value")
        );
        replace(&store, secret_id, "new-value").unwrap();
        assert_eq!(
            read(&store, secret_id).unwrap().as_deref(),
            Some("new-value")
        );

        delete(&store, secret_id).unwrap();
        delete(&store, secret_id).unwrap();
        assert_eq!(read(&store, secret_id).unwrap(), None);
        assert!(!configured(&store, secret_id).unwrap());
    }

    #[test]
    fn namespace_delete_removes_all_llm_records_and_preserves_other_namespaces() {
        let root = TestRoot::new("namespace-delete");
        let store = LocalEncryptedSecretStore::new(root.path()).unwrap();
        let first_llm_id = "llm.openai_compatible.endpoint-one";
        let second_llm_id = "llm.openai_compatible.endpoint-two";
        let unrelated_id = "integration.unrelated";

        replace(&store, first_llm_id, "first-secret").unwrap();
        replace(&store, second_llm_id, "second-secret").unwrap();
        replace(&store, unrelated_id, "unrelated-secret").unwrap();

        assert_eq!(
            delete_namespace(&store, "llm.openai_compatible.").unwrap(),
            2
        );
        assert_eq!(read(&store, first_llm_id).unwrap(), None);
        assert_eq!(read(&store, second_llm_id).unwrap(), None);
        assert_eq!(
            read(&store, unrelated_id).unwrap().as_deref(),
            Some("unrelated-secret")
        );
        assert_eq!(record_count(&root), 1);
    }

    #[test]
    fn namespace_replace_atomically_converges_old_records_and_preserves_other_namespaces() {
        let root = TestRoot::new("namespace-replace-converge");
        let store = LocalEncryptedSecretStore::new(root.path()).unwrap();
        let namespace = "llm.openai_compatible.";
        let first_old_id = "llm.openai_compatible.endpoint-one";
        let second_old_id = "llm.openai_compatible.endpoint-two";
        let new_id = "llm.openai_compatible.endpoint-three";
        let unrelated_id = "integration.unrelated";

        replace(&store, first_old_id, "first-old-secret").unwrap();
        replace(&store, second_old_id, "second-old-secret").unwrap();
        replace(&store, unrelated_id, "unrelated-secret").unwrap();

        replace_namespace(&store, namespace, new_id, "new-secret").unwrap();

        assert_eq!(record_count(&root), 2);
        assert!(!configured(&store, first_old_id).unwrap());
        assert!(!configured(&store, second_old_id).unwrap());
        let cache_after_commit = cache_snapshot(&store);
        assert_eq!(cache_after_commit.len(), 1);
        assert!(cache_after_commit.iter().any(|entry| {
            entry.0 == new_id && entry.1 == "new-secret" && entry.2 == raw_record(&root, new_id)
        }));
        assert_eq!(read(&store, new_id).unwrap().as_deref(), Some("new-secret"));
        assert_eq!(
            read(&store, unrelated_id).unwrap().as_deref(),
            Some("unrelated-secret")
        );
    }

    #[test]
    fn namespace_replace_updates_the_same_endpoint_with_fresh_ciphertext() {
        let root = TestRoot::new("namespace-replace-same-endpoint");
        let store = LocalEncryptedSecretStore::new(root.path()).unwrap();
        let namespace = "llm.openai_compatible.";
        let secret_id = "llm.openai_compatible.same-endpoint";
        let unrelated_id = "integration.unrelated";

        replace(&store, secret_id, "old-secret").unwrap();
        replace(&store, unrelated_id, "unrelated-secret").unwrap();
        let old_record = raw_record(&root, secret_id);

        replace_namespace(&store, namespace, secret_id, "new-secret").unwrap();

        let new_record = raw_record(&root, secret_id);
        assert_ne!(new_record, old_record);
        assert_eq!(record_count(&root), 2);
        let cache_after_commit = cache_snapshot(&store);
        assert_eq!(cache_after_commit.len(), 1);
        assert_eq!(cache_after_commit[0].0, secret_id);
        assert_eq!(cache_after_commit[0].1, "new-secret");
        assert_eq!(cache_after_commit[0].2, new_record);
        assert_eq!(
            read(&store, secret_id).unwrap().as_deref(),
            Some("new-secret")
        );
        assert_eq!(
            read(&store, unrelated_id).unwrap().as_deref(),
            Some("unrelated-secret")
        );
    }

    #[test]
    fn namespace_replace_failures_preserve_records_and_clear_plaintext_cache() {
        let root = TestRoot::new("namespace-replace-preflight-failure");
        let store = LocalEncryptedSecretStore::new(root.path()).unwrap();
        let namespace = "llm.openai_compatible.";
        let first_id = "llm.openai_compatible.endpoint-one";
        let second_id = "llm.openai_compatible.endpoint-two";

        replace(&store, first_id, "first-secret").unwrap();
        replace(&store, second_id, "second-secret").unwrap();
        let first_before = raw_record(&root, first_id);
        let second_before = raw_record(&root, second_id);

        assert_eq!(
            replace_namespace(&store, "bad namespace", first_id, "replacement")
                .unwrap_err()
                .code,
            "secret.id_invalid"
        );
        assert_eq!(raw_record(&root, first_id), first_before);
        assert_eq!(raw_record(&root, second_id), second_before);
        assert!(cache_snapshot(&store).is_empty());

        assert!(read(&store, first_id).unwrap().is_some());
        assert!(read(&store, second_id).unwrap().is_some());
        assert_eq!(
            replace_namespace(
                &store,
                namespace,
                "integration.outside-namespace",
                "replacement"
            )
            .unwrap_err()
            .code,
            "secret.namespace_mismatch"
        );
        assert_eq!(raw_record(&root, first_id), first_before);
        assert_eq!(raw_record(&root, second_id), second_before);
        assert!(cache_snapshot(&store).is_empty());

        assert!(read(&store, first_id).unwrap().is_some());
        assert!(read(&store, second_id).unwrap().is_some());
        let connection = Connection::open(root.database_path()).unwrap();
        connection
            .execute(
                "UPDATE secrets
                 SET ciphertext = zeroblob(length(ciphertext))
                 WHERE secret_id = ?1",
                [second_id],
            )
            .unwrap();
        let damaged_second = raw_record(&root, second_id);
        assert!(!cache_snapshot(&store).is_empty());

        assert_eq!(
            replace_namespace(&store, namespace, first_id, "replacement")
                .unwrap_err()
                .code,
            "secret.reset_required"
        );
        assert_eq!(raw_record(&root, first_id), first_before);
        assert_eq!(raw_record(&root, second_id), damaged_second);
        assert!(cache_snapshot(&store).is_empty());
        assert_eq!(record_count(&root), 2);
    }

    #[test]
    fn namespace_replace_rolls_back_deletes_when_the_upsert_fails() {
        let root = TestRoot::new("namespace-replace-write-failure");
        let store = LocalEncryptedSecretStore::new(root.path()).unwrap();
        let namespace = "llm.openai_compatible.";
        let first_id = "llm.openai_compatible.endpoint-one";
        let second_id = "llm.openai_compatible.endpoint-two";
        let new_id = "llm.openai_compatible.endpoint-three";
        let unrelated_id = "integration.unrelated";

        replace(&store, first_id, "first-secret").unwrap();
        replace(&store, second_id, "second-secret").unwrap();
        replace(&store, unrelated_id, "unrelated-secret").unwrap();
        let first_before = raw_record(&root, first_id);
        let second_before = raw_record(&root, second_id);
        let unrelated_before = raw_record(&root, unrelated_id);

        let connection = Connection::open(root.database_path()).unwrap();
        connection
            .execute_batch(
                "CREATE TRIGGER reject_namespace_replace
                 BEFORE INSERT ON secrets
                 BEGIN
                     SELECT RAISE(FAIL, 'injected namespace replace failure');
                 END;",
            )
            .unwrap();

        assert_eq!(
            replace_namespace(&store, namespace, new_id, "must-not-commit")
                .unwrap_err()
                .code,
            "secret.database_write_failed"
        );
        connection
            .execute_batch("DROP TRIGGER reject_namespace_replace;")
            .unwrap();

        assert_eq!(record_count(&root), 3);
        assert_eq!(raw_record(&root, first_id), first_before);
        assert_eq!(raw_record(&root, second_id), second_before);
        assert_eq!(raw_record(&root, unrelated_id), unrelated_before);
        assert!(!configured(&store, new_id).unwrap());
        assert!(cache_snapshot(&store).is_empty());
        assert_directory_does_not_contain(root.path(), b"must-not-commit");
    }

    #[test]
    fn ciphertext_tampering_is_detected_even_after_a_cache_hit() {
        let root = TestRoot::new("tamper");
        let store = LocalEncryptedSecretStore::new(root.path()).unwrap();
        let secret_id = "provider.tamper";
        replace(&store, secret_id, "tamper-evident-value").unwrap();
        assert!(read(&store, secret_id).unwrap().is_some());

        let connection = Connection::open(root.database_path()).unwrap();
        connection
            .execute(
                "UPDATE secrets
                 SET ciphertext = zeroblob(length(ciphertext))
                 WHERE secret_id = ?1",
                [secret_id],
            )
            .unwrap();

        let error = read(&store, secret_id).unwrap_err();
        assert_eq!(error.code, "secret.authentication_failed");
    }

    #[test]
    fn aad_prevents_moving_an_envelope_between_secret_ids() {
        let root = TestRoot::new("aad");
        let store = LocalEncryptedSecretStore::new(root.path()).unwrap();
        replace(&store, "provider.first", "first-secret").unwrap();
        replace(&store, "provider.second", "second-secret").unwrap();
        let second = raw_record(&root, "provider.second");

        let connection = Connection::open(root.database_path()).unwrap();
        connection
            .execute(
                "UPDATE secrets SET envelope_version = ?1, algorithm = ?2,
                 nonce = ?3, ciphertext = ?4 WHERE secret_id = ?5",
                params![
                    second.envelope_version,
                    second.algorithm,
                    second.nonce,
                    second.ciphertext,
                    "provider.first"
                ],
            )
            .unwrap();

        let error = read(&store, "provider.first").unwrap_err();
        assert_eq!(error.code, "secret.authentication_failed");
    }

    #[test]
    fn deleting_a_missing_master_key_from_a_cached_store_fails_closed_then_recovers() {
        let root = TestRoot::new("missing-key");
        let store = LocalEncryptedSecretStore::new(root.path()).unwrap();
        let secret_id = "provider.missing-key";
        replace(&store, secret_id, "old-secret").unwrap();
        assert!(read(&store, secret_id).unwrap().is_some());

        fs::remove_file(root.master_key_path()).unwrap();
        let error = read(&store, secret_id).unwrap_err();
        assert_eq!(error.code, "secret.master_key_missing");

        assert_eq!(
            delete(&store, secret_id).unwrap_err().code,
            "secret.reset_required"
        );
        reset_unrecoverable(&store, secret_id).unwrap();
        replace(&store, secret_id, "replacement-secret").unwrap();
        assert_eq!(
            read(&store, secret_id).unwrap().as_deref(),
            Some("replacement-secret")
        );
    }

    #[test]
    fn replacing_the_master_key_invalidates_a_cached_secret() {
        let root = TestRoot::new("changed-key");
        let store = LocalEncryptedSecretStore::new(root.path()).unwrap();
        let secret_id = "provider.changed-key";
        replace(&store, secret_id, "cached-secret").unwrap();
        assert!(read(&store, secret_id).unwrap().is_some());

        let key_path = root.master_key_path();
        let mut key_bytes = fs::read(&key_path).unwrap();
        let last = key_bytes.last_mut().unwrap();
        *last ^= 0x5a;
        fs::write(&key_path, key_bytes).unwrap();

        let error = read(&store, secret_id).unwrap_err();
        assert_eq!(error.code, "secret.master_key_changed");
        assert_eq!(
            replace(&store, secret_id, "must-not-use-replacement-key")
                .unwrap_err()
                .code,
            "secret.authentication_failed"
        );
        drop(store);

        let reopened = LocalEncryptedSecretStore::new(root.path()).unwrap();
        let error = read(&reopened, secret_id).unwrap_err();
        assert_eq!(error.code, "secret.authentication_failed");
    }

    #[test]
    fn validly_formatted_wrong_key_cannot_create_a_mixed_key_database_after_restart() {
        let root = TestRoot::new("wrong-key-write");
        let existing_id = "provider.existing";
        {
            let store = LocalEncryptedSecretStore::new(root.path()).unwrap();
            replace(&store, existing_id, "original-secret").unwrap();
        }
        let original_record = raw_record(&root, existing_id);

        let key_path = root.master_key_path();
        let mut replacement_key = fs::read(&key_path).unwrap();
        for byte in &mut replacement_key[LEGACY_MASTER_KEY_MAGIC.len() + 4..] {
            *byte ^= 0xa5;
        }
        fs::write(&key_path, replacement_key).unwrap();

        let reopened = LocalEncryptedSecretStore::new(root.path()).unwrap();
        assert_eq!(
            replace(&reopened, existing_id, "must-not-replace")
                .unwrap_err()
                .code,
            "secret.authentication_failed"
        );
        assert_eq!(
            replace(&reopened, "provider.new", "must-not-add")
                .unwrap_err()
                .code,
            "secret.authentication_failed"
        );
        assert_eq!(raw_record(&root, existing_id), original_record);
        assert!(!configured(&reopened, "provider.new").unwrap());
    }

    #[test]
    fn another_store_winning_first_key_creation_is_adopted_safely() {
        let root = TestRoot::new("first-create-race");
        let first = LocalEncryptedSecretStore::new(root.path()).unwrap();
        let second = LocalEncryptedSecretStore::new(root.path()).unwrap();

        replace(&second, "provider.second", "second-secret").unwrap();
        replace(&first, "provider.first", "first-secret").unwrap();

        assert_eq!(
            read(&first, "provider.second").unwrap().as_deref(),
            Some("second-secret")
        );
        assert_eq!(
            read(&second, "provider.first").unwrap().as_deref(),
            Some("first-secret")
        );
    }

    #[test]
    fn recovery_waits_for_the_cross_connection_write_lock_before_replacing_the_key() {
        use std::{sync::mpsc, thread};

        let root = TestRoot::new("recovery-write-lock");
        {
            let store = LocalEncryptedSecretStore::new(root.path()).unwrap();
            replace(&store, "provider.seed", "seed-secret").unwrap();
            delete(&store, "provider.seed").unwrap();
        }
        fs::write(root.master_key_path(), b"corrupt-key-file").unwrap();
        let corrupt_bytes = fs::read(root.master_key_path()).unwrap();
        let recovering = LocalEncryptedSecretStore::new(root.path()).unwrap();

        let lock_connection = Connection::open(root.database_path()).unwrap();
        let lock =
            rusqlite::Transaction::new_unchecked(&lock_connection, TransactionBehavior::Immediate)
                .unwrap();
        let (started_tx, started_rx) = mpsc::channel();
        let worker = thread::spawn(move || {
            started_tx.send(()).unwrap();
            replace(&recovering, "provider.recovered", "recovered-secret")
        });
        started_rx.recv().unwrap();
        thread::sleep(Duration::from_millis(75));

        // A writer waiting on SQLite must not mutate the key file yet.
        assert_eq!(fs::read(root.master_key_path()).unwrap(), corrupt_bytes);
        lock.commit().unwrap();
        worker.join().unwrap().unwrap();

        let reopened = LocalEncryptedSecretStore::new(root.path()).unwrap();
        assert_eq!(
            read(&reopened, "provider.recovered").unwrap().as_deref(),
            Some("recovered-secret")
        );
    }

    #[test]
    fn corrupt_master_key_requires_deleting_unrecoverable_ciphertext_before_reentry() {
        let root = TestRoot::new("corrupt-key");
        let secret_id = "provider.corrupt-key";
        {
            let store = LocalEncryptedSecretStore::new(root.path()).unwrap();
            replace(&store, secret_id, "unrecoverable-secret").unwrap();
        }
        fs::write(root.master_key_path(), b"corrupt-key-file").unwrap();

        let reopened = LocalEncryptedSecretStore::new(root.path()).unwrap();
        assert!(configured(&reopened, secret_id).unwrap());
        assert_eq!(
            read(&reopened, secret_id).unwrap_err().code,
            "secret.master_key_format_invalid"
        );
        assert_eq!(
            replace(&reopened, secret_id, "must-not-overwrite")
                .unwrap_err()
                .code,
            "secret.master_key_unavailable"
        );

        assert_eq!(
            delete(&reopened, secret_id).unwrap_err().code,
            "secret.reset_required"
        );
        reset_unrecoverable(&reopened, secret_id).unwrap();
        replace(&reopened, secret_id, "reentered-secret").unwrap();
        assert_eq!(
            read(&reopened, secret_id).unwrap().as_deref(),
            Some("reentered-secret")
        );
    }

    #[test]
    fn unknown_master_key_format_version_fails_closed() {
        let root = TestRoot::new("key-version");
        let secret_id = "provider.key-version";
        {
            let store = LocalEncryptedSecretStore::new(root.path()).unwrap();
            replace(&store, secret_id, "version-protected-secret").unwrap();
        }
        let record_before = raw_record(&root, secret_id);

        let key_path = root.master_key_path();
        let mut bytes = fs::read(&key_path).unwrap();
        let version_offset = LEGACY_MASTER_KEY_MAGIC.len();
        bytes[version_offset..version_offset + 4].copy_from_slice(&99_u32.to_be_bytes());
        fs::write(&key_path, bytes).unwrap();

        let reopened = LocalEncryptedSecretStore::new(root.path()).unwrap();
        assert!(configured(&reopened, secret_id).unwrap());
        assert_eq!(
            read(&reopened, secret_id).unwrap_err().code,
            "secret.master_key_version_unsupported"
        );
        assert_eq!(
            inspect_store(&reopened).unwrap_err().code,
            "secret.master_key_version_unsupported"
        );
        assert_eq!(
            reset_unrecoverable_store(&reopened).unwrap_err().code,
            "secret.master_key_version_unsupported"
        );
        assert_eq!(raw_record(&root, secret_id), record_before);
        assert_eq!(record_count(&root), 1);
    }

    #[test]
    fn oversized_master_key_file_is_rejected_before_parsing() {
        let root = TestRoot::new("oversized-key");
        let secret_id = "provider.oversized-key";
        {
            let store = LocalEncryptedSecretStore::new(root.path()).unwrap();
            replace(&store, secret_id, "unrecoverable-secret").unwrap();
        }
        fs::write(root.master_key_path(), vec![0_u8; 1024 * 1024]).unwrap();
        #[cfg(unix)]
        set_unix_mode(
            &root.master_key_path(),
            0o600,
            "secret.master_key_permission_invalid",
        )
        .unwrap();

        let store = LocalEncryptedSecretStore::new(root.path()).unwrap();
        assert_eq!(
            read(&store, secret_id).unwrap_err().code,
            "secret.master_key_format_invalid"
        );
        assert_eq!(
            replace(&store, secret_id, "must-not-overwrite")
                .unwrap_err()
                .code,
            "secret.master_key_unavailable"
        );
    }

    #[test]
    fn oversized_database_envelope_is_rejected_before_materialization() {
        let root = TestRoot::new("oversized-envelope");
        let store = LocalEncryptedSecretStore::new(root.path()).unwrap();
        let secret_id = "provider.oversized-envelope";
        replace(&store, secret_id, "bounded-secret").unwrap();

        let connection = Connection::open(root.database_path()).unwrap();
        connection
            .execute_batch("PRAGMA ignore_check_constraints = ON;")
            .unwrap();
        connection
            .execute(
                "UPDATE secrets SET ciphertext = zeroblob(?1)
                 WHERE secret_id = ?2",
                params![(MAX_CIPHERTEXT_LENGTH + 1) as i64, secret_id],
            )
            .unwrap();

        assert_eq!(
            read(&store, secret_id).unwrap_err().code,
            "secret.envelope_unsupported"
        );
        assert_eq!(
            replace(&store, secret_id, "must-not-overwrite")
                .unwrap_err()
                .code,
            "secret.envelope_unsupported"
        );
    }

    #[test]
    fn database_write_failure_keeps_the_previous_ciphertext_and_clears_cache() {
        let root = TestRoot::new("write-failure");
        let store = LocalEncryptedSecretStore::new(root.path()).unwrap();
        let secret_id = "provider.write-failure";
        replace(&store, secret_id, "previous-secret").unwrap();
        assert!(read(&store, secret_id).unwrap().is_some());

        let connection = Connection::open(root.database_path()).unwrap();
        connection
            .execute_batch(
                "CREATE TRIGGER reject_secret_writes
                 BEFORE INSERT ON secrets
                 BEGIN
                     SELECT RAISE(FAIL, 'injected write failure');
                 END;",
            )
            .unwrap();

        assert_eq!(
            replace(&store, secret_id, "must-not-commit")
                .unwrap_err()
                .code,
            "secret.database_write_failed"
        );
        connection
            .execute_batch("DROP TRIGGER reject_secret_writes;")
            .unwrap();

        assert_eq!(
            read(&store, secret_id).unwrap().as_deref(),
            Some("previous-secret")
        );
        assert_directory_does_not_contain(root.path(), b"must-not-commit");
    }

    #[test]
    fn unknown_envelope_and_database_versions_fail_closed() {
        let root = TestRoot::new("versions");
        {
            let store = LocalEncryptedSecretStore::new(root.path()).unwrap();
            replace(&store, "provider.version", "versioned-secret").unwrap();
            let connection = Connection::open(root.database_path()).unwrap();
            connection
                .execute(
                    "UPDATE secrets SET envelope_version = 99
                     WHERE secret_id = 'provider.version'",
                    [],
                )
                .unwrap();
            let record_before = raw_record(&root, "provider.version");
            let error = read(&store, "provider.version").unwrap_err();
            assert_eq!(error.code, "secret.envelope_unsupported");
            assert_eq!(
                inspect_store(&store).unwrap_err().code,
                "secret.envelope_unsupported"
            );
            assert_eq!(
                reset_unrecoverable_store(&store).unwrap_err().code,
                "secret.envelope_unsupported"
            );
            assert_eq!(raw_record(&root, "provider.version"), record_before);
            assert_eq!(record_count(&root), 1);
        }

        let connection = Connection::open(root.database_path()).unwrap();
        connection.pragma_update(None, "user_version", 99).unwrap();
        drop(connection);
        let error = LocalEncryptedSecretStore::new(root.path())
            .err()
            .expect("unknown schema version must be rejected");
        assert_eq!(error.code, "secret.schema_unsupported");
    }

    #[test]
    fn invalid_inputs_clear_the_cache_and_return_stable_errors() {
        let root = TestRoot::new("invalid-input");
        let store = LocalEncryptedSecretStore::new(root.path()).unwrap();
        replace(&store, "provider.valid", "valid-secret").unwrap();
        assert!(read(&store, "provider.valid").unwrap().is_some());

        assert_eq!(
            replace(&store, "bad id", "another").unwrap_err().code,
            "secret.id_invalid"
        );
        assert!(store.state.lock().unwrap().cache.is_empty());
        assert_eq!(
            replace(&store, "provider.empty", "").unwrap_err().code,
            "secret.value_invalid"
        );
        assert!(store.state.lock().unwrap().cache.is_empty());
        assert_eq!(
            read(&store, "provider.valid").unwrap().as_deref(),
            Some("valid-secret")
        );
    }

    #[test]
    fn unavailable_store_reports_the_original_stable_error_for_every_operation() {
        let unavailable = UnavailableSecretStore::from_error(secret_error("secret.test_failure"));
        assert_eq!(
            configured(&unavailable, "provider.test").unwrap_err().code,
            "secret.test_failure"
        );
        assert_eq!(
            inspect(&unavailable, "provider.test").unwrap_err().code,
            "secret.test_failure"
        );
        assert_eq!(
            inspect_store(&unavailable).unwrap_err().code,
            "secret.test_failure"
        );
        assert_eq!(
            read(&unavailable, "provider.test").unwrap_err().code,
            "secret.test_failure"
        );
        assert_eq!(
            replace(&unavailable, "provider.test", "value")
                .unwrap_err()
                .code,
            "secret.test_failure"
        );
        assert_eq!(
            replace_namespace(&unavailable, "provider.", "provider.test", "value")
                .unwrap_err()
                .code,
            "secret.test_failure"
        );
        assert_eq!(
            delete(&unavailable, "provider.test").unwrap_err().code,
            "secret.test_failure"
        );
        assert_eq!(
            delete_namespace(&unavailable, "provider.")
                .unwrap_err()
                .code,
            "secret.test_failure"
        );
        assert_eq!(
            reset_unrecoverable(&unavailable, "provider.test")
                .unwrap_err()
                .code,
            "secret.test_failure"
        );
        assert_eq!(
            reset_unrecoverable_store(&unavailable).unwrap_err().code,
            "secret.test_failure"
        );
    }

    #[test]
    fn namespace_replace_default_is_fail_closed() {
        assert_eq!(
            replace_namespace(
                &DefaultNamespaceReplaceStore,
                "provider.",
                "provider.test",
                "value"
            )
            .unwrap_err()
            .code,
            "secret.namespace_replace_unsupported"
        );
    }

    #[cfg(unix)]
    #[test]
    fn unix_permissions_are_private_and_symlink_roots_are_rejected() {
        use std::os::unix::fs::{PermissionsExt, symlink};

        let root = TestRoot::new("permissions");
        let store = LocalEncryptedSecretStore::new(root.path()).unwrap();
        replace(&store, "provider.permissions", "private-value").unwrap();

        let directory_mode = fs::metadata(root.path()).unwrap().permissions().mode() & 0o777;
        let database_mode = fs::metadata(root.database_path())
            .unwrap()
            .permissions()
            .mode()
            & 0o777;
        let key_mode = fs::metadata(root.master_key_path())
            .unwrap()
            .permissions()
            .mode()
            & 0o777;
        assert_eq!(directory_mode, 0o700);
        assert_eq!(database_mode, 0o600);
        assert_eq!(key_mode, 0o600);
        drop(store);

        let link_parent = TestRoot::new("root-symlink");
        let real_root = link_parent.path().join("real");
        let linked_root = link_parent.path().join("linked");
        fs::create_dir_all(&real_root).unwrap();
        symlink(&real_root, &linked_root).unwrap();
        let error = LocalEncryptedSecretStore::new(&linked_root)
            .err()
            .expect("symlink root must be rejected");
        assert_eq!(error.code, "secret.directory_invalid");
    }

    #[cfg(unix)]
    #[test]
    fn symlink_database_and_master_key_files_are_rejected() {
        use std::os::unix::fs::symlink;

        let database_root = TestRoot::new("database-symlink");
        fs::create_dir_all(database_root.path()).unwrap();
        let external_database = database_root.path().join("external.sqlite3");
        fs::write(&external_database, []).unwrap();
        symlink(&external_database, database_root.database_path()).unwrap();
        let error = LocalEncryptedSecretStore::new(database_root.path())
            .err()
            .expect("symlink database must be rejected");
        assert_eq!(error.code, "secret.database_file_invalid");

        let key_root = TestRoot::new("key-symlink");
        let store = LocalEncryptedSecretStore::new(key_root.path()).unwrap();
        let external_key = key_root.path().join("external-key");
        fs::write(&external_key, [0_u8; MASTER_KEY_FILE_LENGTH]).unwrap();
        symlink(&external_key, key_root.master_key_path()).unwrap();
        drop(store);

        let reopened = LocalEncryptedSecretStore::new(key_root.path()).unwrap();
        let error = replace(&reopened, "provider.symlink", "secret").unwrap_err();
        assert_eq!(error.code, "secret.master_key_unavailable");
    }

    #[cfg(unix)]
    #[test]
    fn reset_rejects_unavailable_master_key_paths_without_deleting_ciphertext() {
        use std::os::unix::fs::symlink;

        let root = TestRoot::new("reset-unavailable-key");
        let secret_id = "provider.reset-unavailable";
        {
            let store = LocalEncryptedSecretStore::new(root.path()).unwrap();
            replace(&store, secret_id, "must-remain-encrypted").unwrap();
        }
        let record_before = raw_record(&root, secret_id);

        fs::remove_file(root.master_key_path()).unwrap();
        let external_key = root.path().join("external-master-key");
        fs::write(&external_key, [0_u8; MASTER_KEY_FILE_LENGTH]).unwrap();
        symlink(&external_key, root.master_key_path()).unwrap();

        let reopened = LocalEncryptedSecretStore::new(root.path()).unwrap();
        assert_eq!(
            inspect(&reopened, secret_id).unwrap_err().code,
            "secret.master_key_file_invalid"
        );
        assert_eq!(
            reset_unrecoverable(&reopened, secret_id).unwrap_err().code,
            "secret.master_key_file_invalid"
        );
        assert_eq!(raw_record(&root, secret_id), record_before);
        assert_eq!(record_count(&root), 1);
        assert!(
            fs::symlink_metadata(root.master_key_path())
                .unwrap()
                .file_type()
                .is_symlink()
        );
    }

    #[cfg(unix)]
    #[test]
    fn reset_rejects_invalid_master_key_permissions_without_deleting_ciphertext() {
        let root = TestRoot::new("reset-key-permissions");
        let secret_id = "provider.reset-key-permissions";
        {
            let store = LocalEncryptedSecretStore::new(root.path()).unwrap();
            replace(&store, secret_id, "must-remain-encrypted").unwrap();
        }
        let record_before = raw_record(&root, secret_id);
        set_unix_mode(
            &root.master_key_path(),
            0o644,
            "secret.master_key_permission_invalid",
        )
        .unwrap();

        let reopened = LocalEncryptedSecretStore::new(root.path()).unwrap();
        assert_eq!(
            inspect_store(&reopened).unwrap_err().code,
            "secret.master_key_permission_invalid"
        );
        assert_eq!(
            reset_unrecoverable_store(&reopened).unwrap_err().code,
            "secret.master_key_permission_invalid"
        );
        assert_eq!(raw_record(&root, secret_id), record_before);
        assert_eq!(record_count(&root), 1);
    }
}
