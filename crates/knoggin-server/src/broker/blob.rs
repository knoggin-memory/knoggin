// SPDX-License-Identifier: AGPL-3.0-only
//! The one-host blob route: `GET` and `PUT /internal/v1/blob/<object key>` of the
//! broker. A user container has no road to the bucket; its signed links point here,
//! and the router forwards each request to the deployment's own bucket, for an object of
//! a store the caller holds only, with the signed query and the signed headers and nothing
//! else. The bytes stream through; nothing is kept.

use std::sync::Arc;

use axum::body::Body;
use axum::extract::{Extension, Request, State};
use axum::http::{HeaderMap, HeaderName, HeaderValue, Method, StatusCode, header};
use axum::response::{IntoResponse, Response};
use futures_util::StreamExt;
use knoggin_http::ctx::headers;
use knoggin_http::error::Code;

use super::api::paths;
use super::{Broker, BrokerError, check, held_store, storage_failed};
use crate::link::PeerAddr;
use crate::link::idle_limited;
use crate::storage::names;

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
        || name == headers::SLOT.as_str()
        || name == headers::CONTAINER_ID.as_str()
        || name == headers::PROTOCOL.as_str()
        || name == header::HOST.as_str()
}

/// The headers that go to the bucket: those the signature names, each sent once by the
/// caller (else `None`), without the link headers, and with `length` as the one
/// `Content-Length` of an upload.
fn outbound_headers(signed: &[String], from: &HeaderMap, length: Option<u64>) -> Option<HeaderMap> {
    let mut out = HeaderMap::new();
    for name in signed {
        if is_link_header(name) || name == header::CONTENT_LENGTH.as_str() {
            continue;
        }
        let mut values = from.get_all(name.as_str()).iter();
        let (Some(value), None) = (values.next(), values.next()) else {
            return None;
        };
        out.insert(HeaderName::from_bytes(name.as_bytes()).ok()?, value.clone());
    }
    if let Some(len) = length {
        out.insert(header::CONTENT_LENGTH, HeaderValue::from(len));
    }
    Some(out)
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
    let refused = BrokerError::invalid;
    // The raw path, before any decoding: one pattern and nothing else.
    let key = req
        .uri()
        .path()
        .strip_prefix(paths::BLOB_PREFIX)
        .ok_or_else(refused)?
        .to_owned();
    let (store, _) = names::parse_snapshot_key(&key).ok_or_else(refused)?;
    let record = held_store(&broker, &checked, &store.to_string()).await?;
    // The hourly download budget belongs to the store's owner.
    let owner = record.value.user_id;
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
    } else if !broker.downloads.has_room(owner) {
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
    let headers = outbound_headers(&signed_names, req.headers(), length).ok_or_else(refused)?;
    outbound = outbound.headers(headers);
    let call = checked.call;
    if length.is_some() {
        let body = idle_limited(req.into_body().into_data_stream(), ());
        outbound = outbound.body(reqwest::Body::wrap_stream(body));
    }
    let answer = outbound.send().await.map_err(|_| {
        tracing::warn!(
            kind = "bucket",
            "a blob transfer could not reach the bucket"
        );
        BrokerError::unavailable()
    })?;
    // Only the broker's own answers may mean deleted or lease lost: a bucket's refusal
    // other than 412 (the name exists) is an outage to the container.
    let status = match answer.status().as_u16() {
        code @ (200..=299 | 412) => {
            StatusCode::from_u16(code).map_err(|_| BrokerError::unavailable())?
        }
        code => {
            tracing::warn!(
                kind = "bucket",
                status = code,
                "the bucket refused a blob transfer"
            );
            return Err(BrokerError::unavailable());
        }
    };
    let content_length = answer.content_length();
    let counted = broker.clone();
    let stream = answer.bytes_stream().map(move |chunk| {
        if method == Method::GET
            && let Ok(bytes) = &chunk
        {
            counted.downloads.add(owner, bytes.len() as u64);
        }
        chunk
    });
    let mut response = Body::from_stream(idle_limited(stream, call)).into_response();
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

    #[test]
    fn an_upload_sends_content_length_once() {
        let mut from = HeaderMap::new();
        from.insert(header::CONTENT_LENGTH, HeaderValue::from(5));
        from.insert(header::IF_NONE_MATCH, HeaderValue::from_static("*"));
        from.insert(header::AUTHORIZATION, HeaderValue::from_static("Bearer t"));
        from.insert(header::HOST, HeaderValue::from_static("router:8443"));
        from.insert("x-extra", HeaderValue::from_static("1"));
        let signed: Vec<String> = ["content-length", "host", "if-none-match", "authorization"]
            .map(String::from)
            .to_vec();
        let out = outbound_headers(&signed, &from, Some(5)).unwrap();
        let request = reqwest::Client::new()
            .put("http://bucket.test/k")
            .headers(out)
            .body("12345")
            .build()
            .unwrap();
        // What hyper writes to the bucket: every value in the map, so one length.
        let lengths: Vec<_> = request
            .headers()
            .get_all(header::CONTENT_LENGTH)
            .iter()
            .collect();
        assert_eq!(lengths, [HeaderValue::from(5)]);
        let mut names: Vec<_> = request.headers().keys().map(|n| n.as_str()).collect();
        names.sort_unstable();
        assert_eq!(names, ["content-length", "if-none-match"]);
        // A signed header sent twice is refused; a download sends no length.
        from.append(header::IF_NONE_MATCH, HeaderValue::from_static("*"));
        assert!(outbound_headers(&signed, &from, Some(5)).is_none());
        let get = outbound_headers(&["host".into()], &from, None).unwrap();
        assert!(get.is_empty());
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
        assert!(is_link_header("knoggin-slot"));
    }
}
