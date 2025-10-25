use crate::db::TeleportRequirement;
use crate::models::RequirementKV;
use serde_json::Value;
use std::collections::HashMap;
#[derive(Debug, Clone)]
pub struct RequirementEvaluator {
    caller: HashMap<String, Value>,
}

impl RequirementEvaluator {
    pub fn new(caller_requirements: &[RequirementKV]) -> Self {
        let mut map = HashMap::with_capacity(caller_requirements.len());
        for kv in caller_requirements {
            map.insert(kv.key.clone(), kv.value.clone());
        }
        Self { caller: map }
    }

    pub fn satisfies_all(&self, db_reqs: &[TeleportRequirement]) -> bool {
        for r in db_reqs {
            if !self.satisfies_one(r) {
                return false;
            }
        }
        true
    }

    fn satisfies_one(&self, r: &TeleportRequirement) -> bool {
        let key = match r.key.as_ref() { Some(k) if !k.is_empty() => k, _ => return false };
        let db_val = match r.value.as_ref() { Some(v) => v.as_str(), None => return false };
        let op = match r.comparison.as_ref() { Some(op) if !op.is_empty() => op.trim(), _ => return false };

        let caller_val = match self.caller.get(key) { Some(v) => v, None => return false };
        eval(caller_val, db_val, op)
    }
}

fn eval(lhs: &Value, rhs_str: &str, op: &str) -> bool {
    match op {
        "==" | "=" => eq(lhs, rhs_str),
        "!=" => ne(lhs, rhs_str),
        ">=" => cmp_rel(lhs, rhs_str, |a, b| a >= b),
        ">" => cmp_rel(lhs, rhs_str, |a, b| a > b),
        "<=" => cmp_rel(lhs, rhs_str, |a, b| a <= b),
        "<" => cmp_rel(lhs, rhs_str, |a, b| a < b),
        _ => false,
    }
}

fn eq(lhs: &Value, rhs_str: &str) -> bool {
    if let (Some(a), Some(b)) = (value_to_f64(lhs), parse_str_to_f64(rhs_str)) {
        a == b
    } else {
        match (value_to_scalar_string(lhs), Some(rhs_str.trim().to_string())) {
            (Some(ls), Some(rs)) => ls == rs,
            _ => false,
        }
    }
}

fn ne(lhs: &Value, rhs_str: &str) -> bool {
    if let (Some(a), Some(b)) = (value_to_f64(lhs), parse_str_to_f64(rhs_str)) {
        a != b
    } else {
        match (value_to_scalar_string(lhs), Some(rhs_str.trim().to_string())) {
            (Some(ls), Some(rs)) => ls != rs,
            _ => false,
        }
    }
}

fn cmp_rel<F: Fn(f64, f64) -> bool>(lhs: &Value, rhs_str: &str, f: F) -> bool {
    let a = match value_to_f64(lhs) {
        Some(v) => v,
        None => return false,
    };
    let b = match parse_str_to_f64(rhs_str) {
        Some(v) => v,
        None => return false,
    };
    f(a, b)
}

fn value_to_f64(v: &Value) -> Option<f64> {
    match v {
        Value::Number(n) => n.as_f64(),
        Value::String(s) => parse_str_to_f64(s),
        _ => None,
    }
}

fn parse_str_to_f64(s: &str) -> Option<f64> {
    let t = s.trim();
    if t.is_empty() { return None; }
    // Allow standard Rust float parsing
    t.parse::<f64>().ok()
}

fn value_to_scalar_string(v: &Value) -> Option<String> {
    match v {
        Value::String(s) => Some(s.clone()),
        Value::Number(n) => Some(n.to_string()),
        Value::Bool(b) => Some(if *b { "true".to_string() } else { "false".to_string() }),
        Value::Null => Some("null".to_string()),
        _ => None,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::db::TeleportRequirement;
    use crate::models::RequirementKV;

    fn tr(key: &str, val: &str, op: &str) -> TeleportRequirement {
        TeleportRequirement { id: 0, meta_info: None, key: Some(key.to_string()), value: Some(val.to_string()), comparison: Some(op.to_string()) }
    }

    #[test]
    fn eq_and_neq_numeric_and_string() {
        let caller = vec![
            RequirementKV { key: "level".into(), value: Value::Number(serde_json::Number::from(50)) },
            RequirementKV { key: "quest".into(), value: Value::String("done".into()) },
            RequirementKV { key: "token".into(), value: Value::Bool(true) },
        ];
        let ev = RequirementEvaluator::new(&caller);
        assert!(ev.satisfies_all(&[tr("level", "50", "==")]));
        assert!(ev.satisfies_all(&[tr("level", "50", "=")]));
        assert!(!ev.satisfies_all(&[tr("level", "40", "==")]));
        assert!(ev.satisfies_all(&[tr("quest", "done", "==")]));
        assert!(ev.satisfies_all(&[tr("quest", "not", "!=")]));
        assert!(ev.satisfies_all(&[tr("token", "true", "==")]));
        assert!(ev.satisfies_all(&[tr("token", "false", "!=")]));
    }

    #[test]
    fn relational_numeric_only() {
        let caller = vec![
            RequirementKV { key: "level".into(), value: Value::Number(serde_json::Number::from(50)) },
            RequirementKV { key: "strnum".into(), value: Value::String("5".into()) },
            RequirementKV { key: "quest".into(), value: Value::String("done".into()) },
        ];
        let ev = RequirementEvaluator::new(&caller);
        assert!(ev.satisfies_all(&[tr("level", "10", ">=")]));
        assert!(ev.satisfies_all(&[tr("level", "49", ">")]));
        assert!(!ev.satisfies_all(&[tr("level", "50", ">")]));
        assert!(ev.satisfies_all(&[tr("level", "50", ">=")]));
        assert!(ev.satisfies_all(&[tr("level", "50", "<=")]));
        assert!(ev.satisfies_all(&[tr("strnum", "4", ">=")]));
        assert!(!ev.satisfies_all(&[tr("quest", "1", ">")]));
    }

    #[test]
    fn missing_key_or_bad_operator_or_missing_fields() {
        let caller = vec![RequirementKV { key: "level".into(), value: Value::Number(serde_json::Number::from(10)) }];
        let ev = RequirementEvaluator::new(&caller);

        assert!(!ev.satisfies_all(&[tr("rank", "1", ">=")]));

        assert!(!ev.satisfies_all(&[tr("level", "10", "??")]));

        let bad = TeleportRequirement { id: 0, meta_info: None, key: None, value: Some("10".into()), comparison: Some(">".into()) };
        assert!(!ev.satisfies_all(&[bad]));
        let bad2 = TeleportRequirement { id: 0, meta_info: None, key: Some("level".into()), value: None, comparison: Some(">".into()) };
        assert!(!ev.satisfies_all(&[bad2]));
    }

    #[test]
    fn duplicate_keys_last_wins_and_trim_handling() {
        let caller = vec![
            RequirementKV { key: "skill".into(), value: Value::String("10".into()) },
            RequirementKV { key: "skill".into(), value: Value::String("12".into()) },
        ];
        let ev = RequirementEvaluator::new(&caller);
        let req = TeleportRequirement { id: 0, meta_info: None, key: Some("skill".into()), value: Some(" 12 ".into()), comparison: Some("  >=  ".into()) };
        assert!(ev.satisfies_all(&[req]));
    }

    #[test]
    fn non_scalar_values_unsatisfied() {
        let caller = vec![
            RequirementKV { key: "flags".into(), value: serde_json::json!([1,2,3]) },
            RequirementKV { key: "meta".into(), value: serde_json::json!({"a":1}) },
        ];
        let ev = RequirementEvaluator::new(&caller);
        assert!(!ev.satisfies_all(&[tr("flags", "1", "==")]));
        assert!(!ev.satisfies_all(&[tr("meta", "{\"a\":1}", "==")]));
        assert!(!ev.satisfies_all(&[tr("flags", "1", ">=")]));
    }
}
