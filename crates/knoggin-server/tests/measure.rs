// SPDX-License-Identifier: AGPL-3.0-only
//! Build-time measurements M2, M3, M4 and the SeaweedFS half of M7 (tab "Overview",
//! Build-time measurements). Ignored; run one size at a time, for example:
//!
//! ```sh
//! KNOGGIN_MEASURE_SIZE=15000 cargo test -p knoggin-server --test measure -- \
//!     --ignored --nocapture --test-threads 2 server local_daemon
//! ```
//!
//! - `server`: the one-host stack with the real model. Fills the personal store through
//!   the import route from a `knoggin-bench gen-export` file and waits until the embedding
//!   queue is empty (M2 says to measure only then), then measures M3, M2 and M4.
//! - `local_daemon`: `knoggin daemon` in a temporary `KNOGGIN_HOME`, filled the same way,
//!   for M4 on local disk.
//! - `seaweedfs`: the bucket self-test and an upload that outlasts its link (M7).
//!
//! Environment: `KNOGGIN_MEASURE_SIZE` (memories, default 1,500), `KNOGGIN_MEASURE_MEMORY_MIB`
//! (`containers.memory_mib`, default 1,024; the spec says 4,096 at 75,000),
//! `KNOGGIN_MEASURE_IMAGE` (default `knoggin:measure`, built from the working tree without
//! `SKIP_MODEL` when missing), `KNOGGIN_MEASURE_DIR` (default `target/measure`),
//! `KNOGGIN_MEASURE_RUNS` (M2 runs, default 5), `KNOGGIN_MEASURE_FILL_ONLY=1` (`server`
//! stops once the store is filled, for example to fill with a larger `memory_mib` and
//! measure with the default).
//!
//! Each size keeps its state in `<dir>/server-<size>` (secrets, Postgres and SeaweedFS
//! data, bind-mounted) and `<dir>/local-<size>` (the daemon's home), so a later run reuses
//! the filled store instead of embedding it again. Delete those folders to start over.
//!
//! Memory is read from the user container's cgroup (`/sys/fs/cgroup/docker/<id>`):
//! `memory.peak` for the whole life of the container, and `memory.current` and `memory.stat`
//! sampled every 20 ms for the phases. "Resident" below is `memory.current` less the page
//! cache (`file` in `memory.stat`), which the kernel reclaims before it kills.
#![allow(
    clippy::unwrap_used,
    clippy::expect_used,
    clippy::print_stdout,
    clippy::print_stderr,
    clippy::cast_precision_loss,
    clippy::cast_possible_truncation,
    clippy::cast_sign_loss,
    dead_code,
    unreachable_pub
)]

#[path = "support/stack.rs"]
mod stack;

use std::collections::HashMap;
use std::io::{BufRead, BufReader};
use std::path::{Path, PathBuf};
use std::process::{Child, Command, Stdio};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use reqwest::Method;
use serde_json::{Value, json};
use stack::{Answer, Cred, ROOT, Stack, docker, docker_ok, free_port, pinned_image, text};

const MIB: f64 = 1024.0 * 1024.0;

struct Env {
    size: usize,
    memory_mib: u64,
    image: String,
    dir: PathBuf,
    runs: usize,
}

fn env() -> Env {
    let var = |k: &str| std::env::var(k).ok().filter(|v| !v.is_empty());
    Env {
        size: var("KNOGGIN_MEASURE_SIZE").map_or(1500, |v| v.parse().unwrap()),
        memory_mib: var("KNOGGIN_MEASURE_MEMORY_MIB").map_or(1024, |v| v.parse().unwrap()),
        image: var("KNOGGIN_MEASURE_IMAGE").unwrap_or_else(|| "knoggin:measure".into()),
        dir: var("KNOGGIN_MEASURE_DIR")
            .map_or_else(|| Path::new(ROOT).join("target/measure"), PathBuf::from),
        runs: var("KNOGGIN_MEASURE_RUNS").map_or(5, |v| v.parse().unwrap()),
    }
}

/// The image with the real model; built once if it is missing.
fn ensure_image(image: &str) {
    if !docker(&["image", "inspect", image]).status.success() {
        stack::build(image, false);
    }
}

/// The `gen-export` file of `size` memories (seed 1), written once.
fn export_file(dir: &Path, size: usize) -> PathBuf {
    let path = dir.join(format!("export-{size}.ndjson"));
    if !path.exists() {
        std::fs::create_dir_all(dir).unwrap();
        // Written beside and renamed, so that two tests never read half a file.
        let part = dir.join(format!(
            "export-{size}.{}.part",
            std::process::id() ^ rand_tag()
        ));
        let ok = Command::new("cargo")
            .args([
                "run",
                "--locked",
                "-q",
                "-p",
                "knoggin-bench",
                "--",
                "gen-export",
            ])
            .args(["--count", &size.to_string(), "--seed", "1", "--out"])
            .arg(&part)
            .current_dir(ROOT)
            .status()
            .unwrap()
            .success();
        assert!(ok, "gen-export failed");
        std::fs::rename(&part, &path).unwrap();
    }
    path
}

fn rand_tag() -> u32 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap()
        .subsec_nanos()
}

fn mib(bytes: u64) -> String {
    format!("{:.0}", bytes as f64 / MIB)
}

fn median(v: &[f64]) -> f64 {
    let mut s = v.to_vec();
    s.sort_by(f64::total_cmp);
    if s.is_empty() {
        return 0.0;
    }
    let n = s.len();
    if n % 2 == 1 {
        s[n / 2]
    } else {
        (s[n / 2 - 1] + s[n / 2]) / 2.0
    }
}

/// Words for long texts: the same kind of text as gen-export, from a fixed list.
fn words(bytes: usize, salt: usize) -> String {
    const W: [&str; 16] = [
        "river",
        "Lisbon",
        "meeting",
        "budget",
        "quarterly",
        "design",
        "garden",
        "server",
        "travel",
        "Kafka",
        "recipe",
        "invoice",
        "hiking",
        "migration",
        "concert",
        "deadline",
    ];
    let mut out = String::new();
    let mut i = salt;
    while out.len() < bytes {
        if !out.is_empty() {
            out.push(' ');
        }
        out.push_str(W[(i * 7 + i / 3) % W.len()]);
        i += 1;
    }
    out.truncate(bytes);
    out.trim_end().to_owned()
}

// ------------------------------------------------------------------- cgroup memory

/// One reading of a container's cgroup.
#[derive(Debug, Clone, Copy, Default)]
struct Mem {
    current: u64,
    peak: u64,
    anon: u64,
    file: u64,
}

impl Mem {
    fn resident(&self) -> u64 {
        self.current.saturating_sub(self.file)
    }
}

const READ_MEM: &str = r#"f=/sys/fs/cgroup/docker/$ID
read c < $f/memory.current; read p < $f/memory.peak
a=0; fl=0
while read k v; do case $k in anon) a=$v;; file) fl=$v;; esac; done < $f/memory.stat
echo "$c $p $a $fl""#;

fn parse_mem(line: &str) -> Option<Mem> {
    let v: Vec<u64> = line
        .split_whitespace()
        .filter_map(|x| x.parse().ok())
        .collect();
    (v.len() == 4).then(|| Mem {
        current: v[0],
        peak: v[1],
        anon: v[2],
        file: v[3],
    })
}

fn cgroup_args(name: Option<&str>) -> Vec<String> {
    let mut a: Vec<String> = vec!["run".into(), "--rm".into()];
    if let Some(n) = name {
        a.extend(["--name".into(), n.into()]);
    }
    a.extend(
        [
            "--cgroupns",
            "host",
            "--cap-drop",
            "ALL",
            "--security-opt",
            "no-new-privileges",
            "-v",
            "/sys/fs/cgroup:/sys/fs/cgroup:ro",
        ]
        .map(String::from),
    );
    a
}

/// The full ID of a container: its cgroup is named by it.
fn full_id(id: &str) -> String {
    docker_ok(&["inspect", "--format", "{{.Id}}", id])
        .trim()
        .to_owned()
}

/// One reading of the container `id`.
fn read_mem(id: &str) -> Mem {
    let id = full_id(id);
    let mut args = cgroup_args(None);
    args.extend([
        "-e".into(),
        format!("ID={id}"),
        pinned_image("busybox"),
        "sh".into(),
        "-c".into(),
        READ_MEM.into(),
    ]);
    let args: Vec<&str> = args.iter().map(String::as_str).collect();
    parse_mem(docker_ok(&args).trim()).unwrap_or_default()
}

/// Samples a container's cgroup every 20 ms until dropped.
struct Watch {
    id: String,
    name: String,
    child: Child,
    samples: Arc<Mutex<Vec<(Instant, Mem)>>>,
}

impl Watch {
    fn start(id: &str) -> Self {
        let id = full_id(id);
        let name = format!("knmeasure-watch-{}", &id[..12]);
        let mut args = cgroup_args(Some(&name));
        let script = format!("while :; do {READ_MEM}; usleep 20000; done");
        args.extend([
            "-e".into(),
            format!("ID={id}"),
            pinned_image("busybox"),
            "sh".into(),
            "-c".into(),
            script,
        ]);
        let mut child = Command::new("docker")
            .args(&args)
            .stdin(Stdio::null())
            .stdout(Stdio::piped())
            .stderr(Stdio::null())
            .spawn()
            .unwrap();
        let samples = Arc::new(Mutex::new(Vec::new()));
        let out = child.stdout.take().unwrap();
        let s = samples.clone();
        std::thread::spawn(move || {
            for line in BufReader::new(out).lines() {
                let Ok(line) = line else { return };
                if let Some(m) = parse_mem(&line) {
                    s.lock().unwrap().push((Instant::now(), m));
                }
            }
        });
        // The first sample shows that the watch runs.
        let deadline = Instant::now() + Duration::from_secs(30);
        while samples.lock().unwrap().is_empty() {
            if Instant::now() > deadline {
                let _ = docker(&["rm", "-f", &name]);
                panic!("the memory watch gave no sample");
            }
            std::thread::sleep(Duration::from_millis(50));
        }
        Self {
            id,
            name,
            child,
            samples,
        }
    }

    /// The highest `memory.current` and the highest resident size between `from` and now,
    /// and `memory.peak` at the end.
    /// Whether the kernel killed the container for memory.
    fn oom_killed(&self) -> bool {
        docker(&["inspect", "--format", "{{.State.OOMKilled}}", &self.id])
            .stdout
            .starts_with(b"true")
    }

    fn window(&self, from: Instant) -> (u64, u64, u64) {
        let s = self.samples.lock().unwrap();
        let inside = s.iter().filter(|(t, _)| *t >= from);
        let (mut cur, mut res, mut peak) = (0, 0, 0);
        for (_, m) in inside {
            cur = cur.max(m.current);
            res = res.max(m.resident());
            peak = peak.max(m.peak);
        }
        (cur, res, peak)
    }
}

impl Drop for Watch {
    fn drop(&mut self) {
        let _ = docker(&["rm", "-f", &self.name]);
        let _ = self.child.kill();
        let _ = self.child.wait();
    }
}

// ------------------------------------------------------------------------ the stack

const SERVER_CONFIG: &str = r#"# Written by tests/measure.rs: like config.test.toml, without test mode (so that user
# containers load the real model) and so with sign-in off.
[auth]
mode = "none"

[log]
level = "info"
format = "json"

[metrics]
bind = "0.0.0.0:9091"

[state]
backend = "postgres"
postgres_url_file = "/run/secrets/postgres_url"
postgres_doc_key_file = "/run/secrets/state_key"

[storage]
bucket = "knoggin"
region = "us-east-1"
endpoint_url = "http://s3:8333"
force_path_style = true
credentials_file = "/run/secrets/s3_credentials"

[keys]
provider = "key_file"
master_key_file = "/run/secrets/master_key"

[launcher]
kind = "docker"

[launcher.docker]
url = "http://launcher:7070"
token_file = "/run/secrets/launcher_token"

[containers]
user_net_cidr = "10.201.0.0/16"
memory_mib = MEMORY_MIB
pool_target = 2
max = 4
idle_seconds = 60
claim_wait_ms = 60000

[internal]
secret_file = "/run/secrets/internal_secret"
cert_file = "/run/secrets/internal_cert.pem"
key_file = "/run/secrets/internal_key.pem"
"#;

/// Brings up the measurement stack whose state lives in `own`.
/// With `publish_s3`, SeaweedFS also joins `edge` so that its S3 port can be published,
/// and advertises the name `s3` (else it would advertise its `edge` address, which
/// `s3-init` on `store` cannot reach).
async fn measure_stack(e: &Env, own: &Path, project: &str, publish_s3: bool) -> (Stack, u16) {
    ensure_image(&e.image);
    for d in ["db", "s3"] {
        std::fs::create_dir_all(own.join(d)).unwrap();
    }
    let s = Stack::create_in(&[], &e.image, own, project);
    let s3_port = free_port();
    let config = own.join("config.toml");
    std::fs::write(
        &config,
        SERVER_CONFIG.replace("MEMORY_MIB", &e.memory_mib.to_string()),
    )
    .unwrap();
    let c = config.display();
    let knoggin = format!(
        "    image: {image}\n    volumes:\n      - {c}:/etc/knoggin/config.toml:ro\n",
        image = e.image
    );
    let compose = format!(
        "# Written by tests/measure.rs.\nservices:\n  router:\n{knoggin}    environment:\n      KNOGGIN_SERVER__PUBLIC_URL: https://127.0.0.1:{port}\n      KNOGGIN_SERVER__TRUSTED_PROXIES: {prefix}.0.1/32\n    ports:\n      - \"127.0.0.1:{port}:8080\"\n      - \"127.0.0.1:{metrics}:9091\"\n  launcher:\n    image: {image}\n    volumes:\n      - {c}:/etc/knoggin/config.toml:ro\n      - /var/run/docker.sock:/var/run/docker.sock\n  lifecycle:\n{knoggin}  db:\n    volumes:\n      - {db}:/var/lib/postgresql\n  s3:\n    volumes:\n      - {s3}:/data\n{publish}",
        prefix = s.prefix,
        port = s.port,
        // The metrics port: the stack's issuer port, which nothing else uses here.
        metrics = s.issuer_port,
        image = e.image,
        db = own.join("db").display(),
        s3 = own.join("s3").display(),
        publish = if publish_s3 {
            format!(
                "    networks: [store, edge]\n    ports:\n      - \"127.0.0.1:{s3_port}:8333\"\n    command:\n      - |\n        until [ -s /config/s3.json ]; do sleep 1; done\n        exec weed server -dir=/data -ip=s3 -ip.bind=0.0.0.0 -master.telemetry=false -s3 -s3.port=8333 -s3.config=/config/s3.json\n"
            )
        } else {
            String::new()
        },
    );
    std::fs::write(own.join("compose.override.yaml"), compose).unwrap();
    let out = s.compose(&["up", "-d", "--quiet-pull"]);
    assert!(out.status.success(), "compose up: {}", text(&out));
    s.wait_ready(Duration::from_secs(300)).await;
    (s, s3_port)
}

async fn get(s: &Stack, path: &str) -> Answer {
    s.call(Method::GET, path, &Cred::None, &[], None).await
}

/// `GET /v1/context`: counts as activity. `(pending, total)` of the embedding queue.
async fn context(s: &Stack) -> (u64, u64) {
    context_in(s, None).await
}

/// The same for `store` (the personal store without one).
async fn context_in(s: &Stack, store: Option<&str>) -> (u64, u64) {
    let headers: Vec<(&str, &str)> = store.map(|st| ("knoggin-store", st)).into_iter().collect();
    let a = s
        .call(Method::GET, "/v1/context", &Cred::None, &headers, None)
        .await;
    assert_eq!(a.status, 200, "{}", a.body);
    let v = a.json();
    (
        v["embedding"]["pending"].as_u64().unwrap_or(0),
        v["embedding"]["total"].as_u64().unwrap_or(0),
    )
}

/// Router metrics, from `metrics.bind` published on loopback (the router accepts it from
/// `server.trusted_proxies`, where published-port requests come from).
async fn router_metrics(s: &Stack) -> HashMap<String, f64> {
    let port = s.issuer_port;
    let body = reqwest::get(format!("http://127.0.0.1:{port}/metrics"))
        .await
        .unwrap()
        .text()
        .await
        .unwrap();
    parse_metrics(&body)
}

fn parse_metrics(body: &str) -> HashMap<String, f64> {
    body.lines()
        .filter(|l| !l.starts_with('#'))
        .filter_map(|l| {
            let (k, v) = l.rsplit_once(' ')?;
            Some((k.to_owned(), v.parse().ok()?))
        })
        .collect()
}

fn metric(m: &HashMap<String, f64>, name: &str) -> f64 {
    m.get(name).copied().unwrap_or(0.0)
}

/// Imports the export file and waits until the embedding queue is empty, sending
/// `GET /v1/context` (activity) every minute. Returns the fill rate in memories per second
/// over what was embedded while this ran, if anything was.
async fn fill(
    label: &str,
    size: usize,
    export: &Path,
    ctx: impl AsyncFn() -> (u64, u64),
    import: impl AsyncFnOnce(),
) -> Option<f64> {
    let (_, total) = ctx().await;
    if (total as usize) < size {
        println!("[{label}] importing {size} memories");
        let t = Instant::now();
        import().await;
        println!(
            "[{label}] import done in {:.1} s",
            t.elapsed().as_secs_f64()
        );
    }
    let _ = export;
    let (first, total) = ctx().await;
    println!("[{label}] store holds {total} embeddable memories, {first} pending");
    let started = Instant::now();
    let mut pending = first;
    while pending > 0 {
        tokio::time::sleep(Duration::from_secs(60)).await;
        pending = ctx().await.0;
        let done = first - pending;
        let secs = started.elapsed().as_secs_f64();
        let rate = done as f64 / secs;
        println!(
            "[{label}] {:.0} min: {done} embedded, {pending} pending, {rate:.3}/s, about {:.0} min left",
            secs / 60.0,
            if rate > 0.0 {
                pending as f64 / rate / 60.0
            } else {
                f64::NAN
            }
        );
    }
    (first > 0).then(|| first as f64 / started.elapsed().as_secs_f64())
}

/// Times `n` saves, one after the other, for `secs` seconds: latencies in ms.
/// The saves go to the namespace `m4-probe`, which is deleted afterwards, so that the
/// store keeps its size from one run to the next.
async fn save_loop(url: &str, token: Option<&str>, secs: u64) -> Vec<f64> {
    let http = reqwest::Client::new();
    let auth = |b: reqwest::RequestBuilder| match token {
        Some(t) => b.bearer_auth(t),
        None => b,
    };
    let created = auth(http.post(format!("{url}/v1/namespaces")))
        .json(&json!({ "name": "m4-probe" }))
        .send()
        .await
        .unwrap()
        .status();
    assert!(
        created.is_success() || created == 409,
        "creating m4-probe: {created}"
    );
    let end = Instant::now() + Duration::from_secs(secs);
    let mut out = Vec::new();
    let mut i = 0;
    while Instant::now() < end {
        let b = auth(http.post(format!("{url}/v1/memories"))).json(&json!({
            "namespace": "m4-probe",
            "summary": format!("save-wait probe {i}: {}", words(120, i)),
        }));
        let t = Instant::now();
        let r = b.send().await.unwrap();
        let status = r.status();
        let _ = r.bytes().await;
        assert!(status.is_success(), "a save answered {status}");
        out.push(t.elapsed().as_secs_f64() * 1000.0);
        i += 1;
        tokio::time::sleep(Duration::from_millis(100)).await;
    }
    let deleted = auth(http.delete(format!("{url}/v1/namespaces/m4-probe?force=true")))
        .send()
        .await
        .unwrap()
        .status();
    assert!(deleted.is_success(), "deleting m4-probe: {deleted}");
    out
}

/// A memory whose text is longer than 1,024 tokens, and a recall with a query of the
/// longest allowed length (4,096 bytes), sent at the same moment.
async fn two_embeddings(url: &str, token: Option<&str>, store: Option<&str>) -> (u16, u16) {
    let http = reqwest::Client::new();
    let auth = |b: reqwest::RequestBuilder| {
        let b = match store {
            Some(st) => b.header("knoggin-store", st),
            None => b,
        };
        match token {
            Some(t) => b.bearer_auth(t),
            None => b,
        }
    };
    let save = auth(http.post(format!("{url}/v1/memories"))).json(&json!({
        "summary": words(300, 1),
        "fullText": words(9000, 2),
    }));
    let recall =
        auth(http.post(format!("{url}/v1/recall"))).json(&json!({ "query": words(4096, 3) }));
    let (a, b) = tokio::join!(save.send(), async {
        // The save returns before its vector is computed; the recall follows at once.
        tokio::time::sleep(Duration::from_millis(20)).await;
        recall.send().await
    });
    (a.unwrap().status().as_u16(), b.unwrap().status().as_u16())
}

#[derive(Default)]
struct PhaseRow {
    phase: String,
    current: u64,
    resident: u64,
    peak: u64,
    oom_killed: bool,
}

impl PhaseRow {
    fn line(&self) -> String {
        format!(
            "| {} | {} | {} | {} | {} |",
            self.phase,
            mib(self.current),
            mib(self.resident),
            mib(self.peak),
            if self.oom_killed {
                "killed for memory"
            } else {
                "ran"
            }
        )
    }
}

/// The phase that started at `t`, printed at once.
fn row(phase: String, w: &Watch, t: Instant) -> PhaseRow {
    let (current, resident, peak) = w.window(t);
    let r = PhaseRow {
        phase,
        current,
        resident,
        peak,
        oom_killed: w.oom_killed(),
    };
    println!("{}", r.line());
    r
}

fn print_phases(title: &str, rows: &[PhaseRow]) {
    println!(
        "\n{title}\n| Phase | Highest memory.current (MiB) | Highest resident (MiB) | memory.peak so far (MiB) | Container |\n| --- | --- | --- | --- | --- |"
    );
    for r in rows {
        println!("{}", r.line());
    }
}

async fn snapshot_count(s: &Stack, store: &str) -> usize {
    get(s, &format!("/v1/admin/stores/{store}/snapshots"))
        .await
        .json()["items"]
        .as_array()
        .map_or(0, Vec::len)
}

/// The running container that was claimed (there is one user).
fn claimed(s: &Stack) -> Option<stack::UserContainer> {
    s.user_containers()
        .into_iter()
        .find(|c| c.running && text(&docker(&["logs", &c.docker_id])).contains("\"claimed\""))
}

/// Waits until the user's container has parked and a pool container is ready.
async fn wait_parked(s: &Stack) {
    let ok = stack::eventually(Duration::from_secs(600), || async {
        if claimed(s).is_some() {
            return false;
        }
        let a = get(s, "/v1/admin/status").await;
        a.json()["containers"]["ready"].as_u64().unwrap_or(0) >= 1
    })
    .await;
    assert!(
        ok,
        "the container did not park, or no pool container became ready"
    );
}

#[tokio::test(flavor = "multi_thread")]
#[ignore = "measurement"]
async fn server() {
    let e = env();
    let own = e.dir.join(format!("server-{}", e.size));
    let export = export_file(&e.dir, e.size);
    let (s, _) = measure_stack(&e, &own, &format!("knmeasure-{}", e.size), false).await;
    // Sign-in is off: every caller is the one local user, and the first store is theirs.
    let stores = get(&s, "/v1/stores").await;
    let stores = serde_json::from_str::<Value>(&stores.body)
        .unwrap_or(json!({ "body": stores.body, "status": stores.status }));
    let personal = stores["items"][0]["id"]
        .as_str()
        .unwrap_or_else(|| panic!("no personal store in {stores}"))
        .to_owned();
    println!(
        "[server {}] memory_mib {}, store {personal}",
        e.size, e.memory_mib
    );
    let mut phases = Vec::new();

    // Fill.
    let rate = fill(
        &format!("server {}", e.size),
        e.size,
        &export,
        || context(&s),
        || async {
            let body = std::fs::read(&export).unwrap();
            let a = reqwest::Client::new()
                .post(format!("{}/v1/import", s.base))
                .header("content-type", "application/x-ndjson")
                .body(body)
                .send()
                .await
                .unwrap();
            let status = a.status();
            let v: Value = a.json().await.unwrap_or(Value::Null);
            println!(
                "import: {status} imported {} skipped {} errors {}",
                v["imported"],
                v["skipped"],
                v["errors"].as_array().map_or(0, Vec::len)
            );
            assert!(status.is_success(), "{v}");
        },
    )
    .await;
    if let Some(r) = rate {
        println!(
            "\nfill rate: {r:.3} memories/s ({:.2} s each). Projected fill: 15,000 in {:.1} h, 75,000 in {:.1} h",
            1.0 / r,
            15_000.0 / r / 3600.0,
            75_000.0 / r / 3600.0
        );
    }
    if std::env::var("KNOGGIN_MEASURE_FILL_ONLY").as_deref() == Ok("1") {
        s.teardown();
        return;
    }

    // The size estimate's inputs, from the namespace stats.
    let stats = get(&s, "/v1/namespaces/default/stats").await.json();
    let (text_bytes, memories, edges) = (
        stats["textBytes"].as_u64().unwrap_or(0),
        stats["memoryCount"].as_u64().unwrap_or(0),
        stats["edgeCount"].as_u64().unwrap_or(0),
    );
    let ghosts = stats["phaseCounts"]["ghost"].as_u64().unwrap_or(0);
    let estimate = 3 * text_bytes + 4 * 768 * (memories - ghosts) + 300 * memories + 100 * edges;
    println!(
        "stats: {memories} memories, {edges} edges, {text_bytes} text bytes; estimate without idempotency entries {} MiB",
        mib(estimate)
    );

    // M3, empty store: a fresh container that opens only a new, empty app store. The
    // two-embeddings case there sets the model reserve.
    let created = s
        .call(
            Method::POST,
            "/v1/stores",
            &Cred::None,
            &[],
            Some(json!({ "name": format!("m3-empty-{}", rand_tag()) })),
        )
        .await;
    assert_eq!(created.status, 201, "{}", created.body);
    let empty = created.json()["id"].as_str().unwrap().to_owned();
    wait_parked(&s).await;
    let a = s
        .call(
            Method::GET,
            "/v1/memories?limit=1",
            &Cred::None,
            &[("knoggin-store", &empty)],
            None,
        )
        .await;
    assert_eq!(a.status, 200, "{}", a.body);
    let c = claimed(&s).expect("a claimed container");
    let w = Watch::start(&c.docker_id);
    let t = Instant::now();
    println!(
        "two embeddings, empty store: {:?}",
        two_embeddings(&s.base, None, Some(&empty)).await
    );
    let _ = stack::eventually(Duration::from_secs(120), || async {
        context_in(&s, Some(&empty)).await.0 == 0
    })
    .await;
    tokio::time::sleep(Duration::from_secs(1)).await;
    phases.push(row("two embeddings, empty store".into(), &w, t));
    drop(w);

    // M2 run 1 and M3 load: park, then claim from a ready pool container.
    let mut m2 = Vec::new();
    wait_parked(&s).await;
    let before: HashMap<String, Mem> = s
        .user_containers()
        .into_iter()
        .filter(|c| c.running)
        .map(|c| {
            let m = read_mem(&c.docker_id);
            (c.docker_id, m)
        })
        .collect();
    let t = Instant::now();
    let a = get(&s, "/v1/memories?limit=1").await;
    m2.push(t.elapsed().as_secs_f64());
    assert_eq!(a.status, 200, "{}", a.body);
    let c = claimed(&s).expect("a claimed container");
    let loaded = read_mem(&c.docker_id);
    // A ready pool container: the model loaded, no store. The claimed one if it was
    // listed, otherwise the largest of the others (they are alike).
    let idle = before.get(&c.docker_id).copied().unwrap_or_else(|| {
        before
            .values()
            .copied()
            .max_by_key(Mem::resident)
            .unwrap_or_default()
    });
    println!(
        "ready pool containers before the claim (resident MiB): {:?}",
        before
            .values()
            .map(|m| mib(m.resident()))
            .collect::<Vec<_>>()
    );
    let r = PhaseRow {
        phase: "load (claim to first answer)".into(),
        current: loaded.current,
        resident: loaded.resident(),
        peak: loaded.peak,
        oom_killed: false,
    };
    println!("{}", r.line());
    phases.push(r);
    println!(
        "pool container before the claim: resident {} MiB (current {}); after loading the store: resident {} MiB (current {}). The store adds {} MiB against an estimate of {} MiB",
        mib(idle.resident()),
        mib(idle.current),
        mib(loaded.resident()),
        mib(loaded.current),
        mib(loaded.resident().saturating_sub(idle.resident())),
        mib(estimate)
    );

    // M3: recall, snapshot, the two-embeddings case.
    let w = Watch::start(&c.docker_id);
    let t = Instant::now();
    for i in 0..10 {
        let r = s
            .call(
                Method::POST,
                "/v1/recall",
                &Cred::None,
                &[],
                Some(json!({ "query": words(60, i * 11) })),
            )
            .await;
        assert_eq!(r.status, 200, "{}", r.body);
    }
    phases.push(row("10 recalls".into(), &w, t));

    let n0 = snapshot_count(&s, &personal).await;
    let t = Instant::now();
    assert_eq!(
        s.save(&Cred::None, None, "snapshot trigger").await.status,
        201
    );
    assert!(
        stack::eventually(Duration::from_secs(90), || async {
            snapshot_count(&s, &personal).await > n0
        })
        .await,
        "no snapshot"
    );
    phases.push(row("snapshot".into(), &w, t));

    let t = Instant::now();
    println!(
        "two embeddings: {:?}",
        two_embeddings(&s.base, None, None).await
    );
    let _ = stack::eventually(Duration::from_secs(120), || async {
        context(&s).await.0 == 0
    })
    .await;
    tokio::time::sleep(Duration::from_secs(1)).await;
    phases.push(row(format!("two embeddings, {} memories", e.size), &w, t));
    drop(w);
    print_phases(
        &format!("M3 at {} memories, memory_mib {}", e.size, e.memory_mib),
        &phases,
    );

    // M2, the other runs.
    for _ in 1..e.runs {
        wait_parked(&s).await;
        let t = Instant::now();
        let a = get(&s, "/v1/memories?limit=1").await;
        m2.push(t.elapsed().as_secs_f64());
        assert_eq!(a.status, 200, "{}", a.body);
    }
    println!(
        "\nM2 at {}: median {:.2} s, runs {}",
        e.size,
        median(&m2),
        m2.iter()
            .map(|x| format!("{x:.2}"))
            .collect::<Vec<_>>()
            .join(", ")
    );

    // M4 on SeaweedFS: the router sums the claimed containers' counters every 30 s, so
    // first claim and let one pass include the container; then saves for 45 s (a snapshot
    // at least every 30 s), then quiet until the next pass.
    // Its counters only rise (`absolute` keeps the highest value seen), so a container
    // with lower totals than an earlier one would not show: restart the router first.
    s.compose_ok(&["restart", "router"]);
    s.wait_ready(Duration::from_secs(300)).await;
    for _ in 0..4 {
        let _ = get(&s, "/v1/tags").await;
        tokio::time::sleep(Duration::from_secs(10)).await;
    }
    let m0 = router_metrics(&s).await;
    let lat = save_loop(&s.base, None, 45).await;
    for _ in 0..4 {
        tokio::time::sleep(Duration::from_secs(10)).await;
        let _ = get(&s, "/v1/tags").await;
    }
    let m1 = router_metrics(&s).await;
    let d = |k: &str| metric(&m1, k) - metric(&m0, k);
    let n = d("knoggin_container_snapshot_count_total");
    for (k, v) in &m1 {
        if k.starts_with("knoggin_container_snapshot") {
            println!("  {k} {} -> {v}", metric(&m0, k));
        }
    }
    let newest = get(&s, &format!("/v1/admin/stores/{}/snapshots", personal))
        .await
        .json();
    println!(
        "\nM4 SeaweedFS at {}: {n} snapshots, mean {:.0} ms, mean {:.1} MiB; newest object {:.1} MiB; {} saves: median {:.1} ms, longest {:.0} ms",
        e.size,
        d("knoggin_container_snapshot_millis_total") / n.max(1.0),
        d("knoggin_container_snapshot_bytes_total") / n.max(1.0) / MIB,
        newest["items"][0]["sizeBytes"].as_f64().unwrap_or(0.0) / MIB,
        lat.len(),
        median(&lat),
        lat.iter().copied().fold(0.0, f64::max)
    );
    s.teardown();
}

// -------------------------------------------------------------------- local daemon

#[tokio::test(flavor = "multi_thread")]
#[ignore = "measurement"]
async fn local_daemon() {
    let e = env();
    ensure_image(&e.image);
    let export = export_file(&e.dir, e.size);
    let home = e.dir.join(format!("local-{}", e.size));
    std::fs::create_dir_all(&home).unwrap();
    let (port, metrics_port) = (free_port(), free_port());
    std::fs::write(
        home.join("config.toml"),
        format!(
            "[local]\nmaster_key = \"key_file\"\n\n[server]\nbind = \"127.0.0.1:{port}\"\n\n[metrics]\nbind = \"127.0.0.1:{metrics_port}\"\n"
        ),
    )
    .unwrap();
    let name = format!("knmeasure-local-{}", e.size);
    let _ = docker(&["rm", "-f", &name]);
    // Host networking, so that the loopback-only listener is reachable from the test.
    // One CPU, like a user container.
    docker_ok(&[
        "run",
        "-d",
        "--name",
        &name,
        "--network",
        "host",
        "--cpus",
        "1",
        "--cap-drop",
        "ALL",
        "--security-opt",
        "no-new-privileges",
        "--read-only",
        "-e",
        "KNOGGIN_HOME=/home/kn",
        "-e",
        "KNOGGIN_CONFIG=/home/kn/config.toml",
        "-v",
        &format!("{}:/home/kn", home.display()),
        "-v",
        &format!("{}:/export.ndjson:ro", export.display()),
        &e.image,
        "daemon",
    ]);
    struct Gone(String);
    impl Drop for Gone {
        fn drop(&mut self) {
            if std::thread::panicking() {
                eprintln!("{}", stack::tail(&text(&docker(&["logs", &self.0])), 60));
            }
            let _ = docker(&["stop", "-t", "60", &self.0]);
            let _ = docker(&["rm", "-f", &self.0]);
        }
    }
    let _gone = Gone(name.clone());
    let base = format!("http://127.0.0.1:{port}");
    let http = reqwest::Client::new();
    assert!(
        stack::eventually(Duration::from_secs(300), || async {
            http.get(format!("{base}/readyz"))
                .send()
                .await
                .is_ok_and(|r| r.status() == 200)
        })
        .await,
        "the daemon was not ready"
    );
    let key = docker_ok(&[
        "exec", &name, "knoggin", "keys", "create", "--name", "measure",
    ]);
    let key = key.lines().next().unwrap().trim().to_owned();
    let ctx = || async {
        let v: Value = http
            .get(format!("{base}/v1/context"))
            .bearer_auth(&key)
            .send()
            .await
            .unwrap()
            .json()
            .await
            .unwrap();
        (
            v["embedding"]["pending"].as_u64().unwrap_or(0),
            v["embedding"]["total"].as_u64().unwrap_or(0),
        )
    };
    let label = format!("local {}", e.size);
    let rate = fill(&label, e.size, &export, ctx, || async {
        println!(
            "{}",
            text(&docker(&[
                "exec",
                &name,
                "knoggin",
                "import",
                "/export.ndjson"
            ]))
        );
    })
    .await;
    if let Some(r) = rate {
        println!("[{label}] fill rate {r:.3}/s");
    }
    let metrics = || async {
        parse_metrics(
            &http
                .get(format!("http://127.0.0.1:{metrics_port}/metrics"))
                .send()
                .await
                .unwrap()
                .text()
                .await
                .unwrap(),
        )
    };
    let m0 = metrics().await;
    let lat = save_loop(&base, Some(&key), 30).await;
    tokio::time::sleep(Duration::from_secs(5)).await;
    let m1 = metrics().await;
    let d = |k: &str| metric(&m1, k) - metric(&m0, k);
    let n = d("knoggin_snapshot_seconds_count");
    // The newest snapshot file of the personal store.
    let mut files: Vec<(std::time::SystemTime, u64)> = Vec::new();
    let mut stack_dirs = vec![home.clone()];
    while let Some(d) = stack_dirs.pop() {
        for f in std::fs::read_dir(d).unwrap().flatten() {
            let md = f.metadata().unwrap();
            if md.is_dir() {
                stack_dirs.push(f.path());
            } else if f.path().extension().is_some_and(|x| x == "kgs") {
                files.push((md.modified().unwrap(), md.len()));
            }
        }
    }
    files.sort();
    println!(
        "\nM4 local disk at {}: {n} snapshots, mean {:.0} ms, mean {:.1} MiB; newest file {:.1} MiB; {} saves: median {:.1} ms, longest {:.0} ms",
        e.size,
        d("knoggin_snapshot_seconds_sum") * 1000.0 / n.max(1.0),
        d("knoggin_snapshot_bytes_sum") / n.max(1.0) / MIB,
        files.last().map_or(0.0, |f| f.1 as f64 / MIB),
        lat.len(),
        median(&lat),
        lat.iter().copied().fold(0.0, f64::max)
    );
}

// ------------------------------------------------------------------------------ M7

#[tokio::test(flavor = "multi_thread")]
#[ignore = "measurement"]
async fn seaweedfs() {
    let e = env();
    let own = e.dir.join("m7");
    let (s, s3_port) = measure_stack(&e, &own, "knmeasure-m7", true).await;
    let selftest = s.knoggin(&["admin", "selftest"]);
    println!(
        "admin selftest (exit {:?}):\n{}",
        selftest.status.code(),
        text(&selftest)
    );

    // An upload link signed as the router signs them, with a 5-second lifetime, used
    // for an upload that takes about 16 seconds; and two controls at full speed: one in
    // time, and one with a link signed 10 seconds earlier, which must be refused.
    let settings: knoggin_server::settings::StorageSettings = toml::from_str(&format!(
        "bucket = \"knoggin\"\nregion = \"us-east-1\"\nendpoint_url = \"http://127.0.0.1:{s3_port}\"\nforce_path_style = true\ncredentials_file = \"{}\"\n",
        s.secrets_dir().join("s3_credentials").display()
    ))
    .unwrap();
    let bucket = knoggin_server::storage::S3Storage::connect(&settings).await;
    let http = reqwest::Client::new();
    const CHUNK: usize = 4096;
    const CHUNKS: usize = 16;
    for (name, pause, signed_ago) in [
        ("fast", Duration::ZERO, 0),
        ("expired", Duration::ZERO, 10_000),
        ("slow", Duration::from_secs(1), 0),
    ] {
        let now = knoggin_core::Timestamp::from_millis(
            i64::try_from(
                std::time::SystemTime::now()
                    .duration_since(std::time::UNIX_EPOCH)
                    .unwrap()
                    .as_millis(),
            )
            .unwrap()
                - signed_ago,
        );
        let link = knoggin_server::ObjectStorage::presign_put_new(
            &bucket,
            &format!("measure/m7-{name}-{}", now.millis()),
            (CHUNK * CHUNKS) as u64,
            Duration::from_secs(5),
            now,
        )
        .await
        .unwrap();
        let body = futures_util::stream::unfold(0usize, move |i| async move {
            if i == CHUNKS {
                return None;
            }
            if i > 0 {
                tokio::time::sleep(pause).await;
            }
            Some((
                Ok::<_, std::io::Error>(bytes::Bytes::from(vec![b'k'; CHUNK])),
                i + 1,
            ))
        });
        let mut req = http.request(
            Method::from_bytes(link.method.as_bytes()).unwrap(),
            &link.url,
        );
        for (k, v) in &link.headers {
            req = req.header(k, v);
        }
        let t = Instant::now();
        let r = req.body(reqwest::Body::wrap_stream(body)).send().await;
        let elapsed = t.elapsed().as_secs_f64();
        match r {
            Ok(r) => {
                let status = r.status();
                let body = r.text().await.unwrap_or_default();
                println!(
                    "M7 {name} upload: {status} after {elapsed:.1} s (link lifetime 5 s) {}",
                    stack::tail(&body, 3)
                );
            }
            Err(err) => {
                let mut chain = String::new();
                let mut e: Option<&dyn std::error::Error> = Some(&err);
                while let Some(x) = e {
                    chain.push_str(&format!(" / {x}"));
                    e = x.source();
                }
                println!(
                    "M7 {name} upload: failed after {elapsed:.1} s (link lifetime 5 s){chain}"
                );
            }
        }
    }
    s.teardown();
}
