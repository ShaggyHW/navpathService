#!/usr/bin/env python3
"""Payload invariant checker: does the action list actually describe the returned path?

`build_route_payload` + `optimize_with_surge_dive` translate a node path into client
actions, and nothing else in the test suite checks that the translation is faithful. This
walks both representations in lockstep against a running service and fails on any
disagreement:

  * every action lands on the next consumed geometry tile — a `move` consumes one tile,
    a `dive`/`surge` consumes `tiles_covered`;
  * an ability's `from` is the tile the character actually stands on at that point
    (actions carry only `to`, so an off-by-one here is invisible to the client until it
    tries to execute the hop);
  * an ability's straight-line span is within the 10-tile reach;
  * the walk ends on the goal tile with nothing skipped or replayed.

Both bugs fixed on 2026-07-31 were caught by exactly these checks (leading ability
reported one tile ahead and swallowed the opening step; surge emitted with no range check
at all, spanning up to 10*sqrt(2) = 14.14 tiles on diagonal runs). Run it after any change
to the payload builder or the surge/dive optimizer:

    SNAPSHOT_PATH=./graph.snapshot cargo run -p navpath-service --release &
    python3 tools/verify_actions.py [port]
"""

import json
import math
import os
import sys
import urllib.request

MAX_ABILITY_TILES = 10
# The service compares against MAX_ABILITY_TILES + 0.5 (half-tile slack), so mirror it.
MAX_ABILITY_SPAN = MAX_ABILITY_TILES + 0.5

PORT = sys.argv[1] if len(sys.argv) > 1 else "8080"
URL = f"http://127.0.0.1:{PORT}/route"
CORPUS = os.path.join(os.path.dirname(os.path.abspath(__file__)), "golden_corpus.json")


def post(body):
    req = urllib.request.Request(
        URL, data=json.dumps(body).encode(), headers={"content-type": "application/json"}
    )
    return json.load(urllib.request.urlopen(req))


def dest(action):
    """An action's destination tile. Moves/abilities carry a bare `[x,y,p]`; macro,
    global, fairy and virtual-start actions carry a `{min,max}` block."""
    to = action["to"]
    return tuple(to) if isinstance(to, list) else tuple(to["min"])


def check(entry, seed):
    body = {
        "start": {"wx": entry["start"][0], "wy": entry["start"][1], "plane": entry["start"][2]},
        "goal": {"wx": entry["goal"][0], "wy": entry["goal"][1], "plane": entry["goal"][2]},
        "options": {"return_geometry": True, "only_actions": True},
        "surge": {"enabled": True, "charges": 2, "cooldown_ms": 20400},
        "dive": {"enabled": True, "cooldown_ms": 20400},
    }
    if seed is not None:
        body["seed"] = seed
    if entry.get("profile") == "all":
        body["profile"] = {
            "requirements": [{"key": "coins", "value": 100000000}, {"key": "hasDungCape", "value": 1}]
        }

    resp = post(body)
    if not resp["found"]:
        return None

    geo = [tuple(g) for g in resp["geometry"]]
    acts = resp["actions"]
    errs = []

    # A virtual-start route prepends one synthetic action that consumes no graph edge: it
    # carries the character from the off-graph tile onto path[0]. It is identifiable by
    # landing exactly on the first geometry tile.
    if acts and dest(acts[0]) == geo[0]:
        acts = acts[1:]

    gi = 0
    pos = geo[0]
    for k, a in enumerate(acts):
        kind = a["type"]
        if kind in ("dive", "surge"):
            if tuple(a["from"]) != pos:
                errs.append(f"act{k} {kind}: from={tuple(a['from'])} but character stands at {pos}")
            span = math.hypot(a["to"][0] - a["from"][0], a["to"][1] - a["from"][1])
            if span > MAX_ABILITY_SPAN:
                errs.append(f"act{k} {kind}: spans {span:.2f} tiles, limit {MAX_ABILITY_SPAN}")
            gi += a["tiles_covered"]
        else:
            # move / macro / global / fairy / teleport all consume exactly one graph edge.
            gi += 1
        to = dest(a)
        if gi < len(geo) and to != geo[gi]:
            errs.append(f"act{k} {kind}: to={to} but geometry[{gi}]={geo[gi]}")
        pos = to

    if gi != len(geo) - 1:
        errs.append(f"consumed {gi} graph edges, geometry has {len(geo) - 1}")
    if pos != geo[-1]:
        errs.append(f"ends at {pos}, goal tile is {geo[-1]}")
    return errs


def main():
    entries = json.load(open(CORPUS))["entries"]
    checked = 0
    failed = 0
    for entry in entries:
        for seed in (None, 4242):
            try:
                errs = check(entry, seed)
            except Exception as ex:  # noqa: BLE001 - report and keep going
                print(f'ERROR {entry["name"]} seed={seed}: {ex}')
                failed += 1
                continue
            if errs is None:
                continue
            checked += 1
            if errs:
                failed += 1
                print(f'FAIL {entry["name"]} seed={seed}')
                for e in errs[:6]:
                    print(f"      {e}")
    print(f"verify_actions: {checked} payloads checked, {failed} with violations")
    return 1 if failed else 0


if __name__ == "__main__":
    sys.exit(main())
