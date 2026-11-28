// SPDX-License-Identifier: AGPL-3.0-only
//! The small HTTP client that the local commands and the stdio bridge use over the
//! daemon's Unix socket. It sends no
//! secret, never opens a TCP port, and connects only when the home folder and the socket's
//! peer are this user's own.

use std::fmt;
use std::path::PathBuf;

use axum::body::Body;
use bytes::Bytes;
use http::{HeaderMap, HeaderName, HeaderValue, Method, Request, StatusCode, header};
use http_body_util::BodyExt;
use hyper::body::Incoming;
use hyper_util::rt::TokioIo;
use knoggin_core::Sensitive;
use serde_json::Value;
use tokio::net::UnixStream;
use tokio::task::JoinHandle;

use crate::config::Home;

/// Exit code: the daemon or server could not be reached.
pub(crate) const EXIT_UNREACHABLE: u8 = 3;
/// Exit code: the data is locked.
pub(crate) const EXIT_LOCKED: u8 = 4;

/// A failure that carries its exit code and the sentence for people.
#[derive(Debug)]
pub(crate) struct Failure {
    pub(crate) exit: u8,
    pub(crate) message: String,
}

impl fmt::Display for Failure {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.message)
    }
}

impl std::error::Error for Failure {}

impl Failure {
    pub(crate) fn not_running() -> Self {
        Self {
            exit: EXIT_UNREACHABLE,
            message: "Knoggin is not running. Start it with `knoggin start`.".into(),
        }
    }
}

/// One answer: the status, the headers and the whole body.
#[derive(Debug)]
pub(crate) struct Reply {
    pub(crate) status: StatusCode,
    pub(crate) headers: HeaderMap,
    pub(crate) body: Bytes,
}

impl Reply {
    /// The body as JSON; `Null` when it is empty or not JSON.
    pub(crate) fn json(&self) -> Value {
        serde_json::from_slice(&self.body).unwrap_or(Value::Null)
    }

    /// The body of a success, or a [`Failure`] from the API error: `store_locked` exits
    /// with 4, every other error with 1.
    pub(crate) fn ok(self) -> Result<Value, Failure> {
        if self.status.is_success() {
            return Ok(self.json());
        }
        Err(api_failure(&self.json(), self.status))
    }
}

/// The sentence and exit code of an API error body.
pub(crate) fn api_failure(body: &Value, status: StatusCode) -> Failure {
    let body = body.get("error").unwrap_or(body);
    let code = body.get("code").and_then(Value::as_str).unwrap_or("");
    let message = body
        .get("message")
        .and_then(Value::as_str)
        .map_or_else(|| format!("the daemon answered {status}"), str::to_owned);
    let reason = body.get("reason").and_then(Value::as_str);
    Failure {
        exit: if code == "store_locked" {
            EXIT_LOCKED
        } else {
            1
        },
        message: match reason {
            Some(r) => format!("{message} ({code}: {r})"),
            None if !code.is_empty() => format!("{message} ({code})"),
            None => message,
        },
    }
}

/// The client of one home's daemon.
#[derive(Clone)]
pub(crate) struct Client {
    home: Home,
    socket: PathBuf,
    api_key: Option<Sensitive<String>>,
}

impl fmt::Debug for Client {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("Client")
            .field("socket", &self.socket)
            .field("api_key", &self.api_key.is_some())
            .finish()
    }
}

/// Why a request did not get an answer.
#[derive(Debug)]
pub(crate) enum SendError {
    /// No socket, or nothing listens on it: the daemon is not running.
    NotRunning,
    /// The home or run folder, or the socket's peer, is not this user's own.
    NotPrivate(String),
    /// The connection broke.
    Io(String),
}

impl From<SendError> for Failure {
    fn from(e: SendError) -> Self {
        match e {
            SendError::NotRunning => Failure::not_running(),
            SendError::NotPrivate(m) => Failure {
                exit: 1,
                message: m,
            },
            SendError::Io(m) => Failure {
                exit: EXIT_UNREACHABLE,
                message: format!("the connection to the daemon broke: {m}"),
            },
        }
    }
}

impl Client {
    pub(crate) fn new(home: &Home) -> Self {
        Self {
            home: home.clone(),
            socket: home.socket(),
            api_key: None,
        }
    }

    /// Sends this API key as `Authorization: Bearer`: how a local agent reaches an app store.
    pub(crate) fn with_api_key(mut self, key: Option<Sensitive<String>>) -> Self {
        self.api_key = key;
        self
    }

    async fn connect(&self) -> Result<UnixStream, SendError> {
        for dir in [self.home.root().to_owned(), self.home.run()] {
            match knoggin_http::local::check_private_dir(&dir) {
                Ok(()) => {}
                Err(e) if e.kind() == std::io::ErrorKind::NotFound => {
                    return Err(SendError::NotRunning);
                }
                Err(e) => return Err(SendError::NotPrivate(e.to_string())),
            }
        }
        let stream = UnixStream::connect(&self.socket)
            .await
            .map_err(|e| match e.kind() {
                std::io::ErrorKind::NotFound | std::io::ErrorKind::ConnectionRefused => {
                    SendError::NotRunning
                }
                _ => SendError::Io(e.to_string()),
            })?;
        let own = rustix::process::getuid().as_raw();
        match stream.peer_cred() {
            Ok(c) if c.uid() == own => Ok(stream),
            _ => Err(SendError::NotPrivate(
                "the process on the daemon socket is not this user's".into(),
            )),
        }
    }

    /// Sends one request on a new connection and returns the answer with its body still
    /// streaming, and the connection's task. The task ends once the body has been read or
    /// dropped; the caller joins it then.
    pub(crate) async fn send(
        &self,
        mut request: Request<Body>,
    ) -> Result<(http::Response<Incoming>, JoinHandle<()>), SendError> {
        let stream = self.connect().await?;
        let (mut sender, conn) = hyper::client::conn::http1::handshake(TokioIo::new(stream))
            .await
            .map_err(|e| SendError::Io(e.to_string()))?;
        let task = tokio::spawn(async move {
            let _ = conn.await;
        });
        let headers = request.headers_mut();
        headers.insert(header::HOST, HeaderValue::from_static("localhost"));
        if let Some(key) = &self.api_key
            && let Ok(v) = HeaderValue::from_str(&format!("Bearer {}", key.expose()))
        {
            let mut v = v;
            v.set_sensitive(true);
            headers.insert(header::AUTHORIZATION, v);
        }
        match sender.send_request(request).await {
            Ok(response) => Ok((response, task)),
            Err(e) => {
                task.abort();
                Err(SendError::Io(e.to_string()))
            }
        }
    }

    /// Sends a request and reads the whole answer.
    pub(crate) async fn call(
        &self,
        method: Method,
        path: &str,
        headers: &[(HeaderName, HeaderValue)],
        body: Option<Bytes>,
    ) -> Result<Reply, SendError> {
        let mut builder = Request::builder().method(method).uri(path);
        for (k, v) in headers {
            builder = builder.header(k, v);
        }
        if body.is_some() && !headers.iter().any(|(k, _)| k == header::CONTENT_TYPE) {
            builder = builder.header(header::CONTENT_TYPE, "application/json");
        }
        let request = builder
            .body(body.map_or_else(Body::empty, Body::from))
            .map_err(|e| SendError::Io(e.to_string()))?;
        let (response, conn) = self.send(request).await?;
        let status = response.status();
        let headers = response.headers().clone();
        let body = response.into_body().collect().await;
        let _ = conn.await;
        let body = body.map_err(|e| SendError::Io(e.to_string()))?.to_bytes();
        Ok(Reply {
            status,
            headers,
            body,
        })
    }

    /// A JSON request; the success body, or the API error as a [`Failure`].
    pub(crate) async fn json(
        &self,
        method: Method,
        path: &str,
        body: Option<&Value>,
    ) -> Result<Value, Failure> {
        self.json_on(method, path, None, body).await
    }

    /// [`Client::json`] on a store route of the store `store` (`Knoggin-Store`), or the
    /// personal store.
    pub(crate) async fn json_on(
        &self,
        method: Method,
        path: &str,
        store: Option<&str>,
        body: Option<&Value>,
    ) -> Result<Value, Failure> {
        let mut headers = Vec::new();
        if let Some(s) = store {
            headers.push(store_header(s)?);
        }
        let body = body.map(|b| Bytes::from(b.to_string()));
        self.call(method, path, &headers, body).await?.ok()
    }
}

/// The `Knoggin-Store` header for a store ID.
pub(crate) fn store_header(store: &str) -> Result<(HeaderName, HeaderValue), Failure> {
    let v = HeaderValue::from_str(store).map_err(|_| Failure {
        exit: 1,
        message: "that store ID is not valid".into(),
    })?;
    Ok((HeaderName::from_static("knoggin-store"), v))
}

#[cfg(test)]
mod tests {
    use std::os::unix::fs::PermissionsExt;
    use std::path::Path;

    use axum::Router;
    use axum::routing::get;

    use super::*;

    fn home(dir: &Path) -> Home {
        let root = dir.join("h");
        std::fs::create_dir_all(root.join("run")).unwrap();
        for d in [&root, &root.join("run")] {
            std::fs::set_permissions(d, std::fs::Permissions::from_mode(0o700)).unwrap();
        }
        Home(root)
    }

    #[tokio::test]
    async fn no_socket_means_not_running() {
        let d = tempfile::tempdir().unwrap();
        let c = Client::new(&home(d.path()));
        let e = c.json(Method::GET, "/healthz", None).await.unwrap_err();
        assert_eq!(e.exit, EXIT_UNREACHABLE);
        // A missing home is the same: nothing runs there.
        let c = Client::new(&Home(d.path().join("nope")));
        assert_eq!(
            c.json(Method::GET, "/healthz", None)
                .await
                .unwrap_err()
                .exit,
            3
        );
    }

    #[tokio::test]
    async fn a_home_that_others_can_write_is_refused() {
        let d = tempfile::tempdir().unwrap();
        let h = home(d.path());
        std::fs::set_permissions(h.run(), std::fs::Permissions::from_mode(0o770)).unwrap();
        let e = Client::new(&h)
            .json(Method::GET, "/healthz", None)
            .await
            .unwrap_err();
        assert_eq!(e.exit, 1);
        assert!(e.message.contains("cannot write"), "{}", e.message);
    }

    #[tokio::test]
    async fn it_speaks_http_over_the_socket_and_maps_api_errors() {
        let d = tempfile::tempdir().unwrap();
        let h = home(d.path());
        let listener = tokio::net::UnixListener::bind(h.socket()).unwrap();
        let app = Router::new()
            .route(
                "/healthz",
                get(|headers: HeaderMap| async move {
                    axum::Json(serde_json::json!({
                        "status": "ok",
                        "auth": headers.contains_key(header::AUTHORIZATION),
                    }))
                }),
            )
            .route(
                "/locked",
                get(|| async {
                    (
                        StatusCode::SERVICE_UNAVAILABLE,
                        axum::Json(serde_json::json!({ "error": {
                            "code": "store_locked", "message": "Locked", "reason": "state_unreadable"
                        }})),
                    )
                }),
            );
        let server = tokio::spawn(async move { axum::serve(listener, app).await });
        let c = Client::new(&h);
        let v = c.json(Method::GET, "/healthz", None).await.unwrap();
        assert_eq!(v["status"], "ok");
        assert_eq!(v["auth"], false);
        let keyed = c
            .clone()
            .with_api_key(Some(Sensitive::new("knoggin_live_x".into())));
        assert_eq!(
            keyed.json(Method::GET, "/healthz", None).await.unwrap()["auth"],
            true
        );
        let e = c.json(Method::GET, "/locked", None).await.unwrap_err();
        assert_eq!(e.exit, EXIT_LOCKED);
        assert!(e.message.contains("state_unreadable"));
        server.abort();
    }
}
