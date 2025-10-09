from __future__ import annotations

from typing import Mapping

from .db import RequirementRow

__all__ = ["evaluate_requirement"]


def evaluate_requirement(req: RequirementRow, ctx_map: Mapping[str, int]) -> bool:
    """Evaluate a single integer requirement against a provided context map.

    Rules (R2):
    - Supports integer comparisons for operators: "=", "!=", "<", "<=", ">", ">=".
    - Missing key in ctx_map -> False.
    - Unknown or missing operator/key/value -> False.
    - Pure function: no I/O and no side-effects.
    """

    # Validate required fields
    if req is None or req.key is None or req.value is None or req.comparison is None:
        return False

    key = req.key
    op = req.comparison.strip()

    # Context must provide the value for the given key
    if key not in ctx_map:
        return False

    actual = ctx_map[key]
    expected = req.value

    # Ensure we are comparing integers only
    if not isinstance(actual, int) or not isinstance(expected, int):
        return False

    if op == "=":
        return actual == expected
    if op == "!=":
        return actual != expected
    if op == "<":
        return actual < expected
    if op == "<=":
        return actual <= expected
    if op == ">":
        return actual > expected
    if op == ">=":
        return actual >= expected

    # Unknown operator
    return False
