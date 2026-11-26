# SPDX-License-Identifier: AGPL-3.0-only
"""Checks on the files in integrations/ (tab "SDK and integrations").

Run from the repository root:
    uv run --with pytest --with pyyaml python -m pytest integrations/tests -q
"""

import json
import os
import re
import subprocess
import tomllib
from pathlib import Path
from urllib.parse import urlsplit

import yaml

ROOT = Path(__file__).resolve().parents[2]
INTEGRATIONS = ROOT / "integrations"
# The two marketplace files live at the repository root, where the agents look for them.
MARKETPLACES = [ROOT / ".claude-plugin" / "marketplace.json", ROOT / ".agents" / "plugins" / "marketplace.json"]
SKILL = INTEGRATIONS / "connectors" / "_skill" / "knoggin-memory" / "SKILL.md"
HOOKS = INTEGRATIONS / "plugin" / "hooks" / "hooks.json"
HOOK_LINE = "Knoggin memory is connected. Use it when earlier context matters, and ask before saving."
REPO_URL = "https://github.com/knoggin-memory/knoggin"
URL = re.compile(r"[a-zA-Z][a-zA-Z0-9+.-]*://[^\s\"'`<>)\]]+")
LIVE_KEY = re.compile(r"knoggin_live_[A-Za-z0-9_-]{43}")
SKIP_DIRS = {"__pycache__", ".pytest_cache"}


def files() -> list[Path]:
    found = [p for p in INTEGRATIONS.rglob("*") if p.is_file() and not SKIP_DIRS & set(p.parts)]
    return sorted(found) + MARKETPLACES


def parse(path: Path) -> object:
    text = path.read_text(encoding="utf-8")
    match path.suffix:
        case ".json":
            return json.loads(text)
        case ".yaml" | ".yml":
            return yaml.safe_load(text)
        case ".toml":
            return tomllib.loads(text)
    raise AssertionError(f"not a config file: {path}")


def config_files() -> list[Path]:
    return [p for p in files() if p.suffix in {".json", ".yaml", ".yml", ".toml"}]


def schema_urls() -> set[str]:
    """`$schema` values name a schema version; they are identifiers that no agent fetches."""
    return {d["$schema"] for p in config_files() if p.suffix == ".json" and isinstance(d := parse(p), dict) and "$schema" in d}


def allowed(url: str) -> bool:
    if url == REPO_URL or url.startswith(REPO_URL + "/"):
        return True
    host = (urlsplit(url).hostname or "").lower()
    return (
        host in {"localhost", "::1"}
        or host.startswith("127.")
        or host == "example.com"
        or host.endswith(".example.com")
    )


def test_every_config_file_parses() -> None:
    checked = config_files()
    assert len(checked) > 20
    for path in checked:
        assert parse(path) is not None, path


def test_every_url_is_a_placeholder_or_loopback() -> None:
    exempt = schema_urls()
    bad = []
    for path in files():
        for url in URL.findall(path.read_text(encoding="utf-8")):
            url = url.rstrip(".,;:")
            if url not in exempt and not allowed(url):
                bad.append(f"{path.relative_to(ROOT)}: {url}")
    assert not bad, "\n".join(bad)


def test_no_literal_api_key() -> None:
    bad = [str(p.relative_to(ROOT)) for p in files() if LIVE_KEY.search(p.read_text(encoding="utf-8"))]
    assert not bad, bad


def test_skill_copies_match_the_source() -> None:
    source = SKILL.read_bytes()
    copies = [p for p in INTEGRATIONS.rglob("SKILL.md") if p != SKILL]
    assert INTEGRATIONS / "plugin" / "skills" / "knoggin-memory" / "SKILL.md" in copies
    assert len(copies) > 1
    for copy in copies:
        assert copy.parent.name == "knoggin-memory", copy
        assert copy.read_bytes() == source, f"{copy.relative_to(ROOT)} differs from {SKILL.relative_to(ROOT)}"


def test_the_hook_prints_only_the_fixed_line(tmp_path: Path) -> None:
    hooks = json.loads(HOOKS.read_text(encoding="utf-8"))["hooks"]
    assert set(hooks) == {"SessionStart"}
    commands = [h for group in hooks["SessionStart"] for h in group["hooks"]]
    assert len(commands) == 1 and commands[0]["type"] == "command"
    # An empty environment and an empty folder: the command can read nothing of the user's.
    run = subprocess.run(
        ["/bin/sh", "-c", commands[0]["command"]],
        capture_output=True,
        text=True,
        cwd=tmp_path,
        env={"PATH": os.defpath},
        timeout=10,
        check=False,
    )
    assert (run.returncode, run.stdout, run.stderr) == (0, HOOK_LINE + "\n", "")


def test_registry_entry_shape() -> None:
    entry = json.loads((INTEGRATIONS / "registry" / "server.json").read_text(encoding="utf-8"))
    assert re.search(r"/schemas/\d{4}-\d{2}-\d{2}/server\.schema\.json$", entry["$schema"])
    assert entry["name"] == "io.github.knoggin-memory/knoggin"
    assert 0 < len(entry["description"]) <= 100
    assert entry["repository"] == {"url": REPO_URL, "source": "github"}
    (package,) = entry["packages"]
    assert package["registryType"] == "pypi" and package["identifier"] == "knoggin"
    assert package["transport"] == {"type": "stdio"}
    assert [a["value"] for a in package["packageArguments"]] == ["mcp"]
    assert package["version"] == entry["version"]


def test_versions_follow_the_release_version() -> None:
    version = tomllib.loads((ROOT / "Cargo.toml").read_text(encoding="utf-8"))["workspace"]["package"]["version"]
    plugin = INTEGRATIONS / "plugin"
    versions = {
        "plugin/.claude-plugin/plugin.json": json.loads((plugin / ".claude-plugin" / "plugin.json").read_text())["version"],
        "plugin/.codex-plugin/plugin.json": json.loads((plugin / ".codex-plugin" / "plugin.json").read_text())["version"],
        "registry/server.json": json.loads((INTEGRATIONS / "registry" / "server.json").read_text())["version"],
    }
    assert versions == dict.fromkeys(versions, version)


def test_plugin_entries_use_the_plugin_tool_set() -> None:
    plugin = INTEGRATIONS / "plugin"
    claude = json.loads((plugin / ".mcp.json").read_text())["knoggin"]
    codex = json.loads((plugin / "mcp.json").read_text())["mcpServers"]["knoggin"]
    for entry in (claude, codex):
        assert (entry["command"], entry["args"]) == ("knoggin", ["mcp", "--tool-set", "plugin"])
    assert json.loads((plugin / ".codex-plugin" / "plugin.json").read_text())["mcpServers"] == "./mcp.json"
