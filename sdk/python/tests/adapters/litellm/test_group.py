# SPDX-License-Identifier: Apache-2.0
"""The LiteLLM dependency group, and the network guard of the adapter tests."""

from __future__ import annotations

from ..frameworks import check_group, check_no_network


def test_the_group_installs_the_extra_and_import_knoggin_leaves_it_out() -> None:
    check_group("litellm", "litellm")


def test_no_connection_leaves_the_machine(no_network: list[str]) -> None:
    check_no_network(no_network)
