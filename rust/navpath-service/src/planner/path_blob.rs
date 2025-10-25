use crate::models::Tile;

pub trait PathBlobDecoder {
    fn decode(&self, blob: &[u8]) -> Option<Vec<Tile>>;
}

pub struct JsonPathBlobDecoder;

impl PathBlobDecoder for JsonPathBlobDecoder {
    fn decode(&self, blob: &[u8]) -> Option<Vec<Tile>> {
        let s = std::str::from_utf8(blob).ok()?;
        // Accept formats: [[x,y,plane], ...] or [{"x":..,"y":..,"plane":..}, ...]
        // Try array-of-arrays first for compactness.
        if let Ok(list) = serde_json::from_str::<Vec<[i32; 3]>>(s) {
            let mut out = Vec::with_capacity(list.len());
            for [x, y, plane] in list {
                out.push(Tile { x, y, plane });
            }
            return Some(out);
        }
        // Fallback to array of objects
        if let Ok(list) = serde_json::from_str::<Vec<serde_json::Value>>(s) {
            let mut out = Vec::with_capacity(list.len());
            for v in list {
                let x = v.get("x")?.as_i64()? as i32;
                let y = v.get("y")?.as_i64()? as i32;
                let plane = v.get("plane")?.as_i64()? as i32;
                out.push(Tile { x, y, plane });
            }
            return Some(out);
        }
        None
    }
}

pub fn try_decode_with_decoders<'a>(blob: &[u8], decoders: impl IntoIterator<Item = &'a dyn PathBlobDecoder>) -> Option<Vec<Tile>> {
    for d in decoders {
        if let Some(path) = d.decode(blob) {
            return Some(path);
        }
    }
    None
}

pub fn try_decode_default(blob: &[u8]) -> Option<Vec<Tile>> {
    let json = JsonPathBlobDecoder;
    try_decode_with_decoders(blob, [&json as &dyn PathBlobDecoder])
}
