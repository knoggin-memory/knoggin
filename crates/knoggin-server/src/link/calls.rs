// SPDX-License-Identifier: AGPL-3.0-only
//! Who may call on the link. A container requires the router token on every call but
//! `hello`; a router recomputes the container token from the `Knoggin-Container-Id` that
//! the call names. Tokens are compared in constant time. The headers themselves are read
//! and written by `knoggin_http::ctx`.

use http::HeaderMap;
use knoggin_http::ctx;
use knoggin_state::record::Slot;
use uuid::Uuid;

use super::{InternalSecret, LaunchSecret, Token};

/// Why a call on the link was refused. The callee answers `internal` for a missing or
/// unknown protocol version; the broker turns the rest into its own answers.
#[derive(Debug, Clone, Copy, PartialEq, Eq, thiserror::Error)]
pub enum CallRefused {
    /// No `Knoggin-Protocol`, sent twice, or a version this build does not speak.
    #[error("unknown internal protocol version")]
    Protocol,
    /// The identity headers are missing, repeated or not UUIDs.
    #[error("missing caller identity")]
    Identity,
    /// No token, or the wrong one.
    #[error("wrong token")]
    Token,
}

/// The identity a container's call proved: the slot whose lease should name it, and
/// itself. The broker still checks the lease, its state and the caller's address.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ContainerCaller {
    /// `Knoggin-Slot`.
    pub slot: Slot,
    /// `Knoggin-Container-Id`.
    pub container_id: Uuid,
}

/// The router's check of a container's call: the protocol, the two identity headers, and
/// the container token recomputed from `internal.secret` and the named container.
pub fn verify_container_call(
    secret: &InternalSecret,
    headers: &HeaderMap,
) -> Result<ContainerCaller, CallRefused> {
    if !ctx::link_protocol_ok(headers) {
        return Err(CallRefused::Protocol);
    }
    let (slot, container_id) = ctx::link_identity(headers).ok_or(CallRefused::Identity)?;
    let presented = ctx::link_bearer(headers).ok_or(CallRefused::Token)?;
    if !secret
        .launch_secret(container_id)
        .container_token()
        .matches(presented)
    {
        return Err(CallRefused::Token);
    }
    Ok(ContainerCaller { slot, container_id })
}

/// The container's check of a router's call (every route but `hello`): the protocol and
/// the router token.
pub fn verify_router_call(router_token: &Token, headers: &HeaderMap) -> Result<(), CallRefused> {
    if !ctx::link_protocol_ok(headers) {
        return Err(CallRefused::Protocol);
    }
    match ctx::link_bearer(headers) {
        Some(presented) if router_token.matches(presented) => Ok(()),
        _ => Err(CallRefused::Token),
    }
}

/// Writes a container's identity on its call to a router, from its launch secret.
pub fn write_container_call(
    headers: &mut HeaderMap,
    launch: &LaunchSecret,
    slot: Slot,
    container_id: Uuid,
) {
    ctx::write_link_identity(
        headers,
        slot,
        container_id,
        launch.container_token().bearer(),
    );
}

/// Writes the router token and the protocol on a router's call to a container.
pub fn write_router_call(headers: &mut HeaderMap, router_token: &Token) {
    ctx::write_link_bearer(headers, router_token.bearer());
}

#[cfg(test)]
mod tests {
    use http::HeaderValue;

    use super::*;

    #[test]
    fn a_container_proves_itself_with_its_own_token_only() {
        let secret = InternalSecret::from_bytes([3u8; 32]);
        let (a, b) = (Uuid::now_v7(), Uuid::now_v7());
        let user = Slot::User(knoggin_core::UserId::from_uuid(Uuid::now_v7()));
        let mut headers = HeaderMap::new();
        write_container_call(&mut headers, &secret.launch_secret(a), user, a);
        assert_eq!(
            verify_container_call(&secret, &headers),
            Ok(ContainerCaller {
                slot: user,
                container_id: a
            })
        );
        // A's token naming container B.
        let mut swapped = headers.clone();
        swapped.insert(
            ctx::headers::CONTAINER_ID,
            HeaderValue::from_str(&b.to_string()).unwrap(),
        );
        assert_eq!(
            verify_container_call(&secret, &swapped),
            Err(CallRefused::Token)
        );
        // Another deployment's secret.
        let other = InternalSecret::from_bytes([4u8; 32]);
        assert_eq!(
            verify_container_call(&other, &headers),
            Err(CallRefused::Token)
        );
        let mut no_protocol = headers.clone();
        no_protocol.remove(ctx::headers::PROTOCOL);
        assert_eq!(
            verify_container_call(&secret, &no_protocol),
            Err(CallRefused::Protocol)
        );
        let mut no_user = headers.clone();
        no_user.remove(ctx::headers::SLOT);
        assert_eq!(
            verify_container_call(&secret, &no_user),
            Err(CallRefused::Identity)
        );
        let mut no_token = headers;
        no_token.remove(http::header::AUTHORIZATION);
        assert_eq!(
            verify_container_call(&secret, &no_token),
            Err(CallRefused::Token)
        );
    }

    #[test]
    fn a_container_takes_only_its_router_token() {
        let launch = InternalSecret::from_bytes([5u8; 32]).launch_secret(Uuid::now_v7());
        let mut headers = HeaderMap::new();
        write_router_call(&mut headers, &launch.router_token());
        assert_eq!(verify_router_call(&launch.router_token(), &headers), Ok(()));
        // The container's own token is not the router's.
        let mut wrong = HeaderMap::new();
        write_router_call(&mut wrong, &launch.container_token());
        assert_eq!(
            verify_router_call(&launch.router_token(), &wrong),
            Err(CallRefused::Token)
        );
        headers.remove(ctx::headers::PROTOCOL);
        assert_eq!(
            verify_router_call(&launch.router_token(), &headers),
            Err(CallRefused::Protocol)
        );
    }
}
