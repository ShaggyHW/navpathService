use rusqlite::{params, OptionalExtension, Row};
use serde::{Deserialize, Serialize};

use super::{is_no_such_table, Database};

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct CellRow {
    pub id: i32,
    pub plane: i32,
    pub kind: String,
    pub wkb: Vec<u8>,
    pub area: f64,
    pub minx: f64,
    pub miny: f64,
    pub maxx: f64,
    pub maxy: f64,
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct PortalRow {
    pub id: i32,
    pub plane: i32,
    pub a_id: i32,
    pub b_id: i32,
    pub x1: f64,
    pub y1: f64,
    pub x2: f64,
    pub y2: f64,
    pub length: f64,
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct OffmeshLinkRow {
    pub id: i32,
    pub link_type: String,
    pub node_table: String,
    pub node_id: i32,
    pub requirement_id: Option<i32>,
    pub cost: Option<f64>,
    pub plane_from: Option<i32>,
    pub plane_to: i32,
    pub src_cell_id: Option<i32>,
    pub dst_cell_id: i32,
    pub meta_json: Option<String>,
}

fn map_cell_row(r: &Row) -> rusqlite::Result<CellRow> {
    Ok(CellRow {
        id: r.get("id")?,
        plane: r.get("plane")?,
        kind: r.get("kind")?,
        wkb: r.get("wkb")?,
        area: r.get("area")?,
        minx: r.get("minx")?,
        miny: r.get("miny")?,
        maxx: r.get("maxx")?,
        maxy: r.get("maxy")?,
    })
}

fn map_portal_row(r: &Row) -> rusqlite::Result<PortalRow> {
    Ok(PortalRow {
        id: r.get("id")?,
        plane: r.get("plane")?,
        a_id: r.get("a_id")?,
        b_id: r.get("b_id")?,
        x1: r.get("x1")?,
        y1: r.get("y1")?,
        x2: r.get("x2")?,
        y2: r.get("y2")?,
        length: r.get("length")?,
    })
}

fn map_offmesh_link_row(r: &Row) -> rusqlite::Result<OffmeshLinkRow> {
    Ok(OffmeshLinkRow {
        id: r.get("id")?,
        link_type: r.get("link_type")?,
        node_table: r.get("node_table")?,
        node_id: r.get("node_id")?,
        requirement_id: r.get("requirement_id")?,
        cost: r.get("cost")?,
        plane_from: r.get("plane_from")?,
        plane_to: r.get("plane_to")?,
        src_cell_id: r.get("src_cell_id")?,
        dst_cell_id: r.get("dst_cell_id")?,
        meta_json: r.get("meta_json")?,
    })
}

// SQL (keep deterministic ORDER BY where applicable)
const CELL_BY_ID: &str = "SELECT id, plane, kind, wkb, area, minx, miny, maxx, maxy FROM cells WHERE id = ?1";
const CELLS_BY_PLANE: &str = "SELECT id, plane, kind, wkb, area, minx, miny, maxx, maxy FROM cells WHERE plane = ?1 ORDER BY id ASC";
const CELLS_RINTERSECT_RECT_BY_PLANE: &str = "\
SELECT c.id, c.plane, c.kind, c.wkb, c.area, c.minx, c.miny, c.maxx, c.maxy \
FROM rtree_cells rt \
JOIN cells c ON c.id = rt.id \
WHERE c.plane = ?5 AND rt.minx <= ?3 AND rt.maxx >= ?1 AND rt.miny <= ?4 AND rt.maxy >= ?2 \
ORDER BY c.id ASC"; // params: minx, miny, maxx, maxy, plane

const PORTALS_BY_PLANE: &str = "SELECT id, plane, a_id, b_id, x1, y1, x2, y2, length FROM portals WHERE plane = ?1 ORDER BY id ASC";
const PORTALS_TOUCHING_CELL: &str = "SELECT id, plane, a_id, b_id, x1, y1, x2, y2, length FROM portals WHERE plane = ?1 AND (a_id = ?2 OR b_id = ?2) ORDER BY id ASC";

const OFFMESH_BY_ID: &str = "SELECT id, link_type, node_table, node_id, requirement_id, cost, plane_from, plane_to, src_cell_id, dst_cell_id, meta_json FROM offmesh_links WHERE id = ?1";
const OFFMESH_BY_DST: &str = "SELECT id, link_type, node_table, node_id, requirement_id, cost, plane_from, plane_to, src_cell_id, dst_cell_id, meta_json FROM offmesh_links WHERE dst_cell_id = ?1 ORDER BY id ASC";
const OFFMESH_BY_SRC: &str = "SELECT id, link_type, node_table, node_id, requirement_id, cost, plane_from, plane_to, src_cell_id, dst_cell_id, meta_json FROM offmesh_links WHERE src_cell_id = ?1 ORDER BY id ASC";
const OFFMESH_GLOBAL: &str = "SELECT id, link_type, node_table, node_id, requirement_id, cost, plane_from, plane_to, src_cell_id, dst_cell_id, meta_json FROM offmesh_links WHERE src_cell_id IS NULL ORDER BY id ASC";

impl Database {
    pub fn fetch_cell(&self, id: i32) -> rusqlite::Result<Option<CellRow>> {
        let mut stmt = match self.conn.prepare_cached(CELL_BY_ID) {
            Ok(s) => s,
            Err(e) if is_no_such_table(&e) => return Ok(None),
            Err(e) => return Err(e),
        };
        let row = stmt.query_row(params![id], |r| map_cell_row(r)).optional()?;
        Ok(row)
    }

    pub fn iter_cells_by_plane(&self, plane: i32) -> rusqlite::Result<Vec<CellRow>> {
        let mut stmt = match self.conn.prepare_cached(CELLS_BY_PLANE) {
            Ok(s) => s,
            Err(e) if is_no_such_table(&e) => return Ok(vec![]),
            Err(e) => return Err(e),
        };
        let mut rows = stmt.query(params![plane])?;
        let mut out = Vec::new();
        while let Some(r) = rows.next()? { out.push(map_cell_row(r)?); }
        Ok(out)
    }

    /// Query cells that intersect the given rectangle on a specific plane using the rtree index.
    pub fn query_cells_intersecting_rect_on_plane(
        &self,
        minx: f64,
        miny: f64,
        maxx: f64,
        maxy: f64,
        plane: i32,
    ) -> rusqlite::Result<Vec<CellRow>> {
        let mut stmt = match self.conn.prepare_cached(CELLS_RINTERSECT_RECT_BY_PLANE) {
            Ok(s) => s,
            Err(e) if is_no_such_table(&e) => return Ok(vec![]),
            Err(e) => return Err(e),
        };
        let mut rows = stmt.query(params![minx, miny, maxx, maxy, plane])?;
        let mut out = Vec::new();
        while let Some(r) = rows.next()? { out.push(map_cell_row(r)?); }
        Ok(out)
    }

    pub fn iter_portals_by_plane(&self, plane: i32) -> rusqlite::Result<Vec<PortalRow>> {
        let mut stmt = match self.conn.prepare_cached(PORTALS_BY_PLANE) {
            Ok(s) => s,
            Err(e) if is_no_such_table(&e) => return Ok(vec![]),
            Err(e) => return Err(e),
        };
        let mut rows = stmt.query(params![plane])?;
        let mut out = Vec::new();
        while let Some(r) = rows.next()? { out.push(map_portal_row(r)?); }
        Ok(out)
    }

    pub fn iter_portals_touching_cell(&self, plane: i32, cell_id: i32) -> rusqlite::Result<Vec<PortalRow>> {
        let mut stmt = match self.conn.prepare_cached(PORTALS_TOUCHING_CELL) {
            Ok(s) => s,
            Err(e) if is_no_such_table(&e) => return Ok(vec![]),
            Err(e) => return Err(e),
        };
        let mut rows = stmt.query(params![plane, cell_id])?;
        let mut out = Vec::new();
        while let Some(r) = rows.next()? { out.push(map_portal_row(r)?); }
        Ok(out)
    }

    pub fn fetch_offmesh_link(&self, id: i32) -> rusqlite::Result<Option<OffmeshLinkRow>> {
        let mut stmt = match self.conn.prepare_cached(OFFMESH_BY_ID) {
            Ok(s) => s,
            Err(e) if is_no_such_table(&e) => return Ok(None),
            Err(e) => return Err(e),
        };
        let row = stmt.query_row(params![id], |r| map_offmesh_link_row(r)).optional()?;
        Ok(row)
    }

    pub fn iter_offmesh_links_to_cell(&self, dst_cell_id: i32) -> rusqlite::Result<Vec<OffmeshLinkRow>> {
        let mut stmt = match self.conn.prepare_cached(OFFMESH_BY_DST) {
            Ok(s) => s,
            Err(e) if is_no_such_table(&e) => return Ok(vec![]),
            Err(e) => return Err(e),
        };
        let mut rows = stmt.query(params![dst_cell_id])?;
        let mut out = Vec::new();
        while let Some(r) = rows.next()? { out.push(map_offmesh_link_row(r)?); }
        Ok(out)
    }

    pub fn iter_offmesh_links_from_cell(&self, src_cell_id: i32) -> rusqlite::Result<Vec<OffmeshLinkRow>> {
        let mut stmt = match self.conn.prepare_cached(OFFMESH_BY_SRC) {
            Ok(s) => s,
            Err(e) if is_no_such_table(&e) => return Ok(vec![]),
            Err(e) => return Err(e),
        };
        let mut rows = stmt.query(params![src_cell_id])?;
        let mut out = Vec::new();
        while let Some(r) = rows.next()? { out.push(map_offmesh_link_row(r)?); }
        Ok(out)
    }

    pub fn iter_offmesh_links_global(&self) -> rusqlite::Result<Vec<OffmeshLinkRow>> {
        let mut stmt = match self.conn.prepare_cached(OFFMESH_GLOBAL) {
            Ok(s) => s,
            Err(e) if is_no_such_table(&e) => return Ok(vec![]),
            Err(e) => return Err(e),
        };
        let mut rows = stmt.query([])?;
        let mut out = Vec::new();
        while let Some(r) = rows.next()? { out.push(map_offmesh_link_row(r)?); }
        Ok(out)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use rusqlite::Connection;

    #[test]
    fn cells_queries_work() {
        let conn = Connection::open_in_memory().unwrap();
        conn.execute_batch(
            r#"
            CREATE TABLE cells (
                id INTEGER PRIMARY KEY,
                plane INTEGER NOT NULL,
                kind TEXT NOT NULL,
                wkb BLOB NOT NULL,
                area REAL NOT NULL,
                minx REAL NOT NULL,
                miny REAL NOT NULL,
                maxx REAL NOT NULL,
                maxy REAL NOT NULL
            );
            CREATE VIRTUAL TABLE rtree_cells USING rtree(id, minx, maxx, miny, maxy);
            "#,
        ).unwrap();
        // Insert two cells on different planes
        conn.execute("INSERT INTO cells (id, plane, kind, wkb, area, minx, miny, maxx, maxy) VALUES (1, 1, 'polygon', x'00', 10.0, 100.0, 100.0, 110.0, 110.0)", []).unwrap();
        conn.execute("INSERT INTO cells (id, plane, kind, wkb, area, minx, miny, maxx, maxy) VALUES (2, 2, 'polygon', x'00', 12.0, 200.0, 200.0, 210.0, 210.0)", []).unwrap();
        // Mirror into rtree
        conn.execute("INSERT INTO rtree_cells (id, minx, maxx, miny, maxy) VALUES (1, 100.0, 110.0, 100.0, 110.0)", []).unwrap();
        conn.execute("INSERT INTO rtree_cells (id, minx, maxx, miny, maxy) VALUES (2, 200.0, 210.0, 200.0, 210.0)", []).unwrap();

        let db = Database::from_connection(conn);
        let p1 = db.iter_cells_by_plane(1).unwrap();
        assert_eq!(p1.len(), 1);
        assert_eq!(p1[0].id, 1);

        let rect_hit = db.query_cells_intersecting_rect_on_plane(95.0, 95.0, 105.0, 105.0, 1).unwrap();
        assert_eq!(rect_hit.len(), 1);
        assert_eq!(rect_hit[0].id, 1);

        let none_hit = db.query_cells_intersecting_rect_on_plane(0.0, 0.0, 1.0, 1.0, 1).unwrap();
        assert!(none_hit.is_empty());

        let c = db.fetch_cell(1).unwrap().unwrap();
        assert_eq!(c.plane, 1);
    }

    #[test]
    fn portal_queries_work() {
        let conn = Connection::open_in_memory().unwrap();
        conn.execute_batch(
            r#"
            CREATE TABLE portals (
                id INTEGER PRIMARY KEY,
                plane INTEGER NOT NULL,
                a_id INTEGER NOT NULL,
                b_id INTEGER NOT NULL,
                x1 REAL NOT NULL,
                y1 REAL NOT NULL,
                x2 REAL NOT NULL,
                y2 REAL NOT NULL,
                length REAL NOT NULL
            );
            "#,
        ).unwrap();
        conn.execute("INSERT INTO portals (id,plane,a_id,b_id,x1,y1,x2,y2,length) VALUES (1,1,1,4,10.0,10.0,20.0,10.0,10.0)", []).unwrap();
        conn.execute("INSERT INTO portals (id,plane,a_id,b_id,x1,y1,x2,y2,length) VALUES (2,1,2,1,30.0,30.0,40.0,30.0,10.0)", []).unwrap();
        conn.execute("INSERT INTO portals (id,plane,a_id,b_id,x1,y1,x2,y2,length) VALUES (3,2,3,4,50.0,50.0,60.0,50.0,10.0)", []).unwrap();

        let db = Database::from_connection(conn);
        let on_plane = db.iter_portals_by_plane(1).unwrap();
        assert_eq!(on_plane.len(), 2);
        assert_eq!(on_plane[0].id, 1);
        assert_eq!(on_plane[1].id, 2);

        let touching = db.iter_portals_touching_cell(1, 1).unwrap();
        assert_eq!(touching.len(), 2);
        assert_eq!(touching[0].id, 1);
        assert_eq!(touching[1].id, 2);
    }

    #[test]
    fn offmesh_queries_work() {
        let conn = Connection::open_in_memory().unwrap();
        conn.execute_batch(
            r#"
            CREATE TABLE offmesh_links (
                id INTEGER PRIMARY KEY,
                link_type TEXT NOT NULL,
                node_table TEXT NOT NULL,
                node_id INTEGER NOT NULL,
                requirement_id INTEGER NULL,
                cost REAL NULL,
                plane_from INTEGER NULL,
                plane_to INTEGER NOT NULL,
                src_cell_id INTEGER NULL,
                dst_cell_id INTEGER NOT NULL,
                meta_json TEXT NULL
            );
            "#,
        ).unwrap();
        conn.execute(r#"INSERT INTO offmesh_links (id,link_type,node_table,node_id,requirement_id,cost,plane_from,plane_to,src_cell_id,dst_cell_id,meta_json) VALUES (1,'lodestone','lodestone_nodes',23,27,17000.0,NULL,1,NULL,39,'{"lodestone":"PRIFDDINAS"}')"#, []) .unwrap();
        conn.execute("INSERT INTO offmesh_links (id,link_type,node_table,node_id,requirement_id,cost,plane_from,plane_to,src_cell_id,dst_cell_id,meta_json) VALUES (2,'door','door_nodes',5,NULL,10.0,1,1,10,39,NULL)", []).unwrap();

        let db = Database::from_connection(conn);
        let to_cell = db.iter_offmesh_links_to_cell(39).unwrap();
        assert_eq!(to_cell.len(), 2);
        assert_eq!(to_cell[0].id, 1);
        assert_eq!(to_cell[1].id, 2);

        let one = db.fetch_offmesh_link(2).unwrap().unwrap();
        assert_eq!(one.node_table, "door_nodes");
    }

    #[test]
    fn missing_tables_graceful() {
        let conn = Connection::open_in_memory().unwrap();
        let db = Database::from_connection(conn);
        assert!(db.fetch_cell(1).unwrap().is_none());
        assert!(db.fetch_offmesh_link(1).unwrap().is_none());
        assert_eq!(db.iter_cells_by_plane(0).unwrap().len(), 0);
        assert_eq!(db.query_cells_intersecting_rect_on_plane(0.0,0.0,1.0,1.0,0).unwrap().len(), 0);
        assert_eq!(db.iter_portals_by_plane(0).unwrap().len(), 0);
        assert_eq!(db.iter_portals_touching_cell(0, 1).unwrap().len(), 0);
        assert_eq!(db.iter_offmesh_links_to_cell(1).unwrap().len(), 0);
    }
}
