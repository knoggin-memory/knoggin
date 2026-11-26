// SPDX-License-Identifier: AGPL-3.0-only
//! Shared helpers of the binary's tests: a temporary home in test mode with a key file,
//! the `knoggin` binary run in it, a daemon that is stopped when dropped, and plain
//! HTTP/1.1 over the Unix socket or a TCP port.
#![allow(
    clippy::unwrap_used,
    clippy::expect_used,
    clippy::print_stdout,
    clippy::print_stderr,
    dead_code,
    unreachable_pub
)]

use std::io::{Read, Write};
use std::os::unix::net::UnixStream;
use std::path::{Path, PathBuf};
use std::process::{Child, Command, Output, Stdio};
use std::time::{Duration, Instant};

use serde_json::Value;
use tempfile::TempDir;

pub const BIN: &str = env!("CARGO_BIN_EXE_knoggin");

/// A temporary home. Short paths: the socket path must stay under 100 bytes.
pub struct Home {
    pub dir: TempDir,
    pub extra: Vec<(String, String)>,
}

pub struct Out {
    pub code: i32,
    pub stdout: String,
    pub stderr: String,
}

impl Out {
    pub fn json(&self) -> Value {
        serde_json::from_str(&self.stdout).unwrap_or_else(|_| panic!("not JSON: {}", self.stdout))
    }
}

impl Home {
    pub fn new() -> Self {
        let dir = tempfile::Builder::new().prefix("kg").tempdir().unwrap();
        Self {
            dir,
            extra: Vec::new(),
        }
    }

    pub fn with(mut self, k: &str, v: &str) -> Self {
        self.extra.push((k.into(), v.into()));
        self
    }

    /// `KNOGGIN_HOME`.
    pub fn root(&self) -> PathBuf {
        self.dir.path().join("k")
    }

    pub fn data(&self) -> PathBuf {
        self.root().join("data")
    }

    pub fn socket(&self) -> PathBuf {
        self.root().join("run/daemon.sock")
    }

    /// A command in this home: test mode, a key file, port 0, nothing from the real
    /// environment but `PATH`.
    pub fn cmd(&self, args: &[&str]) -> Command {
        let mut c = Command::new(BIN);
        c.args(args)
            .env_clear()
            .env("PATH", std::env::var("PATH").unwrap_or_default())
            .env("HOME", self.dir.path())
            .env("KNOGGIN_HOME", self.root())
            .env("KNOGGIN_TEST_MODE", "1")
            .env("KNOGGIN_LOCAL__MASTER_KEY", "key_file")
            .env("KNOGGIN_SERVER__BIND", "127.0.0.1:0")
            .current_dir(self.dir.path());
        for (k, v) in &self.extra {
            c.env(k, v);
        }
        c
    }

    pub fn run(&self, args: &[&str]) -> Out {
        let o: Output = self.cmd(args).stdin(Stdio::null()).output().unwrap();
        Out {
            code: o.status.code().unwrap_or(-1),
            stdout: String::from_utf8_lossy(&o.stdout).into_owned(),
            stderr: String::from_utf8_lossy(&o.stderr).into_owned(),
        }
    }

    /// Runs a command with `--json` that must succeed.
    pub fn json(&self, args: &[&str]) -> Value {
        let mut all = args.to_vec();
        all.push("--json");
        let o = self.run(&all);
        assert_eq!(o.code, 0, "{args:?}: {}{}", o.stdout, o.stderr);
        o.json()
    }

    /// Starts `knoggin daemon` in the foreground, its stderr in `<dir>/daemon-<n>.err`, and
    /// waits until the socket answers.
    pub fn daemon(&self) -> Daemon {
        self.daemon_with(|_| {})
    }

    /// [`Home::daemon`] with a change to its command, such as an environment variable.
    pub fn daemon_with(&self, change: impl FnOnce(&mut Command)) -> Daemon {
        let n = std::fs::read_dir(self.dir.path()).unwrap().count();
        let err = self.dir.path().join(format!("daemon-{n}.err"));
        let mut cmd = self.cmd(&["daemon"]);
        change(&mut cmd);
        let child = cmd
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(std::fs::File::create(&err).unwrap())
            .spawn()
            .unwrap();
        let mut d = Daemon {
            child: Some(child),
            err,
            socket: self.socket(),
        };
        d.wait_ready(self);
        d
    }

    /// The fixed listener's address from the lock file.
    pub fn address(&self) -> String {
        let v: Value =
            serde_json::from_slice(&std::fs::read(self.root().join("run/daemon.lock")).unwrap())
                .unwrap();
        v["address"].as_str().unwrap().to_owned()
    }

    pub fn pid(&self) -> i32 {
        let v: Value =
            serde_json::from_slice(&std::fs::read(self.root().join("run/daemon.lock")).unwrap())
                .unwrap();
        v["pid"].as_i64().unwrap() as i32
    }

    /// A request on the Unix socket.
    pub fn sock(
        &self,
        method: &str,
        path: &str,
        headers: &[(&str, &str)],
        body: Option<&Value>,
    ) -> Resp {
        let s = UnixStream::connect(self.socket()).unwrap();
        request(
            s,
            method,
            path,
            headers,
            body.map(|b| b.to_string().into_bytes()),
        )
    }

    /// A request on the fixed TCP listener.
    pub fn tcp(
        &self,
        method: &str,
        path: &str,
        headers: &[(&str, &str)],
        body: Option<&Value>,
    ) -> Resp {
        let addr = self.address();
        let s = std::net::TcpStream::connect(&addr).unwrap();
        let mut h = vec![("Host", addr.as_str())];
        h.extend_from_slice(headers);
        request(
            s,
            method,
            path,
            &h,
            body.map(|b| b.to_string().into_bytes()),
        )
    }

    /// Every byte under the data folder, for marker searches.
    pub fn data_bytes(&self) -> Vec<(PathBuf, Vec<u8>)> {
        fn walk(dir: &Path, out: &mut Vec<(PathBuf, Vec<u8>)>) {
            for e in std::fs::read_dir(dir).unwrap().flatten() {
                let p = e.path();
                if p.is_dir() {
                    walk(&p, out);
                } else {
                    out.push((p.clone(), std::fs::read(&p).unwrap()));
                }
            }
        }
        let mut out = Vec::new();
        walk(&self.data(), &mut out);
        out
    }
}

pub struct Daemon {
    pub child: Option<Child>,
    pub err: PathBuf,
    pub socket: PathBuf,
}

impl Daemon {
    fn wait_ready(&mut self, home: &Home) {
        let deadline = Instant::now() + Duration::from_secs(30);
        loop {
            if let Some(status) = self.child.as_mut().unwrap().try_wait().unwrap() {
                panic!("the daemon exited with {status}: {}", self.stderr());
            }
            if home.root().join("run/daemon.lock").exists()
                && std::fs::read(home.root().join("run/daemon.lock")).is_ok_and(|b| !b.is_empty())
                && UnixStream::connect(&self.socket).is_ok()
            {
                return;
            }
            assert!(
                Instant::now() < deadline,
                "the daemon did not start: {}",
                self.stderr()
            );
            std::thread::sleep(Duration::from_millis(50));
        }
    }

    pub fn stderr(&self) -> String {
        std::fs::read_to_string(&self.err).unwrap_or_default()
    }

    /// Sends SIGTERM and waits for the exit code.
    pub fn stop(&mut self) -> i32 {
        let Some(mut child) = self.child.take() else {
            return 0;
        };
        let _ = Command::new("kill")
            .arg("-TERM")
            .arg(child.id().to_string())
            .status();
        let deadline = Instant::now() + Duration::from_secs(60);
        loop {
            if let Some(s) = child.try_wait().unwrap() {
                return s.code().unwrap_or(-1);
            }
            if Instant::now() > deadline {
                let _ = child.kill();
                let _ = child.wait();
                panic!("the daemon did not stop");
            }
            std::thread::sleep(Duration::from_millis(50));
        }
    }

    /// SIGKILL: a crash.
    pub fn kill(&mut self) {
        if let Some(mut child) = self.child.take() {
            let _ = child.kill();
            let _ = child.wait();
        }
    }
}

impl Drop for Daemon {
    fn drop(&mut self) {
        if self.child.is_some() {
            self.stop();
        }
    }
}

/// Stops a daemon that `knoggin start` started, whatever the test did.
pub struct Started<'a>(pub &'a Home);

impl Drop for Started<'_> {
    fn drop(&mut self) {
        let _ = self.0.run(&["stop"]);
    }
}

pub struct Resp {
    pub status: u16,
    pub headers: String,
    pub body: Vec<u8>,
}

impl Resp {
    pub fn json(&self) -> Value {
        serde_json::from_slice(&self.body).unwrap_or(Value::Null)
    }
    pub fn text(&self) -> String {
        String::from_utf8_lossy(&self.body).into_owned()
    }
}

fn request(
    mut s: impl Read + Write,
    method: &str,
    path: &str,
    headers: &[(&str, &str)],
    body: Option<Vec<u8>>,
) -> Resp {
    let mut head = format!("{method} {path} HTTP/1.1\r\nConnection: close\r\n");
    if !headers.iter().any(|(k, _)| k.eq_ignore_ascii_case("host")) {
        head.push_str("Host: localhost\r\n");
    }
    for (k, v) in headers {
        head.push_str(&format!("{k}: {v}\r\n"));
    }
    let body = body.unwrap_or_default();
    if !body.is_empty() {
        if !headers
            .iter()
            .any(|(k, _)| k.eq_ignore_ascii_case("content-type"))
        {
            head.push_str("Content-Type: application/json\r\n");
        }
        head.push_str(&format!("Content-Length: {}\r\n", body.len()));
    }
    head.push_str("\r\n");
    s.write_all(head.as_bytes()).unwrap();
    s.write_all(&body).unwrap();
    let mut raw = Vec::new();
    s.read_to_end(&mut raw).unwrap();
    let split = raw
        .windows(4)
        .position(|w| w == b"\r\n\r\n")
        .expect("no header end");
    let headers = String::from_utf8_lossy(&raw[..split]).into_owned();
    let status = headers[9..12].parse().unwrap();
    let mut body = raw[split + 4..].to_vec();
    if headers
        .to_ascii_lowercase()
        .contains("transfer-encoding: chunked")
    {
        body = dechunk(&body);
    }
    Resp {
        status,
        headers,
        body,
    }
}

fn dechunk(mut b: &[u8]) -> Vec<u8> {
    let mut out = Vec::new();
    loop {
        let Some(end) = b.windows(2).position(|w| w == b"\r\n") else {
            return out;
        };
        let len = usize::from_str_radix(String::from_utf8_lossy(&b[..end]).trim(), 16).unwrap_or(0);
        if len == 0 {
            return out;
        }
        out.extend_from_slice(&b[end + 2..end + 2 + len]);
        b = &b[end + 2 + len + 2..];
    }
}

/// Runs `knoggin mcp` in `home` with `lines` on stdin and returns its output lines.
pub fn bridge(
    home: &Home,
    args: &[&str],
    lines: &[Value],
    env: &[(&str, &str)],
) -> (Vec<Value>, String) {
    let mut all = vec!["mcp"];
    all.extend_from_slice(args);
    let mut cmd = home.cmd(&all);
    for (k, v) in env {
        cmd.env(k, v);
    }
    let mut child = cmd
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .unwrap();
    let mut input = String::new();
    for l in lines {
        input.push_str(&l.to_string());
        input.push('\n');
    }
    child
        .stdin
        .take()
        .unwrap()
        .write_all(input.as_bytes())
        .unwrap();
    let out = child.wait_with_output().unwrap();
    let text = String::from_utf8_lossy(&out.stdout).into_owned();
    let values = text
        .lines()
        .map(|l| serde_json::from_str(l).unwrap_or_else(|_| panic!("not JSON: {l}")))
        .collect();
    (values, String::from_utf8_lossy(&out.stderr).into_owned())
}

/// A current-revision request.
pub fn modern(id: i64, method: &str, mut params: Value) -> Value {
    params["_meta"] = serde_json::json!({
        "io.modelcontextprotocol/protocolVersion": "2026-07-28",
        "io.modelcontextprotocol/clientInfo": { "name": "bridge-test", "version": "1" },
        "io.modelcontextprotocol/clientCapabilities": {},
    });
    serde_json::json!({ "jsonrpc": "2.0", "id": id, "method": method, "params": params })
}

/// A `tools/call` on the current revision.
pub fn call(id: i64, tool: &str, args: Value) -> Value {
    modern(
        id,
        "tools/call",
        serde_json::json!({ "name": tool, "arguments": args }),
    )
}

/// The JSON text inside a successful tool result.
pub fn tool_json(answer: &Value) -> Value {
    assert!(answer["result"]["isError"].is_null(), "{answer}");
    serde_json::from_str(answer["result"]["content"][0]["text"].as_str().unwrap()).unwrap()
}
