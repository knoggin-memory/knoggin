// SPDX-License-Identifier: AGPL-3.0-only
//! The one-host stack in tests (tab "Testing, CI, releases"): builds `knoggin:test` from
//! the working tree with `SKIP_MODEL=1` (once per test process; set
//! `KNOGGIN_TEST_SKIP_BUILD=1` to use the image that is there), writes the secrets into a
//! temporary directory as `make-secrets.sh` would, and brings up `deploy/docker/compose.yaml`
//! with `compose.test.yaml` under a project name of its own, a network prefix `10.<n>`
//! (n from 210 to 249) that no network on the host uses, two free loopback ports and the
//! profiles `local-s3` and `test`. It waits for `/readyz`, signs people in through the
//! test issuer, and tears everything down afterwards: the Compose services, networks and
//! volumes, and the user containers and per-user networks that the launcher made.
//!
//! Included by the `stack_*.rs` tests with `#[path = "support/stack.rs"] mod stack;`.

use std::collections::BTreeSet;
use std::net::TcpListener;
use std::os::unix::fs::PermissionsExt;
use std::path::{Path, PathBuf};
use std::process::{Command, Output, Stdio};
use std::sync::OnceLock;
use std::time::{Duration, Instant};

use base64::Engine;
use knoggin_crypto::MasterKeySet;
use knoggin_crypto::source::create_key_file;
use reqwest::Method;
use reqwest::header::{HeaderMap, LOCATION, SET_COOKIE};
use serde_json::{Value, json};

/// The repository root.
pub const ROOT: &str = concat!(env!("CARGO_MANIFEST_DIR"), "/../..");
/// The image the stack runs.
pub const IMAGE: &str = "knoggin:test";
/// `auth.issuer` in `config.test.toml`: a name on the stack's network.
pub const ISSUER: &str = "http://test-issuer:9000";
/// The web client secret the test issuer accepts.
pub const WEB_CLIENT_SECRET: &str = knoggin_http::testing::issuer::TEST_SECRET;

/// `<repository>@sha256:<digest>` of `name` in `tests/images.toml`.
pub fn pinned_image(name: &str) -> String {
    let text = std::fs::read_to_string(format!("{ROOT}/tests/images.toml")).unwrap();
    text.lines()
        .find_map(|line| {
            let (key, value) = line.split_once('=')?;
            (key.trim() == name).then(|| value.trim().trim_matches('"').to_owned())
        })
        .unwrap_or_else(|| panic!("no image `{name}` in tests/images.toml"))
}

/// Runs `docker <args>`.
pub fn docker(args: &[&str]) -> Output {
    Command::new("docker")
        .args(args)
        .stdin(Stdio::null())
        .output()
        .expect("the docker command runs")
}

/// Runs `docker <args>`, which must succeed, and returns its standard output.
pub fn docker_ok(args: &[&str]) -> String {
    let out = docker(args);
    assert!(
        out.status.success(),
        "docker {}: {}",
        args.join(" "),
        String::from_utf8_lossy(&out.stderr)
    );
    String::from_utf8_lossy(&out.stdout).into_owned()
}

/// Standard output and error of a command, as text.
pub fn text(out: &Output) -> String {
    format!(
        "{}{}",
        String::from_utf8_lossy(&out.stdout),
        String::from_utf8_lossy(&out.stderr)
    )
}

/// Builds `knoggin:test` once per test process.
pub fn build_image() {
    static BUILT: OnceLock<()> = OnceLock::new();
    BUILT.get_or_init(|| {
        if std::env::var("KNOGGIN_TEST_SKIP_BUILD").as_deref() == Ok("1") {
            assert!(
                docker(&["image", "inspect", IMAGE]).status.success(),
                "KNOGGIN_TEST_SKIP_BUILD=1, but there is no image {IMAGE}"
            );
            return;
        }
        let mut build = Command::new("docker");
        build
            .args([
                "build",
                "-f",
                "deploy/Dockerfile",
                "--build-arg",
                "SKIP_MODEL=1",
            ])
            .args(["-t", IMAGE, "."])
            .current_dir(ROOT)
            .stdin(Stdio::null());
        if let Ok(jobs) = std::env::var("CARGO_BUILD_JOBS") {
            build.args(["--build-arg", &format!("CARGO_BUILD_JOBS={jobs}")]);
        }
        assert!(build.status().unwrap().success(), "the image build failed");
    });
}

fn random_hex(bytes: usize) -> String {
    let mut buf = vec![0u8; bytes];
    aws_lc_rs::rand::fill(&mut buf).unwrap();
    buf.iter().map(|b| format!("{b:02x}")).collect()
}

fn free_port() -> u16 {
    TcpListener::bind("127.0.0.1:0")
        .unwrap()
        .local_addr()
        .unwrap()
        .port()
}

/// The second parts `n` of every `10.n.` range that a network on the host uses.
fn used_prefixes() -> BTreeSet<u8> {
    let ids = docker_ok(&["network", "ls", "-q"]);
    let mut args = vec![
        "network",
        "inspect",
        "--format",
        "{{range .IPAM.Config}}{{.Subnet}} {{end}}",
    ];
    args.extend(ids.split_whitespace());
    // A network may go away between the two calls; the subnets of the rest still count.
    let out = docker(&args);
    String::from_utf8_lossy(&out.stdout)
        .split_whitespace()
        .filter_map(|subnet| {
            let mut parts = subnet.split('.');
            (parts.next()? == "10").then_some(())?;
            parts.next()?.parse().ok()
        })
        .collect()
}

/// A free prefix `10.<n>`, n from 210 to 249, that is not in `avoid`.
pub fn free_prefix(avoid: &[&str]) -> String {
    let used = used_prefixes();
    let start = u8::try_from(std::process::id() % 40).unwrap();
    (0..40u8)
        .map(|i| 210 + (start + i) % 40)
        .map(|n| (n, format!("10.{n}")))
        .find(|(n, p)| !used.contains(n) && !avoid.contains(&p.as_str()))
        .map(|(_, p)| p)
        .expect("a free network prefix between 10.210 and 10.249")
}

/// Writes the secret files of tab "Deployment" into `dir` (mode 0700, files 0444), and
/// returns every secret value, for the log searches.
fn write_secrets(dir: &Path) -> Vec<String> {
    std::fs::create_dir(dir).unwrap();
    std::fs::set_permissions(dir, std::fs::Permissions::from_mode(0o700)).unwrap();
    let mut values = Vec::new();
    let write = |name: &str, content: String| {
        std::fs::write(dir.join(name), content).unwrap();
    };
    let superuser = random_hex(32);
    write("postgres_superuser_password", format!("{superuser}\n"));
    values.push(superuser);
    for (file, user) in [
        ("postgres_url", "knoggin"),
        ("postgres_url_lifecycle", "knoggin_lifecycle"),
        ("postgres_url_backup", "knoggin_backup"),
    ] {
        let password = random_hex(32);
        write(
            file,
            format!("postgres://{user}:{password}@db:5432/knoggin?sslmode=disable\n"),
        );
        values.push(password);
    }
    for file in ["s3_credentials", "s3_credentials_lifecycle"] {
        let (id, secret) = (random_hex(10), random_hex(20));
        write(
            file,
            format!("[default]\naws_access_key_id = {id}\naws_secret_access_key = {secret}\n"),
        );
        values.extend([id, secret]);
    }
    let mut internal = [0u8; 32];
    aws_lc_rs::rand::fill(&mut internal).unwrap();
    let internal = base64::engine::general_purpose::STANDARD.encode(internal);
    write("internal_secret", format!("{internal}\n"));
    values.push(internal);
    let token = random_hex(32);
    write("launcher_token", format!("{token}\n"));
    values.push(token);
    write("web_client_secret", format!("{WEB_CLIENT_SECRET}\n"));
    values.push(WEB_CLIENT_SECRET.to_owned());
    let now = knoggin_core::Timestamp::from_millis(
        i64::try_from(
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_millis(),
        )
        .unwrap(),
    );
    let (cert, key) = knoggin_server::link::new_internal_cert(now).unwrap();
    write("internal_cert.pem", cert);
    write("internal_key.pem", key.to_string());
    values.extend(
        key.lines()
            .filter(|l| !l.starts_with("-----") && l.len() > 20)
            .map(str::to_owned),
    );
    for file in ["master_key", "state_key"] {
        let path = dir.join(file);
        create_key_file(&path, &MasterKeySet::generate().unwrap()).unwrap();
        let set: Value = serde_json::from_str(&std::fs::read_to_string(&path).unwrap()).unwrap();
        values.extend(
            set["keys"]
                .as_object()
                .unwrap()
                .values()
                .filter_map(|v| v.as_str().map(str::to_owned)),
        );
    }
    for entry in std::fs::read_dir(dir).unwrap() {
        std::fs::set_permissions(
            entry.unwrap().path(),
            std::fs::Permissions::from_mode(0o444),
        )
        .unwrap();
    }
    values
}

fn client() -> reqwest::Client {
    reqwest::Client::builder()
        .redirect(reqwest::redirect::Policy::none())
        .timeout(Duration::from_secs(90))
        .build()
        .unwrap()
}

/// A credential for a request.
#[derive(Debug, Clone)]
pub enum Cred {
    None,
    /// A browser session: the cookie's value.
    Session(String),
    /// An API key or an OAuth access token.
    Bearer(String),
}

/// An answer, read in full.
#[derive(Debug)]
pub struct Answer {
    pub status: u16,
    pub headers: HeaderMap,
    pub body: String,
}

impl Answer {
    pub fn json(&self) -> Value {
        serde_json::from_str(&self.body).unwrap_or(Value::Null)
    }

    /// The error code, if the body is an error.
    pub fn code(&self) -> String {
        self.json()["error"]["code"]
            .as_str()
            .unwrap_or_default()
            .to_owned()
    }
}

/// A person signed in through the test issuer.
#[derive(Debug, Clone)]
pub struct User {
    pub sub: String,
    pub id: String,
    pub personal: String,
    pub session: Cred,
    pub token: Cred,
}

/// A user container the launcher started for this stack.
#[derive(Debug, Clone)]
pub struct UserContainer {
    pub docker_id: String,
    pub knoggin_id: String,
    pub running: bool,
    /// Its one network and its address there.
    pub network: String,
    pub ip: String,
}

/// One stack, torn down on drop.
#[derive(Debug)]
pub struct Stack {
    pub project: String,
    pub prefix: String,
    pub port: u16,
    pub issuer_port: u16,
    /// `http://127.0.0.1:<port>`, which is also `server.public_url`.
    pub base: String,
    /// Every secret value the stack was configured with.
    pub secrets: Vec<String>,
    dir: tempfile::TempDir,
    http: reqwest::Client,
    /// A deployed stack that this test did not start and must not tear down.
    remote: bool,
}

impl Stack {
    /// Brings up a stack and waits until the router is ready.
    pub async fn up() -> Self {
        Self::up_avoiding(&[]).await
    }

    /// The same, with a network prefix that is none of `avoid`.
    pub async fn up_avoiding(avoid: &[&str]) -> Self {
        let stack = Self::create(avoid);
        let out = stack.compose(&["up", "-d", "--quiet-pull"]);
        assert!(out.status.success(), "compose up: {}", text(&out));
        stack.wait_ready(Duration::from_secs(240)).await;
        stack
    }

    /// Writes the secrets and picks the names, ports and prefix; starts nothing.
    pub fn create(avoid: &[&str]) -> Self {
        build_image();
        let dir = tempfile::tempdir().unwrap();
        let secrets = write_secrets(&dir.path().join("secrets"));
        std::fs::write(dir.path().join("empty.env"), "").unwrap();
        let port = free_port();
        let mut issuer_port = free_port();
        while issuer_port == port {
            issuer_port = free_port();
        }
        Self {
            project: format!("kntest-{}-{}", std::process::id(), random_hex(3)),
            prefix: free_prefix(avoid),
            port,
            issuer_port,
            base: format!("http://127.0.0.1:{port}"),
            secrets,
            dir,
            http: client(),
            remote: false,
        }
    }

    /// A deployed stack at `base`, reached over the network only: nothing to build,
    /// start or tear down, and no secrets of its own.
    pub fn remote(base: &str) -> Self {
        Self {
            project: String::new(),
            prefix: String::new(),
            port: 0,
            issuer_port: 0,
            base: base.to_owned(),
            secrets: Vec::new(),
            dir: tempfile::tempdir().unwrap(),
            http: client(),
            remote: true,
        }
    }

    pub fn secrets_dir(&self) -> PathBuf {
        self.dir.path().join("secrets")
    }

    /// `docker compose` for this stack, with its environment.
    pub fn compose_command(&self, args: &[&str]) -> Command {
        let files = format!("{ROOT}/deploy/docker");
        let mut cmd = Command::new("docker");
        cmd.arg("compose")
            .args(["--project-name", &self.project])
            .arg("--env-file")
            .arg(self.dir.path().join("empty.env"))
            .args(["-f", &format!("{files}/compose.yaml")])
            .args(["-f", &format!("{files}/compose.test.yaml")])
            .args(["--profile", "local-s3", "--profile", "test"])
            .args(args)
            .env("KNOGGIN_TEST_PORT", self.port.to_string())
            .env("KNOGGIN_TEST_ISSUER_PORT", self.issuer_port.to_string())
            .env("KNOGGIN_NET_PREFIX", &self.prefix)
            .env("KNOGGIN_SECRETS_DIR", self.secrets_dir())
            .env("KNOGGIN_IMAGE", IMAGE)
            .env_remove("COMPOSE_PROFILES")
            .env_remove("COMPOSE_FILE")
            .env_remove("COMPOSE_PROJECT_NAME")
            .stdin(Stdio::null());
        cmd
    }

    pub fn compose(&self, args: &[&str]) -> Output {
        self.compose_command(args).output().unwrap()
    }

    pub fn compose_ok(&self, args: &[&str]) -> String {
        let out = self.compose(args);
        assert!(
            out.status.success(),
            "compose {}: {}",
            args.join(" "),
            text(&out)
        );
        String::from_utf8_lossy(&out.stdout).into_owned()
    }

    /// `knoggin <args>` inside the running router container (where admin commands run).
    pub fn knoggin(&self, args: &[&str]) -> Output {
        let mut all = vec!["exec", "-T", "router", "knoggin"];
        all.extend(args);
        self.compose(&all)
    }

    /// The log of one Compose service.
    pub fn logs(&self, service: &str) -> String {
        text(&self.compose(&["logs", "--no-color", "--no-log-prefix", service]))
    }

    /// The Docker ID of a Compose service's container.
    pub fn service_id(&self, service: &str) -> String {
        self.compose_ok(&["ps", "-aq", service]).trim().to_owned()
    }

    /// The name of one of the stack's own networks, such as `edge`.
    pub fn network(&self, name: &str) -> String {
        format!("{}_{name}", self.project)
    }

    pub async fn wait_ready(&self, limit: Duration) {
        let deadline = Instant::now() + limit;
        loop {
            if let Ok(r) = self
                .http
                .get(format!("{}/readyz", self.base))
                .timeout(Duration::from_secs(5))
                .send()
                .await
                && r.status() == 200
            {
                return;
            }
            assert!(
                Instant::now() < deadline,
                "the router was not ready within {limit:?}:\n{}",
                tail(&self.logs("router"), 60)
            );
            tokio::time::sleep(Duration::from_millis(500)).await;
        }
    }

    /// Every user container the launcher made for this stack, running or not.
    pub fn user_containers(&self) -> Vec<UserContainer> {
        let ids = docker_ok(&[
            "ps",
            "-aq",
            "--filter",
            &format!("label=knoggin.deployment={}", self.project),
            "--filter",
            "label=knoggin.role=user",
        ]);
        ids.split_whitespace()
            .filter_map(|id| {
                let out = docker(&[
                    "inspect",
                    "--format",
                    "{{index .Config.Labels \"knoggin.container-id\"}}|{{.State.Running}}|{{range $k, $v := .NetworkSettings.Networks}}{{$k}}={{$v.IPAddress}} {{end}}",
                    id,
                ]);
                // A container removed in between is simply not listed.
                let line = String::from_utf8_lossy(&out.stdout).trim().to_owned();
                let mut parts = line.splitn(3, '|');
                let knoggin_id = parts.next()?.to_owned();
                let running = parts.next()? == "true";
                let (network, ip) = parts.next()?.trim().split_once('=')?;
                Some(UserContainer {
                    docker_id: id.to_owned(),
                    knoggin_id,
                    running,
                    network: network.to_owned(),
                    ip: ip.to_owned(),
                })
            })
            .collect()
    }

    /// The per-user networks of this stack.
    pub fn user_networks(&self) -> Vec<String> {
        docker_ok(&[
            "network",
            "ls",
            "--filter",
            &format!("label=knoggin.deployment={}", self.project),
            "--filter",
            "label=knoggin.role=user",
            "--format",
            "{{.Name}}",
        ])
        .split_whitespace()
        .map(str::to_owned)
        .collect()
    }

    /// The running container that was claimed for `user`, found by its `claimed` log line.
    pub fn container_of(&self, user: &User) -> Option<UserContainer> {
        self.user_containers().into_iter().find(|c| {
            c.running && {
                let log = text(&docker(&["logs", &c.docker_id]));
                log.lines()
                    .any(|l| l.contains("\"claimed\"") && l.contains(&user.id))
            }
        })
    }

    /// Runs the pinned busybox with `--network <network>` (a network name, or
    /// `container:<id>` to share a container's network namespace) and returns its output.
    pub fn probe(&self, network: &str, script: &str) -> String {
        docker_ok(&[
            "run",
            "--rm",
            "--network",
            network,
            "--cap-drop",
            "ALL",
            "--security-opt",
            "no-new-privileges",
            &pinned_image("busybox"),
            "sh",
            "-c",
            script,
        ])
    }

    /// A request to the router's published port.
    pub fn request(&self, method: Method, path: &str, cred: &Cred) -> reqwest::RequestBuilder {
        let b = self.http.request(method, format!("{}{path}", self.base));
        match cred {
            Cred::None => b,
            Cred::Session(cookie) => b
                .header("cookie", format!("{}={cookie}", self.session_cookie()))
                .header("x-knoggin-ui", "1"),
            Cred::Bearer(token) => b.bearer_auth(token),
        }
    }

    /// The session cookie's name: `__Host-` over `https`, as the router names it.
    fn session_cookie(&self) -> &'static str {
        if self.base.starts_with("https://") {
            "__Host-session"
        } else {
            "knoggin_session"
        }
    }

    /// Sends a request and reads the whole answer.
    pub async fn call(
        &self,
        method: Method,
        path: &str,
        cred: &Cred,
        headers: &[(&str, &str)],
        body: Option<Value>,
    ) -> Answer {
        let mut b = self.request(method, path, cred);
        for (k, v) in headers {
            b = b.header(*k, *v);
        }
        if let Some(body) = body {
            b = b.json(&body);
        }
        let r = b.send().await.unwrap();
        Answer {
            status: r.status().as_u16(),
            headers: r.headers().clone(),
            body: r.text().await.unwrap_or_default(),
        }
    }

    /// An OAuth access token for `sub`, minted by the test issuer for this server.
    pub async fn token(&self, sub: &str) -> String {
        let now = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_secs();
        let claims = json!({
            "iss": ISSUER, "sub": sub, "aud": self.base, "iat": now, "exp": now + 3600,
            "email": format!("{sub}@test.example"), "email_verified": true, "name": sub,
        });
        self.http
            .post(format!("http://127.0.0.1:{}/mint", self.issuer_port))
            .json(&claims)
            .send()
            .await
            .unwrap()
            .text()
            .await
            .unwrap()
    }

    /// A browser sign-in as `sub`, following the redirects by hand and sending the
    /// issuer's requests to its published port: the session cookie's value.
    pub async fn session(&self, sub: &str) -> String {
        let login = self
            .http
            .get(format!(
                "{}/auth/login?return_to=/&login_hint={sub}",
                self.base
            ))
            .send()
            .await
            .unwrap();
        assert_eq!(login.status(), 303, "{}", login.text().await.unwrap());
        let login_cookie = cookie(login.headers(), "knoggin_login").expect("a login cookie");
        let to = location(login.headers());
        let to = to
            .strip_prefix(ISSUER)
            .map(|rest| format!("http://127.0.0.1:{}{rest}", self.issuer_port))
            .expect("a redirect to the test issuer");
        let back = self.http.get(&to).send().await.unwrap();
        assert_eq!(back.status(), 303);
        let callback = location(back.headers());
        assert!(callback.starts_with(&self.base), "{callback}");
        let done = self
            .http
            .get(&callback)
            .header("cookie", format!("knoggin_login={login_cookie}"))
            .send()
            .await
            .unwrap();
        assert_eq!(done.status(), 303);
        assert_eq!(location(done.headers()), "/");
        cookie(done.headers(), "knoggin_session").expect("a session cookie")
    }

    /// Signs `sub` in with a browser session and an access token.
    pub async fn sign_in(&self, sub: &str) -> User {
        let session = self.session(sub).await;
        let token = self.token(sub).await;
        self.signed_in(sub, &session, Some(&token)).await
    }

    /// The user of a session cookie's value and, if given, an access token of the same
    /// person, signed in elsewhere. `sub` is only a label.
    pub async fn signed_in(&self, sub: &str, session: &str, token: Option<&str>) -> User {
        let session = Cred::Session(session.to_owned());
        let token = token.map_or(Cred::None, |t| Cred::Bearer(t.to_owned()));
        let me = self
            .call(Method::GET, "/v1/session", &session, &[], None)
            .await
            .json();
        assert_eq!(me["signedIn"], true, "{sub}'s session: {me}");
        // Credentials pasted into a file can be mixed up; the test issuer's cannot.
        if self.remote && matches!(token, Cred::Bearer(_)) {
            let by_token = self
                .call(Method::GET, "/v1/session", &token, &[], None)
                .await
                .json();
            assert_eq!(
                by_token["user"]["id"], me["user"]["id"],
                "{sub}'s token and session belong to one person"
            );
        }
        User {
            sub: sub.to_owned(),
            id: me["user"]["id"].as_str().unwrap().to_owned(),
            personal: me["personalStoreId"].as_str().unwrap().to_owned(),
            session,
            token,
        }
    }

    /// Creates an app for `user`: its store ID.
    pub async fn app(&self, user: &User, name: &str) -> String {
        let a = self
            .call(
                Method::POST,
                "/v1/stores",
                &user.session,
                &[],
                Some(json!({ "name": name })),
            )
            .await;
        assert_eq!(a.status, 201, "{}", a.body);
        a.json()["id"].as_str().unwrap().to_owned()
    }

    /// Creates an API key on `store`: the key's ID and the key itself.
    pub async fn key(&self, user: &User, store: &str, name: &str) -> (String, Cred) {
        let a = self
            .call(
                Method::POST,
                &format!("/v1/stores/{store}/keys"),
                &user.session,
                &[],
                Some(json!({ "name": name })),
            )
            .await;
        assert_eq!(a.status, 201, "{}", a.body);
        let v = a.json();
        (
            v["key"]["id"].as_str().unwrap().to_owned(),
            Cred::Bearer(v["secret"].as_str().unwrap().to_owned()),
        )
    }

    /// Saves a memory; `store` is sent as `Knoggin-Store` when given.
    pub async fn save(&self, cred: &Cred, store: Option<&str>, summary: &str) -> Answer {
        let headers: Vec<(&str, &str)> = store.map(|s| ("knoggin-store", s)).into_iter().collect();
        self.call(
            Method::POST,
            "/v1/memories",
            cred,
            &headers,
            Some(json!({ "summary": summary })),
        )
        .await
    }

    /// The summaries in a store, newest first.
    pub async fn summaries(&self, cred: &Cred, store: Option<&str>) -> (u16, Vec<String>) {
        let headers: Vec<(&str, &str)> = store.map(|s| ("knoggin-store", s)).into_iter().collect();
        let a = self
            .call(Method::GET, "/v1/memories", cred, &headers, None)
            .await;
        let items = a.json()["items"]
            .as_array()
            .map(|items| {
                items
                    .iter()
                    .filter_map(|m| m["summary"].as_str().map(str::to_owned))
                    .collect()
            })
            .unwrap_or_default();
        (a.status, items)
    }

    /// Keeps the claimed containers of `creds` busy until the guard is dropped, so that
    /// they do not park (the test stack parks after 5 idle seconds).
    pub fn keep_alive(&self, creds: Vec<Cred>) -> KeepAlive {
        let (base, http) = (self.base.clone(), self.http.clone());
        KeepAlive(tokio::spawn(async move {
            loop {
                for c in &creds {
                    if let Cred::Bearer(t) = c {
                        let _ = http
                            .get(format!("{base}/v1/tags"))
                            .bearer_auth(t)
                            .send()
                            .await;
                    }
                }
                tokio::time::sleep(Duration::from_millis(1500)).await;
            }
        }))
    }

    /// Removes everything this stack made. Safe to call twice.
    pub fn teardown(&self) {
        if self.remote {
            return;
        }
        let label = format!("label=knoggin.deployment={}", self.project);
        // The router first, without its park (a stop would wait for every container).
        let _ = self.compose(&["kill", "router", "launcher"]);
        let users = docker_ok(&["ps", "-aq", "--filter", &label]);
        for id in users.split_whitespace() {
            let _ = docker(&["rm", "-f", "-v", id]);
        }
        let _ = self.compose(&["down", "--volumes", "--remove-orphans", "--timeout", "10"]);
        let networks = docker_ok(&["network", "ls", "-q", "--filter", &label]);
        for id in networks.split_whitespace() {
            let _ = docker(&["network", "rm", id]);
        }
    }
}

impl Drop for Stack {
    fn drop(&mut self) {
        if self.remote {
            return;
        }
        if std::thread::panicking() {
            for service in ["router", "launcher", "lifecycle", "s3-init"] {
                eprintln!(
                    "---- {service} ({})\n{}",
                    self.project,
                    tail(&self.logs(service), 80)
                );
            }
            for c in self.user_containers() {
                eprintln!(
                    "---- user container {} ({})\n{}",
                    c.knoggin_id,
                    c.docker_id,
                    tail(&text(&docker(&["logs", &c.docker_id])), 40)
                );
            }
        }
        self.teardown();
    }
}

/// Stops the requests of [`Stack::keep_alive`] when dropped.
#[derive(Debug)]
pub struct KeepAlive(tokio::task::JoinHandle<()>);

impl Drop for KeepAlive {
    fn drop(&mut self) {
        self.0.abort();
    }
}

/// The last `n` lines of `text`.
pub fn tail(text: &str, n: usize) -> String {
    let lines: Vec<&str> = text.lines().collect();
    lines[lines.len().saturating_sub(n)..].join("\n")
}

fn location(headers: &HeaderMap) -> String {
    headers[LOCATION].to_str().unwrap().to_owned()
}

fn cookie(headers: &HeaderMap, name: &str) -> Option<String> {
    headers.get_all(SET_COOKIE).iter().find_map(|c| {
        let (pair, _) = c.to_str().ok()?.split_once(';')?;
        let (k, v) = pair.split_once('=')?;
        (k == name && !v.is_empty()).then(|| v.to_owned())
    })
}

/// Waits until `check` holds, up to `limit`.
pub async fn eventually<F, Fut>(limit: Duration, mut check: F) -> bool
where
    F: FnMut() -> Fut,
    Fut: std::future::Future<Output = bool>,
{
    let deadline = Instant::now() + limit;
    loop {
        if check().await {
            return true;
        }
        if Instant::now() >= deadline {
            return false;
        }
        tokio::time::sleep(Duration::from_millis(500)).await;
    }
}
