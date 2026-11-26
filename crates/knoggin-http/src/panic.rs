// SPDX-License-Identifier: AGPL-3.0-only
//! The body of the panic hook. Only `main` installs a hook (tab "Engineering rules").

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

#[cfg(test)]
mod tests {
    use std::sync::{Arc, Mutex};

    use super::*;

    #[test]
    fn the_line_names_the_place_and_not_the_message() {
        let seen = Arc::new(Mutex::new(String::new()));
        let out = seen.clone();
        let previous = std::panic::take_hook();
        std::panic::set_hook(Box::new(move |info| {
            *out.lock().unwrap() = location_line(info);
        }));
        let result = std::panic::catch_unwind(|| panic!("marker-secret-text"));
        std::panic::set_hook(previous);
        assert!(result.is_err());
        let line = seen.lock().unwrap().clone();
        assert!(
            line.starts_with("panic at ") && line.contains("panic.rs"),
            "{line}"
        );
        assert!(!line.contains("marker"), "{line}");
    }
}
