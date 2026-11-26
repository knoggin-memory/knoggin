// SPDX-License-Identifier: AGPL-3.0-only
//! The `knoggin` binary: parses the command line, loads the config, builds the pieces and
//! runs a role or a command (tab "Workspace and crates", knoggin (the binary)).

mod bridge;
mod cli;
mod client;
mod config;
mod daemon;
mod keys;
mod logging;
mod model;
mod transfer;

use std::process::ExitCode;

use clap::Parser;

fn main() -> ExitCode {
    // The providers first, so that a problem with either stops the start and not the
    // first request.
    if rustls::crypto::aws_lc_rs::default_provider()
        .install_default()
        .is_err()
    {
        return cli::fail_early("the TLS provider could not be installed");
    }
    if jsonwebtoken::crypto::aws_lc::DEFAULT_PROVIDER
        .install_default()
        .is_err()
    {
        return cli::fail_early("the token-check provider could not be installed");
    }
    let args = match cli::Cli::try_parse() {
        Ok(args) => args,
        Err(e) => {
            // `--help` and `--version` are not errors; wrong usage exits with 2.
            let _ = e.print();
            return ExitCode::from(u8::try_from(e.exit_code()).unwrap_or(2));
        }
    };
    let exits = args.exits_on_panic();
    std::panic::set_hook(Box::new(move |info| {
        knoggin_http::panic::log_location(info);
        if exits {
            std::process::exit(101);
        }
    }));
    no_core_dumps();
    let env = match config::Env::from_process() {
        Ok(env) => env,
        Err(e) => return cli::fail_early(&format!("{e:#}")),
    };
    let mut runtime = tokio::runtime::Builder::new_multi_thread();
    if !args.is_daemon() {
        // A command needs little: keep it off the machine's other cores.
        runtime.worker_threads(2);
    }
    let runtime = match runtime.enable_all().build() {
        Ok(rt) => rt,
        Err(e) => return cli::fail_early(&format!("could not start the runtime: {e}")),
    };
    let code = runtime.block_on(cli::run(args, env));
    // Blocking reads of stdin (the bridge) must not hold the exit up.
    runtime.shutdown_timeout(std::time::Duration::from_secs(1));
    ExitCode::from(code)
}

/// Sets the core-dump limit to 0 and, on Linux, marks the process not dumpable.
fn no_core_dumps() {
    let zero = rustix::process::Rlimit {
        current: Some(0),
        maximum: Some(0),
    };
    if rustix::process::setrlimit(rustix::process::Resource::Core, zero).is_err() {
        tracing::warn!("could not set the core-dump limit to 0");
    }
    #[cfg(target_os = "linux")]
    if rustix::process::set_dumpable_behavior(rustix::process::DumpableBehavior::NotDumpable)
        .is_err()
    {
        tracing::warn!("could not mark the process not dumpable");
    }
}
