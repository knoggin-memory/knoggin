<!-- Checked 2026-11-26 against `codex mcp add --help` of Codex CLI 0.172.0 on the build machine. The `config.toml` keys (`command`, `args`, `url`, `bearer_token_env_var`) and the skills folder are UNVERIFIED: the vendor's documentation was not read (the build contacts no outside service). -->

# Codex

`uvx knoggin init` makes this change for you. To do it by hand, run one command.

**Local install:**

```sh
codex mcp add knoggin -- knoggin mcp
```

**Server:**

```sh
codex mcp add knoggin --url https://memory.example.com/mcp
codex mcp login knoggin
```

Use your server's address in place of `memory.example.com`. `codex mcp login` opens your identity provider's sign-in page in the browser; when you are done, you are back in Codex. Knoggin shows no page of its own.

To use an API key instead, add `--bearer-token-env-var KNOGGIN_API_KEY` and keep the key in that variable. The files `local.toml` and `server.toml` hold the same entries for `~/.codex/config.toml`.

**The skill.** Copy `skills/knoggin-memory` to `~/.codex/skills/`. Or install the [Knoggin plugin](../../plugin/README.md), which brings the skill and the plugin tool set.
