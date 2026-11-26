// SPDX-License-Identifier: AGPL-3.0-only
//! Checks and downloads the pinned model files: `fetch_model --dir <models root>`.
#![allow(
    clippy::unwrap_used,
    clippy::expect_used,
    clippy::print_stdout,
    clippy::print_stderr,
    dead_code,
    unreachable_pub
)]

use std::path::PathBuf;
use std::process::ExitCode;

#[tokio::main]
async fn main() -> ExitCode {
    let mut args = std::env::args().skip(1);
    let dir = match (args.next().as_deref(), args.next(), args.next()) {
        (Some("--dir"), Some(dir), None) => PathBuf::from(dir),
        _ => {
            eprintln!("usage: fetch_model --dir <models root>");
            return ExitCode::from(2);
        }
    };
    let mut shown = u64::MAX;
    let progress = move |done: u64, total: u64| {
        let percent = done * 100 / total.max(1);
        if percent != shown {
            shown = percent;
            println!("{percent:3}%  {done} of {total} bytes");
        }
    };
    match knoggin_embed::model::ensure(&dir, true, progress).await {
        Ok(path) => {
            println!("model files ready in {}", path.display());
            ExitCode::SUCCESS
        }
        Err(e) => {
            eprintln!("error ({:?}): {e}", e.reason());
            ExitCode::FAILURE
        }
    }
}
