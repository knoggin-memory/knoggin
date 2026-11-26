# SPDX-License-Identifier: AGPL-3.0-only
"""`knoggin init` against a temporary home with fake agent commands on PATH, and the launcher of the real binary."""

from __future__ import annotations

import json
import os
import stat
from pathlib import Path
from typing import Any

import pytest

from knoggin import cli
from knoggin.cli import init


@pytest.fixture
def home(tmp_path: Path, monkeypatch: pytest.MonkeyPatch) -> Path:
    """A home with Claude Code, Codex and VS Code as commands, Cursor, Zed and opencode as config folders."""
    h = tmp_path / "home"
    bin_dir = tmp_path / "bin"
    bin_dir.mkdir()
    for command in ("claude", "codex", "code"):
        script = bin_dir / command
        script.write_text(f'#!/bin/sh\necho "{command} $*" >> "{tmp_path}/calls.log"\n')
        script.chmod(0o755)
    (h / ".cursor").mkdir(parents=True)
    zed = h / ".config" / "zed"
    zed.mkdir(parents=True)
    (zed / "settings.json").write_text('{"theme": "One Dark", "context_servers": {"other": {"command": "x"}}}')
    (zed / "settings.json").chmod(0o640)
    (h / ".config" / "opencode").mkdir(parents=True)
    monkeypatch.setenv("HOME", str(h))
    monkeypatch.setenv("PATH", f"{bin_dir}:/usr/bin:/bin")
    monkeypatch.delenv("XDG_CONFIG_HOME", raising=False)
    # The bundled installer: a fake that puts a binary in place.
    fake = tmp_path / "install.sh"
    fake.write_text(
        f'mkdir -p "{h}/.knoggin/bin" && printf "#!/bin/sh\\n" > "{h}/.knoggin/bin/knoggin" && chmod 755 "{h}/.knoggin/bin/knoggin"\n'
    )
    monkeypatch.setattr(cli, "installer", lambda: fake)
    return h


def calls(home: Path) -> list[str]:
    log = home.parent / "calls.log"
    return log.read_text().splitlines() if log.exists() else []


def files(home: Path) -> dict[str, Any]:
    out: dict[str, Any] = {}
    for p in (
        home / ".cursor" / "mcp.json",
        home / ".config" / "zed" / "settings.json",
        home / ".config" / "opencode" / "opencode.json",
    ):
        out[p.parent.name] = json.loads(p.read_text()) if p.exists() else None
    return out


def test_dry_run_prints_every_change_and_makes_none(home: Path, capsys: pytest.CaptureFixture[str]) -> None:
    before = files(home)
    assert init.run(["--dry-run"]) == 0
    out = capsys.readouterr().out
    for agent in ("Knoggin: install", "Claude Code", "Codex", "VS Code", "Cursor", "Zed", "opencode"):
        assert agent in out
    assert "Dry run: nothing was changed." in out
    assert calls(home) == [] and files(home) == before
    assert not (home / ".knoggin").exists()


def test_yes_installs_and_connects_every_agent(home: Path, capsys: pytest.CaptureFixture[str]) -> None:
    exe = str(home / ".knoggin" / "bin" / "knoggin")
    assert init.run(["--yes"]) == 0
    assert (home / ".knoggin" / "bin" / "knoggin").is_file()
    got = calls(home)
    assert got[0] == f"claude mcp add --scope user knoggin -- {exe} mcp"
    assert got[1] == f"codex mcp add knoggin -- {exe} mcp"
    assert json.loads(got[2].removeprefix("code --add-mcp ")) == {
        "name": "knoggin",
        "type": "stdio",
        "command": exe,
        "args": ["mcp"],
    }
    f = files(home)
    assert f[".cursor"] == {"mcpServers": {"knoggin": {"command": exe, "args": ["mcp"]}}}
    assert f["zed"]["theme"] == "One Dark"
    assert f["zed"]["context_servers"] == {"other": {"command": "x"}, "knoggin": {"command": exe, "args": ["mcp"]}}
    assert f["opencode"] == {"mcp": {"knoggin": {"type": "local", "command": [exe, "mcp"], "enabled": True}}}
    # A new file is 0600; a changed file keeps its mode, and so does its backup.
    assert stat.S_IMODE((home / ".cursor" / "mcp.json").stat().st_mode) == 0o600
    zed = home / ".config" / "zed"
    assert stat.S_IMODE((zed / "settings.json").stat().st_mode) == 0o640
    (backup,) = zed.glob("settings.json.knoggin-backup-*")
    assert stat.S_IMODE(backup.stat().st_mode) == 0o640
    assert json.loads(backup.read_text()) == {"theme": "One Dark", "context_servers": {"other": {"command": "x"}}}
    assert [p.name for p in zed.iterdir() if p.name.startswith(".settings")] == []  # no temporary file left
    out = capsys.readouterr().out
    assert f"backup: {backup}" in out and "knoggin init --uninstall" in out
    # Nothing secret is written anywhere.
    assert "knoggin_live_" not in json.dumps(files(home))
    # A second run finds nothing left to change in the files.
    init.run(["--dry-run"])
    second = capsys.readouterr().out
    assert "Cursor:" not in second and "Zed:" not in second and "Knoggin: install" not in second


def test_server_target_uses_the_mcp_address(home: Path) -> None:
    assert init.run(["--yes", "--server", "https://memory.example.com/"]) == 0
    got = calls(home)
    assert got[0] == "claude mcp add --scope user --transport http knoggin https://memory.example.com/mcp"
    assert got[1] == "codex mcp add knoggin --url https://memory.example.com/mcp"
    assert files(home)[".cursor"] == {"mcpServers": {"knoggin": {"url": "https://memory.example.com/mcp"}}}
    assert files(home)["opencode"]["mcp"]["knoggin"] == {
        "type": "remote",
        "url": "https://memory.example.com/mcp",
        "enabled": True,
    }


def test_a_bad_server_address_is_refused(home: Path) -> None:
    assert init.run(["--server", "ftp://x"]) == 2


def test_each_change_is_asked_about(home: Path, monkeypatch: pytest.MonkeyPatch) -> None:
    answers = iter(["y", "n", "n", "yes", "n", "n", "n"])  # install, claude, codex, cursor, vs code, zed, opencode
    monkeypatch.setattr("builtins.input", lambda _prompt: next(answers))  # pyright: ignore[reportUnknownLambdaType, reportUnknownArgumentType]
    assert init.run([]) == 0
    assert calls(home) == []
    f = files(home)
    assert f[".cursor"] is not None and "knoggin" not in f["zed"]["context_servers"] and f["opencode"] is None


def test_a_file_with_comments_is_left_alone(home: Path, capsys: pytest.CaptureFixture[str]) -> None:
    settings = home / ".config" / "zed" / "settings.json"
    settings.write_text('// my settings\n{"theme": "x"}\n')
    assert init.run(["--yes"]) == 0
    assert settings.read_text() == '// my settings\n{"theme": "x"}\n'
    assert "is not plain JSON" in capsys.readouterr().out


def test_uninstall_removes_the_entries_and_the_program_but_not_the_data(home: Path) -> None:
    assert init.run(["--yes"]) == 0
    data = home / ".knoggin" / "data"
    data.mkdir()
    (data / "store").write_text("memories")
    (home / ".knoggin" / "lib").mkdir()
    assert init.run(["--uninstall", "--yes"]) == 0
    got = calls(home)[3:]
    assert got == ["claude mcp remove --scope user knoggin", "codex mcp remove knoggin"]
    f = files(home)
    assert f[".cursor"] == {"mcpServers": {}}
    assert f["zed"]["context_servers"] == {"other": {"command": "x"}}
    assert f["opencode"] == {"mcp": {}}
    assert not (home / ".knoggin" / "bin").exists() and not (home / ".knoggin" / "lib").exists()
    assert (data / "store").read_text() == "memories"


def test_no_agent_and_an_installed_binary_means_nothing_to_do(tmp_path: Path, monkeypatch: pytest.MonkeyPatch) -> None:
    monkeypatch.setenv("HOME", str(tmp_path))
    monkeypatch.setenv("PATH", "/usr/bin:/bin")
    (tmp_path / ".knoggin" / "bin").mkdir(parents=True)
    (tmp_path / ".knoggin" / "bin" / "knoggin").touch()
    assert init.run(["--yes"]) == 0


# --- the launcher -------------------------------------------------------------------------------


def test_other_arguments_go_to_the_real_binary(
    home: Path, monkeypatch: pytest.MonkeyPatch, capsys: pytest.CaptureFixture[str]
) -> None:
    seen: list[tuple[str, list[str]]] = []
    monkeypatch.setattr(os, "execv", lambda path, args: seen.append((str(path), list(args))))  # pyright: ignore[reportUnknownLambdaType, reportUnknownArgumentType]
    cli.main(["mcp", "--tool-set", "plugin"])
    exe = str(home / ".knoggin" / "bin" / "knoggin")
    assert seen == [(exe, [exe, "mcp", "--tool-set", "plugin"])]
    assert "installing it first" in capsys.readouterr().err  # it was missing, so it was installed


def test_without_an_installer_the_launcher_says_so(
    home: Path, monkeypatch: pytest.MonkeyPatch, capsys: pytest.CaptureFixture[str]
) -> None:
    monkeypatch.setattr(cli, "installer", lambda: None)
    assert cli.main(["status"]) == 1
    assert "has no installer" in capsys.readouterr().err
