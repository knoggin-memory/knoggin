# SPDX-License-Identifier: AGPL-3.0-only
"""Settings: constructor arguments win over environment variables. And the two rules checked before anything is sent."""

from __future__ import annotations

import ipaddress
import os
import stat
import sys
from dataclasses import dataclass
from pathlib import Path

import httpx

from .errors import ConfigurationError

NOT_RUNNING = "Knoggin is not running. Start it with `knoggin start`."


@dataclass(frozen=True)
class Settings:
    base_url: str | None
    api_key: str | None
    namespace: str | None
    timeout: float
    max_retries: int
    home: Path

    @property
    def socket(self) -> Path:
        return self.home / "run" / "daemon.sock"

    def __repr__(self) -> str:  # never print the key
        return f"Settings(base_url={self.base_url!r}, api_key={'set' if self.api_key else None}, namespace={self.namespace!r})"


def resolve(
    base_url: str | None,
    api_key: str | None,
    namespace: str | None,
    timeout: float,
    max_retries: int,
) -> Settings:
    base_url = base_url or os.environ.get("KNOGGIN_BASE_URL") or None
    settings = Settings(
        base_url=base_url.rstrip("/") if base_url else None,
        api_key=api_key or os.environ.get("KNOGGIN_API_KEY") or None,
        namespace=namespace or os.environ.get("KNOGGIN_NAMESPACE") or None,
        timeout=timeout,
        max_retries=max_retries,
        home=knoggin_home(),
    )
    if settings.base_url is None:
        if sys.platform == "win32":
            raise ConfigurationError(
                "There is no local Knoggin daemon on Windows: give base_url or set KNOGGIN_BASE_URL."
            )
    else:
        check_url(settings.base_url, settings.api_key)
    return settings


def knoggin_home() -> Path:
    home = os.environ.get("KNOGGIN_HOME")
    return Path(home) if home else Path.home() / ".knoggin"


def is_loopback(host: str) -> bool:
    """`localhost`, an address in 127.0.0.0/8, or ::1, as written. The name is never resolved."""
    if host.lower() == "localhost":
        return True
    try:
        return ipaddress.ip_address(host.strip("[]")).is_loopback
    except ValueError:
        return False


def check_url(url: str, api_key: str | None) -> None:
    """A key never travels in the clear: plain http only to a loopback address."""
    try:
        parsed = httpx.URL(url)
    except httpx.InvalidURL as e:
        raise ConfigurationError(f"base_url is not a valid address: {e}") from None
    if parsed.scheme not in ("http", "https") or not parsed.host:
        raise ConfigurationError("base_url must be an http or https address")
    if api_key and parsed.scheme == "http" and not is_loopback(parsed.host):
        raise ConfigurationError(
            f"Refusing to send an API key over plain http to {parsed.host}: use https, or a loopback address"
        )


def check_socket(settings: Settings) -> None:
    """The local daemon: `<home>` and `<home>/run` must be this user's and not writable by others, and the socket must exist."""
    for path in (settings.home, settings.home / "run"):
        try:
            st = path.lstat()
        except FileNotFoundError:
            raise ConfigurationError(NOT_RUNNING) from None
        if not stat.S_ISDIR(st.st_mode) or st.st_uid != os.getuid() or st.st_mode & 0o022:
            raise ConfigurationError(f"{path} must be a folder of this user that group and others cannot write")
    if not settings.socket.exists():
        raise ConfigurationError(NOT_RUNNING)
