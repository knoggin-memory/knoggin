// SPDX-License-Identifier: AGPL-3.0-only
//! `PostgresDb`: the state database on Postgres, used on one host. Every document is sealed
//! with the state key before it is written, so the table and its dumps show only `pk`, `sk`,
//! `version` and `expires_ms`.
//!
//! A sealed document is a random 12-byte nonce, the AES-256-GCM-SIV ciphertext of the JSON
//! text and the 16-byte tag, with the associated data `pk`, a newline and `sk`. The key is
//! HKDF-SHA-256 (empty salt, info `knoggin-state-doc-v1`) of the `current` key of the key set
//! file that `state.postgres_doc_key_file` names.

use std::fmt;
use std::future::Future;
use std::str::FromStr;
use std::time::Duration;

use async_trait::async_trait;
use aws_lc_rs::aead::{Aad, NONCE_LEN, Nonce};
use aws_lc_rs::rand::{SecureRandom, SystemRandom};
use knoggin_crypto::{DataKey, MasterKeySet};
use serde_json::Value;
use sqlx::PgConnection;
use sqlx::postgres::{PgConnectOptions, PgPool, PgPoolOptions, PgSslMode};

use crate::db::{Clock, Cond, Item, StateDb, StateError, Write, validate};
use crate::seal::cipher;

/// HKDF info of the state key.
pub const DOC_KEY_INFO: &[u8] = b"knoggin-state-doc-v1";
/// At most this many connections per router.
const MAX_CONNECTIONS: u32 = 10;
/// How long a call waits for a free or new connection.
const ACQUIRE_TIMEOUT: Duration = Duration::from_secs(10);
/// The server cancels a statement that runs longer (`statement_timeout`, in milliseconds).
const STATEMENT_TIMEOUT_MS: &str = "10000";
/// A whole call, so that a connection that went silent cannot hold a request forever.
const CALL_TIMEOUT: Duration = Duration::from_secs(20);
const TAG_LEN: usize = 16;

static MIGRATOR: sqlx::migrate::Migrator = sqlx::migrate!();

const GET: &str = "SELECT version, expires_ms, doc FROM knoggin_state \
     WHERE pk = $1 AND sk = $2 AND (expires_ms IS NULL OR expires_ms > $3)";
const QUERY: &str = "SELECT sk, version, expires_ms, doc FROM knoggin_state \
     WHERE pk = $1 AND sk >= $2 AND starts_with(sk, $2) AND ($3::text IS NULL OR sk > $3) \
     AND (expires_ms IS NULL OR expires_ms > $4) ORDER BY sk LIMIT $5";
const DELETE_EXPIRED_ONE: &str =
    "DELETE FROM knoggin_state WHERE pk = $1 AND sk = $2 AND expires_ms <= $3";
const DELETE_EXPIRED: &str = "DELETE FROM knoggin_state WHERE expires_ms <= $1";
const PUT_ABSENT: &str = "INSERT INTO knoggin_state (pk, sk, version, expires_ms, doc) \
     VALUES ($1, $2, 1, $3, $4) ON CONFLICT DO NOTHING";
const PUT_ANY: &str = "INSERT INTO knoggin_state (pk, sk, version, expires_ms, doc) \
     VALUES ($1, $2, 1, $3, $4) ON CONFLICT (pk, sk) DO UPDATE \
     SET version = knoggin_state.version + 1, expires_ms = EXCLUDED.expires_ms, doc = EXCLUDED.doc";
const PUT_VERSION: &str = "UPDATE knoggin_state SET version = version + 1, expires_ms = $3, \
     doc = $4 WHERE pk = $1 AND sk = $2 AND version = $5";
const DELETE_ANY: &str = "DELETE FROM knoggin_state WHERE pk = $1 AND sk = $2";
const DELETE_VERSION: &str = "DELETE FROM knoggin_state WHERE pk = $1 AND sk = $2 AND version = $3";
const CHECK: &str = "SELECT version FROM knoggin_state WHERE pk = $1 AND sk = $2 FOR UPDATE";

/// The state database on Postgres. Holds a pool of at most 10 connections.
pub struct PostgresDb {
    pool: PgPool,
    doc_key: DataKey,
    clock: Clock,
}

impl fmt::Debug for PostgresDb {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("PostgresDb").finish_non_exhaustive()
    }
}

impl PostgresDb {
    /// Connects with `url` (which must state `sslmode` as `verify-full`, `verify-ca` or
    /// `disable`) and derives the state key from the `current` key of `state_keys`. Opens
    /// one connection, so an unreachable database fails here.
    pub async fn connect(
        url: &str,
        state_keys: &MasterKeySet,
        clock: Clock,
    ) -> Result<Self, StateError> {
        let options = connect_options(url)?;
        let doc_key = state_keys.derive_current(DOC_KEY_INFO)?;
        let pool = timed(async {
            PgPoolOptions::new()
                .max_connections(MAX_CONNECTIONS)
                .acquire_timeout(ACQUIRE_TIMEOUT)
                .connect_with(options)
                .await
                .map_err(db_err)
        })
        .await?;
        Ok(Self {
            pool,
            doc_key,
            clock,
        })
    }

    /// Applies `migrations/` (the router does this at start; its login owns the table).
    pub async fn migrate(&self) -> Result<(), StateError> {
        timed(async {
            MIGRATOR.run(&self.pool).await.map_err(|e| match e {
                sqlx::migrate::MigrateError::Execute(e) => db_err(e),
                _ => StateError::Unavailable("the migration failed".into()),
            })
        })
        .await
    }

    /// Deletes every expired row; returns how many. The lifecycle task's housekeeping.
    pub async fn delete_expired(&self) -> Result<u64, StateError> {
        let now = (self.clock)();
        timed(async {
            let done = sqlx::query(DELETE_EXPIRED)
                .bind(now)
                .execute(&self.pool)
                .await
                .map_err(db_err)?;
            Ok(done.rows_affected())
        })
        .await
    }

    fn item(
        &self,
        pk: String,
        sk: String,
        version: i64,
        expires_ms: Option<i64>,
        doc: Vec<u8>,
    ) -> Result<Item, StateError> {
        let doc = open_doc(&self.doc_key, &pk, &sk, doc)?;
        let version =
            u64::try_from(version).map_err(|_| StateError::Corrupt("a negative version"))?;
        Ok(Item {
            pk,
            sk,
            version,
            expires_ms,
            doc,
        })
    }
}

#[async_trait]
impl StateDb for PostgresDb {
    async fn get(&self, pk: &str, sk: &str) -> Result<Option<Item>, StateError> {
        let now = (self.clock)();
        let row: Option<(i64, Option<i64>, Vec<u8>)> = timed(async {
            sqlx::query_as(GET)
                .bind(pk)
                .bind(sk)
                .bind(now)
                .fetch_optional(&self.pool)
                .await
                .map_err(db_err)
        })
        .await?;
        row.map(|(v, e, doc)| self.item(pk.to_owned(), sk.to_owned(), v, e, doc))
            .transpose()
    }

    async fn query(
        &self,
        pk: &str,
        sk_prefix: &str,
        limit: usize,
        after: Option<&str>,
    ) -> Result<Vec<Item>, StateError> {
        let now = (self.clock)();
        let limit = i64::try_from(limit).unwrap_or(i64::MAX);
        let rows: Vec<(String, i64, Option<i64>, Vec<u8>)> = timed(async {
            sqlx::query_as(QUERY)
                .bind(pk)
                .bind(sk_prefix)
                .bind(after)
                .bind(now)
                .bind(limit)
                .fetch_all(&self.pool)
                .await
                .map_err(db_err)
        })
        .await?;
        // A row that does not open fails the whole call; it is never skipped.
        rows.into_iter()
            .map(|(sk, v, e, doc)| self.item(pk.to_owned(), sk, v, e, doc))
            .collect()
    }

    async fn write(&self, mut writes: Vec<Write>) -> Result<(), StateError> {
        validate(&writes)?;
        if writes.is_empty() {
            return Ok(());
        }
        // Rows are locked in key order, so two writes naming the same items never deadlock.
        writes.sort_by(|a, b| a.key().cmp(&b.key()));
        let sealed = writes
            .iter()
            .map(|w| match w {
                Write::Put { item, .. } => {
                    seal_doc(&self.doc_key, &item.pk, &item.sk, &item.doc).map(Some)
                }
                _ => Ok(None),
            })
            .collect::<Result<Vec<_>, _>>()?;
        let now = (self.clock)();
        timed(async {
            // Dropped without a commit (a conflict, an error), the transaction rolls back.
            let mut tx = self.pool.begin().await.map_err(db_err)?;
            for (w, doc) in writes.iter().zip(&sealed) {
                if !apply(&mut tx, w, doc.as_deref(), now).await? {
                    return Err(StateError::Conflict);
                }
            }
            tx.commit().await.map_err(db_err)
        })
        .await
    }
}

/// Runs one write inside the transaction; false when its condition does not hold.
async fn apply(
    conn: &mut PgConnection,
    w: &Write,
    doc: Option<&[u8]>,
    now: i64,
) -> Result<bool, StateError> {
    let (pk, sk) = w.key();
    // An expired item does not exist, for conditions too: remove it first.
    sqlx::query(DELETE_EXPIRED_ONE)
        .bind(pk)
        .bind(sk)
        .bind(now)
        .execute(&mut *conn)
        .await
        .map_err(db_err)?;
    // A version above i64::MAX was never stored, so that condition cannot hold.
    let version = |n: u64| i64::try_from(n).ok();
    let rows = match (w, doc) {
        (Write::Put { item, cond }, Some(doc)) => {
            let (sql, n) = match *cond {
                Cond::Any => (PUT_ANY, None),
                Cond::Absent => (PUT_ABSENT, None),
                Cond::Version(n) => match version(n) {
                    Some(n) => (PUT_VERSION, Some(n)),
                    None => return Ok(false),
                },
            };
            let mut q = sqlx::query(sql)
                .bind(pk)
                .bind(sk)
                .bind(item.expires_ms)
                .bind(doc);
            if let Some(n) = n {
                q = q.bind(n);
            }
            let done = q.execute(&mut *conn).await.map_err(db_err)?;
            if *cond == Cond::Any {
                return Ok(true);
            }
            done.rows_affected()
        }
        (Write::Delete { cond, .. }, None) => {
            let q = match *cond {
                Cond::Any => sqlx::query(DELETE_ANY).bind(pk).bind(sk),
                Cond::Version(n) => match version(n) {
                    Some(n) => sqlx::query(DELETE_VERSION).bind(pk).bind(sk).bind(n),
                    None => return Ok(false),
                },
                Cond::Absent => return Err(StateError::Invalid("a delete with Absent")),
            };
            let done = q.execute(&mut *conn).await.map_err(db_err)?;
            if *cond == Cond::Any {
                return Ok(true);
            }
            done.rows_affected()
        }
        (Write::Check { cond, .. }, None) => {
            let found: Option<i64> = sqlx::query_scalar(CHECK)
                .bind(pk)
                .bind(sk)
                .fetch_optional(&mut *conn)
                .await
                .map_err(db_err)?;
            return Ok(match *cond {
                Cond::Absent => found.is_none(),
                Cond::Version(n) => found.is_some() && found == version(n),
                Cond::Any => return Err(StateError::Invalid("a check with Any")),
            });
        }
        _ => return Err(StateError::Internal),
    };
    Ok(rows == 1)
}

/// Parses the URL and refuses it unless it states `sslmode` as `verify-full`, `verify-ca` or
/// `disable`: a missing value would mean `prefer`, which, like `require`, checks no
/// certificate. The URL itself must say it, not `PGSSLMODE`.
fn connect_options(url: &str) -> Result<PgConnectOptions, StateError> {
    const REFUSED: StateError = StateError::Invalid(
        "the Postgres URL must state sslmode as verify-full, verify-ca or disable",
    );
    // The driver applies the pairs in order, so the last one counts.
    let stated = url
        .split_once('?')
        .into_iter()
        .flat_map(|(_, q)| q.split('&'))
        .filter_map(|pair| pair.split_once('='))
        .filter(|(k, _)| matches!(*k, "sslmode" | "ssl-mode"))
        .map(|(_, v)| v)
        .next_back();
    if !matches!(stated, Some("verify-full" | "verify-ca" | "disable")) {
        return Err(REFUSED);
    }
    // The error would quote the URL, which holds the password.
    let options = PgConnectOptions::from_str(url)
        .map_err(|_| StateError::Invalid("the Postgres URL does not parse"))?;
    if !matches!(
        options.get_ssl_mode(),
        PgSslMode::VerifyFull | PgSslMode::VerifyCa | PgSslMode::Disable
    ) {
        return Err(REFUSED);
    }
    Ok(options.options([("statement_timeout", STATEMENT_TIMEOUT_MS)]))
}

async fn timed<T>(call: impl Future<Output = Result<T, StateError>>) -> Result<T, StateError> {
    tokio::time::timeout(CALL_TIMEOUT, call)
        .await
        .unwrap_or_else(|_| {
            Err(StateError::Unavailable(
                "the state database did not answer in time".into(),
            ))
        })
}

/// Maps a driver error. Only codes and fixed text leave here: a database message can quote
/// values, and a connection error can quote the URL.
fn db_err(e: sqlx::Error) -> StateError {
    use sqlx::Error as E;
    match e {
        E::ColumnDecode { .. }
        | E::Decode(_)
        | E::ColumnNotFound(_)
        | E::ColumnIndexOutOfBounds { .. }
        | E::TypeNotFound { .. } => StateError::Corrupt("a row does not decode"),
        E::Database(d) => StateError::Unavailable(format!(
            "database error {}",
            d.code().as_deref().unwrap_or("without a code")
        )),
        E::PoolTimedOut => {
            StateError::Unavailable("no database connection within the time limit".into())
        }
        E::PoolClosed => StateError::Unavailable("the connection pool is closed".into()),
        E::Io(io) => StateError::Unavailable(format!("database connection: {}", io.kind())),
        E::Tls(_) => StateError::Unavailable("TLS to the database failed".into()),
        _ => StateError::Unavailable("the database call failed".into()),
    }
}

fn doc_aad(pk: &str, sk: &str) -> Vec<u8> {
    format!("{pk}\n{sk}").into_bytes()
}

/// Nonce, ciphertext of the JSON text, tag.
fn seal_doc(key: &DataKey, pk: &str, sk: &str, doc: &Value) -> Result<Vec<u8>, StateError> {
    let mut nonce = [0u8; NONCE_LEN];
    SystemRandom::new()
        .fill(&mut nonce)
        .map_err(|_| StateError::Internal)?;
    let mut out = nonce.to_vec();
    serde_json::to_writer(&mut out, doc).map_err(|_| StateError::Internal)?;
    let tag = cipher(key)?
        .seal_in_place_separate_tag(
            Nonce::assume_unique_for_key(nonce),
            Aad::from(doc_aad(pk, sk)),
            &mut out[NONCE_LEN..],
        )
        .map_err(|_| StateError::Internal)?;
    out.extend_from_slice(tag.as_ref());
    Ok(out)
}

fn open_doc(key: &DataKey, pk: &str, sk: &str, mut sealed: Vec<u8>) -> Result<Value, StateError> {
    let bad = || StateError::Corrupt("a document does not open");
    if sealed.len() < NONCE_LEN + TAG_LEN {
        return Err(bad());
    }
    let (nonce, body) = sealed.split_at_mut(NONCE_LEN);
    let nonce = Nonce::try_assume_unique_for_key(nonce).map_err(|_| bad())?;
    let plain = cipher(key)?
        .open_in_place(nonce, Aad::from(doc_aad(pk, sk)), body)
        .map_err(|_| bad())?;
    serde_json::from_slice(plain).map_err(|_| StateError::Corrupt("a document is not JSON"))
}

#[cfg(test)]
mod tests {
    use std::borrow::Cow;
    use std::error::Error;

    use knoggin_crypto::testing::{TEST_MASTER_KEY, test_key_set};
    use serde_json::json;
    use sqlx::error::{DatabaseError, ErrorKind};

    use super::*;
    use crate::seal::hex;

    #[test]
    fn the_url_must_state_a_checked_sslmode() {
        let base = "postgres://u:secret@db:5432/knoggin";
        for ok in [
            "?sslmode=verify-full",
            "?sslmode=verify-ca&sslrootcert=/run/secrets/ca.pem",
            "?application_name=k&sslmode=disable",
            "?ssl-mode=disable",
            "?sslmode=require&sslmode=disable",
        ] {
            assert!(connect_options(&format!("{base}{ok}")).is_ok(), "{ok}");
        }
        for refused in [
            "",
            "?",
            "?application_name=k",
            "?sslmode=prefer",
            "?sslmode=require",
            "?sslmode=allow",
            "?sslmode=",
            "?sslmode=DISABLE",
            "?sslmode=disable&sslmode=prefer",
        ] {
            let e = connect_options(&format!("{base}{refused}")).unwrap_err();
            assert!(matches!(e, StateError::Invalid(_)), "{refused}: {e:?}");
            assert!(!e.to_string().contains("secret"));
        }
        let e = connect_options("not a url?sslmode=disable").unwrap_err();
        assert!(matches!(e, StateError::Invalid(_)));
    }

    #[test]
    fn the_state_key_is_hkdf_of_the_current_key() {
        let key = test_key_set().derive_current(DOC_KEY_INFO).unwrap();
        assert_eq!(
            key.expose(),
            DataKey::from_bytes(TEST_MASTER_KEY)
                .derive(b"knoggin-state-doc-v1")
                .expose()
        );
        // Pinned (checked against an independent HKDF), so a change of the derivation fails here.
        assert_eq!(
            hex(key.expose()),
            "a6b2ff8843ade855db02d42d405bd63fff7f6271d8c7e6ae1e210592e73187c5"
        );
        let field_seal = DataKey::from_bytes(TEST_MASTER_KEY).derive(b"knoggin-field-seal-v1");
        assert_ne!(key.expose(), field_seal.expose());
    }

    #[test]
    fn a_sealed_document_opens_only_in_its_own_row() {
        let key = test_key_set().derive_current(DOC_KEY_INFO).unwrap();
        let doc = json!({ "email": "marker@example.com", "n": 1 });
        let sealed = seal_doc(&key, "user#a", "-", &doc).unwrap();
        let plain_len = serde_json::to_vec(&doc).unwrap().len();
        assert_eq!(sealed.len(), NONCE_LEN + plain_len + TAG_LEN);
        assert!(!sealed.windows(6).any(|w| w == b"marker"));
        assert!(serde_json::from_slice::<Value>(&sealed).is_err());
        assert_ne!(sealed, seal_doc(&key, "user#a", "-", &doc).unwrap());
        assert_eq!(open_doc(&key, "user#a", "-", sealed.clone()).unwrap(), doc);
        for (pk, sk) in [
            ("user#b", "-"),
            ("user#a", "x"),
            ("user#a\n", ""),
            ("user#", "a\n-"),
        ] {
            assert!(matches!(
                open_doc(&key, pk, sk, sealed.clone()),
                Err(StateError::Corrupt(_))
            ));
        }
        let other = DataKey::from_bytes([7; 32]);
        assert!(open_doc(&other, "user#a", "-", sealed.clone()).is_err());
        let mut flipped = sealed.clone();
        flipped[NONCE_LEN] ^= 1;
        assert!(open_doc(&key, "user#a", "-", flipped).is_err());
        assert!(
            open_doc(
                &key,
                "user#a",
                "-",
                sealed[..NONCE_LEN + TAG_LEN - 1].to_vec()
            )
            .is_err()
        );
    }

    #[derive(Debug)]
    struct FakeDbError(&'static str);

    impl fmt::Display for FakeDbError {
        fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
            f.write_str("value 'marker@example.com' violates something")
        }
    }

    impl Error for FakeDbError {}

    impl DatabaseError for FakeDbError {
        fn message(&self) -> &str {
            "value 'marker@example.com' violates something"
        }
        fn code(&self) -> Option<Cow<'_, str>> {
            Some(Cow::Borrowed(self.0))
        }
        fn as_error(&self) -> &(dyn Error + Send + Sync + 'static) {
            self
        }
        fn as_error_mut(&mut self) -> &mut (dyn Error + Send + Sync + 'static) {
            self
        }
        fn into_error(self: Box<Self>) -> Box<dyn Error + Send + Sync + 'static> {
            self
        }
        fn kind(&self) -> ErrorKind {
            ErrorKind::Other
        }
    }

    #[test]
    fn driver_errors_map_without_their_text() {
        let e = db_err(sqlx::Error::Database(Box::new(FakeDbError("57014"))));
        assert!(
            matches!(&e, StateError::Unavailable(m) if m == "database error 57014"),
            "{e:?}"
        );
        assert!(!e.to_string().contains("marker"));
        assert!(matches!(
            db_err(sqlx::Error::PoolTimedOut),
            StateError::Unavailable(_)
        ));
        assert!(matches!(
            db_err(sqlx::Error::Io(std::io::ErrorKind::ConnectionReset.into())),
            StateError::Unavailable(_)
        ));
        assert!(matches!(
            db_err(sqlx::Error::Tls("postgres://u:secret@db".into())),
            StateError::Unavailable(m) if !m.contains("secret")
        ));
        assert!(matches!(
            db_err(sqlx::Error::ColumnNotFound("doc".into())),
            StateError::Corrupt(_)
        ));
        assert!(matches!(
            db_err(sqlx::Error::Decode("bad".into())),
            StateError::Corrupt(_)
        ));
    }
}
