# SPDX-License-Identifier: AGPL-3.0-only
"""A Knoggin daemon in test mode in a temporary home, for the contract tests. Never `~/.knoggin`."""

from __future__ import annotations

import json
import os
import shutil
import signal
import subprocess
import tempfile
import time
from pathlib import Path
from typing import Any

import httpx

REPO = Path(__file__).resolve().parents[3]


def binary() -> Path | None:
    """`KNOGGIN_BIN`, else the debug build of this repository."""
    configured = os.environ.get("KNOGGIN_BIN")
    path = Path(configured) if configured else REPO / "target" / "debug" / "knoggin"
    return path if path.is_file() else None


class Daemon:
    """`knoggin daemon` in the foreground, with a fake model and a key file, on a loopback port the system picks."""

    def __init__(self, exe: Path) -> None:
        # A short path: the socket path must stay under 100 bytes.
        self.dir = Path(tempfile.mkdtemp(prefix="kg"))
        self.home = self.dir / "k"
        self.env = {
            "PATH": os.environ.get("PATH", ""),
            "HOME": str(self.dir),
            "KNOGGIN_HOME": str(self.home),
            "KNOGGIN_TEST_MODE": "1",
            "KNOGGIN_LOCAL__MASTER_KEY": "key_file",
            "KNOGGIN_SERVER__BIND": "127.0.0.1:0",
        }
        self.err = (self.dir / "daemon.err").open("wb")
        self.proc = subprocess.Popen(
            [str(exe), "daemon"],
            env=self.env,
            cwd=self.dir,
            stdin=subprocess.DEVNULL,
            stdout=subprocess.DEVNULL,
            stderr=self.err,
        )
        self._wait_ready()

    @property
    def socket(self) -> Path:
        return self.home / "run" / "daemon.sock"

    @property
    def base_url(self) -> str:
        lock = json.loads((self.home / "run" / "daemon.lock").read_text())
        return f"http://{lock['address']}"

    def _wait_ready(self) -> None:
        deadline = time.monotonic() + 60
        while time.monotonic() < deadline:
            if self.proc.poll() is not None:
                raise RuntimeError(f"the daemon exited: {self.stderr()}")
            lock = self.home / "run" / "daemon.lock"
            if lock.exists() and lock.read_text().strip() and self.socket.exists():
                try:
                    if self.control("GET", "/readyz").status_code == 200:
                        return
                except httpx.TransportError:
                    pass
            time.sleep(0.05)
        self.stop()
        raise RuntimeError(f"the daemon did not start: {self.stderr()}")

    def stderr(self) -> str:
        return (self.dir / "daemon.err").read_text(errors="replace")

    def control(self, method: str, path: str, json: Any = None) -> httpx.Response:
        """A request on the socket, as the owner: control routes, which the SDK has no methods for."""
        with httpx.Client(transport=httpx.HTTPTransport(uds=str(self.socket)), base_url="http://localhost") as c:
            return c.request(method, path, json=json)

    def app_key(self, name: str, permissions: list[str] | None = None) -> tuple[str, str, str]:
        """A new app store and a key for it: (store ID, key ID, secret)."""
        store = self.control("POST", "/v1/stores", {"name": name})
        assert store.status_code == 201, store.text
        store_id = store.json()["id"]
        body: dict[str, Any] = {"name": name}
        if permissions:
            body["permissions"] = permissions
        key = self.control("POST", f"/v1/stores/{store_id}/keys", body)
        assert key.status_code == 201, key.text
        return store_id, key.json()["key"]["id"], key.json()["secret"]

    def stop(self) -> None:
        if self.proc.poll() is None:
            self.proc.send_signal(signal.SIGTERM)
            try:
                self.proc.wait(60)
            except subprocess.TimeoutExpired:
                self.proc.kill()
                self.proc.wait()
        self.err.close()

    def remove(self) -> None:
        self.stop()
        shutil.rmtree(self.dir, ignore_errors=True)
