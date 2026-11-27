# SPDX-License-Identifier: AGPL-3.0-only
"""Typed memories: save a Pydantic object as a memory, and recall memories as objects of a class.

Used as `client.memories.store_model(obj)` and `client.memories.recall_models(query, as_=Cls)`.
"""

from __future__ import annotations

import logging
from typing import Any, TypeVar

from pydantic import BaseModel, ValidationError

from . import _ops
from ._transport import Op
from .models import RecallResult, StoredMemory

M = TypeVar("M", bound=BaseModel)

log = logging.getLogger("knoggin")


def type_tag(cls: type[BaseModel]) -> str:
    """The tag that marks a memory as an object of `cls`: `type/<lower-cased class name>`."""
    return f"type/{cls.__name__.lower()}"


def _with_tag(tags: Any, tag: str) -> tuple[str, ...]:
    return tuple(dict.fromkeys((*tags, tag)))


def store_model(obj: BaseModel, *, summary: str | None = None, **kwargs: Any) -> Op[StoredMemory]:
    """Saves `obj` as JSON in the full text, tagged with its type. The summary defaults to the class name and the start of the JSON."""
    text = obj.model_dump_json()
    tags = _with_tag(kwargs.pop("tags", ()), type_tag(type(obj)))
    return _ops.store(summary or f"{type(obj).__name__} {text[:200]}", full_text=text, tags=tags, **kwargs)


def recall_models_op(query: str, as_: type[BaseModel], **kwargs: Any) -> Op[RecallResult]:
    limit = kwargs.pop("limit", 25)
    tags = _with_tag(kwargs.pop("tags", ()), type_tag(as_))
    return _ops.recall(query, limit=limit, full_text_count=limit, tags=tags, **kwargs)


def parse_models(result: RecallResult, as_: type[M]) -> list[M]:
    """Each hit's full text parsed into `as_`; a hit that does not validate is skipped and logged at debug level."""
    out: list[M] = []
    for hit in result.memories:
        try:
            out.append(as_.model_validate_json(hit.full_text or ""))
        except ValidationError:
            log.debug("skipped memory %s: its full text is not a valid %s", hit.id, as_.__name__)
    return out
