# SPDX-License-Identifier: AGPL-3.0-only
"""`knoggin init`: install the binary and connect the agents on this machine, showing every change first.

The agents' own commands are preferred over editing their files. A file is backed up before it is
changed, written to a temporary file with the original's mode (0600 for a new file) and renamed.
No token or key is ever written: the local entry is the command `knoggin mcp`, which reaches the
daemon over its socket, and a server entry is its `/mcp` address, where the agent signs in itself.
"""

from __future__ import annotations

import argparse
import json
import os
import shutil
import subprocess
import sys
import tempfile
from collections.abc import Callable
from dataclasses import dataclass, field
from datetime import datetime
from pathlib import Path
from typing import Any

from .._config import check_url
from ..errors import ConfigurationError
from . import binary, install_binary, install_dir

NAME = "knoggin"
ADD = f"add the {NAME} MCP server with the agent's own command"
REMOVE = f"remove the {NAME} MCP server with the agent's own command"


@dataclass
class Target:
    """Where the agents connect: the local daemon (`url` None) or a server's `/mcp` address."""

    url: str | None

    def stdio(self) -> dict[str, Any]:
        return {"command": str(binary()), "args": ["mcp"]}


@dataclass
class Change:
    """One planned change, shown before it is made."""

    agent: str
    what: str
    detail: str
    apply: Callable[[], str]
    backups: list[Path] = field(default_factory=lambda: [])


def _home() -> Path:
    return Path.home()


def _vscode_dir() -> Path:
    if sys.platform == "darwin":
        return _home() / "Library" / "Application Support" / "Code" / "User"
    return Path(os.environ.get("XDG_CONFIG_HOME") or _home() / ".config") / "Code" / "User"


def _config_dir() -> Path:
    return Path(os.environ.get("XDG_CONFIG_HOME") or _home() / ".config")


# --- files ------------------------------------------------------------------------------------


def _read_json(path: Path) -> dict[str, Any] | None:
    """The file's object; {} for a missing file; None when it cannot be read as plain JSON (for example JSON with comments)."""
    try:
        data = json.loads(path.read_text(encoding="utf-8"))
    except FileNotFoundError:
        return {}
    except (ValueError, OSError):
        return None
    return data if isinstance(data, dict) else None  # pyright: ignore[reportUnknownVariableType]


def write_file(path: Path, text: str, backups: list[Path]) -> None:
    """Backs up the file, then writes a temporary file with the original's mode and renames it over the original.

    A symbolic link (for example into a dotfiles repository) is written through: the file it points to is
    backed up and replaced, and the link stays.
    """
    if path.is_symlink():
        path = path.resolve()
    mode = 0o600
    if path.exists():
        mode = path.stat().st_mode & 0o777
        backup = path.with_name(f"{path.name}.knoggin-backup-{datetime.now().strftime('%Y%m%d-%H%M%S-%f')}")
        fd = os.open(backup, os.O_WRONLY | os.O_CREAT | os.O_EXCL, mode)
        with os.fdopen(fd, "wb") as f:
            f.write(path.read_bytes())
        os.chmod(backup, mode)
        backups.append(backup)
    path.parent.mkdir(mode=0o700, parents=True, exist_ok=True)
    fd, tmp = tempfile.mkstemp(dir=path.parent, prefix=f".{path.name}.")
    try:
        os.chmod(tmp, mode)
        with os.fdopen(fd, "w", encoding="utf-8") as f:
            f.write(text)
        os.replace(tmp, path)
    except BaseException:
        Path(tmp).unlink(missing_ok=True)
        raise


def _json_change(agent: str, path: Path, key: str, entry: dict[str, Any] | None) -> Change | None:
    """Sets (or with `entry` None removes) `<key>.knoggin` in a JSON file. None when there is nothing to do."""
    data = _read_json(path)
    if data is None:
        snippet = json.dumps({key: {NAME: entry}}) if entry else f'the "{NAME}" entry under "{key}"'
        return Change(
            agent, f"{path} is not plain JSON, so it is left alone", f"Change it by hand: {snippet}", lambda: "skipped"
        )
    servers = data.get(key)
    current = servers.get(NAME) if isinstance(servers, dict) else None  # pyright: ignore[reportUnknownMemberType, reportUnknownVariableType]
    if current == entry:
        return None
    if entry is None:
        what = f"remove the {NAME} entry from {path}"
    else:
        what = f"{'replace' if current else 'add'} the {NAME} entry in {path}"
    change = Change(agent, what, json.dumps({key: {NAME: entry}}) if entry else f'delete "{key}.{NAME}"', lambda: "")

    def apply() -> str:
        fresh = _read_json(path)
        if fresh is None:
            return f"skipped: {path} changed and is no longer plain JSON"
        table = fresh.get(key)
        if not isinstance(table, dict):
            table = {}
            fresh[key] = table
        if entry is None:
            table.pop(NAME, None)  # pyright: ignore[reportUnknownMemberType]
        else:
            table[NAME] = entry
        write_file(path, json.dumps(fresh, indent=2) + "\n", change.backups)
        return "done"

    change.apply = apply
    return change


def _command_change(agent: str, what: str, argv: list[str]) -> Change:
    def apply() -> str:
        done = subprocess.run(argv, capture_output=True, text=True, check=False)
        if done.returncode != 0:
            return f"failed ({done.returncode}): {(done.stderr or done.stdout).strip()}"
        return "done"

    return Change(agent, what, "runs: " + " ".join(argv), apply)


# --- agents -----------------------------------------------------------------------------------


@dataclass
class Agent:
    name: str
    command: str | None
    config_dir: Callable[[], Path]
    plan: Callable[[Target | None], Change | None]
    """The change that adds the entry for a target, or removes it for None."""

    def detected(self) -> bool:
        return bool(self.command and shutil.which(self.command)) or self.config_dir().is_dir()


def _claude(target: Target | None) -> Change | None:
    if not shutil.which("claude"):
        return _manual("Claude Code", "claude", target) if target else None
    if target is None:
        return _command_change("Claude Code", REMOVE, ["claude", "mcp", "remove", "--scope", "user", NAME])
    if target.url:
        return _command_change(
            "Claude Code", ADD, ["claude", "mcp", "add", "--scope", "user", "--transport", "http", NAME, target.url]
        )
    s = target.stdio()
    return _command_change(
        "Claude Code", ADD, ["claude", "mcp", "add", "--scope", "user", NAME, "--", s["command"], *s["args"]]
    )


def _codex(target: Target | None) -> Change | None:
    if not shutil.which("codex"):
        return _manual("Codex", "codex", target) if target else None
    if target is None:
        return _command_change("Codex", REMOVE, ["codex", "mcp", "remove", NAME])
    if target.url:
        return _command_change("Codex", ADD, ["codex", "mcp", "add", NAME, "--url", target.url])
    s = target.stdio()
    return _command_change("Codex", ADD, ["codex", "mcp", "add", NAME, "--", s["command"], *s["args"]])


def _manual(agent: str, command: str, target: Target) -> Change:
    entry = target.url or " ".join([target.stdio()["command"], *target.stdio()["args"]])
    detail = f"Install the `{command}` command and run `uvx knoggin init` again, or add the MCP server {entry} by hand."
    return Change(agent, f"the `{command}` command is not on PATH, so nothing is changed", detail, lambda: "skipped")


def _vscode(target: Target | None) -> Change | None:
    entry = (
        None
        if target is None
        else ({"type": "http", "url": target.url} if target.url else {"type": "stdio", **target.stdio()})
    )
    if entry and shutil.which("code"):
        return _command_change("VS Code", ADD, ["code", "--add-mcp", json.dumps({"name": NAME, **entry})])
    return _json_change("VS Code", _vscode_dir() / "mcp.json", "servers", entry)


def _cursor(target: Target | None) -> Change | None:
    entry = None if target is None else ({"url": target.url} if target.url else target.stdio())
    return _json_change("Cursor", _home() / ".cursor" / "mcp.json", "mcpServers", entry)


def _zed(target: Target | None) -> Change | None:
    entry = None if target is None else ({"url": target.url} if target.url else target.stdio())
    return _json_change("Zed", _config_dir() / "zed" / "settings.json", "context_servers", entry)


def _opencode(target: Target | None) -> Change | None:
    if target is None:
        entry = None
    elif target.url:
        entry = {"type": "remote", "url": target.url, "enabled": True}
    else:
        s = target.stdio()
        entry = {"type": "local", "command": [s["command"], *s["args"]], "enabled": True}
    return _json_change("opencode", _config_dir() / "opencode" / "opencode.json", "mcp", entry)


AGENTS = [
    Agent("Claude Code", "claude", lambda: _home() / ".claude", _claude),
    Agent("Codex", "codex", lambda: _home() / ".codex", _codex),
    Agent("Cursor", "cursor", lambda: _home() / ".cursor", _cursor),
    Agent("VS Code", "code", _vscode_dir, _vscode),
    Agent("Zed", "zed", lambda: _config_dir() / "zed", _zed),
    Agent("opencode", "opencode", lambda: _config_dir() / "opencode", _opencode),
]


# --- the command ------------------------------------------------------------------------------


def _install_change() -> Change:
    def apply() -> str:
        return "done" if install_binary() and binary().is_file() else "failed"

    return Change(
        "Knoggin",
        f"install the knoggin binary into {install_dir()}",
        "runs the installer that ships in this package",
        apply,
    )


def _remove_install_change() -> Change | None:
    parts = [p for p in (install_dir() / "bin", install_dir() / "lib") if p.exists()]
    link = _home() / ".local" / "bin" / NAME
    linked = link.is_symlink() and Path(os.readlink(link)) == binary()
    if not parts and not linked:
        return None

    def apply() -> str:
        for p in parts:
            shutil.rmtree(p)
        if linked:
            link.unlink()
        return "done"

    shown = [*map(str, parts), *([str(link)] if linked else [])]
    return Change("Knoggin", "remove the installed program (your memories are kept)", ", ".join(shown), apply)


def _target(server: str | None) -> Target:
    if server is None:
        return Target(None)
    check_url(server, None)
    return Target(server.rstrip("/") + "/mcp")


def plan(target: Target | None) -> list[Change]:
    """Every change, in order: the program, then one per detected agent. `target` None plans the uninstall."""
    changes: list[Change] = []
    if target is not None and not binary().is_file():
        changes.append(_install_change())
    for agent in AGENTS:
        if agent.detected() and (change := agent.plan(target)) is not None:
            changes.append(change)
    if target is None and (removal := _remove_install_change()) is not None:
        changes.append(removal)
    return changes


def _ask(question: str) -> bool:
    try:
        return input(f"{question} [y/N] ").strip().lower() in ("y", "yes")
    except EOFError:
        return False


def run(argv: list[str]) -> int:
    parser = argparse.ArgumentParser(
        prog="knoggin init", description="Install Knoggin and connect the agents on this machine."
    )
    parser.add_argument("--server", metavar="URL", help="connect the agents to this server instead of the local daemon")
    parser.add_argument("--dry-run", action="store_true", help="only print the planned changes")
    parser.add_argument("--yes", action="store_true", help="make every planned change without asking")
    parser.add_argument("--uninstall", action="store_true", help="remove what init added; never removes memories")
    args = parser.parse_args(argv)
    try:
        target = None if args.uninstall else _target(args.server)
    except ConfigurationError as e:
        print(f"knoggin init: {e}", file=sys.stderr)
        return 2
    changes = plan(target)
    if not changes:
        print("Nothing to do: no agent found that needs a change.")
        return 0
    print("Planned changes:")
    for n, c in enumerate(changes, 1):
        print(f"  {n}. {c.agent}: {c.what}\n       {c.detail}")
    if args.dry_run:
        print("Dry run: nothing was changed.")
        return 0
    results: list[tuple[Change, str]] = []
    for c in changes:
        if args.yes or _ask(f"{c.agent}: {c.what}?"):
            results.append((c, c.apply()))
        else:
            results.append((c, "not done"))
    print("\nResult:")
    for c, outcome in results:
        print(f"  {c.agent}: {c.what}: {outcome}")
        for b in c.backups:
            print(f"       backup: {b}")
    if target is not None:
        print("\nTo undo, run `uvx knoggin init --uninstall`, or put back the backups above.")
    failed = any(o.startswith("failed") for _, o in results)
    return 1 if failed else 0
