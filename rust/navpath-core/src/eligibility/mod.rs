use std::collections::HashMap;

/// Simple 32-bit FNV-1a hash for stable key/value ids
pub fn fnv1a32(s: &str) -> u32 {
    const FNV_OFFSET: u32 = 0x811C9DC5;
    const FNV_PRIME: u32 = 16777619;
    let mut hash = FNV_OFFSET;
    for b in s.as_bytes() {
        hash ^= *b as u32;
        hash = hash.wrapping_mul(FNV_PRIME);
    }
    hash
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Op { Eq, Ne, Ge, Gt, Le, Lt }

impl Op {
    pub fn from_db_str(s: &str) -> Option<Self> {
        match s.trim() {
            "==" | "=" => Some(Op::Eq),
            "!=" => Some(Op::Ne),
            ">=" => Some(Op::Ge),
            ">" => Some(Op::Gt),
            "<=" => Some(Op::Le),
            "<" => Some(Op::Lt),
            _ => None,
        }
    }
    pub fn code(self) -> u32 {
        match self {
            Op::Eq => 0,
            Op::Ne => 1,
            Op::Ge => 2,
            Op::Gt => 3,
            Op::Le => 4,
            Op::Lt => 5,
        }
    }
    pub fn from_code(code: u32) -> Option<Self> {
        match code {
            0 => Some(Op::Eq),
            1 => Some(Op::Ne),
            2 => Some(Op::Ge),
            3 => Some(Op::Gt),
            4 => Some(Op::Le),
            5 => Some(Op::Lt),
            _ => None,
        }
    }
}

/// Bit flag in opbits indicating the tag value is numeric
pub const OPBIT_NUMERIC: u32 = 1u32 << 31;
/// Mask to extract the operator code from opbits
pub const OPBIT_MASK: u32 = 0x000000FF;

/// Encode op and numeric flag into a u32
pub fn encode_opbits(op: Op, is_numeric: bool) -> u32 {
    let mut v = op.code() & OPBIT_MASK;
    if is_numeric { v |= OPBIT_NUMERIC; }
    v
}

/// Decode opbits into (Op, is_numeric)
pub fn decode_opbits(opbits: u32) -> Option<(Op, bool)> {
    let is_num = (opbits & OPBIT_NUMERIC) != 0;
    let code = opbits & OPBIT_MASK;
    let op = Op::from_code(code)?;
    Some((op, is_num))
}

/// Client-supplied profile values for requirement evaluation
#[derive(Debug, Clone)]
pub enum ClientValue<'a> { Str(&'a str), Num(i64) }

/// Compact tag encoding used in the snapshot requirement tags section.
/// Each tag uses 4 u32 words in this order:
/// [0] req_id      - the source teleports_requirements.id (u32)
/// [1] key_id      - FNV-1a hash of normalized key (lowercased, trimmed)
/// [2] opbits      - low 8 bits = Op code; highest bit = numeric flag
/// [3] value_id    - if numeric: i32 as u32 (two's complement);
///                   if string: FNV-1a hash of normalized value (lowercased, trimmed)
pub type EncodedTag = [u32; 4];

/// Evaluate a single encoded tag against the client's key-values
pub fn eval_encoded_tag<'a>(tag: &EncodedTag, client_map: &ClientMap<'a>) -> bool {
    let _req_id = tag[0];
    let key_id = tag[1];
    let opbits = tag[2];
    let val = tag[3];
    let Some((op, is_numeric)) = decode_opbits(opbits) else { return false; };

    let Some(values) = client_map.get(&key_id) else { return false; };

    if is_numeric {
        // interpret tag value as i32
        let rhs = (val as i32) as i64;
        if let Some(lhs) = values.num {
            return eval_num(op, lhs, rhs);
        } else {
            return false; // client did not provide numeric value for numeric requirement
        }
    } else {
        // string equality only for Eq/Ne; other ops treated as unsatisfied
        let rhs_hash = val;
        if let Some(lhs_hash) = values.str_hash {
            return match op {
                Op::Eq => lhs_hash == rhs_hash,
                Op::Ne => lhs_hash != rhs_hash,
                _ => false,
            };
        } else {
            return false;
        }
    }
}

fn eval_num(op: Op, lhs: i64, rhs: i64) -> bool {
    match op {
        Op::Eq => lhs == rhs,
        Op::Ne => lhs != rhs,
        Op::Ge => lhs >= rhs,
        Op::Gt => lhs > rhs,
        Op::Le => lhs <= rhs,
        Op::Lt => lhs < rhs,
    }
}

/// Internal representation of a client's values for a given key
#[derive(Debug, Clone, Copy, Default)]
pub struct ClientVals { pub num: Option<i64>, pub str_hash: Option<u32> }

pub type ClientMap<'a> = HashMap<u32, ClientVals>;

/// Build a client map from an iterator of (key, ClientValue)
pub fn build_client_map<'a, I>(iter: I) -> ClientMap<'a>
where I: IntoIterator<Item = (&'a str, ClientValue<'a>)>
{
    let mut map: ClientMap<'a> = HashMap::new();
    for (k, v) in iter {
        let key_norm = k.trim().to_ascii_lowercase();
        let key_id = fnv1a32(&key_norm);
        let entry = map.entry(key_id).or_insert(ClientVals::default());
        match v {
            ClientValue::Num(n) => entry.num = Some(n),
            ClientValue::Str(s) => entry.str_hash = Some(fnv1a32(&s.trim().to_ascii_lowercase())),
        }
    }
    map
}

/// Per-tag satisfaction vector aligned with the encoded tags order
#[derive(Debug, Clone)]
pub struct EligibilityMask { pub satisfied: Vec<bool> }

impl EligibilityMask {
    pub fn is_satisfied(&self, tag_index: usize) -> bool {
        self.satisfied.get(tag_index).copied().unwrap_or(false)
    }
    pub fn len(&self) -> usize { self.satisfied.len() }
    pub fn is_empty(&self) -> bool { self.satisfied.is_empty() }
}

/// Build an EligibilityMask from a flat u32 buffer (4 words per tag)
pub fn build_mask_from_u32<'a, I>(req_tags_words: &[u32], client_iter: I) -> EligibilityMask
where I: IntoIterator<Item = (&'a str, ClientValue<'a>)>
{
    let client_map = build_client_map(client_iter);
    let mut satisfied = Vec::new();
    let mut i = 0usize;
    while i + 3 < req_tags_words.len() {
        let tag: EncodedTag = [
            req_tags_words[i],
            req_tags_words[i + 1],
            req_tags_words[i + 2],
            req_tags_words[i + 3],
        ];
        let ok = eval_encoded_tag(&tag, &client_map);
        satisfied.push(ok);
        i += 4;
    }
    EligibilityMask { satisfied }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn enc(req_id: u32, key: &str, op: Op, numeric: bool, val_str: &str) -> EncodedTag {
        let key_id = fnv1a32(&key.trim().to_ascii_lowercase());
        let opbits = encode_opbits(op, numeric);
        let v = if numeric {
            let n: i64 = val_str.parse().unwrap();
            (n as i32) as u32
        } else {
            fnv1a32(&val_str.trim().to_ascii_lowercase())
        };
        [req_id, key_id, opbits, v]
    }

    #[test]
    fn numeric_and_string_eval() {
        let tags = vec![
            enc(1, "coins", Op::Ge, true, "100"),
            enc(2, "varbit_38", Op::Eq, true, "1"),
            enc(3, "quest_state", Op::Eq, false, "done"),
            enc(4, "membership", Op::Ne, false, "ironman"),
            enc(5, "level", Op::Lt, true, "50"),
        ];
        let mut words = Vec::new();
        for t in &tags { words.extend_from_slice(t); }

        let mask = build_mask_from_u32(
            &words,
            [
                ("coins", ClientValue::Num(150)),
                ("varbit_38", ClientValue::Num(1)),
                ("quest_state", ClientValue::Str("done")),
                ("membership", ClientValue::Str("normal")),
                ("level", ClientValue::Num(40)),
            ],
        );
        assert_eq!(mask.len(), 5);
        assert_eq!(mask.satisfied, vec![true, true, true, true, true]);
    }

    #[test]
    fn unsatisfied_cases() {
        let tags = vec![
            enc(10, "coins", Op::Ge, true, "100"),
            enc(11, "varbit_38", Op::Eq, true, "1"),
            enc(12, "quest_state", Op::Eq, false, "done"),
            enc(13, "membership", Op::Ne, false, "ironman"),
            enc(14, "level", Op::Gt, true, "50"),
        ];
        let mut words = Vec::new();
        for t in &tags { words.extend_from_slice(t); }

        // Missing coins, wrong quest, wrong type for varbit, level not > 50
        let mask = build_mask_from_u32(
            &words,
            [
                ("varbit_38", ClientValue::Str("1")), // wrong type -> unsatisfied
                ("quest_state", ClientValue::Str("not_done")),
                ("membership", ClientValue::Str("ironman")), // Ne should be false
                ("level", ClientValue::Num(50)), // not > 50
            ],
        );
        assert_eq!(mask.len(), 5);
        assert_eq!(mask.satisfied, vec![false, false, false, false, false]);
    }
}
