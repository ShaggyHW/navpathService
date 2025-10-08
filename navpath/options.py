"""Search configuration data models for the navpath service."""

from __future__ import annotations

from dataclasses import dataclass, field
from typing import Any, Dict, Optional

DEFAULT_MAX_EXPANSIONS = 250_000_000
DEFAULT_TIMEOUT_MS = 1_000_000_000
DEFAULT_MAX_CHAIN_DEPTH = 8


@dataclass(slots=True)
class SearchOptions:
    """Configuration toggles and limits for pathfinding searches.

    The structure mirrors the requirements document, making it safe to
    serialize to and from JSON using :func:`dataclasses.asdict` or the
    provided :meth:`to_json_dict` helper. All values are part of the
    public API surface.
    """

    use_doors: bool = True
    """Enable traversal through door nodes when ``True``."""

    use_lodestones: bool = True
    """Enable teleport edges from lodestone nodes when ``True``."""

    use_objects: bool = True
    """Enable action edges from object nodes when ``True``."""

    use_ifslots: bool = True
    """Enable action edges from ifslot nodes when ``True``."""

    use_npcs: bool = True
    """Enable action edges from NPC nodes when ``True``."""

    use_items: bool = True
    """Enable action edges from item nodes when ``True``."""

    max_expansions: int = DEFAULT_MAX_EXPANSIONS
    """Maximum allowable node expansions before returning ``reason="max-expansions"``."""

    timeout_ms: int = DEFAULT_TIMEOUT_MS
    """Maximum wall-clock time in milliseconds before returning ``reason="timeout"``."""

    max_chain_depth: int = DEFAULT_MAX_CHAIN_DEPTH
    """Maximum allowed length for `next_node` chains before rejecting them."""

    door_cost_override: Optional[int] = None
    """Optional override for door traversal cost in milliseconds."""

    lodestone_cost_override: Optional[int] = None
    """Optional override for lodestone teleport cost in milliseconds."""

    object_cost_override: Optional[int] = None
    """Optional override for object node action cost in milliseconds."""

    ifslot_cost_override: Optional[int] = None
    """Optional override for ifslot node action cost in milliseconds."""

    npc_cost_override: Optional[int] = None
    """Optional override for NPC node action cost in milliseconds."""

    item_cost_override: Optional[int] = None
    """Optional override for item node action cost in milliseconds."""

    extras: Dict[str, Any] = field(default_factory=dict)
    """Arbitrary additional flags kept for forward compatibility."""

    def to_json_dict(self) -> Dict[str, Any]:
        """Return a JSON-serializable dictionary representation."""

        return {
            "use_doors": self.use_doors,
            "use_lodestones": self.use_lodestones,
            "use_objects": self.use_objects,
            "use_ifslots": self.use_ifslots,
            "use_npcs": self.use_npcs,
            "use_items": self.use_items,
            "max_expansions": self.max_expansions,
            "timeout_ms": self.timeout_ms,
            "max_chain_depth": self.max_chain_depth,
            "door_cost_override": self.door_cost_override,
            "lodestone_cost_override": self.lodestone_cost_override,
            "object_cost_override": self.object_cost_override,
            "ifslot_cost_override": self.ifslot_cost_override,
            "npc_cost_override": self.npc_cost_override,
            "item_cost_override": self.item_cost_override,
            "extras": dict(self.extras),
        }
