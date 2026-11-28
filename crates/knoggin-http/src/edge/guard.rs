// SPDX-License-Identifier: AGPL-3.0-only
//! The router's start-up guards for sign-in, as plain functions so that they are tested
//! without a router. The daemon's own guards are in the binary.

use knoggin_state::users::LOCAL_IDENTITY;
use knoggin_state::{StateError, Users};

use super::{AuthMode, EdgeConfig};

/// Whether `url` is `https`, or plain `http` on a loopback address in test mode. The
/// error says which rule it breaks.
pub fn check_public_url(url: &str, test_mode: bool) -> Result<(), String> {
    let parsed =
        reqwest::Url::parse(url).map_err(|_| format!("server.public_url {url:?} is not a URL"))?;
    let loopback = parsed.host_str().is_some_and(|h| {
        h.eq_ignore_ascii_case("localhost")
            || h.trim_start_matches('[')
                .trim_end_matches(']')
                .parse::<std::net::IpAddr>()
                .is_ok_and(|ip| ip.is_loopback())
    });
    if test_mode && !loopback {
        return Err("test mode refuses a server.public_url that is not a loopback address".into());
    }
    match parsed.scheme() {
        "https" => Ok(()),
        "http" if test_mode => Ok(()),
        _ => Err(
            "server.public_url must be https (plain http only in test mode, on a loopback address)"
                .into(),
        ),
    }
}

/// The router's checks on its settings: the warnings to log at start, or why it refuses
/// to start.
pub fn router_settings(c: &EdgeConfig) -> Result<Vec<&'static str>, String> {
    let url = c
        .public_url
        .as_deref()
        .ok_or("the router needs server.public_url")?;
    check_public_url(url, c.test_mode)?;
    let mut warnings = Vec::new();
    match (c.auth_mode, &c.oauth) {
        (AuthMode::None, _) => warnings.push(
            "sign-in is off (auth.mode = \"none\"): anyone who can reach this server can read and write its memories",
        ),
        (AuthMode::Oauth, None) => {
            return Err(
                "auth.mode = \"oauth\" needs auth.issuer, auth.web_client_id and auth.web_client_secret"
                    .into(),
            );
        }
        (AuthMode::Oauth, Some(o)) => {
            if o.issuer.is_empty()
                || o.web_client_id.is_empty()
                || o.web_client_secret.expose().is_empty()
            {
                return Err("auth.mode = \"oauth\" needs auth.issuer, auth.web_client_id and auth.web_client_secret".into());
            }
            if !o.admin_emails.is_empty() && o.email_claim.is_some() {
                return Err("auth.admin_emails and auth.email_claim cannot both be set: an email from a claim is not one the provider verified; use auth.admin_subjects".into());
            }
            if !o.allow_list_set() {
                warnings.push(
                    "no allow-list is set: the identity provider alone decides who can sign in",
                );
            }
        }
    }
    Ok(warnings)
}

/// With sign-in off, the router refuses to start while the state database holds any user
/// other than the local one: every caller would act as that one user, an admin, and could
/// list and remove the others.
pub async fn router_users(c: &EdgeConfig, users: &Users) -> Result<(), String> {
    if c.auth_mode != AuthMode::None {
        return Ok(());
    }
    let mut others = 0usize;
    let mut after = None;
    loop {
        let page = users
            .list(200, after)
            .await
            .map_err(|e: StateError| format!("reading the users: {e}"))?;
        others += page
            .iter()
            .filter(|u| u.value.issuer != LOCAL_IDENTITY)
            .count();
        match page.last() {
            Some(u) if page.len() == 200 => after = Some(u.value.id),
            _ => break,
        }
    }
    if others > 0 {
        return Err(format!(
            "sign-in is off, but the state database holds {others} users besides the local one; turn sign-in back on (auth.mode = \"oauth\")"
        ));
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use std::sync::Arc;

    use knoggin_core::{Sensitive, StoreId, UserId};
    use knoggin_crypto::KeyProvider;
    use knoggin_state::testing::{TestClock, test_field_keys};
    use knoggin_state::{LocalFileDb, NewStore, NewUser, StateDb};
    use uuid::Uuid;

    use super::*;
    use crate::edge::OauthConfig;

    fn oauth() -> OauthConfig {
        OauthConfig {
            issuer: "https://idp.example".into(),
            audiences: vec![],
            provider_name: "x".into(),
            web_client_id: "c".into(),
            web_client_secret: Sensitive::new("s".into()),
            admin_emails: vec![],
            admin_subjects: vec![],
            allowed_emails: vec![],
            allowed_email_domains: vec![],
            email_claim: None,
            session_max_days: 7,
            session_idle_hours: 24,
        }
    }

    fn server(mode: AuthMode, url: &str, test_mode: bool) -> EdgeConfig {
        let mut c = EdgeConfig::local("http://127.0.0.1:1".into(), "0.1.0".into());
        c.role = super::super::Role::Server;
        c.auth_mode = mode;
        c.public_url = Some(url.into());
        c.test_mode = test_mode;
        c.oauth = Some(oauth());
        c
    }

    #[test]
    fn the_router_refuses_an_http_address_outside_test_mode_in_both_modes() {
        for mode in [AuthMode::None, AuthMode::Oauth] {
            assert!(router_settings(&server(mode, "http://memory.example", false)).is_err());
            assert!(router_settings(&server(mode, "http://127.0.0.1:8080", false)).is_err());
            assert!(router_settings(&server(mode, "http://127.0.0.1:8080", true)).is_ok());
            assert!(router_settings(&server(mode, "https://memory.example", true)).is_err());
            assert!(router_settings(&server(mode, "https://memory.example", false)).is_ok());
        }
        let mut none = server(AuthMode::None, "https://memory.example", false);
        none.public_url = None;
        assert!(router_settings(&none).is_err());
    }

    #[test]
    fn the_router_warns_with_sign_in_off_and_without_an_allow_list() {
        let w = router_settings(&server(AuthMode::None, "https://m.example", false)).unwrap();
        assert!(w[0].contains("anyone who can reach"));
        let w = router_settings(&server(AuthMode::Oauth, "https://m.example", false)).unwrap();
        assert!(w[0].contains("alone decides"));
        let mut listed = server(AuthMode::Oauth, "https://m.example", false);
        listed.oauth.as_mut().unwrap().allowed_email_domains = vec!["example.com".into()];
        assert!(router_settings(&listed).unwrap().is_empty());
    }

    #[test]
    fn the_router_refuses_admin_emails_with_an_email_claim() {
        let mut c = server(AuthMode::Oauth, "https://m.example", false);
        let o = c.oauth.as_mut().unwrap();
        o.admin_emails = vec!["a@example.com".into()];
        o.email_claim = Some("work_email".into());
        assert!(router_settings(&c).unwrap_err().contains("admin_emails"));
        c.oauth = None;
        assert!(router_settings(&c).is_err());
    }

    #[tokio::test]
    async fn with_sign_in_off_the_router_refuses_users_besides_the_local_one() {
        let clock = TestClock::new();
        let db: Arc<dyn StateDb> = Arc::new(LocalFileDb::in_memory(clock.clock()));
        let users = Users::new(db, Arc::new(test_field_keys()));
        let keys = knoggin_crypto::testing::test_provider();
        crate::local::ensure_local_user(&users, &keys, clock.now())
            .await
            .unwrap();
        let none = server(AuthMode::None, "https://m.example", false);
        router_users(&none, &users).await.unwrap();
        let store = StoreId::from_uuid(Uuid::now_v7());
        let (_, wrapped) = keys
            .generate(knoggin_crypto::KeyScope {
                store,
                generation: 0,
            })
            .await
            .unwrap();
        users
            .create(
                NewUser {
                    id: UserId::from_uuid(Uuid::now_v7()),
                    issuer: "https://idp.example".into(),
                    subject: Sensitive::new("maya".into()),
                    email: None,
                    email_verified: false,
                    name: None,
                },
                NewStore {
                    id: store,
                    name: Sensitive::new("Personal".into()),
                    wrapped,
                },
                clock.now(),
            )
            .await
            .unwrap();
        let e = router_users(&none, &users).await.unwrap_err();
        assert!(e.contains("1 users besides") && e.contains("oauth"), "{e}");
        let on = server(AuthMode::Oauth, "https://m.example", false);
        router_users(&on, &users).await.unwrap();
    }
}
