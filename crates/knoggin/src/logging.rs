// SPDX-License-Identifier: AGPL-3.0-only
//! Logging (tab "Engineering rules", Logging): plain text on stderr for the daemon and the
//! command line, JSON on stdout for server roles, `log.level` for Knoggin's own crates and
//! `warn` for every other crate, and the daemon's own log file, rotated at 10 MiB.

use std::fs::{File, OpenOptions};
use std::io::{self, Write};
use std::os::unix::fs::{DirBuilderExt, OpenOptionsExt};
use std::path::{Path, PathBuf};
use std::sync::{Mutex, PoisonError};

use tracing::level_filters::LevelFilter;
use tracing_subscriber::filter::Targets;
use tracing_subscriber::fmt::MakeWriter;
use tracing_subscriber::layer::SubscriberExt;
use tracing_subscriber::util::SubscriberInitExt;
use tracing_subscriber::{Layer, Registry};

use crate::config::{Config, LogFormat, LogLevel};

/// The daemon's log file is rotated when a write would take it past this.
const ROTATE_AT: u64 = 10 * 1024 * 1024;
/// `daemon.log.1` to `.3` are kept.
const KEEP: u32 = 3;

fn level(l: LogLevel) -> LevelFilter {
    match l {
        LogLevel::Error => LevelFilter::ERROR,
        LogLevel::Warn => LevelFilter::WARN,
        LogLevel::Info => LevelFilter::INFO,
        LogLevel::Debug => LevelFilter::DEBUG,
        LogLevel::Trace => LevelFilter::TRACE,
    }
}

/// Where the log lines go besides the file.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum Sink {
    /// Standard error: the daemon and the commands.
    Stderr,
    /// Standard output: the server roles.
    #[allow(dead_code, reason = "the server roles arrive in milestone 6")]
    Stdout,
}

/// Installs the global subscriber. `file` is the daemon's log file.
pub(crate) fn init(config: &Config, sink: Sink, file: Option<&Path>) -> anyhow::Result<()> {
    // Targets match by prefix: `knoggin` covers `knoggin_core`, `knoggin_http` and the rest.
    let filter = Targets::new()
        .with_default(LevelFilter::WARN)
        .with_target("knoggin", level(config.log.level));
    let json = config.log.format == Some(LogFormat::Json);
    let main = match (sink, json) {
        (Sink::Stderr, false) => layer(io::stderr, false),
        (Sink::Stderr, true) => layer(io::stderr, true),
        (Sink::Stdout, false) => layer(io::stdout, false),
        (Sink::Stdout, true) => layer(io::stdout, true),
    };
    let file = match file {
        Some(path) => Some(layer(RotatingFile::open(path)?, json)),
        None => None,
    };
    tracing_subscriber::registry()
        .with(main.and_then(file).with_filter(filter))
        .try_init()
        .map_err(|e| anyhow::anyhow!("could not install the logger: {e}"))
}

type Boxed = Box<dyn Layer<Registry> + Send + Sync>;

fn layer<W>(writer: W, json: bool) -> Boxed
where
    W: for<'a> MakeWriter<'a> + Send + Sync + 'static,
{
    let base = tracing_subscriber::fmt::layer()
        .with_writer(writer)
        .with_ansi(false);
    if json {
        base.json().boxed()
    } else {
        base.boxed()
    }
}

/// The daemon's log file. Rotated by hand, no rotation crate: when a write would take the
/// file past 10 MiB it becomes `daemon.log.1`, the older ones move up to `.3`, the oldest
/// goes, and a new file is opened.
pub(crate) struct RotatingFile {
    path: PathBuf,
    state: Mutex<(File, u64)>,
}

impl std::fmt::Debug for RotatingFile {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("RotatingFile")
            .field("path", &self.path)
            .finish()
    }
}

fn open_log(path: &Path) -> io::Result<File> {
    OpenOptions::new()
        .create(true)
        .append(true)
        .mode(0o600)
        .open(path)
}

impl RotatingFile {
    pub(crate) fn open(path: &Path) -> io::Result<Self> {
        if let Some(dir) = path.parent() {
            std::fs::DirBuilder::new()
                .recursive(true)
                .mode(0o700)
                .create(dir)?;
        }
        let file = open_log(path)?;
        let len = file.metadata()?.len();
        Ok(Self {
            path: path.to_owned(),
            state: Mutex::new((file, len)),
        })
    }

    fn numbered(&self, n: u32) -> PathBuf {
        let mut p = self.path.clone().into_os_string();
        p.push(format!(".{n}"));
        PathBuf::from(p)
    }

    fn write_line(&self, buf: &[u8]) -> io::Result<()> {
        let mut state = self.state.lock().unwrap_or_else(PoisonError::into_inner);
        let (file, len) = &mut *state;
        if *len > 0 && *len + buf.len() as u64 > ROTATE_AT {
            for n in (1..KEEP).rev() {
                match std::fs::rename(self.numbered(n), self.numbered(n + 1)) {
                    Err(e) if e.kind() != io::ErrorKind::NotFound => return Err(e),
                    _ => {}
                }
            }
            std::fs::rename(&self.path, self.numbered(1))?;
            *file = open_log(&self.path)?;
            *len = 0;
        }
        file.write_all(buf)?;
        *len += buf.len() as u64;
        Ok(())
    }
}

/// One `write` per log event: the formatter hands over a whole line.
pub(crate) struct FileWriter<'a>(&'a RotatingFile);

impl Write for FileWriter<'_> {
    fn write(&mut self, buf: &[u8]) -> io::Result<usize> {
        self.0.write_line(buf)?;
        Ok(buf.len())
    }
    fn flush(&mut self) -> io::Result<()> {
        Ok(())
    }
}

impl<'a> MakeWriter<'a> for RotatingFile {
    type Writer = FileWriter<'a>;
    fn make_writer(&'a self) -> Self::Writer {
        FileWriter(self)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_file_rotates_past_10_mib_and_keeps_three() {
        let d = tempfile::tempdir().unwrap();
        let path = d.path().join("logs/daemon.log");
        let f = RotatingFile::open(&path).unwrap();
        let line = vec![b'x'; 4 * 1024 * 1024];
        for _ in 0..12 {
            f.write_line(&line).unwrap();
        }
        // 12 lines of 4 MiB, two per file: the newest file and three older ones remain.
        let names: Vec<String> = std::fs::read_dir(path.parent().unwrap())
            .unwrap()
            .map(|e| e.unwrap().file_name().into_string().unwrap())
            .collect();
        assert_eq!(names.len(), 4, "{names:?}");
        for n in ["daemon.log", "daemon.log.1", "daemon.log.2", "daemon.log.3"] {
            let len = std::fs::metadata(path.parent().unwrap().join(n))
                .unwrap()
                .len();
            assert!(len <= ROTATE_AT, "{n}: {len}");
        }
    }
}
