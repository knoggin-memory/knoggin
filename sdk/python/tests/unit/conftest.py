# SPDX-License-Identifier: Apache-2.0
"""Shared fixtures of the unit tests: answers in the REST shapes, a respx router, and no real waiting."""

from __future__ import annotations

from collections.abc import Iterator
from typing import Any

import httpx
import pytest
import respx

from knoggin import _transport

BASE = "https://kg.test"
KEY = "knoggin_live_" + "a" * 43
ID = "0193a1f2-7c3e-7b1a-9f00-2b6f3c1d9e10"
T = "2026-11-23T10:15:30.123Z"

MEMORY: dict[str, Any] = {
    "id": ID,
    "namespace": "default",
    "createdAt": T,
    "summary": "Maya prefers window seats",
    "fullText": "Said so while booking the Lisbon trip.",
    "tags": ["travel"],
    "entities": ["Maya"],
    "topics": [],
    "emotions": [],
    "phase": "full",
    "strength": 0.93,
    "effectiveStrength": 0.94,
    "stability": 3.7145,
    "isPermastore": False,
    "retrievedAt": T,
    "reinforcedAt": None,
    "accessCount": 3,
    "edgeCount": 4,
    "embedding": "ready",
    "savedBy": "api_key:0193a1f2-7c3e-7b1a-9f00-2b6f3c1d9e11",
    "client": "claude-code",
}
STORED: dict[str, Any] = {
    "id": ID,
    "namespace": "default",
    "phase": "full",
    "strength": 1.0,
    "stability": 3.7145,
    "createdAt": T,
    "embedding": "pending",
}
POLICY: dict[str, Any] = {
    "initialStability": 3.7145,
    "defaultDifficulty": 5.0,
    "phaseThresholds": {"fullToSummary": 0.7, "summaryToGhost": 0.3, "ghostToDelete": 0.05},
    "permastoreThreshold": 1500.0,
    "decayRateMultiplier": None,
}
NAMESPACE: dict[str, Any] = {"name": "work", "createdAt": T, "memoryCount": 3, "policy": POLICY}
COMPACT: dict[str, Any] = {"id": ID, "date": "Mon 2026-11-23", "summary": "Maya prefers window seats"}
CONTEXT: dict[str, Any] = {
    "defaultNamespace": "default",
    "namespaces": [{"name": "default", "memoryCount": 1}],
    "memories": [COMPACT],
    "model": {"state": "ready", "identity": "fake-64", "name": "Fake model"},
    "embedding": {"pending": 0, "total": 1},
    "webUrl": None,
}
STATS: dict[str, Any] = {
    "name": "work",
    "memoryCount": 3,
    "phaseCounts": {"full": 3, "summary": 0, "ghost": 0},
    "permastoreCount": 0,
    "averageStrength": 0.9,
    "edgeCount": 1,
    "embedding": {"ready": 3, "pending": 0, "failed": 0, "none": 0},
    "textBytes": 120,
    "oldestCreatedAt": T,
    "newestCreatedAt": T,
    "policy": POLICY,
}


def requests(route: respx.Route) -> list[httpx.Request]:
    """Every request a route saw. respx's call list is untyped."""
    return [call.request for call in route.calls]  # pyright: ignore[reportUnknownVariableType, reportUnknownMemberType]


def error(code: str, message: str = "nope", **extra: Any) -> dict[str, Any]:
    return {"error": {"code": code, "message": message, "requestId": "0193a1f2-0000-7000-8000-000000000000", **extra}}


@pytest.fixture(autouse=True)
def no_waiting(monkeypatch: pytest.MonkeyPatch) -> list[float]:
    """Retries record their waits instead of sleeping."""
    waits: list[float] = []

    async def async_sleep(seconds: float) -> None:
        waits.append(seconds)

    monkeypatch.setattr(_transport, "sleep", waits.append)
    monkeypatch.setattr(_transport, "async_sleep", async_sleep)
    return waits


@pytest.fixture(autouse=True)
def clean_env(monkeypatch: pytest.MonkeyPatch) -> None:
    for name in ("KNOGGIN_BASE_URL", "KNOGGIN_API_KEY", "KNOGGIN_NAMESPACE", "KNOGGIN_HOME"):
        monkeypatch.delenv(name, raising=False)


@pytest.fixture
def api() -> Iterator[respx.MockRouter]:
    with respx.mock(base_url=BASE, assert_all_called=False) as router:
        yield router
