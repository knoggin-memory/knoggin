<!-- UNVERIFIED 2026-11-26: the MCP Registry's current schema could not be read from the build machine (the build contacts no outside service). `server.json` is written against schema version 2025-09-29 as known on that date; `integrations/tests/test_integrations.py` checks its shape, not the schema itself. Check it with the registry's publisher tool before publishing. -->

# MCP Registry entry

`server.json` describes Knoggin for the official MCP Registry: a local server that `uvx knoggin mcp` starts, from the PyPI package `knoggin`. On a fresh machine that command installs the Knoggin binary first, then runs the stdio bridge to the local daemon.

Publishing needs proof that the project owns the name `io.github.knoggin-memory/knoggin`: a sign-in as the GitHub organization `knoggin-memory`. The registry also checks the PyPI package's README for the line `mcp-name: io.github.knoggin-memory/knoggin`; `sdk/python/README.md` carries it in an HTML comment, which PyPI does not show. The sign-in is an owner step (prerequisite P3). The build only checks the files.

The versions follow the shared release version. Bump them with every release.
