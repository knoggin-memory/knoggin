<!-- Checked on 2026-11-27 against the registry's schema version 2025-12-11 (CurrentSchemaVersion in github.com/modelcontextprotocol/registry, pkg/model/constants.go), its docs/reference/server-json/generic-server-json.md, and docs/modelcontextprotocol-io/package-types.mdx and authentication.mdx. `integrations/tests/test_integrations.py` checks the file's shape, not the schema itself; the registry's publisher tool checks the rest at publishing. -->

# MCP Registry entry

`server.json` describes Knoggin for the official MCP Registry: a local server that `uvx knoggin mcp` starts, from the PyPI package `knoggin`. On a fresh machine that command installs the Knoggin binary first, then runs the stdio bridge to the local daemon.

Publishing needs proof that the project owns the name `io.github.knoggin-memory/knoggin`: the publisher must be an Owner of the GitHub organization `knoggin-memory`. `mcp-publisher login github` cannot publish to an organization's name yet, so publish from GitHub Actions (OIDC) or with `mcp-publisher login github --token` and a token that can read the organization's members. The registry also checks the PyPI package's README for the line `mcp-name: io.github.knoggin-memory/knoggin`; `sdk/python/README.md` carries it in an HTML comment, which PyPI does not show. Signing in is a maintainer step; the repository's tests only check the files.

The versions follow the shared release version. Bump them with every release.
