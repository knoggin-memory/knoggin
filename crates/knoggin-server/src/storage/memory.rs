// SPDX-License-Identifier: AGPL-3.0-only
//! `MemoryStorage`, the bucket of the fast tests (tab "Testing, CI, releases", stand-ins):
//! a small HTTP server on a loopback port that keeps objects in memory. Its links are real
//! addresses, signed with an HMAC and an expiry in query parameters named as S3's
//! (`X-Amz-Algorithm` to `X-Amz-Signature`, and `x-id`), so the code that downloads and
//! uploads, and the one-host blob route, treat it as they treat a real bucket. It refuses
//! to overwrite, checks every signed header, and has switches that make it fail, stall or
//! go away.

use std::collections::BTreeMap;
use std::fmt;
use std::net::SocketAddr;
use std::sync::{Arc, Mutex, PoisonError};
use std::time::Duration;

use async_trait::async_trait;
use aws_lc_rs::constant_time::verify_slices_are_equal;
use aws_lc_rs::hmac;
use aws_lc_rs::rand::{SecureRandom, SystemRandom};
use bytes::Bytes;
use http::{HeaderMap, Method, Request, Response, StatusCode};
use http_body_util::{BodyExt, Full, Limited};
use hyper::body::Incoming;
use hyper_util::rt::{TokioIo, TokioTimer};
use knoggin_core::Timestamp;
use knoggin_state::Clock;
use time::macros::format_description;
use time::{OffsetDateTime, PrimitiveDateTime};
use tokio::net::TcpListener;
use tokio::sync::watch;
use tokio::task::{JoinHandle, JoinSet};

use super::{ObjectInfo, ObjectStorage, SignedRequest, StorageError};
use crate::hex;

/// The value of `X-Amz-Algorithm` on its links. Not SigV4: an HMAC of its own.
pub const ALGORITHM: &str = "KNOGGIN-TEST-HMAC-SHA256";

/// The bucket name in its path-style addresses.
pub const BUCKET: &str = "knoggin-test";

/// The largest upload it takes.
const MAX_OBJECT: usize = 256 * 1024 * 1024;

const SIGNED_PARAMS: [&str; 5] = [
    "X-Amz-Algorithm",
    "X-Amz-Credential",
    "X-Amz-Date",
    "X-Amz-Expires",
    "X-Amz-SignedHeaders",
];
const SIGNATURE: &str = "X-Amz-Signature";
const X_ID: &str = "x-id";

/// How the stand-in misbehaves.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum Fault {
    /// Works.
    #[default]
    None,
    /// Answers 503 to every request, and `Unavailable` to `list` and `delete`.
    Fail,
    /// Holds every request, and `list` and `delete`, until the fault changes.
    Stall,
    /// Closes its port and every open connection; `list` and `delete` are `Unavailable`.
    Gone,
    /// Answers every HTTP request with this status and no body; `list` and `delete` work.
    Answer(u16),
}

/// A property of a real bucket that the stand-in can be told to lack, so that the bucket
/// self-test's own tests see each of its checks fail.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Weakness {
    /// An upload to an existing name replaces it instead of answering 412.
    Overwrites,
    /// Uploads and downloads are taken without checking the signature or what it covers.
    Unsigned,
    /// A link works after it has expired.
    NoExpiry,
    /// A listing does not show any object yet.
    StaleListing,
}

/// One request it received, for tests that check what reached the bucket.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SeenRequest {
    /// The method.
    pub method: String,
    /// The object key.
    pub key: String,
    /// The names of the headers it carried, lower case.
    pub header_names: Vec<String>,
    /// The status it was answered with.
    pub status: u16,
}

#[derive(Clone)]
struct Stored {
    bytes: Bytes,
    last_modified: Timestamp,
}

struct Shared {
    objects: Mutex<BTreeMap<String, Stored>>,
    key: hmac::Key,
    clock: Clock,
    fault: watch::Sender<Fault>,
    seen: Mutex<Vec<SeenRequest>>,
    addr: SocketAddr,
    weakness: Mutex<Option<Weakness>>,
}

impl Shared {
    fn weak(&self, w: Weakness) -> bool {
        *lock(&self.weakness) == Some(w)
    }
}

/// The in-memory bucket and its loopback server. The server stops when this is dropped.
pub struct MemoryStorage {
    shared: Arc<Shared>,
    server: Mutex<Option<JoinHandle<()>>>,
}

impl fmt::Debug for MemoryStorage {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("MemoryStorage")
            .field("addr", &self.shared.addr)
            .field("fault", &*self.shared.fault.borrow())
            .finish_non_exhaustive()
    }
}

impl Drop for MemoryStorage {
    fn drop(&mut self) {
        if let Some(task) = lock(&self.server).take() {
            task.abort();
        }
    }
}

fn lock<T>(m: &Mutex<T>) -> std::sync::MutexGuard<'_, T> {
    m.lock().unwrap_or_else(PoisonError::into_inner)
}

impl MemoryStorage {
    /// Starts the server on a free loopback port. `clock` gives the time links are
    /// checked against and objects are stamped with.
    pub async fn start(clock: Clock) -> std::io::Result<Self> {
        let listener = TcpListener::bind("127.0.0.1:0").await?;
        let mut key = [0u8; 32];
        SystemRandom::new()
            .fill(&mut key)
            .map_err(|_| std::io::Error::other("no random bytes"))?;
        let shared = Arc::new(Shared {
            objects: Mutex::new(BTreeMap::new()),
            key: hmac::Key::new(hmac::HMAC_SHA256, &key),
            clock,
            fault: watch::Sender::new(Fault::None),
            seen: Mutex::new(Vec::new()),
            addr: listener.local_addr()?,
            weakness: Mutex::new(None),
        });
        let server = tokio::spawn(serve(listener, shared.clone()));
        Ok(Self {
            shared,
            server: Mutex::new(Some(server)),
        })
    }

    /// `http://127.0.0.1:<port>`: the `storage.endpoint_url` that points at it.
    pub fn endpoint(&self) -> String {
        format!("http://{}", self.shared.addr)
    }

    /// The bucket name in its addresses.
    pub fn bucket(&self) -> &'static str {
        BUCKET
    }

    /// Switches a fault on or off. Leaving `Gone` opens the same port again.
    pub async fn set_fault(&self, fault: Fault) -> std::io::Result<()> {
        let was = self.shared.fault.send_replace(fault);
        if fault == Fault::Gone {
            let task = lock(&self.server).take();
            if let Some(task) = task {
                task.abort();
                // Wait for the listener and the connections to be dropped.
                let _ = task.await;
            }
        } else if was == Fault::Gone {
            let listener = TcpListener::bind(self.shared.addr).await?;
            *lock(&self.server) = Some(tokio::spawn(serve(listener, self.shared.clone())));
        }
        Ok(())
    }

    /// Takes away one property of a real bucket, or with `None` gives it back.
    pub fn weaken(&self, weakness: Option<Weakness>) {
        *lock(&self.shared.weakness) = weakness;
    }

    /// Stores an object directly, as if uploaded at `last_modified`.
    pub fn insert(&self, key: &str, bytes: impl Into<Bytes>, last_modified: Timestamp) {
        lock(&self.shared.objects).insert(
            key.to_owned(),
            Stored {
                bytes: bytes.into(),
                last_modified,
            },
        );
    }

    /// An object's bytes.
    pub fn get(&self, key: &str) -> Option<Bytes> {
        lock(&self.shared.objects).get(key).map(|s| s.bytes.clone())
    }

    /// Removes an object directly.
    pub fn remove(&self, key: &str) {
        lock(&self.shared.objects).remove(key);
    }

    /// Every key it holds, in order.
    pub fn keys(&self) -> Vec<String> {
        lock(&self.shared.objects).keys().cloned().collect()
    }

    /// The requests its server received, oldest first.
    pub fn seen(&self) -> Vec<SeenRequest> {
        lock(&self.shared.seen).clone()
    }

    async fn admit(&self) -> Result<(), StorageError> {
        let mut rx = self.shared.fault.subscribe();
        let fault = *rx
            .wait_for(|f| *f != Fault::Stall)
            .await
            .map_err(|_| StorageError::Unavailable)?;
        match fault {
            Fault::None | Fault::Answer(_) => Ok(()),
            _ => Err(StorageError::Unavailable),
        }
    }

    fn sign(
        &self,
        method: &Method,
        key: &str,
        headers: Vec<(String, String)>,
        ttl: Duration,
        now: Timestamp,
    ) -> Result<SignedRequest, StorageError> {
        let expires = ttl.as_secs();
        if expires == 0 || expires > 604_800 {
            return Err(StorageError::Invalid("link lifetime"));
        }
        let date = amz_date(now).ok_or(StorageError::Invalid("time"))?;
        let host = self.shared.addr.to_string();
        let mut signed_names: Vec<String> = headers.iter().map(|(n, _)| n.clone()).collect();
        signed_names.push("host".into());
        signed_names.sort();
        let x_id = if method == Method::PUT {
            "PutObject"
        } else {
            "GetObject"
        };
        let mut params: Vec<(String, String)> = vec![
            ("X-Amz-Algorithm".into(), ALGORITHM.into()),
            ("X-Amz-Credential".into(), "memory".into()),
            ("X-Amz-Date".into(), date),
            ("X-Amz-Expires".into(), expires.to_string()),
            ("X-Amz-SignedHeaders".into(), signed_names.join(";")),
            (X_ID.into(), x_id.into()),
        ];
        let path = object_path(key);
        let mut header_map = HeaderMap::new();
        for (n, v) in &headers {
            header_map.insert(
                http::HeaderName::from_bytes(n.as_bytes())
                    .map_err(|_| StorageError::Invalid("header"))?,
                v.parse().map_err(|_| StorageError::Invalid("header"))?,
            );
        }
        header_map.insert(
            http::header::HOST,
            host.parse().map_err(|_| StorageError::Invalid("host"))?,
        );
        let signature = signature(
            &self.shared.key,
            method.as_str(),
            &path,
            &params,
            &signed_names,
            &header_map,
        )
        .ok_or(StorageError::Invalid("headers"))?;
        params.push((SIGNATURE.into(), signature));
        let query: Vec<String> = params
            .iter()
            .map(|(k, v)| format!("{k}={}", encode(v)))
            .collect();
        let ttl_ms = i64::try_from(ttl.as_millis()).unwrap_or(i64::MAX);
        Ok(SignedRequest {
            method: method.as_str().to_owned(),
            url: format!("http://{host}{path}?{}", query.join("&")),
            headers,
            expires_ms: now.millis().saturating_add(ttl_ms),
        })
    }
}

#[async_trait]
impl ObjectStorage for MemoryStorage {
    async fn list(&self, prefix: &str) -> Result<Vec<ObjectInfo>, StorageError> {
        self.admit().await?;
        if self.shared.weak(Weakness::StaleListing) {
            return Ok(Vec::new());
        }
        Ok(lock(&self.shared.objects)
            .range(prefix.to_owned()..)
            .take_while(|(k, _)| k.starts_with(prefix))
            .map(|(k, s)| ObjectInfo {
                key: k.clone(),
                size: s.bytes.len() as u64,
                last_modified: s.last_modified,
            })
            .collect())
    }

    async fn presign_get(
        &self,
        key: &str,
        ttl: Duration,
        now: Timestamp,
    ) -> Result<SignedRequest, StorageError> {
        self.sign(&Method::GET, key, Vec::new(), ttl, now)
    }

    async fn presign_put_new(
        &self,
        key: &str,
        len: u64,
        ttl: Duration,
        now: Timestamp,
    ) -> Result<SignedRequest, StorageError> {
        let headers = vec![
            ("content-length".to_owned(), len.to_string()),
            ("if-none-match".to_owned(), "*".to_owned()),
        ];
        self.sign(&Method::PUT, key, headers, ttl, now)
    }

    async fn delete(&self, keys: &[String]) -> Result<(), StorageError> {
        self.admit().await?;
        let mut objects = lock(&self.shared.objects);
        for key in keys {
            objects.remove(key);
        }
        Ok(())
    }
}

/// `/<bucket>/<key>`, each character outside the unreserved set and `/` percent-encoded.
fn object_path(key: &str) -> String {
    let mut out = format!("/{BUCKET}/");
    for part in key.split('/').enumerate() {
        if part.0 > 0 {
            out.push('/');
        }
        out.push_str(&encode(part.1));
    }
    out
}

fn encode(text: &str) -> String {
    let mut out = String::with_capacity(text.len());
    for b in text.bytes() {
        if b.is_ascii_alphanumeric() || matches!(b, b'-' | b'_' | b'.' | b'~') {
            out.push(char::from(b));
        } else {
            out.push_str(&format!("%{b:02X}"));
        }
    }
    out
}

fn decode(text: &str) -> Option<String> {
    let bytes = text.as_bytes();
    let mut out = Vec::with_capacity(bytes.len());
    let mut i = 0;
    while i < bytes.len() {
        match bytes[i] {
            b'%' => {
                let pair = text.get(i + 1..i + 3)?;
                out.push(*hex::decode::<1>(pair)?.first()?);
                i += 3;
            }
            b'+' => return None,
            b => {
                out.push(b);
                i += 1;
            }
        }
    }
    String::from_utf8(out).ok()
}

fn amz_date(t: Timestamp) -> Option<String> {
    let dt = OffsetDateTime::from_unix_timestamp(t.millis().div_euclid(1000)).ok()?;
    dt.format(format_description!(
        "[year][month][day]T[hour][minute][second]Z"
    ))
    .ok()
}

fn parse_amz_date(text: &str) -> Option<Timestamp> {
    let dt = PrimitiveDateTime::parse(
        text,
        format_description!("[year][month][day]T[hour][minute][second]Z"),
    )
    .ok()?;
    Some(Timestamp::from_millis(
        dt.assume_utc().unix_timestamp().checked_mul(1000)?,
    ))
}

/// The HMAC over the method, the path, the query parameters but the signature (sorted by
/// name) and the signed headers' values as the request carries them. `None` when a signed
/// header is missing or repeated.
fn signature(
    key: &hmac::Key,
    method: &str,
    path: &str,
    params: &[(String, String)],
    signed_names: &[String],
    headers: &HeaderMap,
) -> Option<String> {
    let mut sorted: Vec<&(String, String)> =
        params.iter().filter(|(k, _)| k != SIGNATURE).collect();
    sorted.sort();
    let mut text = format!("{method}\n{path}\n");
    for (k, v) in sorted {
        text.push_str(&format!("{k}={v}&"));
    }
    text.push('\n');
    for name in signed_names {
        let mut values = headers.get_all(name.as_str()).iter();
        let value = values.next()?.to_str().ok()?;
        if values.next().is_some() {
            return None;
        }
        text.push_str(&format!("{name}:{value}\n"));
    }
    Some(hex::encode(hmac::sign(key, text.as_bytes()).as_ref()).to_string())
}

async fn serve(listener: TcpListener, shared: Arc<Shared>) {
    let mut connections = JoinSet::new();
    loop {
        while connections.try_join_next().is_some() {}
        let Ok((stream, _)) = listener.accept().await else {
            tokio::time::sleep(Duration::from_millis(10)).await;
            continue;
        };
        let shared = shared.clone();
        connections.spawn(async move {
            let service = hyper::service::service_fn(move |req| {
                let shared = shared.clone();
                async move { Ok::<_, std::convert::Infallible>(handle(&shared, req).await) }
            });
            let _ = hyper::server::conn::http1::Builder::new()
                .timer(TokioTimer::new())
                .header_read_timeout(Duration::from_secs(30))
                .serve_connection(TokioIo::new(stream), service)
                .await;
        });
    }
}

async fn handle(shared: &Shared, req: Request<Incoming>) -> Response<Full<Bytes>> {
    let method = req.method().clone();
    let key = req
        .uri()
        .path()
        .strip_prefix(&format!("/{BUCKET}/"))
        .and_then(decode)
        .unwrap_or_default();
    let header_names = req
        .headers()
        .keys()
        .map(|n| n.as_str().to_owned())
        .collect();
    let response = answer(shared, req).await;
    lock(&shared.seen).push(SeenRequest {
        method: method.as_str().to_owned(),
        key,
        header_names,
        status: response.status().as_u16(),
    });
    response
}

fn status(code: StatusCode) -> Response<Full<Bytes>> {
    let mut r = Response::new(Full::new(Bytes::new()));
    *r.status_mut() = code;
    r
}

async fn answer(shared: &Shared, req: Request<Incoming>) -> Response<Full<Bytes>> {
    let mut rx = shared.fault.subscribe();
    match rx.wait_for(|f| *f != Fault::Stall).await.map(|f| *f) {
        Ok(Fault::None) => {}
        Ok(Fault::Answer(code)) => {
            return status(StatusCode::from_u16(code).unwrap_or(StatusCode::BAD_GATEWAY));
        }
        _ => return status(StatusCode::SERVICE_UNAVAILABLE),
    }
    let path = req.uri().path().to_owned();
    let Some(key) = path.strip_prefix(&format!("/{BUCKET}/")).and_then(decode) else {
        return status(StatusCode::NOT_FOUND);
    };
    let Some(params) = parse_query(req.uri().query().unwrap_or("")) else {
        return status(StatusCode::BAD_REQUEST);
    };
    let now = (shared.clock)();
    if !shared.weak(Weakness::Unsigned)
        && let Err(code) = authorize(shared, &req, &path, &params, now)
    {
        return status(code);
    }
    match *req.method() {
        Method::GET => match lock(&shared.objects).get(&key) {
            Some(stored) => Response::new(Full::new(stored.bytes.clone())),
            None => status(StatusCode::NOT_FOUND),
        },
        Method::PUT => put(shared, key, req, now).await,
        _ => status(StatusCode::METHOD_NOT_ALLOWED),
    }
}

/// Checks a request's signature and expiry; the status to refuse it with.
fn authorize(
    shared: &Shared,
    req: &Request<Incoming>,
    path: &str,
    params: &[(String, String)],
    now: i64,
) -> Result<(), StatusCode> {
    let one = |name: &str| {
        let mut it = params.iter().filter(|(k, _)| k == name);
        match (it.next(), it.next()) {
            (Some((_, v)), None) => Some(v.clone()),
            _ => None,
        }
    };
    let known = |k: &str| SIGNED_PARAMS.contains(&k) || k == SIGNATURE || k == X_ID;
    if params.iter().any(|(k, _)| !known(k)) {
        return Err(StatusCode::BAD_REQUEST);
    }
    let (Some(algorithm), Some(date), Some(expires), Some(names), Some(given)) = (
        one("X-Amz-Algorithm"),
        one("X-Amz-Date"),
        one("X-Amz-Expires"),
        one("X-Amz-SignedHeaders"),
        one(SIGNATURE),
    ) else {
        return Err(StatusCode::FORBIDDEN);
    };
    if one("X-Amz-Credential").is_none() || algorithm != ALGORITHM {
        return Err(StatusCode::FORBIDDEN);
    }
    let x_id = match params.iter().filter(|(k, _)| k == X_ID).count() {
        0 => None,
        1 => one(X_ID),
        _ => return Err(StatusCode::BAD_REQUEST),
    };
    let expected_id = if req.method() == Method::PUT {
        "PutObject"
    } else {
        "GetObject"
    };
    if x_id.is_some_and(|id| id != expected_id) {
        return Err(StatusCode::BAD_REQUEST);
    }
    let (Some(signed_at), Ok(expires)) = (parse_amz_date(&date), expires.parse::<i64>()) else {
        return Err(StatusCode::FORBIDDEN);
    };
    if !shared.weak(Weakness::NoExpiry)
        && signed_at
            .millis()
            .saturating_add(expires.saturating_mul(1000))
            <= now
    {
        return Err(StatusCode::FORBIDDEN);
    }
    let signed_names: Vec<String> = names.split(';').map(str::to_owned).collect();
    if !signed_names.iter().any(|n| n == "host") {
        return Err(StatusCode::FORBIDDEN);
    }
    let Some(expected) = signature(
        &shared.key,
        req.method().as_str(),
        path,
        params,
        &signed_names,
        req.headers(),
    ) else {
        return Err(StatusCode::FORBIDDEN);
    };
    if verify_slices_are_equal(expected.as_bytes(), given.as_bytes()).is_err() {
        return Err(StatusCode::FORBIDDEN);
    }
    Ok(())
}

async fn put(
    shared: &Shared,
    key: String,
    req: Request<Incoming>,
    now: i64,
) -> Response<Full<Bytes>> {
    let Some(len) = req
        .headers()
        .get(http::header::CONTENT_LENGTH)
        .and_then(|v| v.to_str().ok())
        .and_then(|v| v.parse::<usize>().ok())
    else {
        return status(StatusCode::LENGTH_REQUIRED);
    };
    if len > MAX_OBJECT {
        return status(StatusCode::PAYLOAD_TOO_LARGE);
    }
    let overwrites = shared.weak(Weakness::Overwrites);
    if !overwrites && lock(&shared.objects).contains_key(&key) {
        return status(StatusCode::PRECONDITION_FAILED);
    }
    let Ok(body) = Limited::new(req.into_body(), len).collect().await else {
        return status(StatusCode::BAD_REQUEST);
    };
    let bytes = body.to_bytes();
    if bytes.len() != len {
        return status(StatusCode::BAD_REQUEST);
    }
    let mut objects = lock(&shared.objects);
    if !overwrites && objects.contains_key(&key) {
        return status(StatusCode::PRECONDITION_FAILED);
    }
    objects.insert(
        key,
        Stored {
            bytes,
            last_modified: Timestamp::from_millis(now),
        },
    );
    status(StatusCode::OK)
}

fn parse_query(query: &str) -> Option<Vec<(String, String)>> {
    if query.is_empty() {
        return Some(Vec::new());
    }
    query
        .split('&')
        .map(|pair| {
            let (k, v) = pair.split_once('=')?;
            Some((decode(k)?, decode(v)?))
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use hyper_util::client::legacy::Client;
    use hyper_util::rt::TokioExecutor;
    use knoggin_state::testing::TestClock;

    use super::*;

    const TTL: Duration = Duration::from_secs(300);

    /// Sends `req` as given, with `body`, after `edit` changes it. Returns the status, or
    /// `None` when the connection failed.
    async fn send_with(
        req: &SignedRequest,
        body: &[u8],
        edit: impl FnOnce(&mut Request<Full<Bytes>>),
    ) -> Option<(u16, Bytes)> {
        let mut r = Request::builder()
            .method(req.method.as_str())
            .uri(&req.url)
            .body(Full::new(Bytes::copy_from_slice(body)))
            .unwrap();
        for (n, v) in &req.headers {
            r.headers_mut().insert(
                http::HeaderName::from_bytes(n.as_bytes()).unwrap(),
                v.parse().unwrap(),
            );
        }
        edit(&mut r);
        let client = Client::builder(TokioExecutor::new()).build_http::<Full<Bytes>>();
        let resp = client.request(r).await.ok()?;
        let status = resp.status().as_u16();
        Some((status, resp.into_body().collect().await.ok()?.to_bytes()))
    }

    async fn send(req: &SignedRequest, body: &[u8]) -> Option<u16> {
        send_with(req, body, |_| {}).await.map(|(s, _)| s)
    }

    fn now(clock: &TestClock) -> Timestamp {
        Timestamp::from_millis(clock.now())
    }

    #[tokio::test]
    async fn links_upload_once_download_and_list() {
        let clock = TestClock::new();
        let st = MemoryStorage::start(clock.clock()).await.unwrap();
        let key = "stores/s/snap/0000000001-0000000001.kgs";
        let put = st.presign_put_new(key, 5, TTL, now(&clock)).await.unwrap();
        assert!(
            put.url
                .starts_with(&format!("{}/{BUCKET}/{key}?", st.endpoint()))
        );
        assert!(
            put.url
                .contains("X-Amz-SignedHeaders=content-length%3Bhost%3Bif-none-match")
        );
        assert!(put.url.contains("x-id=PutObject"));
        assert_eq!(send(&put, b"hello").await, Some(200));
        // The same link again, and a new link for the same name: never overwritten.
        assert_eq!(send(&put, b"hello").await, Some(412));
        let again = st.presign_put_new(key, 5, TTL, now(&clock)).await.unwrap();
        assert_eq!(send(&again, b"HELLO").await, Some(412));
        let get = st.presign_get(key, TTL, now(&clock)).await.unwrap();
        let (status, body) = send_with(&get, b"", |_| {}).await.unwrap();
        assert_eq!((status, &body[..]), (200, &b"hello"[..]));
        let missing = st
            .presign_get("stores/none", TTL, now(&clock))
            .await
            .unwrap();
        assert_eq!(send(&missing, b"").await, Some(404));
        let listed = st.list("stores/s/").await.unwrap();
        assert_eq!(
            listed,
            vec![ObjectInfo {
                key: key.into(),
                size: 5,
                last_modified: now(&clock)
            }]
        );
        assert!(st.list("stores/t").await.unwrap().is_empty());
        st.delete(&[key.to_owned(), "stores/none".into()])
            .await
            .unwrap();
        assert!(st.keys().is_empty());
        let seen = st.seen();
        assert_eq!(seen[0].method, "PUT");
        assert_eq!(seen[0].key, key);
        assert!(seen[0].header_names.contains(&"if-none-match".to_owned()));
    }

    #[tokio::test]
    async fn every_signed_part_is_checked() {
        let clock = TestClock::new();
        let st = MemoryStorage::start(clock.clock()).await.unwrap();
        let put = st
            .presign_put_new("selftest/a", 3, TTL, now(&clock))
            .await
            .unwrap();
        let without_condition = send_with(&put, b"abc", |r| {
            r.headers_mut().remove(http::header::IF_NONE_MATCH);
        });
        assert_eq!(without_condition.await.map(|r| r.0), Some(403));
        let other_length = send_with(&put, b"abcd", |r| {
            r.headers_mut()
                .insert(http::header::CONTENT_LENGTH, "4".parse().unwrap());
        });
        assert_eq!(other_length.await.map(|r| r.0), Some(403));
        let rewrite = |from: &str, to: &str| SignedRequest {
            url: put.url.replace(from, to),
            ..put.clone()
        };
        let sig = query_value(&put.url, SIGNATURE);
        let unsigned = rewrite(&format!("&{SIGNATURE}={sig}"), "");
        assert!(!unsigned.url.contains(SIGNATURE));
        assert_eq!(send(&unsigned, b"abc").await, Some(403));
        let flipped = if sig.starts_with('0') { "1" } else { "0" };
        let tampered = rewrite(&format!("={sig}"), &format!("={flipped}{}", &sig[1..]));
        assert_eq!(send(&tampered, b"abc").await, Some(403));
        let other_key = rewrite("/selftest/a?", "/selftest/b?");
        assert_eq!(send(&other_key, b"abc").await, Some(403));
        let extra = SignedRequest {
            url: format!("{}&acl=1", put.url),
            ..put.clone()
        };
        assert_eq!(send(&extra, b"abc").await, Some(400));
        let repeated = SignedRequest {
            url: format!("{}&x-id=PutObject", put.url),
            ..put.clone()
        };
        assert_eq!(send(&repeated, b"abc").await, Some(400));
        let as_get = SignedRequest {
            method: "GET".into(),
            ..put.clone()
        };
        assert_eq!(send(&as_get, b"").await, Some(400));
        assert!(st.keys().is_empty());
        assert_eq!(send(&put, b"abc").await, Some(200));
    }

    fn query_value(url: &str, name: &str) -> String {
        url.split_once('?')
            .unwrap()
            .1
            .split('&')
            .find_map(|p| p.strip_prefix(&format!("{name}=")))
            .unwrap()
            .to_owned()
    }

    #[tokio::test]
    async fn links_expire() {
        let clock = TestClock::new();
        let st = MemoryStorage::start(clock.clock()).await.unwrap();
        // Signed as of 10 seconds ago with a 1-second lifetime: refused at once.
        let early = Timestamp::from_millis(clock.now() - 10_000);
        let stale = st
            .presign_put_new("selftest/x", 1, Duration::from_secs(1), early)
            .await
            .unwrap();
        assert_eq!(send(&stale, b"x").await, Some(403));
        let fresh = st
            .presign_put_new("selftest/x", 1, Duration::from_secs(60), now(&clock))
            .await
            .unwrap();
        clock.advance(60_000);
        assert_eq!(send(&fresh, b"x").await, Some(403));
        let get = st
            .presign_get("selftest/x", Duration::from_secs(60), now(&clock))
            .await
            .unwrap();
        clock.advance(59_000);
        assert_eq!(send(&get, b"").await, Some(404));
        assert!(
            st.presign_get("k", Duration::ZERO, now(&clock))
                .await
                .is_err()
        );
    }

    #[tokio::test]
    async fn switches_fail_stall_and_go_away() {
        let clock = TestClock::new();
        let st = MemoryStorage::start(clock.clock()).await.unwrap();
        st.insert("stores/a", &b"x"[..], now(&clock));
        let get = st.presign_get("stores/a", TTL, now(&clock)).await.unwrap();

        st.set_fault(Fault::Fail).await.unwrap();
        assert_eq!(send(&get, b"").await, Some(503));
        assert_eq!(st.list("stores/").await, Err(StorageError::Unavailable));
        assert_eq!(st.delete(&[]).await, Err(StorageError::Unavailable));

        st.set_fault(Fault::Stall).await.unwrap();
        let held = tokio::spawn({
            let get = get.clone();
            async move { send(&get, b"").await }
        });
        let listing = st.list("stores/");
        tokio::pin!(listing);
        // A short real wait: nothing comes back while the switch is on.
        assert!(
            tokio::time::timeout(Duration::from_millis(200), &mut listing)
                .await
                .is_err()
        );
        assert!(!held.is_finished());
        st.set_fault(Fault::None).await.unwrap();
        assert_eq!(held.await.unwrap(), Some(200));
        assert_eq!(listing.await.unwrap().len(), 1);

        st.set_fault(Fault::Gone).await.unwrap();
        assert_eq!(send(&get, b"").await, None);
        assert_eq!(st.list("stores/").await, Err(StorageError::Unavailable));
        st.set_fault(Fault::None).await.unwrap();
        assert_eq!(send(&get, b"").await, Some(200));
        assert_eq!(st.get("stores/a").as_deref(), Some(&b"x"[..]));
    }
}
