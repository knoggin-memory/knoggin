# SPDX-License-Identifier: Apache-2.0
"""The typed-memory helpers, against a mocked server."""

from __future__ import annotations

import asyncio
import json
import logging

import pytest
import respx
from pydantic import BaseModel

from knoggin import AsyncClient, Client

from .conftest import BASE, COMPACT, STORED


class TripPlan(BaseModel):
    city: str
    days: int


def test_store_model_saves_json_with_a_type_tag(api: respx.MockRouter) -> None:
    route = api.post("/v1/memories").respond(201, json=STORED)
    Client(base_url=BASE).memories.store_model(TripPlan(city="Lisbon", days=3), tags=["travel"], namespace="w")
    sent = json.loads(route.calls.last.request.content)
    assert sent == {
        "summary": 'TripPlan {"city":"Lisbon","days":3}',
        "fullText": '{"city":"Lisbon","days":3}',
        "namespace": "w",
        "tags": ["travel", "type/tripplan"],
    }


def test_store_model_summary_is_cut_at_200_characters_of_json(api: respx.MockRouter) -> None:
    route = api.post("/v1/memories").respond(201, json=STORED)
    Client(base_url=BASE).memories.store_model(TripPlan(city="x" * 500, days=1))
    sent = json.loads(route.calls.last.request.content)
    assert sent["summary"] == "TripPlan " + sent["fullText"][:200]
    route2 = api.post("/v1/memories").respond(201, json=STORED)
    Client(base_url=BASE).memories.store_model(TripPlan(city="x", days=1), summary="my plan")
    assert json.loads(route2.calls.last.request.content)["summary"] == "my plan"


@pytest.mark.parametrize("kind", ["sync", "async"])
def test_recall_models_filters_by_type_and_skips_what_does_not_parse(
    api: respx.MockRouter, kind: str, caplog: pytest.LogCaptureFixture
) -> None:
    hits = [
        {**COMPACT, "fullText": '{"city":"Lisbon","days":3}'},
        {**COMPACT, "fullText": "not json"},
        COMPACT,
    ]
    route = api.post("/v1/recall").respond(json={"memories": hits, "count": 3})
    caplog.set_level(logging.DEBUG, logger="knoggin")
    if kind == "sync":
        got = Client(base_url=BASE).memories.recall_models("trip", TripPlan, limit=7)
    else:
        got = asyncio.run(AsyncClient(base_url=BASE).memories.recall_models("trip", TripPlan, limit=7))
    assert got == [TripPlan(city="Lisbon", days=3)]
    sent = json.loads(route.calls.last.request.content)
    assert (sent["tags"], sent["limit"], sent["fullTextCount"]) == (["type/tripplan"], 7, 7)
    assert sum("is not a valid TripPlan" in r.getMessage() for r in caplog.records) == 2
