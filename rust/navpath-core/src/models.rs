use serde::{Deserialize, Serialize};

pub type Tile = [i32; 3];

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct NodeRef {
    #[serde(rename = "type")]
    pub type_: String,
    pub id: i32,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct Rect {
    pub min: Tile,
    pub max: Tile,
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct ActionStep {
    #[serde(rename = "type")]
    pub type_: String,

    #[serde(rename = "from")]
    pub from_rect: Rect,

    #[serde(rename = "to")]
    pub to_rect: Rect,

    pub cost_ms: i64,

    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub node: Option<NodeRef>,

    // Omit when None or when it's an empty object to match Python emission behavior
    #[serde(default, skip_serializing_if = "crate::json::is_metadata_empty")]
    pub metadata: Option<serde_json::Value>,
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct PathResult {
    pub path: Option<Vec<Tile>>,
    pub actions: Vec<ActionStep>,
    pub reason: Option<String>,
    pub expanded: u64,
    pub cost_ms: i64,
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::{json, Value};

    fn rect(x1: i32, y1: i32, p1: i32, x2: i32, y2: i32, p2: i32) -> Rect {
        Rect { min: [x1, y1, p1], max: [x2, y2, p2] }
    }

    #[test]
    fn node_ref_serializes_with_type_and_id() {
        let n = NodeRef { type_: "door".to_string(), id: 123 };
        let v = serde_json::to_value(&n).unwrap();
        assert_eq!(v["type"], Value::String("door".into()));
        assert_eq!(v["id"], Value::from(123));
    }

    #[test]
    fn action_step_omits_empty_metadata() {
        let step = ActionStep {
            type_: "move".into(),
            from_rect: rect(1, 2, 0, 1, 2, 0),
            to_rect: rect(2, 2, 0, 2, 2, 0),
            cost_ms: 600,
            node: None,
            metadata: Some(json!({})),
        };
        let v = serde_json::to_value(&step).unwrap();
        assert!(v.get("metadata").is_none(), "empty metadata should be omitted");

        let step2 = ActionStep { metadata: None, ..step.clone() };
        let v2 = serde_json::to_value(&step2).unwrap();
        assert!(v2.get("metadata").is_none());

        let step3 = ActionStep { metadata: Some(json!({"db_row": {"k": 1}})), ..step };
        let v3 = serde_json::to_value(&step3).unwrap();
        assert!(v3.get("metadata").is_some());
    }

    #[test]
    fn path_result_round_trip() {
        let action = ActionStep {
            type_: "move".into(),
            from_rect: rect(1, 2, 0, 1, 2, 0),
            to_rect: rect(2, 2, 0, 2, 2, 0),
            cost_ms: 600,
            node: Some(NodeRef { type_: "door".into(), id: 5 }),
            metadata: Some(json!({"k": 1})),
        };
        let pr = PathResult {
            path: Some(vec![[1, 2, 0], [2, 2, 0]]),
            actions: vec![action],
            reason: None,
            expanded: 42,
            cost_ms: 1200,
        };
        let s = serde_json::to_string(&pr).unwrap();
        let de: PathResult = serde_json::from_str(&s).unwrap();
        assert_eq!(pr, de);
    }
}
