# SPDX-License-Identifier: Apache-2.0
"""Every model's fields match the schema of the same name in `docs/openapi.json`."""

from __future__ import annotations

import inspect
import json
from pathlib import Path
from typing import Any

import pytest
from pydantic import BaseModel

from knoggin import models

OPENAPI = json.loads((Path(__file__).resolve().parents[3] / "docs" / "openapi.json").read_text())
SCHEMAS: dict[str, Any] = OPENAPI["components"]["schemas"]

# Models whose name is not a schema's, and the schemas they stand for.
OTHER_NAMES = {
    "NewMemory": ["StoreMemoryItem"],
    "RecallResult": ["CompactRecall", "FullRecall"],
}

MODELS = [
    cls
    for _, cls in inspect.getmembers(models, inspect.isclass)
    if issubclass(cls, BaseModel) and cls.__module__ == models.__name__ and not cls.__name__.startswith("_")
]


def properties(schema: dict[str, Any]) -> tuple[dict[str, Any], set[str]]:
    """Every property and the required names, following `allOf` and `$ref`."""
    if "$ref" in schema:
        return properties(SCHEMAS[schema["$ref"].rsplit("/", 1)[-1]])
    props, required = dict(schema.get("properties", {})), set(schema.get("required", []))
    for part in schema.get("allOf", []):
        p, r = properties(part)
        props |= p
        required |= r
    return props, required


def nullable(prop: dict[str, Any]) -> bool:
    kinds = prop.get("type")
    return (isinstance(kinds, list) and "null" in kinds) or any(o.get("type") == "null" for o in prop.get("oneOf", []))


def test_every_model_has_a_schema() -> None:
    missing = [m.__name__ for m in MODELS if m.__name__ not in SCHEMAS and m.__name__ not in OTHER_NAMES]
    assert missing == []


@pytest.mark.parametrize("model", MODELS, ids=[m.__name__ for m in MODELS])
def test_fields_match_the_schema(model: type[BaseModel]) -> None:
    fields = {f.alias or name: f for name, f in model.model_fields.items()}
    model_required = {a for a, f in fields.items() if f.is_required()}
    for schema_name in OTHER_NAMES.get(model.__name__, [model.__name__]):
        props, required = properties(SCHEMAS[schema_name])
        assert set(fields) == set(props), f"{model.__name__} vs {schema_name}"
        # What the model requires, the server always sends; what the server always sends and is never null, the model requires.
        assert model_required <= required, f"{model.__name__}: required here only"
        assert {r for r in required if not nullable(props[r])} <= model_required, f"{model.__name__} vs {schema_name}"
