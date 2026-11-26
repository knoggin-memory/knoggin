// SPDX-License-Identifier: AGPL-3.0-only
//! The browser safety checks that do not depend on the credential (tab "Sign-in and
//! credentials", Browser safety checks), the internal-header check, and the response
//! headers every answer carries.

use axum::body::HttpBody;
use axum::extract::Request;
use axum::response::Response;
use http::{HeaderMap, HeaderValue, Method, header};

use super::Arrival;
use crate::error::ApiError;

/// A host and an optional port, lower-cased, with one trailing dot removed.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct HostPort {
    host: String,
    port: Option<u16>,
}

impl HostPort {
    /// Parses `host`, `host:port`, `[v6]` or `[v6]:port`.
    pub(crate) fn parse(text: &str) -> Option<Self> {
        let text = text.trim();
        let (host, port) = if let Some(rest) = text.strip_prefix('[') {
            let (inside, after) = rest.split_once(']')?;
            let port = match after {
                "" => None,
                p => Some(p.strip_prefix(':')?.parse().ok()?),
            };
            (format!("[{inside}]"), port)
        } else {
            match text.rsplit_once(':') {
                Some((h, p)) => (h.to_owned(), Some(p.parse().ok()?)),
                None => (text.to_owned(), None),
            }
        };
        let mut host = host.to_ascii_lowercase();
        if host.ends_with('.') {
            host.pop();
        }
        if host.is_empty() || host.contains(['/', '@', '?', '#', ' ']) {
            return None;
        }
        Some(Self { host, port })
    }

    fn matches(&self, other: &Self, default_port: u16) -> bool {
        self.host == other.host
            && self.port.unwrap_or(default_port) == other.port.unwrap_or(default_port)
    }
}

/// An origin: scheme, host and port.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct Origin {
    scheme: String,
    host: HostPort,
}

impl Origin {
    /// Parses `scheme://authority` with nothing after it but an optional `/`.
    pub(crate) fn parse(text: &str) -> Option<Self> {
        let (scheme, rest) = text.split_once("://")?;
        let scheme = scheme.to_ascii_lowercase();
        if scheme != "http" && scheme != "https" {
            return None;
        }
        let authority = rest.strip_suffix('/').unwrap_or(rest);
        Some(Self {
            scheme,
            host: HostPort::parse(authority)?,
        })
    }

    /// `http://<host>:<port>` for a loopback name.
    fn http(host: &str, port: u16) -> Self {
        Self {
            scheme: "http".into(),
            host: HostPort {
                host: host.into(),
                port: Some(port),
            },
        }
    }

    fn default_port(&self) -> u16 {
        if self.scheme == "https" { 443 } else { 80 }
    }

    fn same(&self, other: &Self) -> bool {
        self.scheme == other.scheme && self.host.matches(&other.host, self.default_port())
    }
}

/// The three loopback names of the daemon's fixed listener.
const LOOPBACK: [&str; 3] = ["127.0.0.1", "localhost", "[::1]"];

/// The checks that run before the credential is read. `public` is the origin of
/// `server.public_url`.
pub(crate) fn check(
    request: &Request,
    arrival: &Arrival,
    public: Option<&Origin>,
) -> Result<(), ApiError> {
    internal_headers(request.headers())?;
    if *arrival == Arrival::Socket {
        return Ok(());
    }
    if request.method() == Method::OPTIONS {
        return Err(ApiError::permission_denied(
            "Cross-origin requests are not served",
        ));
    }
    let path = request.uri().path();
    if path != "/healthz" && path != "/readyz" {
        host(request, arrival, public)?;
    }
    if path.starts_with("/v1/") || path == "/mcp" || path.starts_with("/mcp/") {
        let site = request.headers().get("sec-fetch-site");
        if site.is_some_and(|v| v != "same-origin" && v != "none") {
            return Err(ApiError::permission_denied(
                "Cross-site requests are not served",
            ));
        }
    }
    if let Some(origin) = request.headers().get(header::ORIGIN) {
        let ok = origin
            .to_str()
            .ok()
            .and_then(Origin::parse)
            .is_some_and(|o| allowed_origins(arrival, public).iter().any(|a| a.same(&o)));
        if !ok {
            return Err(ApiError::permission_denied(
                "Cross-origin requests are not served",
            ));
        }
    }
    content_type(request)
}

/// Refuses every client header whose name starts with `Knoggin-` but `Knoggin-Store` and
/// `Knoggin-Namespace`: the edge sets the internal ones itself.
fn internal_headers(map: &HeaderMap) -> Result<(), ApiError> {
    for name in map.keys() {
        let n = name.as_str();
        if n.starts_with("knoggin-") && n != "knoggin-store" && n != "knoggin-namespace" {
            return Err(ApiError::invalid_argument(
                n.to_owned(),
                format!("a client may not send the header {n}"),
            ));
        }
    }
    Ok(())
}

fn host(request: &Request, arrival: &Arrival, public: Option<&Origin>) -> Result<(), ApiError> {
    let refused = || ApiError::permission_denied("This address is not served here");
    let sent = match request.uri().authority() {
        Some(a) => a.as_str().to_owned(),
        None => {
            let mut values = request.headers().get_all(header::HOST).iter();
            match (values.next(), values.next()) {
                (Some(v), None) => v.to_str().map_err(|_| refused())?.to_owned(),
                _ => return Err(refused()),
            }
        }
    };
    let sent = HostPort::parse(&sent).ok_or_else(refused)?;
    let ok = match arrival {
        Arrival::Web { authority } => {
            HostPort::parse(authority).is_some_and(|w| w.matches(&sent, 80))
        }
        Arrival::Fixed { port } => {
            public.is_some_and(|p| p.host.matches(&sent, p.default_port()))
                || LOOPBACK.iter().any(|name| {
                    HostPort {
                        host: (*name).into(),
                        port: Some(*port),
                    }
                    .matches(&sent, 80)
                })
        }
        Arrival::Public => public.is_some_and(|p| p.host.matches(&sent, p.default_port())),
        Arrival::Socket => true,
    };
    if ok { Ok(()) } else { Err(refused()) }
}

fn allowed_origins(arrival: &Arrival, public: Option<&Origin>) -> Vec<Origin> {
    let mut out: Vec<Origin> = Vec::new();
    match arrival {
        Arrival::Web { authority } => {
            if let Some(o) = Origin::parse(&format!("http://{authority}")) {
                out.push(o);
            }
        }
        Arrival::Fixed { port } => {
            out.extend(public.cloned());
            out.extend(LOOPBACK.iter().map(|n| Origin::http(n, *port)));
        }
        Arrival::Public => out.extend(public.cloned()),
        Arrival::Socket => {}
    }
    out
}

/// A request with a body must be JSON, or JSON Lines on the import route.
fn content_type(request: &Request) -> Result<(), ApiError> {
    if request.body().is_end_stream() {
        return Ok(());
    }
    let want = if request.uri().path() == "/v1/import" {
        "application/x-ndjson"
    } else {
        "application/json"
    };
    let ok = request
        .headers()
        .get(header::CONTENT_TYPE)
        .and_then(|v| v.to_str().ok())
        .and_then(|v| v.split(';').next())
        .is_some_and(|v| v.trim().eq_ignore_ascii_case(want));
    if ok {
        Ok(())
    } else {
        Err(ApiError::invalid_argument(
            "Content-Type",
            format!("a request body must be sent as {want}"),
        ))
    }
}

/// The headers of every answer. Every answer of the edge is an API answer, so every one
/// carries `Cache-Control: no-store`.
pub(crate) fn response_headers(response: &mut Response) {
    let h = response.headers_mut();
    h.insert(
        "x-content-type-options",
        HeaderValue::from_static("nosniff"),
    );
    h.insert("referrer-policy", HeaderValue::from_static("no-referrer"));
    h.insert("x-frame-options", HeaderValue::from_static("DENY"));
    h.insert(header::CACHE_CONTROL, HeaderValue::from_static("no-store"));
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn hosts_compare_with_default_ports_case_and_a_trailing_dot() {
        let p = Origin::parse("https://Memory.Example.com").unwrap();
        for ok in [
            "memory.example.com",
            "MEMORY.example.com.:443",
            "memory.example.com:443",
        ] {
            assert!(p.host.matches(&HostPort::parse(ok).unwrap(), 443), "{ok}");
        }
        for bad in [
            "memory.example.com:80",
            "evil.example.com",
            "memory.example.com..",
        ] {
            assert!(
                !HostPort::parse(bad).is_some_and(|h| p.host.matches(&h, 443)),
                "{bad}"
            );
        }
        let v6 = HostPort::parse("[::1]:7690").unwrap();
        assert!(v6.matches(&HostPort::parse("[::1]:7690").unwrap(), 80));
        assert!(HostPort::parse("a@b").is_none());
    }

    #[test]
    fn origins() {
        let o = Origin::parse("http://127.0.0.1:7690").unwrap();
        assert!(o.same(&Origin::http("127.0.0.1", 7690)));
        assert!(Origin::parse("null").is_none());
        assert!(Origin::parse("file://x").is_none());
        assert!(
            Origin::parse("https://a.example")
                .unwrap()
                .same(&Origin::parse("https://a.example:443").unwrap())
        );
    }
}
