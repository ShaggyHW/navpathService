use serde_json::Value;

// Helper used by serde's skip_serializing_if to omit empty metadata objects
// and None values to match Python's behavior of only emitting metadata when truthy.
pub fn is_metadata_empty(meta: &Option<Value>) -> bool {
    match meta {
        None => true,
        Some(Value::Object(map)) => map.is_empty(),
        _ => false,
    }
}
