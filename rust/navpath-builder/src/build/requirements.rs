use anyhow::Result;
use rusqlite::{Connection, Row};

use navpath_core::eligibility::{encode_opbits, fnv1a32, Op, OPBIT_NUMERIC};

fn row_to_words(r: &Row) -> Result<[u32; 4], rusqlite::Error> {
    let id: i64 = r.get(0)?;
    let key: String = r.get(1)?;
    let value: String = r.get(2)?;
    let comparison: String = r.get(3)?;

    let key_norm = key.trim().to_ascii_lowercase();
    let key_id = fnv1a32(&key_norm);

    let op_opt = Op::from_db_str(&comparison);

    // numeric if value parses as i64 and fits in i32
    let (is_numeric, encoded_val): (bool, u32) = match value.trim().parse::<i64>() {
        Ok(n) if n >= i32::MIN as i64 && n <= i32::MAX as i64 => (true, (n as i32) as u32),
        _ => (false, fnv1a32(&value.trim().to_ascii_lowercase())),
    };

    let opbits = match op_opt {
        Some(op) => encode_opbits(op, is_numeric),
        None => {
            // Unknown operator: mark unsatisfiable by using invalid opcode (0xFF) and numeric flag if any
            let mut bits = OPBIT_NUMERIC & (if is_numeric { OPBIT_NUMERIC } else { 0 });
            bits |= 0x000000FF; // invalid code 255
            bits
        }
    };

    Ok([id as u32, key_id, opbits, encoded_val])
}

/// Compile all requirement rows into flat u32 words (4 per tag) ordered by id asc
pub fn compile_requirement_tags(conn: &Connection) -> Result<Vec<u32>> {
    let mut st = conn.prepare(
        "SELECT id, key, value, comparison FROM teleports_requirements ORDER BY id ASC",
    )?;
    let words_iter = st.query_map([], |r| row_to_words(r))?;
    let mut out: Vec<u32> = Vec::new();
    for res in words_iter {
        let t = res?;
        out.extend_from_slice(&t);
    }
    Ok(out)
}

