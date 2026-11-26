// SPDX-License-Identifier: AGPL-3.0-only
//! The web UI's files (tab "Web UI", Serving): `ui/dist`, embedded in the binary in a
//! release build and read from disk in a debug build. Every `GET` path that is not the
//! API's is the UI's: a file that exists is served as it is, any other path gets
//! `index.html`, and the app routes on the client.

use std::borrow::Cow;
use std::sync::LazyLock;

use aws_lc_rs::digest;
use axum::extract::Request;
use axum::response::{IntoResponse, Response};
use base64::Engine as _;
use base64::engine::general_purpose::STANDARD;
use http::{HeaderValue, Method, StatusCode, header};

use super::Arrival;

#[derive(rust_embed::Embed)]
#[folder = "../../ui/dist"]
#[allow_missing = true]
struct Assets;

/// The `Cache-Control` a UI file asks for; the edge's way out keeps it.
#[derive(Debug, Clone)]
pub(crate) struct CacheControl(pub(crate) HeaderValue);

const IMMUTABLE: &str = "public, max-age=31536000, immutable";

/// Whether `path` belongs to the API rather than to the UI.
fn is_api(path: &str) -> bool {
    let under = |prefix: &str| path == prefix || path.starts_with(&format!("{prefix}/"));
    under("/v1")
        || under("/mcp")
        || under("/auth")
        || under("/.well-known")
        || path == "/healthz"
        || path == "/readyz"
}

/// Whether the request is for the UI's files: a `GET` or `HEAD` off the API's paths.
/// These need no credential and are not counted by the daemon's fixed bucket.
pub(crate) fn is_ui(request: &Request) -> bool {
    (request.method() == Method::GET || request.method() == Method::HEAD)
        && !is_api(request.uri().path())
}

/// The content security policy of the embedded `index.html`, worked out once.
static CSP: LazyLock<String> = LazyLock::new(|| {
    csp(&Assets::get("index.html")
        .map(|f| f.data.into_owned())
        .unwrap_or_default())
});

/// Scripts and styles from the server's own origin only, and each inline script of
/// `index` (the start-up script and the theme script that runs before first paint) by
/// its SHA-256.
fn csp(index: &[u8]) -> String {
    let hashes: String = inline_scripts(index)
        .iter()
        .map(|script| {
            let d = digest::digest(&digest::SHA256, script.as_bytes());
            format!(" 'sha256-{}'", STANDARD.encode(d.as_ref()))
        })
        .collect();
    format!(
        "default-src 'self'; script-src 'self'{hashes}; style-src 'self'; img-src 'self' data:; \
         object-src 'none'; base-uri 'none'; form-action 'self'; frame-ancestors 'none'"
    )
}

/// The text of every `<script>` element without a `src` attribute.
fn inline_scripts(html: &[u8]) -> Vec<String> {
    let html = String::from_utf8_lossy(html);
    let mut out = Vec::new();
    let mut rest = html.as_ref();
    while let Some(at) = rest.find("<script") {
        rest = &rest[at + "<script".len()..];
        let Some(close) = rest.find('>') else { break };
        let attributes = &rest[..close];
        rest = &rest[close + 1..];
        let Some(end) = rest.find("</script>") else {
            break;
        };
        if !attributes.contains("src=") {
            out.push(rest[..end].to_owned());
        }
        rest = &rest[end..];
    }
    out
}

/// One file: its bytes (borrowed from the binary in a release build) and its media type.
struct File {
    data: Cow<'static, [u8]>,
    mime: String,
}

/// Serves one UI path from the embedded files. A release build copies nothing: the body
/// borrows the embedded bytes. A debug build reads `ui/dist` from disk here, on a runtime
/// thread; that is accepted for development builds only.
pub(crate) fn serve(request: &Request) -> Response {
    let files = |path: &str| {
        Assets::get(path).map(|f| File {
            mime: f.metadata.mimetype().to_owned(),
            data: f.data,
        })
    };
    answer(request, &files, &CSP)
}

/// Serves one UI path from `files`. On the daemon's local web listener every answer is
/// `no-store`.
fn answer(request: &Request, files: &dyn Fn(&str) -> Option<File>, csp: &str) -> Response {
    let path = request.uri().path().trim_start_matches('/');
    let web = matches!(
        request.extensions().get::<Arrival>(),
        Some(Arrival::Web { .. })
    );
    let own_file = !path.is_empty() && path != "index.html" && !path.split('/').any(|s| s == "..");
    let (file, cache) = match own_file.then(|| files(path)).flatten() {
        Some(file) => {
            let cache = if path.starts_with("_app/immutable/") {
                IMMUTABLE
            } else {
                "no-cache"
            };
            (file, cache)
        }
        None => match files("index.html") {
            Some(file) => (file, "no-store"),
            None => {
                return (
                    StatusCode::SERVICE_UNAVAILABLE,
                    [(header::CONTENT_TYPE, "text/plain; charset=utf-8")],
                    "The web UI is not built",
                )
                    .into_response();
            }
        },
    };
    let mut response = file.data.into_response();
    let h = response.headers_mut();
    if let Ok(v) = HeaderValue::from_str(&file.mime) {
        h.insert(header::CONTENT_TYPE, v);
    }
    if file.mime.starts_with("text/html")
        && let Ok(v) = HeaderValue::from_str(csp)
    {
        h.insert(header::CONTENT_SECURITY_POLICY, v);
    }
    let cache = if web { "no-store" } else { cache };
    response
        .extensions_mut()
        .insert(CacheControl(HeaderValue::from_static(cache)));
    response
}

#[cfg(test)]
mod tests {
    use axum::body::Body;
    use http_body_util::BodyExt;

    use super::*;

    const INDEX: &str = "<html><script>boot()</script><script>theme()</script></html>";

    fn fake(path: &str) -> Option<File> {
        let (data, mime) = match path {
            "index.html" => (INDEX, "text/html"),
            "_app/immutable/app.js" => ("js", "text/javascript"),
            "favicon.svg" => ("<svg/>", "image/svg+xml"),
            _ => return None,
        };
        Some(File {
            data: data.as_bytes().into(),
            mime: mime.into(),
        })
    }

    async fn get(
        path: &str,
        arrival: Arrival,
        files: &dyn Fn(&str) -> Option<File>,
    ) -> (Response, String) {
        let mut request = Request::builder().uri(path).body(Body::empty()).unwrap();
        request.extensions_mut().insert(arrival);
        let response = answer(&request, files, &csp(INDEX.as_bytes()));
        let (parts, body) = response.into_parts();
        let text = String::from_utf8(body.collect().await.unwrap().to_bytes().to_vec()).unwrap();
        (Response::from_parts(parts, Body::empty()), text)
    }

    fn cache(r: &Response) -> &str {
        r.extensions()
            .get::<CacheControl>()
            .unwrap()
            .0
            .to_str()
            .unwrap()
    }

    #[tokio::test]
    async fn the_path_table_and_its_cache_headers() {
        let fixed = Arrival::Fixed { port: 7690 };
        let (r, body) = get("/_app/immutable/app.js", fixed.clone(), &fake).await;
        assert_eq!((cache(&r), body.as_str()), (IMMUTABLE, "js"));
        assert_eq!(r.headers()[header::CONTENT_TYPE], "text/javascript");
        assert!(r.headers().get(header::CONTENT_SECURITY_POLICY).is_none());
        let (r, _) = get("/favicon.svg", fixed.clone(), &fake).await;
        assert_eq!(cache(&r), "no-cache");
        for path in [
            "/",
            "/map",
            "/explore?id=x",
            "/index.html",
            "/missing.js",
            "/../x.js",
        ] {
            let (r, body) = get(path, fixed.clone(), &fake).await;
            assert_eq!((cache(&r), body.as_str()), ("no-store", INDEX), "{path}");
            let policy = r.headers()[header::CONTENT_SECURITY_POLICY]
                .to_str()
                .unwrap();
            assert!(policy.contains("style-src 'self'") && !policy.contains("unsafe-inline"));
        }
        // The local web listener answers no-store everywhere.
        let web = Arrival::Web {
            authority: "127.0.0.1:5000".into(),
        };
        let (r, _) = get("/_app/immutable/app.js", web, &fake).await;
        assert_eq!(cache(&r), "no-store");
    }

    #[tokio::test]
    async fn without_index_html_the_ui_is_not_built() {
        let (r, body) = get("/map", Arrival::Socket, &|_| None).await;
        assert_eq!(r.status(), StatusCode::SERVICE_UNAVAILABLE);
        assert_eq!(body, "The web UI is not built");
    }

    #[test]
    fn the_policy_allows_each_inline_script_by_its_hash() {
        let policy = csp(INDEX.as_bytes());
        let hash =
            |s: &str| STANDARD.encode(digest::digest(&digest::SHA256, s.as_bytes()).as_ref());
        assert!(policy.contains(&format!(
            "script-src 'self' 'sha256-{}' 'sha256-{}';",
            hash("boot()"),
            hash("theme()")
        )));
        assert!(policy.contains("frame-ancestors 'none'") && policy.contains("object-src 'none'"));
    }

    #[test]
    fn api_paths_are_not_the_uis() {
        for p in [
            "/v1",
            "/v1/session",
            "/mcp",
            "/mcp/plugin",
            "/auth/login",
            "/.well-known/oauth-protected-resource",
            "/healthz",
            "/readyz",
        ] {
            assert!(is_api(p), "{p}");
        }
        for p in [
            "/",
            "/map",
            "/v1x",
            "/_app/immutable/a.js",
            "/authx",
            "/favicon.svg",
        ] {
            assert!(!is_api(p), "{p}");
        }
    }

    #[test]
    fn inline_scripts_are_found_and_external_ones_skipped() {
        let html = br#"<html><head><script>let a = 1;</script>
            <script type="module" src="/_app/x.js"></script></head>
            <body><script>
  document.documentElement.dataset.theme = 't';
</script></body></html>"#;
        assert_eq!(
            inline_scripts(html),
            vec![
                "let a = 1;".to_owned(),
                "\n  document.documentElement.dataset.theme = 't';\n".to_owned()
            ]
        );
    }
}
