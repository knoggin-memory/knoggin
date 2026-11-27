// SPDX-License-Identifier: AGPL-3.0-only
//! The MCP request and argument parser: a `POST /mcp` body as the endpoint parses it,
//! and the `tools/call` arguments as every tool reads them.
#![no_main]

use libfuzzer_sys::fuzz_target;

fuzz_target!(|data: &[u8]| {
    knoggin_http::mcp::fuzzing::parse_request(data);
});
