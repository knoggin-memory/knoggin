// SPDX-License-Identifier: AGPL-3.0-only
//! The command tree and every command's output (tab "Config, CLI, local install",
//! Commands). Output for people goes to standard output, logs and errors to standard
//! error; `--json` prints the route's body unchanged, or one flat object.
#![allow(clippy::print_stdout, clippy::print_stderr)]

use std::io::{BufRead, Write as _};
use std::path::PathBuf;
use std::process::ExitCode;
use std::sync::Arc;
use std::time::Duration;

use anyhow::{Context, anyhow, bail};
use clap::{Args, Parser, Subcommand};
use http::Method;
use knoggin_core::transfer::OnConflict;
use serde_json::{Value, json};

use crate::bridge::{self, Bridge, Target, ToolSet};
use crate::client::{Client, EXIT_LOCKED, EXIT_UNREACHABLE, Failure};
use crate::config::{Config, Env, Flags, Home, LogLevel, Role};
use crate::logging::{self, Sink};
use crate::{daemon, keys, model, transfer};

/// Knoggin: a memory server for AI agents.
#[derive(Debug, Parser)]
#[command(name = "knoggin", version, about, propagate_version = true)]
pub(crate) struct Cli {
    /// The config file (default `~/.knoggin/config.toml`).
    #[arg(long, global = true, value_name = "PATH")]
    config: Option<PathBuf>,
    /// `error`, `warn`, `info`, `debug` or `trace`.
    #[arg(long, global = true, value_enum, value_name = "LEVEL")]
    log_level: Option<LogLevel>,
    /// Print data as JSON.
    #[arg(long, global = true)]
    json: bool,
    #[command(subcommand)]
    command: Command,
}

#[derive(Debug, Subcommand)]
enum Command {
    /// Run the local daemon in the foreground.
    Daemon {
        /// Start in a session of its own (what `knoggin start` asks for).
        #[arg(long, hide = true)]
        new_session: bool,
    },
    /// Start the daemon detached, if it is not running.
    Start,
    /// Stop the daemon and wait for a clean exit.
    Stop,
    /// Show the daemon, the model, the master-key source and each store.
    Status,
    /// Open the web UI with a one-time code.
    Open {
        /// Only print the address.
        #[arg(long)]
        print_url: bool,
    },
    /// The stdio MCP bridge.
    Mcp {
        /// `full` (/mcp) or `plugin` (/mcp/plugin).
        #[arg(long, value_enum, default_value = "full")]
        tool_set: ToolSet,
        /// A remote server's MCP address; needs KNOGGIN_API_KEY.
        #[arg(long)]
        url: Option<String>,
    },
    /// Manage apps.
    #[command(subcommand)]
    Apps(AppsCommand),
    /// Manage a store's API keys.
    #[command(subcommand)]
    Keys(KeysCommand),
    /// Write a store as an export file.
    Export {
        #[arg(long)]
        app: Option<String>,
        #[arg(long)]
        out: PathBuf,
    },
    /// Read an export file into a store, or `import recalld <dir>`.
    Import(ImportArgs),
    /// Wait until the store's embedding queue is empty.
    Reembed {
        #[arg(long)]
        app: Option<String>,
    },
    /// Back up, restore or rotate the local master key (daemon stopped).
    #[command(subcommand)]
    Key(KeyCommand),
    /// Download or re-check the model files.
    #[command(subcommand)]
    Model(ModelCommand),
    /// The local state file.
    #[command(subcommand)]
    State(StateCommand),
    /// Validate and print the config.
    #[command(subcommand)]
    Config(ConfigCommand),
    /// Call `/readyz` and exit 0 or 1.
    Healthcheck {
        /// An http address to call instead of the local daemon's socket.
        #[arg(long)]
        url: Option<String>,
    },
    /// Recovery tools that also work on a local install.
    #[command(subcommand)]
    Admin(AdminCommand),
    /// Run the router (server).
    Router,
    /// Run inside a user container; reads its settings from the environment only.
    Serve,
    /// Run the lifecycle pass once, or with `--every 10m` forever (server).
    Lifecycle {
        /// Repeat after this long: a whole number followed by s, m or h.
        #[arg(long, value_parser = crate::server::parse_every)]
        every: Option<Duration>,
    },
    /// Run the launcher service (one host only).
    Launcher,
    /// A small identity provider for tests; needs test mode. Never for real users.
    TestIssuer {
        /// The address to listen on.
        #[arg(long)]
        bind: std::net::SocketAddr,
        /// The issuer URL it names itself by.
        #[arg(long)]
        issuer_url: String,
    },
}

#[derive(Debug, Subcommand)]
enum AppsCommand {
    List,
    Create { name: String },
    Delete { name: String },
}

#[derive(Debug, Subcommand)]
enum KeysCommand {
    List {
        #[arg(long)]
        app: Option<String>,
    },
    Create {
        #[arg(long)]
        app: Option<String>,
        #[arg(long, default_value = "cli")]
        name: String,
        #[arg(long)]
        read_only: bool,
        #[arg(long, value_parser = ["30", "90", "180", "365"])]
        expires_days: Option<String>,
    },
    Revoke {
        key_id: String,
        #[arg(long)]
        app: Option<String>,
    },
}

#[derive(Debug, Args)]
#[command(args_conflicts_with_subcommands = true, subcommand_negates_reqs = true)]
struct ImportArgs {
    #[command(subcommand)]
    source: Option<ImportSource>,
    /// The export file. A file named `recalld` must be given as `./recalld`.
    #[arg(required = true)]
    file: Option<PathBuf>,
    #[arg(long)]
    app: Option<String>,
    #[arg(long, value_enum, default_value = "skip")]
    on_conflict: Conflict,
}

#[derive(Debug, Clone, Copy, clap::ValueEnum)]
enum Conflict {
    Skip,
    Error,
}

#[derive(Debug, Subcommand)]
enum ImportSource {
    /// Import a stopped recalld data directory.
    Recalld {
        dir: PathBuf,
        #[arg(long)]
        app: Option<String>,
    },
}

#[derive(Debug, Subcommand)]
enum KeyCommand {
    Export {
        #[arg(long)]
        out: PathBuf,
    },
    Import {
        file: PathBuf,
    },
    Rotate,
    /// Make a new key set file, mode 0600: a master key, or on one host the state key.
    Generate {
        #[arg(long)]
        out: PathBuf,
    },
}

#[derive(Debug, Subcommand)]
enum ModelCommand {
    Fetch {
        #[arg(long)]
        dir: Option<PathBuf>,
    },
    Verify,
}

#[derive(Debug, Subcommand)]
enum StateCommand {
    /// Move an unreadable newest state file aside (daemon stopped).
    UsePrevious {
        /// Do not ask.
        #[arg(long)]
        yes: bool,
    },
}

#[derive(Debug, Subcommand)]
enum ConfigCommand {
    Check {
        #[arg(long, value_enum, default_value = "local")]
        role: Role,
    },
}

#[derive(Debug, Subcommand)]
enum AdminCommand {
    #[command(subcommand)]
    Snapshot(SnapshotCommand),
    /// Check the state database, the key provider and the bucket.
    Selftest,
    /// List a store's snapshots.
    Snapshots {
        #[arg(long)]
        store: String,
    },
    /// Restore a store from one of its snapshots, or cancel a restore.
    #[command(group = clap::ArgGroup::new("what").required(true).args(["snapshot", "cancel"]))]
    Restore {
        #[arg(long)]
        store: String,
        /// `<generation>-<sequence>`; shorter numbers are padded.
        #[arg(long)]
        snapshot: Option<String>,
        #[arg(long)]
        cancel: bool,
    },
    /// Wrap every key again under a new master key (one host, router stopped).
    RotateMasterKey,
    /// Park containers started before a time: `now`, an RFC 3339 time, or `clear`.
    RetireBefore { when: String },
    /// Make the routers' shared internal certificate and key.
    InternalCert {
        #[arg(long)]
        out_dir: PathBuf,
    },
}

#[derive(Debug, Subcommand)]
enum SnapshotCommand {
    Inspect {
        file: PathBuf,
    },
    Export {
        file: PathBuf,
        #[arg(long)]
        out: PathBuf,
    },
}

impl Cli {
    /// `daemon` and `serve` exit with 101 from the panic hook; the router and the
    /// commands do not.
    pub(crate) fn exits_on_panic(&self) -> bool {
        self.is_daemon() || self.is_serve()
    }

    pub(crate) fn is_serve(&self) -> bool {
        matches!(self.command, Command::Serve)
    }

    /// The daemon and the server roles get every core; a command gets two.
    pub(crate) fn is_long_running(&self) -> bool {
        matches!(
            self.command,
            Command::Daemon { .. }
                | Command::Router
                | Command::Serve
                | Command::Lifecycle { .. }
                | Command::Launcher
        )
    }

    pub(crate) fn is_daemon(&self) -> bool {
        matches!(self.command, Command::Daemon { .. })
    }
}

/// Prints `message` and returns exit code 1, before the runtime exists.
pub(crate) fn fail_early(message: &str) -> ExitCode {
    eprintln!("knoggin: {message}");
    ExitCode::from(1)
}

/// Runs the command and returns the exit code. `warnings` came before logging existed.
pub(crate) async fn run(cli: Cli, env: Env, warnings: Vec<&'static str>) -> u8 {
    match dispatch(cli, &env, warnings).await {
        Ok(code) => code,
        Err(e) => {
            let code = e.downcast_ref::<Failure>().map_or(1, |f| f.exit);
            eprintln!("knoggin: {e:#}");
            code
        }
    }
}

struct Ctx<'a> {
    env: &'a Env,
    home: Home,
    json: bool,
    config_path: Option<PathBuf>,
    flags: Flags,
}

impl Ctx<'_> {
    fn config(&self, role: Role) -> anyhow::Result<Config> {
        Config::load(self.env, role, self.config_path.as_deref(), &self.flags)
    }

    fn client(&self) -> Client {
        Client::new(&self.home)
    }

    /// Prints `value` as JSON, or `text` for people.
    fn print(&self, value: &Value, text: impl FnOnce() -> String) {
        if self.json {
            println!("{value}");
        } else {
            let t = text();
            if !t.is_empty() {
                println!("{t}");
            }
        }
    }
}

async fn dispatch(cli: Cli, env: &Env, warnings: Vec<&'static str>) -> anyhow::Result<u8> {
    let ctx = Ctx {
        env,
        home: env.paths(),
        json: cli.json,
        config_path: cli.config.clone(),
        flags: Flags {
            log_level: cli.log_level,
        },
    };
    let role = match &cli.command {
        Command::Config(ConfigCommand::Check { role }) => *role,
        Command::Healthcheck { .. }
        | Command::Admin(_)
        | Command::TestIssuer { .. }
        | Command::Key(KeyCommand::Generate { .. }) => Role::Admin,
        Command::Router => Role::Router,
        Command::Serve => Role::Serve,
        Command::Lifecycle { .. } => Role::Lifecycle,
        Command::Launcher => Role::Launcher,
        _ => Role::Local,
    };
    let config = ctx.config(role)?;
    let log_file = matches!(cli.command, Command::Daemon { .. }).then(|| ctx.home.log_file());
    // A detached daemon's standard error is its log file already (`spawn_detached`).
    let sink = match cli.command {
        Command::Daemon { new_session: true } => Sink::FileOnly,
        _ => Sink::Stderr,
    };
    logging::init(&config, sink, log_file.as_deref())?;
    for w in warnings {
        tracing::warn!(kind = "core_dumps", "{w}");
    }
    match cli.command {
        Command::Daemon { new_session } => {
            if new_session {
                // `knoggin start` runs the daemon in a session of its own.
                let _ = rustix::process::setsid();
            }
            daemon::run(config, env).await
        }
        Command::Start => start(&ctx).await,
        Command::Stop => {
            if daemon::stop(&ctx.home).await? {
                ctx.print(&json!({ "stopped": true }), || "Knoggin stopped.".into());
                Ok(0)
            } else {
                ctx.print(&json!({ "stopped": false }), || {
                    "Knoggin is not running.".into()
                });
                Ok(EXIT_UNREACHABLE)
            }
        }
        Command::Status => status(&ctx, &config).await,
        Command::Open { print_url } => open(&ctx, print_url).await,
        Command::Mcp { tool_set, url } => mcp(&ctx, tool_set, url).await,
        Command::Apps(c) => apps(&ctx, c).await,
        Command::Keys(c) => api_keys(&ctx, c).await,
        Command::Export { app, out } => {
            let client = ctx.client();
            let store = store_of(&client, app.as_deref()).await?;
            let n = transfer::export(&client, store.as_deref(), &out).await?;
            ctx.print(
                &json!({ "out": out.display().to_string(), "bytes": n }),
                || format!("Wrote {} ({n} bytes).", out.display()),
            );
            Ok(0)
        }
        Command::Import(args) => import(&ctx, args).await,
        Command::Reembed { app } => reembed(&ctx, app.as_deref()).await,
        Command::Key(c) => key(&ctx, &config, c).await,
        Command::Model(c) => model_command(&ctx, &config, c).await,
        Command::State(StateCommand::UsePrevious { yes }) => use_previous(&ctx, &config, yes).await,
        Command::Config(ConfigCommand::Check { .. }) => {
            let values = config.effective();
            let mut object = serde_json::Map::new();
            for (k, v) in &values {
                object.insert(k.clone(), serde_json::to_value(v)?);
            }
            ctx.print(&Value::Object(object), || {
                values
                    .iter()
                    .map(|(k, v)| match v {
                        // A secret's state is printed bare: `[set]` or `[not set]`.
                        toml::Value::String(s) if crate::config::SECRETS.contains(&k.as_str()) => {
                            format!("{k} = {s}")
                        }
                        other => format!("{k} = {other}"),
                    })
                    .collect::<Vec<_>>()
                    .join("\n")
            });
            Ok(0)
        }
        Command::Healthcheck { url } => healthcheck(&ctx, url).await,
        Command::TestIssuer { bind, issuer_url } => {
            if !env.test_mode {
                bail!("knoggin test-issuer runs in test mode only (KNOGGIN_TEST_MODE=1)");
            }
            let listener = tokio::net::TcpListener::bind(bind)
                .await
                .with_context(|| format!("binding {bind}"))?;
            let issuer =
                knoggin_http::testing::issuer::TestIssuer::serve(listener, Some(issuer_url), true)
                    .map_err(|e| anyhow!(e))?;
            {
                use tokio::signal::unix::{SignalKind, signal};
                let (mut term, mut int) = (
                    signal(SignalKind::terminate())?,
                    signal(SignalKind::interrupt())?,
                );
                tokio::select! {
                    _ = term.recv() => {}
                    _ = int.recv() => {}
                }
            }
            drop(issuer);
            Ok(0)
        }
        Command::Router => crate::server::router(&config, env).await,
        Command::Serve => crate::server::serve(&config, env).await,
        Command::Lifecycle { every } => crate::server::lifecycle(&config, every).await,
        Command::Launcher => crate::server::launcher(&config, env).await,
        Command::Admin(AdminCommand::Snapshot(SnapshotCommand::Inspect { file })) => {
            let i = transfer::inspect(&file)?;
            let v = serde_json::to_value(&i)?;
            ctx.print(&v, || flat_text(&v));
            Ok(0)
        }
        Command::Admin(AdminCommand::Snapshot(SnapshotCommand::Export { file, out })) => {
            let lines = transfer::snapshot_export(&config, &ctx.home, &file, &out).await?;
            ctx.print(
                &json!({ "out": out.display().to_string(), "lines": lines }),
                || format!("Wrote {} ({lines} lines).", out.display()),
            );
            Ok(0)
        }
        Command::Admin(c) => admin(&ctx, &config, c).await,
    }
}

/// The server's admin commands (tab "Config, CLI, local install", Server commands).
async fn admin(ctx: &Ctx<'_>, config: &Config, c: AdminCommand) -> anyhow::Result<u8> {
    use knoggin_server::admin::{self, Admin, RestoreOutcome};
    use knoggin_state::system_clock;

    use crate::server;
    let clock = system_clock();
    let store_id = |text: &str| {
        uuid::Uuid::parse_str(text)
            .map(knoggin_core::StoreId::from_uuid)
            .map_err(|_| anyhow!("--store {text:?} is not a store ID"))
    };
    if let AdminCommand::InternalCert { out_dir } = &c {
        let now = knoggin_core::Timestamp::from_millis(clock());
        let (cert, key) = knoggin_server::link::new_internal_cert(now)
            .map_err(|e| anyhow!("making the certificate: {e}"))?;
        let cert_path = out_dir.join("internal_cert.pem");
        let key_path = out_dir.join("internal_key.pem");
        transfer::create_private(&cert_path)?.write_all(cert.as_bytes())?;
        transfer::create_private(&key_path)?.write_all(key.as_bytes())?;
        ctx.print(
            &json!({ "cert": cert_path.display().to_string(), "key": key_path.display().to_string() }),
            || format!("Wrote {} and {}.", cert_path.display(), key_path.display()),
        );
        return Ok(0);
    }
    if matches!(c, AdminCommand::RotateMasterKey)
        && config.keys.provider == Some(crate::config::KeyProviderKind::Kms)
    {
        ctx.print(&json!({ "rotated": false }), || {
            "KMS rotates its own key material; nothing to do.".into()
        });
        return Ok(0);
    }
    let keys = server::keys_setting(config)?;
    let state = server::state_setting(config, Role::Admin)?;
    let storage_settings = server::storage_settings(config)?;
    let db: Arc<dyn knoggin_state::StateDb> =
        server::connect_state(&state, clock.clone(), false).await?;
    if let AdminCommand::RotateMasterKey = c {
        let server::Keys::File(path) = &keys;
        let (kek, stores) = admin::rotate_master_key(db, path).await?;
        ctx.print(&json!({ "kekId": kek, "stores": stores }), || {
            format!("The new master key is {kek}; {stores} store keys were wrapped again.")
        });
        return Ok(0);
    }
    let provider = server::key_provider(&keys)?;
    let storage = server::storage(&storage_settings).await;
    if let AdminCommand::Selftest = c {
        let http = reqwest::Client::builder()
            .redirect(reqwest::redirect::Policy::none())
            .build()?;
        let report = admin::selftest(
            db,
            &*provider,
            &*storage,
            &http,
            knoggin_core::Timestamp::from_millis(clock()),
        )
        .await;
        let show = |r: Result<(), String>| r.err().unwrap_or_else(|| "ok".into());
        let v = json!({
            "stateDatabase": show(report.database.clone()),
            "keyProvider": show(report.key_provider.clone()),
            "bucket": show(report.bucket.clone().map_err(|e| e.to_string())),
            "passed": report.passed(),
        });
        ctx.print(&v, || flat_text(&v));
        return Ok(u8::from(!report.passed()));
    }
    let fields = Arc::new(
        knoggin_state::Meta::new(db.clone())
            .field_keys(&*provider)
            .await
            .map_err(|e| anyhow!("the field key: {e}"))?,
    );
    let admin = Admin::new(db, fields, storage, clock);
    match c {
        AdminCommand::Snapshots { store } => {
            let v = admin
                .snapshots(store_id(&store)?, config.retention.days)
                .await?;
            ctx.print(&v, || {
                v["items"]
                    .as_array()
                    .into_iter()
                    .flatten()
                    .map(|i| {
                        format!(
                            "{}  {} bytes  {}  {}",
                            i["name"].as_str().unwrap_or(""),
                            i["sizeBytes"],
                            i["lastModified"].as_str().unwrap_or(""),
                            i["keptBecause"]
                                .as_str()
                                .unwrap_or("(goes at the next lifecycle run)")
                        )
                    })
                    .collect::<Vec<_>>()
                    .join("\n")
            });
            Ok(0)
        }
        AdminCommand::Restore {
            store,
            snapshot,
            cancel,
        } => {
            let store = store_id(&store)?;
            if cancel {
                admin.cancel_restore(store).await?;
                ctx.print(&json!({ "cancelled": true }), || {
                    "The restore is cancelled.".into()
                });
                return Ok(0);
            }
            let name = admin::parse_snapshot(snapshot.as_deref().unwrap_or_default())?;
            match admin.restore(store, name, admin::RESTORE_WAIT).await? {
                RestoreOutcome::Parked => {
                    ctx.print(
                        &json!({ "snapshot": name.to_string(), "parked": true }),
                        || format!("The store opens from {name} on its next request."),
                    );
                    Ok(0)
                }
                RestoreOutcome::Waiting => {
                    ctx.print(&json!({ "snapshot": name.to_string(), "parked": false }), || {
                        format!(
                            "The user's container did not park within 150 seconds; the restore of \
                             {name} takes effect when it next parks."
                        )
                    });
                    Ok(1)
                }
            }
        }
        AdminCommand::RetireBefore { when } => {
            let active = admin.retire_before(when.parse()?).await?;
            ctx.print(&json!({ "activeLeases": active }), || {
                format!("{active} leases are still active.")
            });
            Ok(0)
        }
        AdminCommand::Snapshot(_)
        | AdminCommand::Selftest
        | AdminCommand::RotateMasterKey
        | AdminCommand::InternalCert { .. } => Ok(0),
    }
}

fn flat_text(v: &Value) -> String {
    v.as_object()
        .map(|o| {
            o.iter()
                .map(|(k, v)| {
                    format!(
                        "{k}: {}",
                        v.as_str().map_or_else(|| v.to_string(), str::to_owned)
                    )
                })
                .collect::<Vec<_>>()
                .join("\n")
        })
        .unwrap_or_default()
}

/// Whether the daemon answers `/healthz` on the socket.
async fn healthy(client: &Client) -> bool {
    client.json(Method::GET, "/healthz", None).await.is_ok()
}

/// Starts the daemon if it is not running and waits for `/healthz`.
async fn ensure_running(ctx: &Ctx<'_>) -> anyhow::Result<bool> {
    let client = ctx.client();
    if healthy(&client).await {
        return Ok(false);
    }
    let mut child =
        daemon::spawn_detached(&ctx.home, ctx.config_path.as_deref(), ctx.flags.log_level)?;
    let deadline = tokio::time::Instant::now() + Duration::from_secs(60);
    loop {
        if let Some(status) = child.try_wait()? {
            if healthy(&client).await {
                // Another start won the race; that daemon is the one that runs.
                return Ok(false);
            }
            bail!(
                "the daemon exited with {} before it answered; see {}",
                status
                    .code()
                    .map_or_else(|| "a signal".to_owned(), |c| format!("code {c}")),
                ctx.home.log_file().display()
            );
        }
        if healthy(&client).await {
            return Ok(true);
        }
        if tokio::time::Instant::now() > deadline {
            bail!(
                "the daemon did not answer within 60 seconds; see {}",
                ctx.home.log_file().display()
            );
        }
        tokio::time::sleep(Duration::from_millis(100)).await;
    }
}

async fn start(ctx: &Ctx<'_>) -> anyhow::Result<u8> {
    let started = ensure_running(ctx).await?;
    let address = daemon::read_lock_info(&ctx.home)
        .map(|i| i.address)
        .unwrap_or_default();
    ctx.print(&json!({ "started": started, "address": address }), || {
        let first = if started {
            "Knoggin started"
        } else {
            "Knoggin is already running"
        };
        format!("{first} at http://{address}\nRun `knoggin open` for the web UI.")
    });
    Ok(0)
}

async fn status(ctx: &Ctx<'_>, config: &Config) -> anyhow::Result<u8> {
    let client = ctx.client();
    let source = keys::read_record(&config.local.data_dir)
        .ok()
        .flatten()
        .map(|r| r.source.as_str());
    let stores = match client.json(Method::GET, "/v1/stores", None).await {
        Ok(v) => v,
        Err(f) if f.exit == EXIT_UNREACHABLE => {
            ctx.print(&json!({ "running": false }), || {
                "Knoggin is not running. Start it with `knoggin start`.".into()
            });
            return Ok(EXIT_UNREACHABLE);
        }
        Err(f) if f.exit == EXIT_LOCKED => {
            ctx.print(&json!({ "running": true, "locked": f.message }), || {
                format!("Knoggin is running, but its data is locked: {}", f.message)
            });
            return Ok(EXIT_LOCKED);
        }
        Err(f) => return Err(f.into()),
    };
    let address = daemon::read_lock_info(&ctx.home).map(|i| i.address);
    let mut rows = Vec::new();
    let mut model = Value::Null;
    let mut locked = false;
    for s in stores["items"].as_array().into_iter().flatten() {
        let id = s["id"].as_str().unwrap_or_default();
        let st = client
            .json(Method::GET, &format!("/v1/stores/{id}/status"), None)
            .await
            .unwrap_or(Value::Null);
        if model.is_null() {
            model = st["model"].clone();
        }
        locked |= st["state"] == "locked";
        rows.push(json!({
            "id": id, "name": s["name"], "kind": s["kind"], "status": s["status"],
            "memoryCount": s["memoryCount"], "state": st["state"], "lockedReason": st["lockedReason"],
        }));
    }
    let out = json!({
        "running": true, "address": address, "model": model, "masterKeySource": source, "stores": rows,
    });
    ctx.print(&out, || {
        let mut t = format!(
            "Knoggin is running at http://{}\nModel: {} ({})\nMaster key: {}\nStores:",
            address.as_deref().unwrap_or("?"),
            model["name"].as_str().unwrap_or("?"),
            model["state"].as_str().unwrap_or("?"),
            source.unwrap_or("?"),
        );
        for r in &rows {
            let state = match r["lockedReason"].as_str() {
                Some(reason) => format!("locked: {reason}"),
                None => r["state"].as_str().unwrap_or("?").to_owned(),
            };
            t.push_str(&format!(
                "\n  {} ({}): {} memories, {state}",
                r["name"].as_str().unwrap_or("?"),
                r["kind"].as_str().unwrap_or("?"),
                r["memoryCount"],
            ));
        }
        t
    });
    Ok(if locked { EXIT_LOCKED } else { 0 })
}

async fn open(ctx: &Ctx<'_>, print_url: bool) -> anyhow::Result<u8> {
    ensure_running(ctx).await?;
    let answer = ctx
        .client()
        .json(Method::POST, "/v1/local/browser-code", None)
        .await?;
    let url = answer["url"]
        .as_str()
        .ok_or_else(|| anyhow!("the daemon gave no address"))?
        .to_owned();
    if std::env::consts::OS == "linux" && url.starts_with("http://127.0.0.1") {
        eprintln!(
            "knoggin: the web UI is on 127.0.0.1, not on a name under localhost: this system's \
             resolver cannot keep such a name local, so another account's earlier page on that \
             port could be reused (tab \"Security\", I19)"
        );
    }
    if print_url {
        ctx.print(&answer, || url.clone());
        return Ok(0);
    }
    // The code goes into a private file, so it never appears in a process list.
    let token = knoggin_crypto::token::random_token().map_err(|_| anyhow!("no random bytes"))?;
    let file = ctx.home.run().join(format!("open-{}.html", &token[..16]));
    let page = format!(
        "<!doctype html><meta charset=\"utf-8\"><meta http-equiv=\"refresh\" content=\"0;url={url}\"><title>Knoggin</title>"
    );
    let path = file.clone();
    tokio::task::spawn_blocking(move || {
        transfer::create_private(&path)?.write_all(page.as_bytes())?;
        Ok::<_, anyhow::Error>(())
    })
    .await
    .map_err(|_| anyhow!("internal error"))??;
    let opener = if std::env::consts::OS == "macos" {
        "open"
    } else {
        "xdg-open"
    };
    let status = std::process::Command::new(opener)
        .arg(&file)
        .stdin(std::process::Stdio::null())
        .status()
        .with_context(|| format!("running {opener}"))?;
    if !status.success() {
        bail!(
            "{opener} could not open the page; run `knoggin open --print-url` and open the address yourself"
        );
    }
    Ok(0)
}

async fn mcp(ctx: &Ctx<'_>, tool_set: ToolSet, url: Option<String>) -> anyhow::Result<u8> {
    let target = match url {
        Some(url) => {
            bridge::check_url(&url).map_err(|m| anyhow!(m))?;
            let key = ctx
                .env
                .api_key
                .clone()
                .ok_or_else(|| anyhow!("`knoggin mcp --url` needs KNOGGIN_API_KEY"))?;
            let http = reqwest::Client::builder()
                .connect_timeout(Duration::from_secs(10))
                .build()?;
            Target::Remote { http, url, key }
        }
        None => {
            let (home, config_path, level) = (
                ctx.home.clone(),
                ctx.config_path.clone(),
                ctx.flags.log_level,
            );
            Target::Local {
                client: ctx.client().with_api_key(ctx.env.api_key.clone()),
                start: Box::new(move || {
                    daemon::spawn_detached(&home, config_path.as_deref(), level).map(drop)
                }),
            }
        }
    };
    let mut b = Bridge::new(target, tool_set, bridge::find_namespace(&ctx.env.cwd));
    b.run(
        tokio::io::BufReader::new(tokio::io::stdin()),
        tokio::io::stdout(),
    )
    .await
    .context("the stdio bridge")?;
    Ok(0)
}

/// The store ID for `--app <name>`; `None` means the personal store.
async fn store_of(client: &Client, app: Option<&str>) -> anyhow::Result<Option<String>> {
    let Some(name) = app else { return Ok(None) };
    let stores = client.json(Method::GET, "/v1/stores", None).await?;
    let mut matches: Vec<&Value> = stores["items"]
        .as_array()
        .into_iter()
        .flatten()
        .filter(|s| s["kind"] == "app")
        .filter(|s| {
            s["name"]
                .as_str()
                .is_some_and(|n| n.to_lowercase() == name.trim().to_lowercase())
        })
        .collect();
    // An active app wins over a deleted one of the same name.
    matches.sort_by_key(|s| s["status"] != "active");
    matches
        .first()
        .and_then(|s| s["id"].as_str())
        .map(|id| Some(id.to_owned()))
        .ok_or_else(|| anyhow!("there is no app named {name:?}; `knoggin apps list` shows them"))
}

/// The store ID for `--app`, or the personal store's.
async fn store_id(client: &Client, app: Option<&str>) -> anyhow::Result<String> {
    if let Some(id) = store_of(client, app).await? {
        return Ok(id);
    }
    let stores = client.json(Method::GET, "/v1/stores", None).await?;
    stores["items"]
        .as_array()
        .into_iter()
        .flatten()
        .find(|s| s["kind"] == "personal")
        .and_then(|s| s["id"].as_str())
        .map(str::to_owned)
        .ok_or_else(|| anyhow!("the daemon listed no personal store"))
}

async fn apps(ctx: &Ctx<'_>, c: AppsCommand) -> anyhow::Result<u8> {
    let client = ctx.client();
    match c {
        AppsCommand::List => {
            let v = client.json(Method::GET, "/v1/stores", None).await?;
            ctx.print(&v, || {
                v["items"]
                    .as_array()
                    .into_iter()
                    .flatten()
                    .map(|s| {
                        format!(
                            "{}  {} ({}, {}), {} memories",
                            s["id"].as_str().unwrap_or(""),
                            s["name"].as_str().unwrap_or(""),
                            s["kind"].as_str().unwrap_or(""),
                            s["status"].as_str().unwrap_or(""),
                            s["memoryCount"]
                        )
                    })
                    .collect::<Vec<_>>()
                    .join("\n")
            });
        }
        AppsCommand::Create { name } => {
            let v = client
                .json(Method::POST, "/v1/stores", Some(&json!({ "name": name })))
                .await?;
            ctx.print(&v, || {
                format!(
                    "Created the app {} ({}).",
                    v["name"].as_str().unwrap_or(""),
                    v["id"].as_str().unwrap_or("")
                )
            });
        }
        AppsCommand::Delete { name } => {
            let id = store_id(&client, Some(&name)).await?;
            let v = client
                .json(Method::DELETE, &format!("/v1/stores/{id}"), None)
                .await?;
            ctx.print(&v, || {
                format!(
                    "Deleted the app {name}. It can be undone until {}.",
                    v["deleteAfter"].as_str().unwrap_or("?")
                )
            });
        }
    }
    Ok(0)
}

async fn api_keys(ctx: &Ctx<'_>, c: KeysCommand) -> anyhow::Result<u8> {
    let client = ctx.client();
    match c {
        KeysCommand::List { app } => {
            let id = store_id(&client, app.as_deref()).await?;
            let v = client
                .json(Method::GET, &format!("/v1/stores/{id}/keys"), None)
                .await?;
            ctx.print(&v, || {
                v["items"]
                    .as_array()
                    .into_iter()
                    .flatten()
                    .map(|k| {
                        let state = if k["revokedAt"].is_null() {
                            "active"
                        } else {
                            "revoked"
                        };
                        format!(
                            "{}  {}  knoggin_live_{}...  expires {}  {state}",
                            k["id"].as_str().unwrap_or(""),
                            k["name"].as_str().unwrap_or(""),
                            k["prefix"].as_str().unwrap_or(""),
                            k["expiresAt"].as_str().unwrap_or("")
                        )
                    })
                    .collect::<Vec<_>>()
                    .join("\n")
            });
        }
        KeysCommand::Create {
            app,
            name,
            read_only,
            expires_days,
        } => {
            let id = store_id(&client, app.as_deref()).await?;
            let mut body = json!({ "name": name });
            if read_only {
                body["permissions"] = json!(["memory:read"]);
            }
            if let Some(d) = expires_days {
                body["expiresInDays"] = json!(d.parse::<u32>()?);
            }
            let v = client
                .json(Method::POST, &format!("/v1/stores/{id}/keys"), Some(&body))
                .await?;
            ctx.print(&v, || {
                format!(
                    "{}\nThis is the only time the key is shown. It expires {}.",
                    v["secret"].as_str().unwrap_or(""),
                    v["key"]["expiresAt"].as_str().unwrap_or("?")
                )
            });
        }
        KeysCommand::Revoke { key_id, app } => {
            let id = store_id(&client, app.as_deref()).await?;
            client
                .json(
                    Method::DELETE,
                    &format!("/v1/stores/{id}/keys/{key_id}"),
                    None,
                )
                .await?;
            ctx.print(&json!({ "revoked": key_id }), || {
                format!("Revoked the key {key_id}.")
            });
        }
    }
    Ok(0)
}

async fn import(ctx: &Ctx<'_>, args: ImportArgs) -> anyhow::Result<u8> {
    let client = ctx.client();
    match (args.source, args.file) {
        (Some(ImportSource::Recalld { dir, app }), _) => {
            let store = store_of(&client, app.as_deref()).await?;
            if !ctx.json {
                eprintln!(
                    "knoggin: reading {}; its two database files are copied into {} while they are read, \
                     with memory text in plain text, and deleted afterwards",
                    dir.display(),
                    ctx.home.run().display()
                );
            }
            let s = transfer::import_recalld(&client, &ctx.home, store.as_deref(), &dir).await?;
            let v = serde_json::to_value(&s)?;
            ctx.print(&v, || {
                let mut t = format!(
                    "Namespaces: {}\nMemories: {} full, {} summary, {} ghost\nForgotten memories skipped: {}\n\
                     Edges: {}\nEdges skipped: {}\nWaiting for embedding: {}\nErrors: {}",
                    s.namespaces, s.memories.full, s.memories.summary, s.memories.ghost, s.forgotten_skipped,
                    s.edges, s.edges_skipped, s.pending_embedding, s.errors
                );
                for r in &s.renamed {
                    t.push_str(&format!("\nRenamed {:?} to {:?}", r.from, r.to));
                }
                t.push_str("\nThe daemon embeds them in the background; `knoggin reembed` shows progress.");
                t
            });
            Ok(if s.errors > 0 { 1 } else { 0 })
        }
        (None, Some(file)) => {
            let store = store_of(&client, args.app.as_deref()).await?;
            let on_conflict = match args.on_conflict {
                Conflict::Skip => OnConflict::Skip,
                Conflict::Error => OnConflict::Error,
            };
            let v = transfer::import_file(&client, store.as_deref(), on_conflict, &file).await?;
            ctx.print(&v, || {
                let mut t = format!(
                    "Imported {} lines, skipped {}.",
                    v["imported"], v["skipped"]
                );
                for e in v["errors"].as_array().into_iter().flatten() {
                    t.push_str(&format!(
                        "\nline {}: {} ({})",
                        e["line"],
                        e["message"].as_str().unwrap_or(""),
                        e["code"].as_str().unwrap_or("")
                    ));
                }
                t
            });
            let failed = v["errors"].as_array().is_some_and(|e| !e.is_empty());
            Ok(u8::from(failed))
        }
        (None, None) => bail!("name an export file, or `import recalld <dir>`"),
    }
}

async fn reembed(ctx: &Ctx<'_>, app: Option<&str>) -> anyhow::Result<u8> {
    let client = ctx.client();
    let id = store_id(&client, app).await?;
    let store = store_of(&client, app).await?;
    // Opens the store, if it is parked.
    client
        .json_on(Method::GET, "/v1/namespaces", store.as_deref(), None)
        .await?;
    loop {
        let st = client
            .json(Method::GET, &format!("/v1/stores/{id}/status"), None)
            .await?;
        let pending = st["embedding"]["pending"].as_u64().unwrap_or(0);
        let total = st["embedding"]["total"].as_u64().unwrap_or(0);
        if !ctx.json {
            println!(
                "{} of {total} embedded, {pending} waiting",
                total.saturating_sub(pending)
            );
        }
        if pending == 0 {
            ctx.print(&st, String::new);
            return Ok(0);
        }
        tokio::time::sleep(Duration::from_secs(2)).await;
    }
}

async fn key(ctx: &Ctx<'_>, config: &Config, c: KeyCommand) -> anyhow::Result<u8> {
    match c {
        KeyCommand::Export { out } => {
            let (cf, h, o) = (config.clone(), ctx.home.clone(), out.clone());
            tokio::task::spawn_blocking(move || keys::export(&cf, &h, &o))
                .await
                .map_err(|_| anyhow!("internal error"))??;
            eprintln!(
                "knoggin: anyone with {} and the data folder can read the memories; keep it apart from the data",
                out.display()
            );
            ctx.print(&json!({ "out": out.display().to_string() }), || {
                format!("Wrote {}.", out.display())
            });
        }
        KeyCommand::Import { file } => {
            let (cf, h, f) = (config.clone(), ctx.home.clone(), file.clone());
            let source = tokio::task::spawn_blocking(move || keys::import(&cf, &h, &f))
                .await
                .map_err(|_| anyhow!("internal error"))??;
            ctx.print(&json!({ "source": source.as_str() }), || {
                format!("Loaded the master key into the {}.", source.as_str())
            });
        }
        KeyCommand::Generate { out } => {
            let set = knoggin_crypto::MasterKeySet::generate()?;
            knoggin_crypto::source::create_key_file(&out, &set)
                .with_context(|| format!("writing {}", out.display()))?;
            ctx.print(
                &json!({ "out": out.display().to_string(), "kekId": set.current_kek_id() }),
                || {
                    format!(
                        "Wrote the key set {} to {}.",
                        set.current_kek_id(),
                        out.display()
                    )
                },
            );
        }
        KeyCommand::Rotate => {
            let r = keys::rotate(config, &ctx.home).await?;
            ctx.print(&serde_json::to_value(&r)?, || {
                format!(
                    "The new master key is {}; {} store keys were wrapped again.",
                    r.kek_id, r.stores
                )
            });
        }
    }
    Ok(0)
}

async fn model_command(ctx: &Ctx<'_>, config: &Config, c: ModelCommand) -> anyhow::Result<u8> {
    match c {
        ModelCommand::Fetch { dir } => {
            if dir.is_none() && !config.model.download {
                bail!(
                    "model.download is false, so nothing is written to model.dir {}; \
                     give `--dir <dir>` to fetch the files elsewhere",
                    config.model.dir.display()
                );
            }
            let root = dir.unwrap_or_else(|| config.model.dir.clone());
            let mut last = 0u64;
            let path = model::fetch(&root, |done, total| {
                // One line per 10%.
                let tenth = done.saturating_mul(10) / total.max(1);
                if tenth > last && !ctx.json {
                    last = tenth;
                    eprintln!("knoggin: {done} of {total} bytes");
                }
            })
            .await?;
            ctx.print(&json!({ "dir": path.display().to_string() }), || {
                format!("The model files are in {}.", path.display())
            });
            Ok(0)
        }
        ModelCommand::Verify => {
            let library = model::library_or_hint(ctx.env.ort_dylib.as_deref())?;
            let root = config.model.dir.clone();
            let v = tokio::task::spawn_blocking(move || model::verify(&root, &library))
                .await
                .map_err(|_| anyhow!("internal error"))??;
            let value = serde_json::to_value(&v)?;
            ctx.print(&value, || flat_text(&value));
            Ok(u8::from(!v.ok))
        }
    }
}

async fn use_previous(ctx: &Ctx<'_>, config: &Config, yes: bool) -> anyhow::Result<u8> {
    if ctx.json && !yes {
        // The question cannot be part of JSON output.
        return Err(Failure {
            exit: 2,
            message: "`state use-previous --json` needs `--yes`".into(),
        }
        .into());
    }
    let Some(p) = daemon::previous(config, &ctx.home).await? else {
        bail!("there is no earlier state file to fall back to");
    };
    if !ctx.json {
        println!(
            "The previous state file {} was written at {}. It holds {} stores and {} API keys.\n\
             Anything changed after that time is undone: apps, keys, revocations and deletions.",
            p.file, p.written_at, p.stores, p.keys
        );
        for s in &p.unrecorded_stores {
            println!(
                "The store folder {s} has no record in it; `knoggin admin snapshot export` still reads its snapshots."
            );
        }
    }
    if !yes {
        print!("Use it? Type yes: ");
        let _ = std::io::stdout().flush();
        let mut answer = String::new();
        std::io::stdin().lock().read_line(&mut answer)?;
        if answer.trim() != "yes" {
            println!("Nothing was changed.");
            return Ok(1);
        }
    }
    let bad = daemon::use_previous(config, &ctx.home).await?;
    let moved = bad.map(|b| b.display().to_string());
    let mut out = serde_json::to_value(&p)?;
    out["setAside"] = json!(moved);
    ctx.print(&out, || {
        format!(
            "Moved the newest state file aside to {}.",
            moved.as_deref().unwrap_or("?")
        )
    });
    Ok(0)
}

async fn healthcheck(ctx: &Ctx<'_>, url: Option<String>) -> anyhow::Result<u8> {
    let ready = match url {
        // The address as given, such as `http://127.0.0.1:8080/readyz` (tab "Deployment").
        Some(url) => {
            let http = reqwest::Client::builder()
                .timeout(Duration::from_secs(5))
                .build()?;
            http.get(url)
                .send()
                .await
                .is_ok_and(|r| r.status().is_success())
        }
        None => ctx
            .client()
            .json(Method::GET, "/readyz", None)
            .await
            .is_ok(),
    };
    ctx.print(&json!({ "ready": ready }), || {
        if ready { "ready" } else { "not ready" }.into()
    });
    Ok(u8::from(!ready))
}
