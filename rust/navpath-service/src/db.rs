use anyhow::{Context, Result};
use rusqlite::{params, Connection, OpenFlags, OptionalExtension, Row};
use std::path::Path;

pub struct Db {
    conn: Connection,
}

impl Db {
    pub fn open_read_only<P: AsRef<Path>>(path: P) -> Result<Self> {
        let conn = Connection::open_with_flags(
            path,
            OpenFlags::SQLITE_OPEN_READ_ONLY | OpenFlags::SQLITE_OPEN_URI,
        )
        .with_context(|| "failed to open SQLite database read-only")?;
        Self::apply_pragmas(&conn)?;
        Ok(Self { conn })
    }

    fn apply_pragmas(conn: &Connection) -> Result<()> {
        // Best-effort pragmas for safety/perf; ignore unsupported errors
        let _ = conn.pragma_update(None, "query_only", &1i32);
        let _ = conn.pragma_update(None, "foreign_keys", &1i32);
        let _ = conn.pragma_update(None, "synchronous", &"OFF");
        let _ = conn.pragma_update(None, "journal_mode", &"OFF");
        Ok(())
    }

    pub fn list_clusters(&self, limit: usize) -> Result<Vec<Cluster>> {
        let mut stmt = self
            .conn
            .prepare_cached("SELECT cluster_id, plane, label, tile_count FROM clusters LIMIT ?1")
            .context("prepare list_clusters")?;
        let rows = stmt
            .query_map(params![limit as i64], |r| Ok(Cluster::from_row(r)))
            .context("exec list_clusters")?;
        let mut out = Vec::new();
        for r in rows {
            out.push(r?);
        }
        Ok(out)
    }

    pub fn get_cluster(&self, cluster_id: i64) -> Result<Option<Cluster>> {
        let mut stmt = self
            .conn
            .prepare_cached("SELECT cluster_id, plane, label, tile_count FROM clusters WHERE cluster_id = ?1")
            .context("prepare get_cluster")?;
        let row = stmt
            .query_row(params![cluster_id], |r| Ok(Cluster::from_row(r)))
            .optional()
            .context("exec get_cluster")?;
        Ok(row)
    }

    pub fn list_cluster_tiles(&self, cluster_id: i64) -> Result<Vec<ClusterTile>> {
        let mut stmt = self
            .conn
            .prepare_cached(
                "SELECT cluster_id, x, y, plane FROM cluster_tiles WHERE cluster_id = ?1 ORDER BY plane, x, y",
            )
            .context("prepare list_cluster_tiles")?;
        let rows = stmt
            .query_map(params![cluster_id], |r| Ok(ClusterTile::from_row(r)))
            .context("exec list_cluster_tiles")?;
        let mut out = Vec::new();
        for r in rows {
            out.push(r?);
        }
        Ok(out)
    }

    pub fn list_cluster_entrances_by_cluster(&self, cluster_id: i64) -> Result<Vec<ClusterEntrance>> {
        let mut stmt = self
            .conn
            .prepare_cached(
                "SELECT entrance_id, cluster_id, x, y, plane, neighbor_dir, teleport_edge_id
                 FROM cluster_entrances WHERE cluster_id = ?1 ORDER BY entrance_id",
            )
            .context("prepare list_cluster_entrances_by_cluster")?;
        let rows = stmt
            .query_map(params![cluster_id], |r| Ok(ClusterEntrance::from_row(r)))
            .context("exec list_cluster_entrances_by_cluster")?;
        let mut out = Vec::new();
        for r in rows {
            out.push(r?);
        }
        Ok(out)
    }

    pub fn get_intraconnection(&self, from: i64, to: i64) -> Result<Option<ClusterIntraConnection>> {
        let mut stmt = self
            .conn
            .prepare_cached(
                "SELECT entrance_from, entrance_to, cost, path_blob
                 FROM cluster_intraconnections WHERE entrance_from = ?1 AND entrance_to = ?2",
            )
            .context("prepare get_intraconnection")?;
        let row = stmt
            .query_row(params![from, to], |r| Ok(ClusterIntraConnection::from_row(r)))
            .optional()
            .context("exec get_intraconnection")?;
        Ok(row)
    }

    pub fn get_interconnection(&self, from: i64, to: i64) -> Result<Option<ClusterInterConnection>> {
        let mut stmt = self
            .conn
            .prepare_cached(
                "SELECT entrance_from, entrance_to, cost
                 FROM cluster_interconnections WHERE entrance_from = ?1 AND entrance_to = ?2",
            )
            .context("prepare get_interconnection")?;
        let row = stmt
            .query_row(params![from, to], |r| Ok(ClusterInterConnection::from_row(r)))
            .optional()
            .context("exec get_interconnection")?;
        Ok(row)
    }

    pub fn get_tile(&self, x: i32, y: i32, plane: i32) -> Result<Option<TileRow>> {
        let mut stmt = self
            .conn
            .prepare_cached(
                "SELECT x, y, plane, flag, blocked, walk_mask, blocked_mask, walk_data
                 FROM tiles WHERE x = ?1 AND y = ?2 AND plane = ?3",
            )
            .context("prepare get_tile")?;
        let row = stmt
            .query_row(params![x, y, plane], |r| Ok(TileRow::from_row(r)))
            .optional()
            .context("exec get_tile")?;
        Ok(row)
    }

    pub fn list_abstract_teleport_edges_by_dst(&self, plane: i32, x: i32, y: i32) -> Result<Vec<AbstractTeleportEdge>> {
        let mut stmt = self
            .conn
            .prepare_cached(
                "SELECT edge_id, kind, node_id, src_x, src_y, src_plane, dst_x, dst_y, dst_plane, cost, requirement_id, src_entrance, dst_entrance
                 FROM abstract_teleport_edges WHERE dst_plane = ?1 AND dst_x = ?2 AND dst_y = ?3",
            )
            .context("prepare list_abstract_teleport_edges_by_dst")?;
        let rows = stmt
            .query_map(params![plane, x, y], |r| Ok(AbstractTeleportEdge::from_row(r)))
            .context("exec list_abstract_teleport_edges_by_dst")?;
        let mut out = Vec::new();
        for r in rows {
            out.push(r?);
        }
        Ok(out)
    }

    pub fn get_teleport_requirement(&self, id: i64) -> Result<Option<TeleportRequirement>> {
        let mut stmt = self
            .conn
            .prepare_cached("SELECT id, metaInfo, key, value, comparison FROM teleports_requirements WHERE id = ?1")
            .context("prepare get_teleport_requirement")?;
        let row = stmt
            .query_row(params![id], |r| Ok(TeleportRequirement::from_row(r)))
            .optional()
            .context("exec get_teleport_requirement")?;
        Ok(row)
    }

    pub fn get_door_node(&self, id: i64) -> Result<Option<TeleportDoorNode>> {
        let mut stmt = self.conn.prepare_cached(
            "SELECT id, direction, real_id_open, real_id_closed,
                    location_open_x, location_open_y, location_open_plane,
                    location_closed_x, location_closed_y, location_closed_plane,
                    tile_inside_x, tile_inside_y, tile_inside_plane,
                    tile_outside_x, tile_outside_y, tile_outside_plane,
                    open_action, cost, next_node_type, next_node_id, requirement_id
             FROM teleports_door_nodes WHERE id = ?1",
        )?;
        let row = stmt
            .query_row(params![id], |r| Ok(TeleportDoorNode::from_row(r)))
            .optional()
            .context("exec get_door_node")?;
        Ok(row)
    }

    pub fn get_lodestone_node(&self, id: i64) -> Result<Option<TeleportLodestoneNode>> {
        let mut stmt = self.conn.prepare_cached(
            "SELECT id, lodestone, dest_x, dest_y, dest_plane, cost, next_node_type, next_node_id, requirement_id
             FROM teleports_lodestone_nodes WHERE id = ?1",
        )?;
        let row = stmt
            .query_row(params![id], |r| Ok(TeleportLodestoneNode::from_row(r)))
            .optional()
            .context("exec get_lodestone_node")?;
        Ok(row)
    }

    pub fn get_npc_node(&self, id: i64) -> Result<Option<TeleportNpcNode>> {
        let mut stmt = self.conn.prepare_cached(
            "SELECT id, match_type, npc_id, npc_name, action,
                    dest_min_x, dest_max_x, dest_min_y, dest_max_y, dest_plane,
                    search_radius, cost,
                    orig_min_x, orig_max_x, orig_min_y, orig_max_y, orig_plane,
                    next_node_type, next_node_id, requirement_id
             FROM teleports_npc_nodes WHERE id = ?1",
        )?;
        let row = stmt
            .query_row(params![id], |r| Ok(TeleportNpcNode::from_row(r)))
            .optional()
            .context("exec get_npc_node")?;
        Ok(row)
    }

    pub fn get_object_node(&self, id: i64) -> Result<Option<TeleportObjectNode>> {
        let mut stmt = self.conn.prepare_cached(
            "SELECT id, match_type, object_id, object_name, action,
                    dest_min_x, dest_max_x, dest_min_y, dest_max_y, dest_plane,
                    orig_min_x, orig_max_x, orig_min_y, orig_max_y, orig_plane,
                    search_radius, cost, next_node_type, next_node_id, requirement_id
             FROM teleports_object_nodes WHERE id = ?1",
        )?;
        let row = stmt
            .query_row(params![id], |r| Ok(TeleportObjectNode::from_row(r)))
            .optional()
            .context("exec get_object_node")?;
        Ok(row)
    }

    pub fn get_item_node(&self, id: i64) -> Result<Option<TeleportItemNode>> {
        let mut stmt = self.conn.prepare_cached(
            "SELECT id, item_id, action,
                    dest_min_x, dest_max_x, dest_min_y, dest_max_y, dest_plane,
                    next_node_type, next_node_id, cost, requirement_id
             FROM teleports_item_nodes WHERE id = ?1",
        )?;
        let row = stmt
            .query_row(params![id], |r| Ok(TeleportItemNode::from_row(r)))
            .optional()
            .context("exec get_item_node")?;
        Ok(row)
    }

    pub fn get_ifslot_node(&self, id: i64) -> Result<Option<TeleportIfSlotNode>> {
        let mut stmt = self.conn.prepare_cached(
            "SELECT id, interface_id, component_id, slot_id, click_id,
                    dest_min_x, dest_max_x, dest_min_y, dest_max_y, dest_plane,
                    cost, next_node_type, next_node_id, requirement_id
             FROM teleports_ifslot_nodes WHERE id = ?1",
        )?;
        let row = stmt
            .query_row(params![id], |r| Ok(TeleportIfSlotNode::from_row(r)))
            .optional()
            .context("exec get_ifslot_node")?;
        Ok(row)
    }
}

#[derive(Debug, Clone)]
pub struct Cluster {
    pub cluster_id: i64,
    pub plane: i64,
    pub label: Option<i64>,
    pub tile_count: Option<i64>,
}
impl Cluster {
    fn from_row(r: &Row<'_>) -> Self {
        Self {
            cluster_id: r.get(0).unwrap_or_default(),
            plane: r.get(1).unwrap_or_default(),
            label: r.get(2).ok(),
            tile_count: r.get(3).ok(),
        }
    }
}

#[derive(Debug, Clone)]
pub struct ClusterTile {
    pub cluster_id: i64,
    pub x: i64,
    pub y: i64,
    pub plane: i64,
}
impl ClusterTile {
    fn from_row(r: &Row<'_>) -> Self {
        Self { cluster_id: r.get(0).unwrap_or_default(), x: r.get(1).unwrap_or_default(), y: r.get(2).unwrap_or_default(), plane: r.get(3).unwrap_or_default() }
    }
}

#[derive(Debug, Clone)]
pub struct ClusterEntrance {
    pub entrance_id: i64,
    pub cluster_id: i64,
    pub x: i64,
    pub y: i64,
    pub plane: i64,
    pub neighbor_dir: String,
    pub teleport_edge_id: Option<i64>,
}
impl ClusterEntrance {
    fn from_row(r: &Row<'_>) -> Self {
        Self {
            entrance_id: r.get(0).unwrap_or_default(),
            cluster_id: r.get(1).unwrap_or_default(),
            x: r.get(2).unwrap_or_default(),
            y: r.get(3).unwrap_or_default(),
            plane: r.get(4).unwrap_or_default(),
            neighbor_dir: r.get::<_, Option<String>>(5).ok().flatten().unwrap_or_default(),
            teleport_edge_id: r.get(6).ok(),
        }
    }
}

#[derive(Debug, Clone)]
pub struct ClusterIntraConnection {
    pub entrance_from: i64,
    pub entrance_to: i64,
    pub cost: i64,
    pub path_blob: Option<Vec<u8>>,
}
impl ClusterIntraConnection {
    fn from_row(r: &Row<'_>) -> Self {
        Self {
            entrance_from: r.get(0).unwrap_or_default(),
            entrance_to: r.get(1).unwrap_or_default(),
            cost: r.get(2).unwrap_or_default(),
            path_blob: r.get(3).ok(),
        }
    }
}

#[derive(Debug, Clone)]
pub struct ClusterInterConnection {
    pub entrance_from: i64,
    pub entrance_to: i64,
    pub cost: i64,
}
impl ClusterInterConnection {
    fn from_row(r: &Row<'_>) -> Self {
        Self {
            entrance_from: r.get(0).unwrap_or_default(),
            entrance_to: r.get(1).unwrap_or_default(),
            cost: r.get(2).unwrap_or_default(),
        }
    }
}

#[derive(Debug, Clone)]
pub struct TileRow {
    pub x: i64,
    pub y: i64,
    pub plane: i64,
    pub flag: i64,
    pub blocked: i64,
    pub walk_mask: i64,
    pub blocked_mask: i64,
    pub walk_data: Option<String>,
}
impl TileRow {
    fn from_row(r: &Row<'_>) -> Self {
        Self {
            x: r.get(0).unwrap_or_default(),
            y: r.get(1).unwrap_or_default(),
            plane: r.get(2).unwrap_or_default(),
            flag: r.get(3).unwrap_or_default(),
            blocked: r.get(4).unwrap_or_default(),
            walk_mask: r.get(5).unwrap_or_default(),
            blocked_mask: r.get(6).unwrap_or_default(),
            walk_data: r.get(7).ok(),
        }
    }
}

#[derive(Debug, Clone)]
pub struct AbstractTeleportEdge {
    pub edge_id: i64,
    pub kind: String,
    pub node_id: i64,
    pub src_x: Option<i64>,
    pub src_y: Option<i64>,
    pub src_plane: Option<i64>,
    pub dst_x: i64,
    pub dst_y: i64,
    pub dst_plane: i64,
    pub cost: i64,
    pub requirement_id: Option<i64>,
    pub src_entrance: Option<i64>,
    pub dst_entrance: Option<i64>,
}
impl AbstractTeleportEdge {
    fn from_row(r: &Row<'_>) -> Self {
        Self {
            edge_id: r.get(0).unwrap_or_default(),
            kind: r.get::<_, Option<String>>(1).ok().flatten().unwrap_or_default(),
            node_id: r.get(2).unwrap_or_default(),
            src_x: r.get(3).ok(),
            src_y: r.get(4).ok(),
            src_plane: r.get(5).ok(),
            dst_x: r.get(6).unwrap_or_default(),
            dst_y: r.get(7).unwrap_or_default(),
            dst_plane: r.get(8).unwrap_or_default(),
            cost: r.get(9).unwrap_or_default(),
            requirement_id: r.get(10).ok(),
            src_entrance: r.get(11).ok(),
            dst_entrance: r.get(12).ok(),
        }
    }
}

#[derive(Debug, Clone)]
pub struct TeleportRequirement {
    pub id: i64,
    pub meta_info: Option<String>,
    pub key: Option<String>,
    pub value: Option<String>,
    pub comparison: Option<String>,
}
impl TeleportRequirement {
    fn from_row(r: &Row<'_>) -> Self {
        Self {
            id: r.get(0).unwrap_or_default(),
            meta_info: r.get(1).ok(),
            key: r.get(2).ok(),
            value: r.get(3).ok(),
            comparison: r.get(4).ok(),
        }
    }
}

#[derive(Debug, Clone)]
pub struct TeleportDoorNode {
    pub id: i64,
    pub direction: Option<String>,
    pub real_id_open: Option<i64>,
    pub real_id_closed: Option<i64>,
    pub location_open_x: Option<i64>,
    pub location_open_y: Option<i64>,
    pub location_open_plane: Option<i64>,
    pub location_closed_x: Option<i64>,
    pub location_closed_y: Option<i64>,
    pub location_closed_plane: Option<i64>,
    pub tile_inside_x: Option<i64>,
    pub tile_inside_y: Option<i64>,
    pub tile_inside_plane: Option<i64>,
    pub tile_outside_x: Option<i64>,
    pub tile_outside_y: Option<i64>,
    pub tile_outside_plane: Option<i64>,
    pub open_action: Option<String>,
    pub cost: Option<i64>,
    pub next_node_type: Option<String>,
    pub next_node_id: Option<i64>,
    pub requirement_id: Option<i64>,
}
impl TeleportDoorNode {
    fn from_row(r: &Row<'_>) -> Self {
        Self {
            id: r.get(0).unwrap_or_default(),
            direction: r.get(1).ok(),
            real_id_open: r.get(2).ok(),
            real_id_closed: r.get(3).ok(),
            location_open_x: r.get(4).ok(),
            location_open_y: r.get(5).ok(),
            location_open_plane: r.get(6).ok(),
            location_closed_x: r.get(7).ok(),
            location_closed_y: r.get(8).ok(),
            location_closed_plane: r.get(9).ok(),
            tile_inside_x: r.get(10).ok(),
            tile_inside_y: r.get(11).ok(),
            tile_inside_plane: r.get(12).ok(),
            tile_outside_x: r.get(13).ok(),
            tile_outside_y: r.get(14).ok(),
            tile_outside_plane: r.get(15).ok(),
            open_action: r.get(16).ok(),
            cost: r.get(17).ok(),
            next_node_type: r.get(18).ok(),
            next_node_id: r.get(19).ok(),
            requirement_id: r.get(20).ok(),
        }
    }
}

#[derive(Debug, Clone)]
pub struct TeleportIfSlotNode {
    pub id: i64,
    pub interface_id: Option<i64>,
    pub component_id: Option<i64>,
    pub slot_id: Option<i64>,
    pub click_id: Option<i64>,
    pub dest_min_x: Option<i64>,
    pub dest_max_x: Option<i64>,
    pub dest_min_y: Option<i64>,
    pub dest_max_y: Option<i64>,
    pub dest_plane: Option<i64>,
    pub cost: Option<i64>,
    pub next_node_type: Option<String>,
    pub next_node_id: Option<i64>,
    pub requirement_id: Option<i64>,
}
impl TeleportIfSlotNode {
    fn from_row(r: &Row<'_>) -> Self {
        Self {
            id: r.get(0).unwrap_or_default(),
            interface_id: r.get(1).ok(),
            component_id: r.get(2).ok(),
            slot_id: r.get(3).ok(),
            click_id: r.get(4).ok(),
            dest_min_x: r.get(5).ok(),
            dest_max_x: r.get(6).ok(),
            dest_min_y: r.get(7).ok(),
            dest_max_y: r.get(8).ok(),
            dest_plane: r.get(9).ok(),
            cost: r.get(10).ok(),
            next_node_type: r.get(11).ok(),
            next_node_id: r.get(12).ok(),
            requirement_id: r.get(13).ok(),
        }
    }
}

#[derive(Debug, Clone)]
pub struct TeleportItemNode {
    pub id: i64,
    pub item_id: Option<i64>,
    pub action: Option<String>,
    pub dest_min_x: Option<i64>,
    pub dest_max_x: Option<i64>,
    pub dest_min_y: Option<i64>,
    pub dest_max_y: Option<i64>,
    pub dest_plane: Option<i64>,
    pub next_node_type: Option<String>,
    pub next_node_id: Option<i64>,
    pub cost: Option<i64>,
    pub requirement_id: Option<i64>,
}
impl TeleportItemNode {
    fn from_row(r: &Row<'_>) -> Self {
        Self {
            id: r.get(0).unwrap_or_default(),
            item_id: r.get(1).ok(),
            action: r.get(2).ok(),
            dest_min_x: r.get(3).ok(),
            dest_max_x: r.get(4).ok(),
            dest_min_y: r.get(5).ok(),
            dest_max_y: r.get(6).ok(),
            dest_plane: r.get(7).ok(),
            next_node_type: r.get(8).ok(),
            next_node_id: r.get(9).ok(),
            cost: r.get(10).ok(),
            requirement_id: r.get(11).ok(),
        }
    }
}

#[derive(Debug, Clone)]
pub struct TeleportLodestoneNode {
    pub id: i64,
    pub lodestone: Option<String>,
    pub dest_x: Option<i64>,
    pub dest_y: Option<i64>,
    pub dest_plane: Option<i64>,
    pub cost: Option<i64>,
    pub next_node_type: Option<String>,
    pub next_node_id: Option<i64>,
    pub requirement_id: Option<i64>,
}
impl TeleportLodestoneNode {
    fn from_row(r: &Row<'_>) -> Self {
        Self {
            id: r.get(0).unwrap_or_default(),
            lodestone: r.get(1).ok(),
            dest_x: r.get(2).ok(),
            dest_y: r.get(3).ok(),
            dest_plane: r.get(4).ok(),
            cost: r.get(5).ok(),
            next_node_type: r.get(6).ok(),
            next_node_id: r.get(7).ok(),
            requirement_id: r.get(8).ok(),
        }
    }
}

#[derive(Debug, Clone)]
pub struct TeleportNpcNode {
    pub id: i64,
    pub match_type: Option<String>,
    pub npc_id: Option<i64>,
    pub npc_name: Option<String>,
    pub action: Option<String>,
    pub dest_min_x: Option<i64>,
    pub dest_max_x: Option<i64>,
    pub dest_min_y: Option<i64>,
    pub dest_max_y: Option<i64>,
    pub dest_plane: Option<i64>,
    pub search_radius: Option<i64>,
    pub cost: Option<i64>,
    pub orig_min_x: Option<i64>,
    pub orig_max_x: Option<i64>,
    pub orig_min_y: Option<i64>,
    pub orig_max_y: Option<i64>,
    pub orig_plane: Option<i64>,
    pub next_node_type: Option<String>,
    pub next_node_id: Option<i64>,
    pub requirement_id: Option<i64>,
}
impl TeleportNpcNode {
    fn from_row(r: &Row<'_>) -> Self {
        Self {
            id: r.get(0).unwrap_or_default(),
            match_type: r.get(1).ok(),
            npc_id: r.get(2).ok(),
            npc_name: r.get(3).ok(),
            action: r.get(4).ok(),
            dest_min_x: r.get(5).ok(),
            dest_max_x: r.get(6).ok(),
            dest_min_y: r.get(7).ok(),
            dest_max_y: r.get(8).ok(),
            dest_plane: r.get(9).ok(),
            search_radius: r.get(10).ok(),
            cost: r.get(11).ok(),
            orig_min_x: r.get(12).ok(),
            orig_max_x: r.get(13).ok(),
            orig_min_y: r.get(14).ok(),
            orig_max_y: r.get(15).ok(),
            orig_plane: r.get(16).ok(),
            next_node_type: r.get(17).ok(),
            next_node_id: r.get(18).ok(),
            requirement_id: r.get(19).ok(),
        }
    }
}

#[derive(Debug, Clone)]
pub struct TeleportObjectNode {
    pub id: i64,
    pub match_type: Option<String>,
    pub object_id: Option<i64>,
    pub object_name: Option<String>,
    pub action: Option<String>,
    pub dest_min_x: Option<i64>,
    pub dest_max_x: Option<i64>,
    pub dest_min_y: Option<i64>,
    pub dest_max_y: Option<i64>,
    pub dest_plane: Option<i64>,
    pub orig_min_x: Option<i64>,
    pub orig_max_x: Option<i64>,
    pub orig_min_y: Option<i64>,
    pub orig_max_y: Option<i64>,
    pub orig_plane: Option<i64>,
    pub search_radius: Option<i64>,
    pub cost: Option<i64>,
    pub next_node_type: Option<String>,
    pub next_node_id: Option<i64>,
    pub requirement_id: Option<i64>,
}
impl TeleportObjectNode {
    fn from_row(r: &Row<'_>) -> Self {
        Self {
            id: r.get(0).unwrap_or_default(),
            match_type: r.get(1).ok(),
            object_id: r.get(2).ok(),
            object_name: r.get(3).ok(),
            action: r.get(4).ok(),
            dest_min_x: r.get(5).ok(),
            dest_max_x: r.get(6).ok(),
            dest_min_y: r.get(7).ok(),
            dest_max_y: r.get(8).ok(),
            dest_plane: r.get(9).ok(),
            orig_min_x: r.get(10).ok(),
            orig_max_x: r.get(11).ok(),
            orig_min_y: r.get(12).ok(),
            orig_max_y: r.get(13).ok(),
            orig_plane: r.get(14).ok(),
            search_radius: r.get(15).ok(),
            cost: r.get(16).ok(),
            next_node_type: r.get(17).ok(),
            next_node_id: r.get(18).ok(),
            requirement_id: r.get(19).ok(),
        }
    }
}
