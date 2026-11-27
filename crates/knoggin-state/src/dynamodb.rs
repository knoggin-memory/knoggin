// SPDX-License-Identifier: AGPL-3.0-only
//! `DynamoDb`: the state database on AWS. One table, hash key `pk` and range key `sk`, both
//! strings, with the attributes `version` (number), `doc` (string, the document's JSON text),
//! `expires_ms` (number, the time Knoggin compares) and `expires` (number, the same time in
//! seconds rounded up: the table's time-to-live attribute). DynamoDB encrypts the table at
//! rest; personal fields inside a document are already sealed by the typed helpers.
//!
//! DynamoDB removes expired items late, so every read drops an item whose `expires_ms` has
//! passed, and every condition treats such an item as absent. A put is always `UpdateItem`,
//! because `PutItem` cannot raise the counter without a read; the router's role has no
//! `PutItem`. One write goes alone; several, or any check, go as one `TransactWriteItems`.

use std::collections::HashMap;
use std::fmt;
use std::time::Duration;

use async_trait::async_trait;
use aws_sdk_dynamodb::Client;
use aws_sdk_dynamodb::config::timeout::TimeoutConfig;
use aws_sdk_dynamodb::error::{ProvideErrorMetadata, SdkError};
use aws_sdk_dynamodb::operation::transact_write_items::TransactWriteItemsError;
use aws_sdk_dynamodb::types::{AttributeValue, ConditionCheck, Delete, TransactWriteItem, Update};

use crate::db::{Clock, Cond, Item, StateDb, StateError, Write, validate};

/// Connecting to DynamoDB.
const CONNECT_TIMEOUT: Duration = Duration::from_secs(5);
/// One try of a call.
const ATTEMPT_TIMEOUT: Duration = Duration::from_secs(10);
/// One call with its retries (throttling is retried by the SDK). A query makes one call per
/// page.
const OPERATION_TIMEOUT: Duration = Duration::from_secs(20);

/// A put with `Any` or `Version(n)`: raises the counter without reading it.
const SET_NEXT: &str = "SET version = if_not_exists(version, :zero) + :one, doc = :doc";
/// A put with `Absent`: a new item, or one over an expired item, starts at 1.
const SET_FIRST: &str = "SET version = :one, doc = :doc";
const WITH_EXPIRY: &str = ", expires_ms = :ems, expires = :es";
const WITHOUT_EXPIRY: &str = " REMOVE expires_ms, expires";
/// `Version(n)`: an expired item fails it as an absent one would.
const IF_VERSION: &str = "version = :n AND (attribute_not_exists(expires_ms) OR expires_ms > :now)";
/// `Absent`: missing, or expired.
const IF_ABSENT: &str = "attribute_not_exists(pk) OR expires_ms <= :now";
const KEY_ONLY: &str = "pk = :pk";
const KEY_AND_PREFIX: &str = "pk = :pk AND begins_with(sk, :prefix)";

/// The state database on DynamoDB.
pub struct DynamoDb {
    client: Client,
    table: String,
    clock: Clock,
}

impl fmt::Debug for DynamoDb {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("DynamoDb")
            .field("table", &self.table)
            .finish_non_exhaustive()
    }
}

impl DynamoDb {
    /// A client of `table`. `conf` comes from the shared AWS config (with its dated
    /// behaviour version); this sets the time limits of every call. Nothing connects yet.
    pub fn new(
        conf: aws_sdk_dynamodb::config::Builder,
        table: impl Into<String>,
        clock: Clock,
    ) -> Self {
        let timeouts = TimeoutConfig::builder()
            .connect_timeout(CONNECT_TIMEOUT)
            .read_timeout(ATTEMPT_TIMEOUT)
            .operation_attempt_timeout(ATTEMPT_TIMEOUT)
            .operation_timeout(OPERATION_TIMEOUT)
            .build();
        Self {
            client: Client::from_conf(conf.timeout_config(timeouts).build()),
            table: table.into(),
            clock,
        }
    }

    fn key(pk: &str, sk: &str) -> HashMap<String, AttributeValue> {
        HashMap::from([
            ("pk".to_owned(), AttributeValue::S(pk.to_owned())),
            ("sk".to_owned(), AttributeValue::S(sk.to_owned())),
        ])
    }

    async fn send_one(&self, req: Request) -> Result<(), StateError> {
        let Request {
            kind,
            pk,
            sk,
            update,
            condition,
            values,
        } = req;
        let key = Some(Self::key(&pk, &sk));
        match kind {
            Kind::Update => self
                .client
                .update_item()
                .table_name(&self.table)
                .set_key(key)
                .set_update_expression(update)
                .set_condition_expression(condition)
                .set_expression_attribute_values(values)
                .send()
                .await
                .map(drop)
                .map_err(|e| sdk_err(&e)),
            Kind::Delete => self
                .client
                .delete_item()
                .table_name(&self.table)
                .set_key(key)
                .set_condition_expression(condition)
                .set_expression_attribute_values(values)
                .send()
                .await
                .map(drop)
                .map_err(|e| sdk_err(&e)),
            // A check never travels alone.
            Kind::Check => Err(StateError::Internal),
        }
    }

    async fn send_all(&self, reqs: Vec<Request>) -> Result<(), StateError> {
        let items = reqs
            .into_iter()
            .map(|r| r.transact(&self.table))
            .collect::<Result<Vec<_>, _>>()?;
        self.client
            .transact_write_items()
            .set_transact_items(Some(items))
            .send()
            .await
            .map(drop)
            .map_err(|e| transact_err(&e))
    }
}

#[async_trait]
impl StateDb for DynamoDb {
    async fn get(&self, pk: &str, sk: &str) -> Result<Option<Item>, StateError> {
        let now = (self.clock)();
        let answer = self
            .client
            .get_item()
            .table_name(&self.table)
            .set_key(Some(Self::key(pk, sk)))
            .consistent_read(true)
            .send()
            .await
            .map_err(|e| sdk_err(&e))?;
        match answer.item() {
            Some(attrs) => decode(attrs, now),
            None => Ok(None),
        }
    }

    async fn query(
        &self,
        pk: &str,
        sk_prefix: &str,
        limit: usize,
        after: Option<&str>,
    ) -> Result<Vec<Item>, StateError> {
        let now = (self.clock)();
        let Some(start) = start_after(sk_prefix, after) else {
            return Ok(Vec::new());
        };
        let mut start = start.map(|sk| Self::key(pk, sk));
        let mut out = Vec::new();
        // DynamoDB applies its limit before expired items are dropped, and pages at 1 MB:
        // keep asking until `limit` live items are held or the partition ends.
        while out.len() < limit {
            let page = i32::try_from(limit - out.len()).unwrap_or(i32::MAX);
            let (condition, values) = query_condition(pk, sk_prefix);
            let answer = self
                .client
                .query()
                .table_name(&self.table)
                .key_condition_expression(condition)
                .set_expression_attribute_values(Some(values))
                .consistent_read(true)
                .limit(page)
                .set_exclusive_start_key(start.take())
                .send()
                .await
                .map_err(|e| sdk_err(&e))?;
            for attrs in answer.items() {
                if let Some(item) = decode(attrs, now)? {
                    out.push(item);
                }
            }
            match answer.last_evaluated_key() {
                Some(key) if !key.is_empty() => start = Some(key.clone()),
                _ => break,
            }
        }
        out.truncate(limit);
        Ok(out)
    }

    async fn write(&self, writes: Vec<Write>) -> Result<(), StateError> {
        validate(&writes)?;
        let now = (self.clock)();
        let reqs = writes
            .iter()
            .map(|w| Request::of(w, now))
            .collect::<Result<Vec<_>, _>>()?;
        match <[Request; 1]>::try_from(reqs) {
            Ok([one]) if one.kind != Kind::Check => self.send_one(one).await,
            Ok([one]) => self.send_all(vec![one]).await,
            Err(reqs) if reqs.is_empty() => Ok(()),
            Err(reqs) => self.send_all(reqs).await,
        }
    }
}

/// What a query starts after, given the prefix. `None`: nothing with the prefix sorts after
/// `after`. `Some(None)`: start at the beginning, because `after` sorts before every key
/// with the prefix (DynamoDB refuses a start key outside the key condition).
fn start_after<'a>(prefix: &str, after: Option<&'a str>) -> Option<Option<&'a str>> {
    match after {
        None => Some(None),
        Some(a) if a.starts_with(prefix) => Some(Some(a)),
        Some(a) if a < prefix => Some(None),
        Some(_) => None,
    }
}

fn query_condition(pk: &str, prefix: &str) -> (&'static str, HashMap<String, AttributeValue>) {
    let mut values = HashMap::from([(":pk".to_owned(), AttributeValue::S(pk.to_owned()))]);
    if prefix.is_empty() {
        return (KEY_ONLY, values);
    }
    values.insert(":prefix".to_owned(), AttributeValue::S(prefix.to_owned()));
    (KEY_AND_PREFIX, values)
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Kind {
    Update,
    Delete,
    Check,
}

/// One write as DynamoDB takes it: alone, or as one item of a transaction.
#[derive(Debug, Clone, PartialEq)]
struct Request {
    kind: Kind,
    pk: String,
    sk: String,
    update: Option<String>,
    condition: Option<String>,
    values: Option<HashMap<String, AttributeValue>>,
}

fn n(value: impl fmt::Display) -> AttributeValue {
    AttributeValue::N(value.to_string())
}

/// `expires`: the expiry in seconds, rounded up, for DynamoDB's time-to-live.
fn ttl_seconds(expires_ms: i64) -> i64 {
    expires_ms.div_euclid(1000) + i64::from(expires_ms.rem_euclid(1000) != 0)
}

impl Request {
    fn of(w: &Write, now: i64) -> Result<Self, StateError> {
        let (pk, sk) = w.key();
        let mut values = HashMap::new();
        let condition = |cond: Cond, values: &mut HashMap<String, AttributeValue>| match cond {
            Cond::Any => None,
            Cond::Absent => {
                values.insert(":now".to_owned(), n(now));
                Some(IF_ABSENT.to_owned())
            }
            Cond::Version(v) => {
                values.insert(":n".to_owned(), n(v));
                values.insert(":now".to_owned(), n(now));
                Some(IF_VERSION.to_owned())
            }
        };
        let (kind, update, condition) = match w {
            Write::Put { item, cond } => {
                let mut expr = if *cond == Cond::Absent {
                    SET_FIRST.to_owned()
                } else {
                    values.insert(":zero".to_owned(), n(0));
                    SET_NEXT.to_owned()
                };
                values.insert(":one".to_owned(), n(1));
                let doc = serde_json::to_string(&item.doc).map_err(|_| StateError::Internal)?;
                values.insert(":doc".to_owned(), AttributeValue::S(doc));
                match item.expires_ms {
                    Some(ms) => {
                        expr.push_str(WITH_EXPIRY);
                        values.insert(":ems".to_owned(), n(ms));
                        values.insert(":es".to_owned(), n(ttl_seconds(ms)));
                    }
                    None => expr.push_str(WITHOUT_EXPIRY),
                }
                (Kind::Update, Some(expr), condition(*cond, &mut values))
            }
            Write::Delete { cond, .. } => match cond {
                Cond::Absent => return Err(StateError::Invalid("a delete with Absent")),
                c => (Kind::Delete, None, condition(*c, &mut values)),
            },
            Write::Check { cond, .. } => match cond {
                Cond::Any => return Err(StateError::Invalid("a check with Any")),
                c => (Kind::Check, None, condition(*c, &mut values)),
            },
        };
        Ok(Self {
            kind,
            pk: pk.to_owned(),
            sk: sk.to_owned(),
            update,
            condition,
            // DynamoDB refuses an empty map of values.
            values: (!values.is_empty()).then_some(values),
        })
    }

    fn transact(self, table: &str) -> Result<TransactWriteItem, StateError> {
        let key = Some(DynamoDb::key(&self.pk, &self.sk));
        let built = |_| StateError::Internal;
        let item = TransactWriteItem::builder();
        Ok(match self.kind {
            Kind::Update => item.update(
                Update::builder()
                    .table_name(table)
                    .set_key(key)
                    .set_update_expression(self.update)
                    .set_condition_expression(self.condition)
                    .set_expression_attribute_values(self.values)
                    .build()
                    .map_err(built)?,
            ),
            Kind::Delete => item.delete(
                Delete::builder()
                    .table_name(table)
                    .set_key(key)
                    .set_condition_expression(self.condition)
                    .set_expression_attribute_values(self.values)
                    .build()
                    .map_err(built)?,
            ),
            Kind::Check => item.condition_check(
                ConditionCheck::builder()
                    .table_name(table)
                    .set_key(key)
                    .set_condition_expression(self.condition)
                    .set_expression_attribute_values(self.values)
                    .build()
                    .map_err(built)?,
            ),
        }
        .build())
    }
}

/// An item as DynamoDB returns it, or `None` once its `expires_ms` has passed.
fn decode(attrs: &HashMap<String, AttributeValue>, now: i64) -> Result<Option<Item>, StateError> {
    let text = |name: &'static str| match attrs.get(name) {
        Some(AttributeValue::S(s)) => Ok(s.as_str()),
        _ => Err(StateError::Corrupt("an item lacks a string attribute")),
    };
    let number = |name: &'static str| -> Result<Option<&str>, StateError> {
        match attrs.get(name) {
            None => Ok(None),
            Some(AttributeValue::N(v)) => Ok(Some(v.as_str())),
            Some(_) => Err(StateError::Corrupt("an item attribute is not a number")),
        }
    };
    let expires_ms = number("expires_ms")?
        .map(str::parse::<i64>)
        .transpose()
        .map_err(|_| StateError::Corrupt("expires_ms is not a whole number"))?;
    if expires_ms.is_some_and(|e| e <= now) {
        return Ok(None);
    }
    let version = number("version")?
        .ok_or(StateError::Corrupt("an item has no version"))?
        .parse::<u64>()
        .map_err(|_| StateError::Corrupt("a version is not a whole number"))?;
    let doc = serde_json::from_str(text("doc")?)
        .map_err(|_| StateError::Corrupt("a document is not JSON"))?;
    Ok(Some(Item {
        pk: text("pk")?.to_owned(),
        sk: text("sk")?.to_owned(),
        version,
        expires_ms,
        doc,
    }))
}

/// A failed condition, or a write that met a transaction on the same item, is `Conflict`:
/// nothing was written and the caller reads again. Anything else is `Unavailable` with the
/// service's error code only, since a message can quote values.
fn sdk_err<E: ProvideErrorMetadata, R>(e: &SdkError<E, R>) -> StateError {
    match e {
        SdkError::ServiceError(s) => match s.err().code() {
            Some("ConditionalCheckFailedException" | "TransactionConflictException") => {
                StateError::Conflict
            }
            code => StateError::Unavailable(format!(
                "DynamoDB error {}",
                code.unwrap_or("without a code")
            )),
        },
        SdkError::TimeoutError(_) => {
            StateError::Unavailable("DynamoDB did not answer in time".into())
        }
        _ => StateError::Unavailable("DynamoDB cannot be reached".into()),
    }
}

/// A cancelled transaction is `Conflict` when one of its reasons is a failed condition or
/// another transaction on the same item: either way nothing was written.
fn transact_err<R>(e: &SdkError<TransactWriteItemsError, R>) -> StateError {
    if let Some(TransactWriteItemsError::TransactionCanceledException(t)) = e.as_service_error()
        && t.cancellation_reasons().iter().any(|r| {
            matches!(
                r.code(),
                Some("ConditionalCheckFailed" | "TransactionConflict")
            )
        })
    {
        return StateError::Conflict;
    }
    sdk_err(e)
}

#[cfg(test)]
mod tests {
    use std::sync::Arc;

    use aws_sdk_dynamodb::config::{BehaviorVersion, Credentials, Region};
    use serde_json::{Value, json};
    use wiremock::matchers::{header, method};
    use wiremock::{Mock, MockServer, ResponseTemplate};

    use super::*;

    const NOW: i64 = 1_800_000_000_000;

    fn s(v: &str) -> AttributeValue {
        AttributeValue::S(v.to_owned())
    }

    fn values(pairs: &[(&str, AttributeValue)]) -> Option<HashMap<String, AttributeValue>> {
        Some(
            pairs
                .iter()
                .map(|(k, v)| ((*k).to_owned(), v.clone()))
                .collect(),
        )
    }

    fn put(cond: Cond, expires_ms: Option<i64>) -> Request {
        let item = Item::new("user#a", "-", json!({ "x": "Grüße" })).expiring(expires_ms);
        Request::of(&Write::put(item, cond), NOW).unwrap()
    }

    #[test]
    fn puts_are_update_item_with_the_fixed_expressions() {
        let doc = s(r#"{"x":"Grüße"}"#);
        let r = put(Cond::Any, None);
        assert_eq!(r.kind, Kind::Update);
        assert_eq!((r.pk.as_str(), r.sk.as_str()), ("user#a", "-"));
        assert_eq!(
            r.update.as_deref(),
            Some(
                "SET version = if_not_exists(version, :zero) + :one, doc = :doc \
                 REMOVE expires_ms, expires"
            )
        );
        assert_eq!(r.condition, None);
        assert_eq!(
            r.values,
            values(&[(":zero", n(0)), (":one", n(1)), (":doc", doc.clone())])
        );

        // 1,800,000,000,001 ms rounds up to 1,800,000,001 s.
        let r = put(Cond::Version(7), Some(NOW + 1));
        assert_eq!(
            r.update.as_deref(),
            Some(
                "SET version = if_not_exists(version, :zero) + :one, doc = :doc, \
                 expires_ms = :ems, expires = :es"
            )
        );
        assert_eq!(
            r.condition.as_deref(),
            Some("version = :n AND (attribute_not_exists(expires_ms) OR expires_ms > :now)")
        );
        assert_eq!(
            r.values,
            values(&[
                (":zero", n(0)),
                (":one", n(1)),
                (":doc", doc.clone()),
                (":ems", n(NOW + 1)),
                (":es", n(1_800_000_001)),
                (":n", n(7)),
                (":now", n(NOW)),
            ])
        );

        let r = put(Cond::Absent, Some(NOW));
        assert_eq!(
            r.update.as_deref(),
            Some("SET version = :one, doc = :doc, expires_ms = :ems, expires = :es")
        );
        assert_eq!(
            r.condition.as_deref(),
            Some("attribute_not_exists(pk) OR expires_ms <= :now")
        );
        assert_eq!(
            r.values,
            values(&[
                (":one", n(1)),
                (":doc", doc),
                (":ems", n(NOW)),
                (":es", n(1_800_000_000)),
                (":now", n(NOW)),
            ])
        );
        let r = put(Cond::Absent, None);
        assert_eq!(
            r.update.as_deref(),
            Some("SET version = :one, doc = :doc REMOVE expires_ms, expires")
        );
    }

    #[test]
    fn deletes_and_checks_carry_the_same_conditions() {
        let r = Request::of(&Write::delete("p", "s", Cond::Any), NOW).unwrap();
        assert_eq!(
            (r.kind, r.update, r.condition, r.values),
            (Kind::Delete, None, None, None)
        );
        let r = Request::of(&Write::delete("p", "s", Cond::Version(3)), NOW).unwrap();
        assert_eq!(r.kind, Kind::Delete);
        assert_eq!(r.condition.as_deref(), Some(IF_VERSION));
        assert_eq!(r.values, values(&[(":n", n(3)), (":now", n(NOW))]));
        let r = Request::of(&Write::check("p", "s", Cond::Absent), NOW).unwrap();
        assert_eq!((r.kind, r.update), (Kind::Check, None));
        assert_eq!(r.condition.as_deref(), Some(IF_ABSENT));
        assert_eq!(r.values, values(&[(":now", n(NOW))]));
        let r = Request::of(&Write::check("p", "s", Cond::Version(u64::MAX)), NOW).unwrap();
        assert_eq!(r.values, values(&[(":n", n(u64::MAX)), (":now", n(NOW))]));
        assert!(matches!(
            Request::of(&Write::delete("p", "s", Cond::Absent), NOW),
            Err(StateError::Invalid(_))
        ));
        assert!(matches!(
            Request::of(&Write::check("p", "s", Cond::Any), NOW),
            Err(StateError::Invalid(_))
        ));
    }

    #[test]
    fn expiry_seconds_round_up() {
        assert_eq!(ttl_seconds(0), 0);
        assert_eq!(ttl_seconds(1), 1);
        assert_eq!(ttl_seconds(999), 1);
        assert_eq!(ttl_seconds(1000), 1);
        assert_eq!(ttl_seconds(1001), 2);
        assert_eq!(ttl_seconds(-1), 0);
        assert_eq!(ttl_seconds(-1001), -1);
    }

    #[test]
    fn a_query_starts_inside_its_key_condition() {
        assert_eq!(start_after("a", None), Some(None));
        assert_eq!(start_after("a", Some("a#1")), Some(Some("a#1")));
        assert_eq!(start_after("a", Some("a")), Some(Some("a")));
        assert_eq!(start_after("b", Some("a")), Some(None));
        assert_eq!(start_after("b", Some("az")), Some(None));
        assert_eq!(start_after("a", Some("b")), None);
        assert_eq!(start_after("a#", Some("a$")), None);
        assert_eq!(start_after("", Some("x")), Some(Some("x")));
        assert_eq!(query_condition("p", "").0, KEY_ONLY);
        let (c, v) = query_condition("p", "a#");
        assert_eq!(c, "pk = :pk AND begins_with(sk, :prefix)");
        assert_eq!(v.get(":prefix"), Some(&s("a#")));
        assert_eq!(v.get(":pk"), Some(&s("p")));
    }

    #[test]
    fn reads_drop_expired_items_and_refuse_bad_ones() {
        let attrs = |exp: Option<&str>| {
            let mut m = HashMap::from([
                ("pk".to_owned(), s("p")),
                ("sk".to_owned(), s("s")),
                ("version".to_owned(), n(4)),
                ("doc".to_owned(), s(r#"{"a":[1,null]}"#)),
            ]);
            if let Some(e) = exp {
                m.insert("expires_ms".to_owned(), AttributeValue::N(e.to_owned()));
                m.insert("expires".to_owned(), n(1));
            }
            m
        };
        let item = decode(&attrs(None), NOW).unwrap().unwrap();
        assert_eq!(
            (
                item.pk.as_str(),
                item.sk.as_str(),
                item.version,
                item.expires_ms
            ),
            ("p", "s", 4, None)
        );
        assert_eq!(item.doc, json!({ "a": [1, null] }));
        let live = (NOW + 1).to_string();
        assert_eq!(
            decode(&attrs(Some(&live)), NOW)
                .unwrap()
                .unwrap()
                .expires_ms,
            Some(NOW + 1)
        );
        assert!(
            decode(&attrs(Some(&NOW.to_string())), NOW)
                .unwrap()
                .is_none()
        );
        for (name, bad) in [
            ("version", s("4")),
            ("version", n(-1)),
            ("doc", s("{")),
            ("doc", n(1)),
            ("expires_ms", s("1")),
            ("expires_ms", AttributeValue::N("1.5".into())),
        ] {
            let mut m = attrs(None);
            m.insert(name.to_owned(), bad);
            assert!(
                matches!(decode(&m, NOW), Err(StateError::Corrupt(_))),
                "{name}"
            );
        }
        let mut m = attrs(None);
        m.remove("version");
        assert!(matches!(decode(&m, NOW), Err(StateError::Corrupt(_))));
    }

    #[allow(deprecated, reason = "the dated version the binary uses")]
    fn behavior_version() -> BehaviorVersion {
        BehaviorVersion::v2026_11_17()
    }

    async fn stub() -> (MockServer, DynamoDb) {
        let server = MockServer::start().await;
        let conf = aws_sdk_dynamodb::config::Builder::new()
            .behavior_version(behavior_version())
            .region(Region::new("eu-west-1"))
            .credentials_provider(Credentials::new("AKIDTEST", "secret", None, None, "test"))
            .endpoint_url(server.uri())
            .retry_config(
                aws_sdk_dynamodb::config::retry::RetryConfig::standard()
                    .with_max_attempts(2)
                    .with_initial_backoff(Duration::from_millis(1)),
            );
        let db = DynamoDb::new(conf, "knoggin-state", Arc::new(|| NOW));
        (server, db)
    }

    fn answer(status: u16, body: Value) -> ResponseTemplate {
        ResponseTemplate::new(status)
            .insert_header("content-type", "application/x-amz-json-1.0")
            .set_body_string(body.to_string())
    }

    async fn sent(server: &MockServer) -> Vec<(String, Value)> {
        server
            .received_requests()
            .await
            .unwrap()
            .iter()
            .map(|r| {
                (
                    r.headers["x-amz-target"].to_str().unwrap().to_owned(),
                    serde_json::from_slice(&r.body).unwrap(),
                )
            })
            .collect()
    }

    /// What the wire carries: one write alone, several (and any check) as one transaction,
    /// reads consistent, and the table named everywhere.
    #[tokio::test]
    async fn requests_on_the_wire() {
        let (server, db) = stub().await;
        Mock::given(method("POST"))
            .respond_with(answer(200, json!({})))
            .mount(&server)
            .await;
        db.write(vec![Write::put(Item::new("p", "s", json!({})), Cond::Any)])
            .await
            .unwrap();
        db.write(vec![Write::delete("p", "s", Cond::Version(2))])
            .await
            .unwrap();
        db.write(vec![Write::check("p", "s", Cond::Absent)])
            .await
            .unwrap();
        db.write(vec![
            Write::put(Item::new("p", "t", json!({ "k": 1 })), Cond::Absent),
            Write::delete("p", "u", Cond::Any),
            Write::check("q", "-", Cond::Version(5)),
        ])
        .await
        .unwrap();
        db.write(Vec::new()).await.unwrap();
        assert!(db.get("p", "s").await.unwrap().is_none());
        assert!(db.query("p", "a", 3, None).await.unwrap().is_empty());

        let calls = sent(&server).await;
        let targets: Vec<&str> = calls.iter().map(|(t, _)| t.as_str()).collect();
        assert_eq!(
            targets,
            [
                "DynamoDB_20120810.UpdateItem",
                "DynamoDB_20120810.DeleteItem",
                "DynamoDB_20120810.TransactWriteItems",
                "DynamoDB_20120810.TransactWriteItems",
                "DynamoDB_20120810.GetItem",
                "DynamoDB_20120810.Query",
            ]
        );
        let body = |i: usize| &calls[i].1;
        assert_eq!(body(0)["TableName"], "knoggin-state");
        assert_eq!(
            body(0)["Key"],
            json!({ "pk": { "S": "p" }, "sk": { "S": "s" } })
        );
        assert_eq!(
            body(0)["ExpressionAttributeValues"][":doc"],
            json!({ "S": "{}" })
        );
        assert!(body(0).get("ConditionExpression").is_none());
        assert_eq!(body(1)["ConditionExpression"], IF_VERSION);
        assert_eq!(
            body(1)["ExpressionAttributeValues"][":n"],
            json!({ "N": "2" })
        );
        let alone = body(2)["TransactItems"].as_array().unwrap();
        assert_eq!(alone.len(), 1);
        assert_eq!(alone[0]["ConditionCheck"]["ConditionExpression"], IF_ABSENT);
        let items = body(3)["TransactItems"].as_array().unwrap();
        assert_eq!(items.len(), 3);
        assert_eq!(
            items[0]["Update"]["UpdateExpression"],
            "SET version = :one, doc = :doc REMOVE expires_ms, expires"
        );
        assert_eq!(items[0]["Update"]["TableName"], "knoggin-state");
        assert_eq!(items[1]["Delete"]["Key"]["sk"], json!({ "S": "u" }));
        assert!(items[1]["Delete"].get("ConditionExpression").is_none());
        assert_eq!(items[2]["ConditionCheck"]["Key"]["pk"], json!({ "S": "q" }));
        // The SDK fills the idempotency token, so its own retries cannot apply twice.
        assert!(body(3)["ClientRequestToken"].as_str().is_some());
        assert_eq!(body(4)["ConsistentRead"], true);
        assert_eq!(body(5)["ConsistentRead"], true);
        assert_eq!(body(5)["Limit"], 3);
        assert_eq!(body(5)["KeyConditionExpression"], KEY_AND_PREFIX);
        assert!(body(5).get("ExclusiveStartKey").is_none());
    }

    /// A query keeps asking past expired items and short pages, from DynamoDB's last key,
    /// until it holds `limit` live items.
    #[tokio::test]
    async fn a_query_pages_until_it_holds_its_limit() {
        let (server, db) = stub().await;
        let row = |sk: &str, exp: Option<i64>| {
            let mut v = json!({
                "pk": { "S": "p" }, "sk": { "S": sk },
                "version": { "N": "1" }, "doc": { "S": "{}" },
            });
            if let Some(e) = exp {
                v["expires_ms"] = json!({ "N": e.to_string() });
            }
            v
        };
        let pages = [
            json!({ "Items": [row("k1", Some(NOW)), row("k2", None)],
                    "LastEvaluatedKey": { "pk": { "S": "p" }, "sk": { "S": "k2" } } }),
            json!({ "Items": [row("k3", Some(NOW - 1))],
                    "LastEvaluatedKey": { "pk": { "S": "p" }, "sk": { "S": "k3" } } }),
            json!({ "Items": [row("k4", Some(NOW + 1)), row("k5", None)] }),
        ];
        for (i, page) in pages.into_iter().enumerate() {
            Mock::given(method("POST"))
                .and(header("x-amz-target", "DynamoDB_20120810.Query"))
                .respond_with(answer(200, page))
                .up_to_n_times(1)
                .with_priority(u8::try_from(i + 1).unwrap())
                .mount(&server)
                .await;
        }
        let got = db.query("p", "k", 3, Some("k0")).await.unwrap();
        let sks: Vec<&str> = got.iter().map(|i| i.sk.as_str()).collect();
        assert_eq!(sks, ["k2", "k4", "k5"]);
        let calls = sent(&server).await;
        assert_eq!(calls.len(), 3);
        assert_eq!(calls[0].1["Limit"], 3);
        assert_eq!(calls[0].1["ExclusiveStartKey"]["sk"], json!({ "S": "k0" }));
        assert_eq!(calls[1].1["Limit"], 2);
        assert_eq!(calls[1].1["ExclusiveStartKey"]["sk"], json!({ "S": "k2" }));
        assert_eq!(calls[2].1["Limit"], 2);
        assert_eq!(calls[2].1["ExclusiveStartKey"]["sk"], json!({ "S": "k3" }));
        // `after` beyond every key with the prefix: no call at all.
        assert!(db.query("p", "k", 3, Some("l")).await.unwrap().is_empty());
        assert!(db.query("p", "k", 0, None).await.unwrap().is_empty());
        assert_eq!(sent(&server).await.len(), 3);
    }

    #[tokio::test]
    async fn errors_map_to_conflict_or_unavailable_without_their_text() {
        let (server, db) = stub().await;
        let err = |kind: &str, extra: Value| {
            let mut b = json!({
                "__type": format!("com.amazonaws.dynamodb.v20120810#{kind}"),
                "message": "value 'marker@example.com' was refused",
            });
            if let (Some(b), Some(e)) = (b.as_object_mut(), extra.as_object()) {
                b.extend(e.clone());
            }
            answer(400, b)
        };
        let reasons = |codes: &[&str]| json!({ "CancellationReasons": codes.iter().map(|c| json!({ "Code": c })).collect::<Vec<_>>() });
        let cases = [
            (
                "DynamoDB_20120810.UpdateItem",
                err("ConditionalCheckFailedException", json!({})),
            ),
            (
                "DynamoDB_20120810.TransactWriteItems",
                err(
                    "TransactionCanceledException",
                    reasons(&["None", "ConditionalCheckFailed"]),
                ),
            ),
            // Another transaction held an item: nothing was written, so read again.
            (
                "DynamoDB_20120810.UpdateItem",
                err("TransactionConflictException", json!({})),
            ),
            (
                "DynamoDB_20120810.TransactWriteItems",
                err(
                    "TransactionCanceledException",
                    reasons(&["None", "TransactionConflict"]),
                ),
            ),
        ];
        for (target, response) in cases {
            server.reset().await;
            Mock::given(header("x-amz-target", target))
                .respond_with(response)
                .mount(&server)
                .await;
            let writes = if target.ends_with("UpdateItem") {
                vec![Write::put(Item::new("p", "s", json!({})), Cond::Version(1))]
            } else {
                vec![
                    Write::put(Item::new("p", "s", json!({})), Cond::Version(1)),
                    Write::check("p", "t", Cond::Absent),
                ]
            };
            assert!(
                matches!(db.write(writes).await, Err(StateError::Conflict)),
                "{target}"
            );
        }
        // A transaction cancelled for another reason, a validation error and a throttle
        // that outlasts the SDK's retries are outages, and show only the code.
        let outages = [
            err(
                "TransactionCanceledException",
                reasons(&["None", "ThrottlingError"]),
            ),
            err("ValidationException", json!({})),
            err("ProvisionedThroughputExceededException", json!({})),
            answer(500, json!({ "__type": "InternalServerError" })),
        ];
        for response in outages {
            server.reset().await;
            Mock::given(method("POST"))
                .respond_with(response)
                .mount(&server)
                .await;
            let e = db
                .write(vec![
                    Write::delete("p", "s", Cond::Any),
                    Write::delete("p", "t", Cond::Any),
                ])
                .await
                .unwrap_err();
            assert!(matches!(&e, StateError::Unavailable(_)), "{e:?}");
            assert!(!e.to_string().contains("marker"), "{e}");
            let e = db.get("p", "s").await.unwrap_err();
            assert!(matches!(&e, StateError::Unavailable(_)), "{e:?}");
        }
        assert!(
            sent(&server).await.len() >= 2,
            "the SDK retries a 500 before it gives up"
        );
    }

    /// A table that accepts connections and never answers: every call still ends, as
    /// `unavailable`, within the operation time limit.
    #[tokio::test(start_paused = true)]
    async fn a_silent_endpoint_cannot_hold_a_call() {
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        let held = tokio::spawn(async move {
            let mut open = Vec::new();
            while let Ok((stream, _)) = listener.accept().await {
                open.push(stream);
            }
        });
        let conf = aws_sdk_dynamodb::config::Builder::new()
            .behavior_version(behavior_version())
            .region(Region::new("eu-west-1"))
            .credentials_provider(Credentials::new("AKIDTEST", "secret", None, None, "test"))
            .endpoint_url(format!("http://{addr}"));
        let db = DynamoDb::new(conf, "t", Arc::new(|| NOW));
        let started = tokio::time::Instant::now();
        assert!(matches!(
            db.get("p", "s").await,
            Err(StateError::Unavailable(_))
        ));
        assert!(matches!(
            db.write(vec![Write::delete("p", "s", Cond::Any)]).await,
            Err(StateError::Unavailable(_))
        ));
        assert!(
            started.elapsed() <= 2 * OPERATION_TIMEOUT,
            "{:?}",
            started.elapsed()
        );
        held.abort();
    }
}
