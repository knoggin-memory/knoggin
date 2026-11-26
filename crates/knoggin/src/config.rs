// SPDX-License-Identifier: AGPL-3.0-only
//! The config loader (tab "Config, CLI, local install", Config file): one strict TOML
//! schema for every role, in four layers (defaults, the file, `KNOGGIN_<PATH>` variables,
//! flags), and secrets that come only from `<key>_file` or their environment variable.

use std::collections::BTreeMap;
use std::fmt;
use std::net::SocketAddr;
use std::os::unix::fs::MetadataExt;
use std::path::{Path, PathBuf};

use anyhow::{Context, anyhow, bail};
use knoggin_core::Sensitive;
use serde::{Deserialize, Serialize};
use toml::{Table, Value};

/// What `main` read from the process environment, handed down as plain values: no other
/// code reads an environment variable.
#[derive(Clone)]
pub(crate) struct Env {
    /// `KNOGGIN_HOME`, else `~/.knoggin`, made absolute.
    pub(crate) home: PathBuf,
    /// Whether `KNOGGIN_HOME` was set: it moves the default key file into the home.
    pub(crate) home_set: bool,
    /// `HOME`.
    pub(crate) user_home: PathBuf,
    /// `KNOGGIN_CONFIG`.
    pub(crate) config: Option<PathBuf>,
    /// `KNOGGIN_TEST_MODE=1`.
    pub(crate) test_mode: bool,
    /// `KNOGGIN_API_KEY`.
    pub(crate) api_key: Option<Sensitive<String>>,
    /// Every `KNOGGIN_*` variable, for the settings layer. Values are kept, not logged.
    pub(crate) vars: BTreeMap<String, Sensitive<String>>,
    /// The working directory, where `knoggin mcp` starts looking for `.knoggin.toml`.
    pub(crate) cwd: PathBuf,
    /// `ORT_DYLIB_PATH`, the ONNX Runtime library; read, never set.
    pub(crate) ort_dylib: Option<PathBuf>,
}

impl fmt::Debug for Env {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("Env")
            .field("home", &self.home)
            .field("test_mode", &self.test_mode)
            .finish_non_exhaustive()
    }
}

impl Env {
    /// Reads the process environment. Called once, by `main`.
    pub(crate) fn from_process() -> anyhow::Result<Self> {
        let vars: BTreeMap<String, String> = std::env::vars_os()
            .filter_map(|(k, v)| Some((k.into_string().ok()?, v.into_string().ok()?)))
            .collect();
        let cwd = std::env::current_dir().context("reading the working directory")?;
        Self::from_vars(vars, cwd)
    }

    /// [`Env::from_process`] over given variables, for tests.
    pub(crate) fn from_vars(vars: BTreeMap<String, String>, cwd: PathBuf) -> anyhow::Result<Self> {
        let test_mode = match vars.get("KNOGGIN_TEST_MODE").map(String::as_str) {
            None => false,
            Some("1") => true,
            // A typo must never be read as on or as off.
            Some(_) => bail!("KNOGGIN_TEST_MODE must be 1 or unset"),
        };
        let user_home = vars
            .get("HOME")
            .filter(|h| !h.is_empty())
            .map(PathBuf::from)
            .ok_or_else(|| anyhow!("HOME is not set"))?;
        let (home, home_set) = match vars.get("KNOGGIN_HOME").filter(|h| !h.is_empty()) {
            Some(h) => (PathBuf::from(h), true),
            None => (user_home.join(".knoggin"), false),
        };
        let home = std::path::absolute(&home).context("making KNOGGIN_HOME absolute")?;
        let ort_dylib = vars
            .get("ORT_DYLIB_PATH")
            .filter(|p| !p.is_empty())
            .map(PathBuf::from);
        Ok(Self {
            home,
            home_set,
            user_home,
            config: vars.get("KNOGGIN_CONFIG").map(PathBuf::from),
            test_mode,
            api_key: vars
                .get("KNOGGIN_API_KEY")
                .filter(|k| !k.is_empty())
                .map(|k| Sensitive::new(k.clone())),
            vars: vars
                .into_iter()
                .filter(|(k, _)| k.starts_with("KNOGGIN_"))
                .map(|(k, v)| (k, Sensitive::new(v)))
                .collect(),
            cwd,
            ort_dylib,
        })
    }

    /// The files under the Knoggin home folder.
    pub(crate) fn paths(&self) -> Home {
        Home(self.home.clone())
    }
}

/// `~/.knoggin` (or `KNOGGIN_HOME`) and the fixed names inside it.
#[derive(Debug, Clone)]
pub(crate) struct Home(pub(crate) PathBuf);

impl Home {
    pub(crate) fn root(&self) -> &Path {
        &self.0
    }
    pub(crate) fn run(&self) -> PathBuf {
        self.0.join("run")
    }
    pub(crate) fn socket(&self) -> PathBuf {
        self.0.join("run/daemon.sock")
    }
    pub(crate) fn lock(&self) -> PathBuf {
        self.0.join("run/daemon.lock")
    }
    pub(crate) fn log_file(&self) -> PathBuf {
        self.0.join("logs/daemon.log")
    }
}

/// Which role loads the config: it picks the default file and the defaults that differ
/// between the local install and a server.
#[derive(Debug, Clone, Copy, PartialEq, Eq, clap::ValueEnum)]
pub(crate) enum Role {
    /// The local daemon and the local commands.
    Local,
    /// The router.
    Router,
    /// The lifecycle task.
    Lifecycle,
    /// The launcher service.
    Launcher,
    /// `admin`, `healthcheck`, `key generate`, `test-issuer`.
    Admin,
    /// `serve`, inside a user container: no file, only the environment.
    Serve,
}

impl Role {
    fn is_server(self) -> bool {
        !matches!(self, Self::Local)
    }
}

const ETC_CONFIG: &str = "/etc/knoggin/config.toml";

// The settings. `#[serde(default)]` holds the local defaults; the few that differ on a
// server are `Option`s filled in by `resolve`.

#[derive(Clone, Serialize, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub(crate) struct Config {
    pub(crate) source_url: String,
    pub(crate) server: Server,
    pub(crate) auth: Auth,
    pub(crate) limits: Limits,
    pub(crate) model: Model,
    pub(crate) local: Local,
    pub(crate) log: Log,
    pub(crate) metrics: Metrics,
    pub(crate) state: State,
    pub(crate) storage: Storage,
    pub(crate) retention: Retention,
    pub(crate) keys: Keys,
    pub(crate) launcher: Launcher,
    pub(crate) containers: Containers,
    pub(crate) internal: Internal,
    pub(crate) rollout: Rollout,
    /// Secret values given through their environment variable. Never serialized.
    #[serde(skip)]
    secrets: BTreeMap<&'static str, Sensitive<String>>,
}

/// Names no person: no email, subject, path under a home folder or URL that the owner set.
impl fmt::Debug for Config {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("Config")
            .field("server.bind", &self.server.bind)
            .field("auth", &self.auth)
            .field("log", &self.log)
            .field("metrics.bind", &self.metrics.bind.is_some())
            .field("state.backend", &self.state.backend)
            .field("secrets", &self.secrets.len())
            .finish_non_exhaustive()
    }
}

impl Default for Config {
    fn default() -> Self {
        Self {
            source_url: env!("CARGO_PKG_REPOSITORY").to_owned(),
            server: Server::default(),
            auth: Auth::default(),
            limits: Limits::default(),
            model: Model::default(),
            local: Local::default(),
            log: Log::default(),
            metrics: Metrics::default(),
            state: State::default(),
            storage: Storage::default(),
            retention: Retention::default(),
            keys: Keys::default(),
            launcher: Launcher::default(),
            containers: Containers::default(),
            internal: Internal::default(),
            rollout: Rollout::default(),
            secrets: BTreeMap::new(),
        }
    }
}

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub(crate) struct Server {
    pub(crate) bind: Option<SocketAddr>,
    pub(crate) public_url: Option<String>,
    pub(crate) trusted_proxies: Vec<String>,
    pub(crate) image_digest: Option<String>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub(crate) enum AuthMode {
    None,
    Oauth,
}

#[derive(Clone, Serialize, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub(crate) struct Auth {
    pub(crate) mode: Option<AuthMode>,
    pub(crate) issuer: Option<String>,
    pub(crate) audiences: Vec<String>,
    pub(crate) provider_name: String,
    pub(crate) web_client_id: Option<String>,
    pub(crate) web_client_secret_file: Option<PathBuf>,
    pub(crate) admin_emails: Vec<String>,
    pub(crate) admin_subjects: Vec<String>,
    pub(crate) allowed_emails: Vec<String>,
    pub(crate) allowed_email_domains: Vec<String>,
    pub(crate) email_claim: Option<String>,
    pub(crate) session_max_days: u32,
    pub(crate) session_idle_hours: u32,
}

/// Counts only: the lists hold people's emails and subjects.
impl fmt::Debug for Auth {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("Auth")
            .field("mode", &self.mode)
            .field("admin_emails", &self.admin_emails.len())
            .field("admin_subjects", &self.admin_subjects.len())
            .field("allowed_emails", &self.allowed_emails.len())
            .field("allowed_email_domains", &self.allowed_email_domains.len())
            .finish_non_exhaustive()
    }
}

impl Default for Auth {
    fn default() -> Self {
        Self {
            mode: None,
            issuer: None,
            audiences: Vec::new(),
            provider_name: "Identity provider".into(),
            web_client_id: None,
            web_client_secret_file: None,
            admin_emails: Vec::new(),
            admin_subjects: Vec::new(),
            allowed_emails: Vec::new(),
            allowed_email_domains: Vec::new(),
            email_claim: None,
            session_max_days: 7,
            session_idle_hours: 24,
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub(crate) struct Limits {
    pub(crate) body_bytes: u64,
    pub(crate) request_timeout_ms: u64,
    pub(crate) per_address_rps: f64,
    pub(crate) per_address_burst: f64,
    pub(crate) per_credential_rps: f64,
    pub(crate) per_credential_burst: f64,
    pub(crate) per_user_rps: f64,
    pub(crate) per_user_burst: f64,
    pub(crate) store_concurrency: u32,
    pub(crate) upload_bytes_per_hour: Option<u64>,
}

impl Default for Limits {
    fn default() -> Self {
        Self {
            body_bytes: 10_485_760,
            request_timeout_ms: 30_000,
            per_address_rps: 50.0,
            per_address_burst: 100.0,
            per_credential_rps: 20.0,
            per_credential_burst: 40.0,
            per_user_rps: 100.0,
            per_user_burst: 200.0,
            store_concurrency: 64,
            upload_bytes_per_hour: None,
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub(crate) struct Model {
    /// Empty until `resolve` puts in `<home>/models`.
    pub(crate) dir: PathBuf,
    pub(crate) download: bool,
}

impl Default for Model {
    fn default() -> Self {
        Self {
            dir: PathBuf::new(),
            download: true,
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub(crate) enum MasterKeyChoice {
    #[default]
    Auto,
    Keychain,
    KeyFile,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub(crate) struct Local {
    /// Empty until `resolve` puts in `<home>/data`.
    pub(crate) data_dir: PathBuf,
    pub(crate) master_key: MasterKeyChoice,
    /// Empty until `resolve` puts in the default key file.
    pub(crate) master_key_file: PathBuf,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize, clap::ValueEnum)]
#[serde(rename_all = "snake_case")]
pub(crate) enum LogLevel {
    Error,
    Warn,
    #[default]
    Info,
    Debug,
    Trace,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub(crate) enum LogFormat {
    Text,
    Json,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub(crate) struct Log {
    pub(crate) level: LogLevel,
    pub(crate) format: Option<LogFormat>,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub(crate) struct Metrics {
    /// Off when unset or empty.
    pub(crate) bind: Option<String>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub(crate) enum Backend {
    LocalFile,
    Postgres,
    Dynamodb,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub(crate) struct State {
    pub(crate) backend: Option<Backend>,
    pub(crate) postgres_url_file: Option<PathBuf>,
    pub(crate) postgres_doc_key_file: Option<PathBuf>,
    pub(crate) dynamodb_table: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub(crate) struct Storage {
    pub(crate) bucket: Option<String>,
    pub(crate) region: Option<String>,
    pub(crate) endpoint_url: Option<String>,
    pub(crate) force_path_style: bool,
    pub(crate) credentials_file: Option<PathBuf>,
    pub(crate) link_ttl_seconds: u64,
}

impl Default for Storage {
    fn default() -> Self {
        Self {
            bucket: None,
            region: None,
            endpoint_url: None,
            force_path_style: false,
            credentials_file: None,
            link_ttl_seconds: 300,
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub(crate) struct Retention {
    pub(crate) days: u32,
    pub(crate) soft_delete_days: u32,
}

impl Default for Retention {
    fn default() -> Self {
        Self {
            days: 7,
            soft_delete_days: 7,
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub(crate) enum KeyProviderKind {
    Kms,
    KeyFile,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub(crate) struct Keys {
    pub(crate) provider: Option<KeyProviderKind>,
    pub(crate) kms_key_id: Option<String>,
    pub(crate) master_key_file: Option<PathBuf>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub(crate) enum LauncherKind {
    Ecs,
    Docker,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub(crate) struct Launcher {
    pub(crate) kind: Option<LauncherKind>,
    pub(crate) bind: String,
    pub(crate) ecs: Ecs,
    pub(crate) docker: Docker,
}

impl Default for Launcher {
    fn default() -> Self {
        Self {
            kind: None,
            bind: "0.0.0.0:7070".into(),
            ecs: Ecs::default(),
            docker: Docker::default(),
        }
    }
}

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub(crate) struct Ecs {
    pub(crate) cluster: Option<String>,
    pub(crate) task_definition: Option<String>,
    pub(crate) subnets: Vec<String>,
    pub(crate) security_groups: Vec<String>,
    pub(crate) capacity_provider: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub(crate) struct Docker {
    pub(crate) url: String,
    pub(crate) token_file: Option<PathBuf>,
}

impl Default for Docker {
    fn default() -> Self {
        Self {
            url: "http://launcher:7070".into(),
            token_file: None,
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub(crate) struct Containers {
    pub(crate) user_net_cidr: String,
    pub(crate) memory_mib: u64,
    pub(crate) pool_target: u32,
    pub(crate) max: u32,
    pub(crate) idle_seconds: u64,
    pub(crate) max_lifetime_hours: u64,
    pub(crate) claim_wait_ms: u64,
}

impl Default for Containers {
    fn default() -> Self {
        Self {
            user_net_cidr: "10.201.0.0/16".into(),
            memory_mib: 1024,
            pool_target: 2,
            max: 0,
            idle_seconds: 600,
            max_lifetime_hours: 24,
            claim_wait_ms: 20_000,
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub(crate) struct Internal {
    pub(crate) bind: String,
    pub(crate) router_url: Option<String>,
    pub(crate) secret_file: Option<PathBuf>,
    pub(crate) cert_file: Option<PathBuf>,
    pub(crate) key_file: Option<PathBuf>,
}

impl Default for Internal {
    fn default() -> Self {
        Self {
            bind: "0.0.0.0:8443".into(),
            router_url: None,
            secret_file: None,
            cert_file: None,
            key_file: None,
        }
    }
}

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub(crate) struct Rollout {
    pub(crate) allow_downgrade: bool,
}

/// How an environment variable's text becomes a TOML value.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Kind {
    Str,
    Int,
    Float,
    Bool,
    List,
}

/// Every setting, with its kind: the environment layer reads only these names.
const KEYS: &[(&str, Kind)] = {
    use Kind::*;
    &[
        ("source_url", Str),
        ("server.bind", Str),
        ("server.public_url", Str),
        ("server.trusted_proxies", List),
        ("server.image_digest", Str),
        ("auth.mode", Str),
        ("auth.issuer", Str),
        ("auth.audiences", List),
        ("auth.provider_name", Str),
        ("auth.web_client_id", Str),
        ("auth.web_client_secret_file", Str),
        ("auth.admin_emails", List),
        ("auth.admin_subjects", List),
        ("auth.allowed_emails", List),
        ("auth.allowed_email_domains", List),
        ("auth.email_claim", Str),
        ("auth.session_max_days", Int),
        ("auth.session_idle_hours", Int),
        ("limits.body_bytes", Int),
        ("limits.request_timeout_ms", Int),
        ("limits.per_address_rps", Float),
        ("limits.per_address_burst", Float),
        ("limits.per_credential_rps", Float),
        ("limits.per_credential_burst", Float),
        ("limits.per_user_rps", Float),
        ("limits.per_user_burst", Float),
        ("limits.store_concurrency", Int),
        ("limits.upload_bytes_per_hour", Int),
        ("model.dir", Str),
        ("model.download", Bool),
        ("local.data_dir", Str),
        ("local.master_key", Str),
        ("local.master_key_file", Str),
        ("log.level", Str),
        ("log.format", Str),
        ("metrics.bind", Str),
        ("state.backend", Str),
        ("state.postgres_url_file", Str),
        ("state.postgres_doc_key_file", Str),
        ("state.dynamodb_table", Str),
        ("storage.bucket", Str),
        ("storage.region", Str),
        ("storage.endpoint_url", Str),
        ("storage.force_path_style", Bool),
        ("storage.credentials_file", Str),
        ("storage.link_ttl_seconds", Int),
        ("retention.days", Int),
        ("retention.soft_delete_days", Int),
        ("keys.provider", Str),
        ("keys.kms_key_id", Str),
        ("keys.master_key_file", Str),
        ("launcher.kind", Str),
        ("launcher.bind", Str),
        ("launcher.ecs.cluster", Str),
        ("launcher.ecs.task_definition", Str),
        ("launcher.ecs.subnets", List),
        ("launcher.ecs.security_groups", List),
        ("launcher.ecs.capacity_provider", Str),
        ("launcher.docker.url", Str),
        ("launcher.docker.token_file", Str),
        ("containers.user_net_cidr", Str),
        ("containers.memory_mib", Int),
        ("containers.pool_target", Int),
        ("containers.max", Int),
        ("containers.idle_seconds", Int),
        ("containers.max_lifetime_hours", Int),
        ("containers.claim_wait_ms", Int),
        ("internal.bind", Str),
        ("internal.router_url", Str),
        ("internal.secret_file", Str),
        ("internal.cert_file", Str),
        ("internal.key_file", Str),
        ("rollout.allow_downgrade", Bool),
    ]
};

/// The secret settings: in the file only as `<key>_file`, or the value in their variable.
pub(crate) const SECRETS: &[&str] = &[
    "auth.web_client_secret",
    "state.postgres_url",
    "launcher.docker.token",
    "internal.secret",
    "internal.cert",
    "internal.key",
];

/// `auth.mode` -> `KNOGGIN_AUTH__MODE`.
fn env_name(key: &str) -> String {
    format!("KNOGGIN_{}", key.to_ascii_uppercase().replace('.', "__"))
}

fn convert(name: &str, text: &str, kind: Kind) -> anyhow::Result<Value> {
    let bad = || anyhow!("{name}: {text:?} is not a valid {kind:?} value");
    Ok(match kind {
        Kind::Str => Value::String(text.to_owned()),
        Kind::Int => Value::Integer(text.trim().parse().map_err(|_| bad())?),
        Kind::Float => Value::Float(text.trim().parse().map_err(|_| bad())?),
        Kind::Bool => match text.trim() {
            "true" => Value::Boolean(true),
            "false" => Value::Boolean(false),
            _ => return Err(bad()),
        },
        Kind::List => Value::Array(
            text.split(',')
                .map(str::trim)
                .filter(|s| !s.is_empty())
                .map(|s| Value::String(s.to_owned()))
                .collect(),
        ),
    })
}

fn set_path(table: &mut Table, key: &str, value: Value) {
    let mut parts: Vec<&str> = key.split('.').collect();
    let Some(last) = parts.pop() else { return };
    let mut at = table;
    for part in parts {
        let entry = at
            .entry(part.to_owned())
            .or_insert_with(|| Value::Table(Table::new()));
        if !entry.is_table() {
            *entry = Value::Table(Table::new());
        }
        let Value::Table(next) = entry else { return };
        at = next;
    }
    at.insert(last.to_owned(), value);
}

fn get_path<'a>(table: &'a Table, key: &str) -> Option<&'a Value> {
    let mut parts = key.split('.');
    let mut value = table.get(parts.next()?)?;
    for part in parts {
        value = value.as_table()?.get(part)?;
    }
    Some(value)
}

/// Lays `over` onto `base`, key by key.
fn merge(base: &mut Table, over: Table) {
    for (k, v) in over {
        match (base.get_mut(&k), v) {
            (Some(Value::Table(b)), Value::Table(o)) => merge(b, o),
            (_, v) => {
                base.insert(k, v);
            }
        }
    }
}

/// Settings given on the command line, such as `--log-level`.
#[derive(Debug, Default, Clone)]
pub(crate) struct Flags {
    pub(crate) log_level: Option<LogLevel>,
}

impl Config {
    /// Loads the config for `role`: the file at `explicit` (`--config`), else
    /// `KNOGGIN_CONFIG`, else the role's default file; then the environment and `flags`.
    pub(crate) fn load(
        env: &Env,
        role: Role,
        explicit: Option<&Path>,
        flags: &Flags,
    ) -> anyhow::Result<Self> {
        let (path, named) = match explicit.or(env.config.as_deref()) {
            Some(p) => (p.to_owned(), true),
            None => (default_file(env, role), false),
        };
        let read = if role == Role::Serve {
            // A user container reads no config file (tab "Config, CLI, local install").
            Err(std::io::Error::from(std::io::ErrorKind::NotFound))
        } else {
            std::fs::read_to_string(&path)
        };
        let mut table = match read {
            Ok(text) => text
                .parse::<Table>()
                .with_context(|| format!("config file {}", path.display()))?,
            Err(e)
                if e.kind() == std::io::ErrorKind::NotFound && (!named || role == Role::Serve) =>
            {
                Table::new()
            }
            Err(e) => return Err(e).with_context(|| format!("config file {}", path.display())),
        };
        for secret in SECRETS {
            if get_path(&table, secret).is_some() {
                bail!(
                    "config file {}: `{secret}` is a secret and cannot be written in the file; \
                     set `{secret}_file` to a file that holds it, or {}",
                    path.display(),
                    env_name(secret)
                );
            }
        }
        let mut secrets = BTreeMap::new();
        for (name, value) in &env.vars {
            if name == "KNOGGIN_SOURCE_URL" {
                set_path(
                    &mut table,
                    "source_url",
                    Value::String(value.expose().clone()),
                );
            } else if let Some((key, kind)) = KEYS.iter().find(|(k, _)| env_name(k) == *name) {
                set_path(&mut table, key, convert(name, value.expose(), *kind)?);
            } else if let Some(key) = SECRETS.iter().find(|k| env_name(k) == *name) {
                secrets.insert(*key, value.clone());
            }
            // Any other name is not a setting and is ignored.
        }
        let mut over = Table::new();
        if let Some(level) = flags.log_level {
            over.insert(
                "log".into(),
                Value::Table(Table::try_from(Log {
                    level,
                    format: None,
                })?),
            );
        }
        merge(&mut table, over);
        let mut config: Self = Value::Table(table)
            .try_into()
            .with_context(|| format!("config file {}", path.display()))?;
        config.secrets = secrets;
        config.resolve(env, role);
        config.validate()?;
        Ok(config)
    }

    /// Fills in the defaults that depend on the role or the home folder.
    fn resolve(&mut self, env: &Env, role: Role) {
        let server = role.is_server();
        let bind = if server {
            "0.0.0.0:8080"
        } else {
            "127.0.0.1:7690"
        };
        self.server
            .bind
            .get_or_insert_with(|| bind.parse().unwrap_or(LOCAL_BIND));
        self.auth.mode.get_or_insert(if server {
            AuthMode::Oauth
        } else {
            AuthMode::None
        });
        self.log.format.get_or_insert(if server {
            LogFormat::Json
        } else {
            LogFormat::Text
        });
        if server {
            self.metrics
                .bind
                .get_or_insert_with(|| "0.0.0.0:9091".into());
        } else {
            self.state.backend.get_or_insert(Backend::LocalFile);
        }
        let home = &env.home;
        if self.model.dir.as_os_str().is_empty() {
            self.model.dir = home.join("models");
        }
        if self.local.data_dir.as_os_str().is_empty() {
            self.local.data_dir = home.join("data");
        }
        if self.local.master_key_file.as_os_str().is_empty() {
            self.local.master_key_file = if env.home_set {
                home.join("master.key")
            } else {
                env.user_home.join(".config/knoggin/master.key")
            };
        }
    }

    fn validate(&self) -> anyhow::Result<()> {
        let positive = |name: &str, v: f64| {
            if v.is_finite() && v > 0.0 {
                Ok(())
            } else {
                Err(anyhow!("{name} must be greater than 0"))
            }
        };
        let l = &self.limits;
        positive("limits.body_bytes", l.body_bytes as f64)?;
        positive("limits.request_timeout_ms", l.request_timeout_ms as f64)?;
        positive("limits.store_concurrency", f64::from(l.store_concurrency))?;
        for (name, v) in [
            ("limits.per_address_rps", l.per_address_rps),
            ("limits.per_address_burst", l.per_address_burst),
            ("limits.per_credential_rps", l.per_credential_rps),
            ("limits.per_credential_burst", l.per_credential_burst),
            ("limits.per_user_rps", l.per_user_rps),
            ("limits.per_user_burst", l.per_user_burst),
        ] {
            positive(name, v)?;
        }
        positive(
            "auth.session_max_days",
            f64::from(self.auth.session_max_days),
        )?;
        positive(
            "auth.session_idle_hours",
            f64::from(self.auth.session_idle_hours),
        )?;
        for (name, v) in [
            ("retention.days", self.retention.days),
            (
                "retention.soft_delete_days",
                self.retention.soft_delete_days,
            ),
        ] {
            if !(1..=3650).contains(&v) {
                bail!("{name} must be between 1 and 3650");
            }
        }
        if let Some(bind) = self.metrics.bind.as_deref().filter(|b| !b.is_empty()) {
            bind.parse::<SocketAddr>()
                .map_err(|_| anyhow!("metrics.bind: {bind:?} is not an address and port"))?;
        }
        Ok(())
    }

    /// The fixed listener's address.
    pub(crate) fn bind(&self) -> SocketAddr {
        self.server.bind.unwrap_or(LOCAL_BIND)
    }

    /// `auth.mode` after the role's default.
    pub(crate) fn auth_mode(&self) -> AuthMode {
        self.auth.mode.unwrap_or(AuthMode::None)
    }

    /// The metrics listener, if it is on.
    pub(crate) fn metrics_bind(&self) -> Option<SocketAddr> {
        self.metrics.bind.as_deref()?.parse().ok()
    }

    /// Reads the secret `key` (one of [`SECRETS`]): its variable, else the file in
    /// `<key>_file`, trailing newline ignored. A server role refuses a file that group or
    /// others can write. `None` when neither is given.
    pub(crate) fn secret(
        &self,
        key: &str,
        role: Role,
    ) -> anyhow::Result<Option<Sensitive<String>>> {
        if let Some(v) = self.secrets.get(key) {
            return Ok(Some(v.clone()));
        }
        let Some(path) = self.secret_file(key) else {
            return Ok(None);
        };
        let meta =
            std::fs::metadata(&path).with_context(|| format!("{key}_file {}", path.display()))?;
        if role.is_server() && meta.mode() & 0o022 != 0 {
            bail!(
                "{key}_file {}: group or others can write it; make it writable by its owner only",
                path.display()
            );
        }
        let mut text = std::fs::read_to_string(&path)
            .with_context(|| format!("{key}_file {}", path.display()))?;
        if text.ends_with('\n') {
            text.pop();
            if text.ends_with('\r') {
                text.pop();
            }
        }
        Ok(Some(Sensitive::new(text)))
    }

    fn secret_file(&self, key: &str) -> Option<PathBuf> {
        let table = Table::try_from(self).ok()?;
        get_path(&table, &format!("{key}_file"))?
            .as_str()
            .map(PathBuf::from)
    }

    /// Every effective value as `(dotted key, value)`, secrets as the text `[set]` or
    /// `[not set]`, for `knoggin config check`.
    pub(crate) fn effective(&self) -> Vec<(String, Value)> {
        let mut out = Vec::new();
        if let Ok(table) = Table::try_from(self) {
            flatten("", &table, &mut out);
        }
        for key in SECRETS {
            let set = self.secrets.contains_key(key) || self.secret_file(key).is_some();
            out.push((
                (*key).to_owned(),
                Value::String(if set { "[set]" } else { "[not set]" }.to_owned()),
            ));
        }
        out.sort_by(|a, b| a.0.cmp(&b.0));
        out
    }
}

const LOCAL_BIND: SocketAddr = SocketAddr::V4(std::net::SocketAddrV4::new(
    std::net::Ipv4Addr::LOCALHOST,
    7690,
));

fn default_file(env: &Env, role: Role) -> PathBuf {
    let home = env.home.join("config.toml");
    match role {
        Role::Local => home,
        Role::Router | Role::Lifecycle | Role::Launcher => PathBuf::from(ETC_CONFIG),
        Role::Serve => PathBuf::new(),
        Role::Admin if Path::new(ETC_CONFIG).exists() => PathBuf::from(ETC_CONFIG),
        Role::Admin => home,
    }
}

fn flatten(prefix: &str, table: &Table, out: &mut Vec<(String, Value)>) {
    for (k, v) in table {
        let key = if prefix.is_empty() {
            k.clone()
        } else {
            format!("{prefix}.{k}")
        };
        match v {
            Value::Table(t) => flatten(&key, t, out),
            other => out.push((key, other.clone())),
        }
    }
}

#[cfg(test)]
mod tests {
    use std::os::unix::fs::PermissionsExt;

    use super::*;

    fn env(dir: &Path, vars: &[(&str, &str)]) -> Env {
        let mut all: BTreeMap<String, String> = vars
            .iter()
            .map(|(k, v)| ((*k).to_owned(), (*v).to_owned()))
            .collect();
        all.insert("HOME".into(), dir.display().to_string());
        all.insert("KNOGGIN_HOME".into(), dir.join("k").display().to_string());
        Env::from_vars(all, dir.to_owned()).unwrap()
    }

    fn file(dir: &Path, text: &str) -> PathBuf {
        let p = dir.join("c.toml");
        std::fs::write(&p, text).unwrap();
        p
    }

    fn load(
        dir: &Path,
        text: &str,
        vars: &[(&str, &str)],
        flags: &Flags,
    ) -> anyhow::Result<Config> {
        let p = file(dir, text);
        Config::load(&env(dir, vars), Role::Local, Some(&p), flags)
    }

    #[test]
    fn debug_names_no_person() {
        let d = tempfile::tempdir().unwrap();
        let c = load(
            d.path(),
            "[auth]\nallowed_emails = [\"mkr@example.com\"]\nadmin_subjects = [\"mkrsubject\"]\n",
            &[],
            &Flags::default(),
        )
        .unwrap();
        let shown = format!("{c:?}");
        assert!(!shown.contains("mkr"), "{shown}");
        assert!(shown.contains("allowed_emails: 1"), "{shown}");
        assert!(!shown.contains(&d.path().display().to_string()), "{shown}");
    }

    #[test]
    fn each_layer_overrides_the_one_below() {
        let d = tempfile::tempdir().unwrap();
        let c = load(d.path(), "", &[], &Flags::default()).unwrap();
        assert_eq!(c.log.level, LogLevel::Info);
        assert_eq!(c.bind().to_string(), "127.0.0.1:7690");
        let c = load(
            d.path(),
            "[log]\nlevel = \"warn\"\n",
            &[],
            &Flags::default(),
        )
        .unwrap();
        assert_eq!(c.log.level, LogLevel::Warn);
        let vars = [
            ("KNOGGIN_LOG__LEVEL", "error"),
            ("KNOGGIN_AUTH__ALLOWED_EMAILS", "a@x, b@y"),
        ];
        let c = load(
            d.path(),
            "[log]\nlevel = \"warn\"\n",
            &vars,
            &Flags::default(),
        )
        .unwrap();
        assert_eq!(c.log.level, LogLevel::Error);
        assert_eq!(c.auth.allowed_emails, ["a@x", "b@y"]);
        let flags = Flags {
            log_level: Some(LogLevel::Debug),
        };
        let c = load(d.path(), "[log]\nlevel = \"warn\"\n", &vars, &flags).unwrap();
        assert_eq!(c.log.level, LogLevel::Debug);
        let c = load(
            d.path(),
            "",
            &[("KNOGGIN_SOURCE_URL", "https://x.example/src")],
            &Flags::default(),
        )
        .unwrap();
        assert_eq!(c.source_url, "https://x.example/src");
    }

    #[test]
    fn unknown_keys_wrong_types_bad_values_and_plain_secrets_are_errors() {
        let d = tempfile::tempdir().unwrap();
        let f = Flags::default();
        let e = load(d.path(), "[log]\nlevle = \"warn\"\n", &[], &f).unwrap_err();
        assert!(format!("{e:#}").contains("levle"), "{e:#}");
        let e = load(d.path(), "[nonsense]\n", &[], &f).unwrap_err();
        assert!(format!("{e:#}").contains("nonsense"), "{e:#}");
        let e = load(d.path(), "[limits]\nbody_bytes = \"big\"\n", &[], &f).unwrap_err();
        assert!(format!("{e:#}").contains("body_bytes"), "{e:#}");
        assert!(load(d.path(), "[retention]\ndays = 0\n", &[], &f).is_err());
        assert!(load(d.path(), "[log]\nlevel = \"loud\"\n", &[], &f).is_err());
        assert!(load(d.path(), "", &[("KNOGGIN_LIMITS__BODY_BYTES", "x")], &f).is_err());
        let e = load(d.path(), "[auth]\nweb_client_secret = \"s\"\n", &[], &f).unwrap_err();
        assert!(
            format!("{e:#}").contains("auth.web_client_secret_file"),
            "{e:#}"
        );
        assert!(
            load(
                d.path(),
                "[auth]\nweb_client_secret_file = \"/x\"\n",
                &[],
                &f
            )
            .is_ok()
        );
    }

    #[test]
    fn an_unknown_variable_is_ignored_and_a_missing_named_file_is_an_error() {
        let d = tempfile::tempdir().unwrap();
        let vars = [("KNOGGIN_NOPE__THING", "1"), ("KNOGGIN_WHATEVER", "x")];
        assert!(load(d.path(), "", &vars, &Flags::default()).is_ok());
        let e = env(d.path(), &[]);
        assert!(
            Config::load(
                &e,
                Role::Local,
                Some(&d.path().join("missing.toml")),
                &Flags::default()
            )
            .is_err()
        );
        // The default file may be missing.
        assert!(Config::load(&e, Role::Local, None, &Flags::default()).is_ok());
    }

    #[test]
    fn server_roles_take_their_own_defaults() {
        let d = tempfile::tempdir().unwrap();
        let p = file(d.path(), "");
        let c = Config::load(
            &env(d.path(), &[]),
            Role::Router,
            Some(&p),
            &Flags::default(),
        )
        .unwrap();
        assert_eq!(c.bind().to_string(), "0.0.0.0:8080");
        assert_eq!(c.auth_mode(), AuthMode::Oauth);
        assert_eq!(c.log.format, Some(LogFormat::Json));
        assert_eq!(
            c.metrics_bind().map(|a| a.to_string()).as_deref(),
            Some("0.0.0.0:9091")
        );
        assert_eq!(c.state.backend, None);
        let c = Config::load(
            &env(d.path(), &[]),
            Role::Local,
            Some(&p),
            &Flags::default(),
        )
        .unwrap();
        assert_eq!(c.metrics_bind(), None);
        assert_eq!(c.state.backend, Some(Backend::LocalFile));
        assert_eq!(c.local.master_key_file, d.path().join("k/master.key"));
    }

    #[test]
    fn secret_files_follow_the_server_rule_and_check_prints_no_secret() {
        let d = tempfile::tempdir().unwrap();
        let s = d.path().join("secret");
        std::fs::write(&s, "hunter2-marker\n").unwrap();
        std::fs::set_permissions(&s, std::fs::Permissions::from_mode(0o644)).unwrap();
        let text = format!("[internal]\nsecret_file = \"{}\"\n", s.display());
        let p = file(d.path(), &text);
        let c = Config::load(
            &env(d.path(), &[]),
            Role::Router,
            Some(&p),
            &Flags::default(),
        )
        .unwrap();
        assert_eq!(
            c.secret("internal.secret", Role::Router)
                .unwrap()
                .unwrap()
                .expose(),
            "hunter2-marker"
        );
        std::fs::set_permissions(&s, std::fs::Permissions::from_mode(0o664)).unwrap();
        assert!(c.secret("internal.secret", Role::Router).is_err());
        // The local daemon does not apply the server rule to these.
        assert!(c.secret("internal.secret", Role::Local).is_ok());
        let vars = [("KNOGGIN_AUTH__WEB_CLIENT_SECRET", "env-marker")];
        let c = Config::load(
            &env(d.path(), &vars),
            Role::Router,
            Some(&p),
            &Flags::default(),
        )
        .unwrap();
        assert_eq!(
            c.secret("auth.web_client_secret", Role::Router)
                .unwrap()
                .unwrap()
                .expose(),
            "env-marker"
        );
        let shown = format!("{:?}", c.effective());
        assert!(!shown.contains("marker"), "{shown}");
        assert!(
            shown.contains("\"auth.web_client_secret\", String(\"[set]\")"),
            "{shown}"
        );
        assert!(
            shown.contains("\"state.postgres_url\", String(\"[not set]\")"),
            "{shown}"
        );
    }

    #[test]
    fn test_mode_takes_only_the_value_1() {
        let d = tempfile::tempdir().unwrap();
        assert!(env(d.path(), &[("KNOGGIN_TEST_MODE", "1")]).test_mode);
        assert!(!env(d.path(), &[]).test_mode);
        for bad in ["", "0", "true", "yes"] {
            let mut vars = BTreeMap::new();
            vars.insert("HOME".to_owned(), "/x".to_owned());
            vars.insert("KNOGGIN_TEST_MODE".to_owned(), bad.to_owned());
            assert!(Env::from_vars(vars, d.path().into()).is_err(), "{bad:?}");
        }
    }
}
