"""Cost and heuristic utilities for the navpath service."""

from __future__ import annotations

from dataclasses import dataclass, field
from typing import Optional

from .options import SearchOptions
from .path import Tile

DEFAULT_STEP_COST_MS = 600
DEFAULT_NODE_COST_MS = 600


@dataclass(slots=True)
class CostModel:
    """Provides deterministic cost lookups and heuristic values.

    The model centralizes all cost-related decisions so that callers can
    consistently apply overrides coming from :class:`SearchOptions` while
    honoring the defaults defined in `requirements.md` §3. All returned
    values are expressed in milliseconds.
    """

    options: SearchOptions = field(default_factory=SearchOptions)
    """Runtime configuration controlling overrides and limits."""

    step_cost_ms: int = DEFAULT_STEP_COST_MS
    """Base movement cost per tile transition (cardinal or diagonal)."""

    def movement_cost(self, _from: Tile, _to: Tile) -> int:
        """Return the fixed movement cost between adjacent tiles."""

        return self.step_cost_ms

    def door_cost(self, db_cost: Optional[int]) -> int:
        """Return the traversal cost for a door edge."""

        return self._with_override(self.options.door_cost_override, db_cost)

    def lodestone_cost(self, db_cost: Optional[int]) -> int:
        """Return the traversal cost for a lodestone teleport edge."""

        return self._with_override(self.options.lodestone_cost_override, db_cost)

    def object_cost(self, db_cost: Optional[int]) -> int:
        """Return the traversal cost for an object action edge."""

        return self._with_override(self.options.object_cost_override, db_cost)

    def ifslot_cost(self, db_cost: Optional[int]) -> int:
        """Return the traversal cost for an ifslot action edge."""

        return self._with_override(self.options.ifslot_cost_override, db_cost)

    def npc_cost(self, db_cost: Optional[int]) -> int:
        """Return the traversal cost for an NPC action edge."""

        return self._with_override(self.options.npc_cost_override, db_cost)

    def item_cost(self, db_cost: Optional[int]) -> int:
        """Return the traversal cost for an item action edge."""

        return self._with_override(self.options.item_cost_override, db_cost)

    def heuristic(self, current: Tile, goal: Tile) -> int:
        """Return the admissible Chebyshev heuristic scaled by step cost."""

        return self.chebyshev_distance(current, goal) * self.step_cost_ms

    @staticmethod
    def chebyshev_distance(a: Tile, b: Tile) -> int:
        """Return Chebyshev distance (max delta on x/y axes) between tiles."""

        dx = abs(a[0] - b[0])
        dy = abs(a[1] - b[1])
        return max(dx, dy)

    def _with_override(self, override: Optional[int], db_value: Optional[int]) -> int:
        """Return a node cost honoring overrides and defaults."""

        if override is not None:
            return override
        if db_value is not None:
            return db_value
        return DEFAULT_NODE_COST_MS
