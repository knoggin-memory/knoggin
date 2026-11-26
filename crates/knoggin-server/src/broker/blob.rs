// SPDX-License-Identifier: AGPL-3.0-only
//! The one-host blob route: `GET` and `PUT /internal/v1/blob/<object key>` (tab "Server",
//! the broker). A user container has no road to the bucket; its signed links point here,
//! and the router forwards each request to the deployment's own bucket, for an object of
//! the caller's own stores only, with the signed query and the signed headers and nothing
//! else. The bytes stream through; nothing is kept.

use std::sync::Arc;
use std::time::Duration;

use axum::body::Body;
use axum::extract::{Extension, Request, State};
use axum::http::{HeaderValue, Method, StatusCode, header};
use axum::response::{IntoResponse, Response};
use bytes::Bytes;
use futures_util::{Stream, StreamExt};
use knoggin_http::ctx::headers;
use knoggin_http::error::Code;

use super::api::paths;
use super::{Broker, BrokerError, check, storage_failed, store_of};
use crate::link::PeerAddr;
use crate::storage::names;

/// A transfer ends when no bytes moved for this long.
pub(crate) const IDLE: Duration = Duration::from_secs(60);

/// The query parameters a link carries; any other, or one twice, is refused.
const SIGNED_PARAMS: [&str; 7] = [
    "X-Amz-Algorithm",
    "X-Amz-Credential",
    "X-Amz-Date",
    "X-Amz-Expires",
    "X-Amz-SignedHeaders",
    "X-Amz-Security-Token",
    "X-Amz-Signature",
];
const X_ID: &str = "x-id";

/// The checked query: its parts in their order, and the signed header names.
fn checked_query(query: &str, method: &Method) -> Option<(String, Vec<String>)> {
    let mut seen: Vec<&str> = Vec::new();
    let mut signed = None;
    for pair in query.split('&') {
        let (name, value) = pair.split_once('=')?;
        if seen.contains(&name) {
            return None;
        }
        seen.push(name);
        if name == X_ID {
            let expected = if method == Method::PUT {
                "PutObject"
            } else {
                "GetObject"
            };
            if value != expected {
                return None;
            }
        } else if !SIGNED_PARAMS.contains(&name) {
            return None;
        } else if name == "X-Amz-SignedHeaders" {
            signed = Some(
                value
                    .replace("%3B", ";")
                    .replace("%3b", ";")
                    .split(';')
                    .map(str::to_ascii_lowercase)
                    .collect::<Vec<_>>(),
            );
        }
    }
    Some((query.to_owned(), signed?))
}

/// The headers that never go to the bucket, whatever the signature names: the caller's
/// identity on the link.
fn is_link_header(name: &str) -> bool {
    name == header::AUTHORIZATION.as_str()
        || name == headers::USER_ID.as_str()
        || name == headers::CONTAINER_ID.as_str()
        || name == headers::PROTOCOL.as_str()
        || name == header::HOST.as_str()
}

/// Ends a byte stream with an error when nothing arrives for [`IDLE`], and keeps `guard`
/// for as long as the stream lives.
pub(crate) fn idle_limited<S, E, G>(
    stream: S,
    guard: G,
) -> impl Stream<Item = Result<Bytes, std::io::Error>> + Send + 'static
where
    S: Stream<Item = Result<Bytes, E>> + Send + 'static,
    E: std::fmt::Display + Send + 'static,
    G: Send + 'static,
{
    futures_util::stream::unfold(Some((Box::pin(stream), guard)), |state| async move {
        let (mut stream, guard) = state?;
        match tokio::time::timeout(IDLE, stream.next()).await {
            Ok(Some(Ok(bytes))) => Some((Ok(bytes), Some((stream, guard)))),
            Ok(None) => None,
            Ok(Some(Err(_))) => Some((Err(std::io::Error::other("the transfer broke")), None)),
            Err(_) => Some((
                Err(std::io::Error::new(
                    std::io::ErrorKind::TimedOut,
                    "no bytes moved",
                )),
                None,
            )),
        }
    })
}

pub(super) async fn blob(
    State(broker): State<Arc<Broker>>,
    Extension(peer): Extension<PeerAddr>,
    req: Request,
) -> Result<Response, BrokerError> {
    let method = req.method().clone();
    if method != Method::GET && method != Method::PUT {
        return Err(BrokerError::api(Code::NotFound));
    }
    let checked = check(&broker, peer, req.headers()).await?;
    let user = checked.caller.user_id;
    let refused = BrokerError::invalid;
    // The raw path, before any decoding: one pattern and nothing else.
    let key = req
        .uri()
        .path()
        .strip_prefix(paths::BLOB_PREFIX)
        .ok_or_else(refused)?
        .to_owned();
    let (store, _) = names::parse_snapshot_key(&key).ok_or_else(refused)?;
    store_of(&broker, user, &store.to_string()).await?;
    let (query, signed_names) =
        checked_query(req.uri().query().unwrap_or(""), &method).ok_or_else(refused)?;
    let mut length = None;
    if method == Method::PUT {
        if req.headers().contains_key(header::TRANSFER_ENCODING) {
            return Err(refused());
        }
        let len: u64 = req
            .headers()
            .get(header::CONTENT_LENGTH)
            .and_then(|v| v.to_str().ok())
            .and_then(|v| v.parse().ok())
            .ok_or_else(refused)?;
        if len > broker.size_bound() {
            return Err(refused());
        }
        length = Some(len);
    } else if !broker.downloads.has_room(user) {
        return Err(BrokerError::api(Code::RateLimited));
    }
    // The bucket's address comes from the router's own settings: a fresh signature of the
    // same object gives it, and only its part before the query is used.
    let ttl = broker.config.link_ttl;
    let fresh = match length {
        Some(len) => {
            broker
                .config
                .storage
                .presign_put_new(&key, len, ttl, broker.now())
                .await
        }
        None => {
            broker
                .config
                .storage
                .presign_get(&key, ttl, broker.now())
                .await
        }
    }
    .map_err(storage_failed)?;
    let base = fresh
        .url
        .split_once('?')
        .map_or(fresh.url.as_str(), |(b, _)| b);
    let mut outbound = broker
        .bucket
        .request(method.clone(), format!("{base}?{query}"));
    for name in &signed_names {
        if is_link_header(name) {
            continue;
        }
        let mut values = req.headers().get_all(name.as_str()).iter();
        let (Some(value), None) = (values.next(), values.next()) else {
            return Err(refused());
        };
        outbound = outbound.header(name.as_str(), value);
    }
    let slot = checked.slot;
    if let Some(len) = length {
        let body = idle_limited(req.into_body().into_data_stream(), ());
        outbound = outbound
            .header(header::CONTENT_LENGTH, len)
            .body(reqwest::Body::wrap_stream(body));
    }
    let answer = outbound.send().await.map_err(|_| {
        tracing::warn!(
            kind = "bucket",
            "a blob transfer could not reach the bucket"
        );
        BrokerError::unavailable()
    })?;
    let status = StatusCode::from_u16(answer.status().as_u16()).unwrap_or(StatusCode::BAD_GATEWAY);
    let content_length = answer.content_length();
    let counted = broker.clone();
    let stream = answer.bytes_stream().map(move |chunk| {
        if method == Method::GET
            && let Ok(bytes) = &chunk
        {
            counted.downloads.add(user, bytes.len() as u64);
        }
        chunk
    });
    let mut response = Body::from_stream(idle_limited(stream, slot)).into_response();
    *response.status_mut() = status;
    if let Some(len) = content_length {
        response
            .headers_mut()
            .insert(header::CONTENT_LENGTH, HeaderValue::from(len));
    }
    Ok(response)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test(start_paused = true)]
    async fn a_transfer_ends_after_a_minute_without_bytes() {
        let started = tokio::time::Instant::now();
        let first =
            futures_util::stream::once(async { Ok::<_, std::io::Error>(Bytes::from_static(b"x")) });
        let mut s = Box::pin(idle_limited(
            first.chain(futures_util::stream::pending()),
            (),
        ));
        assert_eq!(s.next().await.unwrap().unwrap(), Bytes::from_static(b"x"));
        let err = s.next().await.unwrap().unwrap_err();
        assert_eq!(err.kind(), std::io::ErrorKind::TimedOut);
        assert!(started.elapsed() >= IDLE);
        assert!(s.next().await.is_none(), "nothing after the error");
    }

    #[test]
    fn only_the_signed_parameters_once_each() {
        let q = "X-Amz-Algorithm=A&X-Amz-Credential=c&X-Amz-Date=d&X-Amz-Expires=300\
                 &X-Amz-SignedHeaders=content-length%3Bhost%3Bif-none-match&X-Amz-Signature=s\
                 &x-id=PutObject";
        let (rebuilt, names) = checked_query(q, &Method::PUT).unwrap();
        assert_eq!(rebuilt, q);
        assert_eq!(names, ["content-length", "host", "if-none-match"]);
        assert!(checked_query(q, &Method::GET).is_none(), "x-id of a PUT");
        assert!(checked_query(&format!("{q}&extra=1"), &Method::PUT).is_none());
        assert!(
            checked_query(&format!("{q}&X-Amz-Date=e"), &Method::PUT).is_none(),
            "repeated"
        );
        assert!(checked_query(&format!("{q}&flag"), &Method::PUT).is_none());
        assert!(checked_query("X-Amz-Signature=s", &Method::GET).is_none());
        assert!(is_link_header("authorization") && is_link_header("knoggin-user-id"));
    }
}
