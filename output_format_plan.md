# Plan to Edit /find_path Output Format for Move Actions

## Current Format
Move actions currently include both "from" and "to" positions, where each position is represented as a bounding box with "min" and "max" arrays (which are identical for point positions):

```json
{
  "cost_ms": 600,
  "from": {
    "max": [2923, 3441, 0],
    "min": [2923, 3441, 0]
  },
  "to": {
    "max": [2923, 3440, 0],
    "min": [2923, 3440, 0]
  },
  "type": "move"
}
```

## Desired Format
Remove the "from" field entirely and simplify "to" to only include the "max" position (since min == max for points):

```json
{
  "cost_ms": 600,
  "to": {
    "max": [2923, 3440, 0]
  },
  "type": "move"
}
```

## Implementation Steps

1. **Locate the Response Building Code**
   - File: `rust/navpath-service/src/routes.rs`
   - Function: `find_path` (around lines 754-758)
   - The response is built as: `serde_json::json!({ "actions": actions })`

2. **Modify Move Actions Post-Serialization**
   - After the `actions` Vec<serde_json::Value> is populated but before building the response
   - Iterate through each action in the `actions` vector
   - For actions where `type == "move"`:
     - Remove the `"from"` key from the JSON object
     - Navigate to `"to"` object and remove the `"min"` key, keeping only `"max"`

3. **Code Changes**
   - Add modification logic after line 752 (`let actions = enriched;`)
   - Use `serde_json::Value` mutation methods to edit the JSON in-place
   - Ensure only move actions are modified (other action types like teleports may still need "from")

4. **Testing**
   - Test with the existing `result.json` to verify the transformation
   - Ensure non-move actions remain unchanged
   - Validate that the simplified format still works with downstream consumers

## Notes
- This change only affects move actions; other action types (teleports, interactions) retain their full structure
- The "max" field represents the exact position since min == max for tile movements
- No changes needed to the `Bounds` struct or serialization functions, as we're modifying the JSON after creation
