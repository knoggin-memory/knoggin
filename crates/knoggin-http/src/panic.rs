// SPDX-License-Identifier: AGPL-3.0-only
//! The body of the panic hook. Only `main` installs a hook.

use std::io::Write;
use std::panic::PanicHookInfo;

/// The one line the hook writes: where the panic happened, never its message, which could
/// hold memory text.
pub fn location_line(info: &PanicHookInfo<'_>) -> String {
    match info.location() {
        Some(at) => format!("panic at {}:{}:{}\n", at.file(), at.line(), at.column()),
        None => "panic at an unknown location\n".to_owned(),
    }
}

/// Writes [`location_line`] straight to stderr, unbuffered. `daemon` and `serve` exit with
/// code 101 after it; the router does not.
pub fn log_location(info: &PanicHookInfo<'_>) {
    // Nothing useful can be done if stderr is gone.
    let _ = std::io::stderr()
        .lock()
        .write_all(location_line(info).as_bytes());
}
