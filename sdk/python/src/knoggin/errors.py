# SPDX-License-Identifier: AGPL-3.0-only
"""The SDK's errors: one base class, and one class per group of the API's error codes."""

from __future__ import annotations

__all__ = [
    "CODE_CLASSES",
    "AuthenticationError",
    "ConfigurationError",
    "ConflictError",
    "InvalidRequestError",
    "KnogginConnectionError",
    "KnogginError",
    "KnogginTimeoutError",
    "NotFoundError",
    "PermissionDeniedError",
    "RateLimitedError",
    "ServerError",
    "StoreUnavailableError",
    "WriteOutcomeUnknown",
]


class KnogginError(Exception):
    """Every error the SDK raises. `code` is the API's error code, or None when no answer came."""

    def __init__(
        self,
        message: str,
        *,
        code: str | None = None,
        field: str | None = None,
        reason: str | None = None,
        request_id: str | None = None,
        status: int | None = None,
    ) -> None:
        super().__init__(message)
        self.message = message
        self.code = code
        self.field = field
        self.reason = reason
        self.request_id = request_id
        self.status = status

    def __str__(self) -> str:
        detail = ", ".join(
            f"{k}={v}"
            for k, v in (
                ("code", self.code),
                ("reason", self.reason),
                ("field", self.field),
                ("request_id", self.request_id),
            )
            if v is not None
        )
        return f"{self.message} ({detail})" if detail else self.message


class ConfigurationError(KnogginError):
    """No running local daemon, or a key that would leave the machine over plain http. Raised before anything is sent."""


class KnogginConnectionError(KnogginError):
    """No response: the connection failed or broke."""


class KnogginTimeoutError(KnogginError):
    """No response within the timeout."""


class AuthenticationError(KnogginError):
    """`unauthenticated`."""


class PermissionDeniedError(KnogginError):
    """`permission_denied`."""


class NotFoundError(KnogginError):
    """`not_found`."""


class ConflictError(KnogginError):
    """`already_exists`, `conflict`, `idempotency_conflict`, `namespace_not_empty`."""


class InvalidRequestError(KnogginError):
    """`invalid_argument`, `payload_too_large`."""


class RateLimitedError(KnogginError):
    """`rate_limited`. `retry_after` is the server's `Retry-After` in seconds, when it sent one."""

    retry_after: float | None = None


class StoreUnavailableError(KnogginError):
    """`store_locked`, `store_deleting`, `resource_exhausted`."""


class ServerError(KnogginError):
    """`internal`, `unavailable`, `upgrading`, `timeout`."""


class WriteOutcomeUnknown(KnogginError):
    """A write got no answer, or only retryable errors, after all retries. It may or may not have been applied."""


CODE_CLASSES: dict[str, type[KnogginError]] = {
    "invalid_argument": InvalidRequestError,
    "payload_too_large": InvalidRequestError,
    "unauthenticated": AuthenticationError,
    "permission_denied": PermissionDeniedError,
    "not_found": NotFoundError,
    "already_exists": ConflictError,
    "conflict": ConflictError,
    "idempotency_conflict": ConflictError,
    "namespace_not_empty": ConflictError,
    "rate_limited": RateLimitedError,
    "store_locked": StoreUnavailableError,
    "store_deleting": StoreUnavailableError,
    "resource_exhausted": StoreUnavailableError,
    "internal": ServerError,
    "unavailable": ServerError,
    "upgrading": ServerError,
    "timeout": ServerError,
}
"""Every code of the API's closed list, and the class it raises."""
