use crate::db::{AbstractTeleportEdge, ClusterEntrance, ClusterIntraConnection, ClusterInterConnection, TeleportRequirement};
use crate::models::Tile;
use crate::requirements::RequirementEvaluator;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub struct NodeId(pub usize);

#[derive(Debug, Clone)]
pub enum NodeKind {
    Entrance { entrance_id: i64, cluster_id: i64, x: i32, y: i32, plane: i32 },
    VirtualStart(Tile),
    VirtualEnd(Tile),
}

#[derive(Debug, Clone)]
pub struct Node {
    pub id: NodeId,
    pub kind: NodeKind,
}

#[derive(Debug, Clone)]
pub enum EdgeKind {
    Intra { path_blob: Option<Vec<u8>> },
    Inter,
    Teleport { edge_id: i64, requirement_id: Option<i64> },
}

#[derive(Debug, Clone)]
pub struct Edge {
    pub from: NodeId,
    pub to: NodeId,
    pub cost: i64,
    pub kind: EdgeKind,
}

#[derive(Debug, Default, Clone)]
pub struct Graph {
    pub nodes: Vec<Node>,
    pub edges: Vec<Edge>,
}

#[derive(Debug, Clone)]
pub struct GraphInputs<'a> {
    pub entrances: &'a [ClusterEntrance],
    pub intra: &'a [ClusterIntraConnection],
    pub inter: &'a [ClusterInterConnection],
    pub teleports: &'a [AbstractTeleportEdge],
    pub teleport_requirements: &'a [TeleportRequirement],
}

pub struct BuildOptions {
    pub start: Tile,
    pub end: Tile,
}

impl Default for BuildOptions {
    fn default() -> Self {
        Self { start: Tile { x: 0, y: 0, plane: 0 }, end: Tile { x: 0, y: 0, plane: 0 } }
    }
}

pub fn build_graph(inputs: &GraphInputs<'_>, evaluator: &RequirementEvaluator, options: &BuildOptions) -> Graph {
    // Deterministic: sort entrances by entrance_id
    let mut entrances: Vec<ClusterEntrance> = inputs.entrances.to_vec();
    entrances.sort_by_key(|e| e.entrance_id);

    let mut nodes: Vec<Node> = Vec::with_capacity(entrances.len() + 2);
    let mut entrance_index = std::collections::BTreeMap::new();

    for (idx, e) in entrances.iter().enumerate() {
        let id = NodeId(idx);
        nodes.push(Node {
            id,
            kind: NodeKind::Entrance {
                entrance_id: e.entrance_id,
                cluster_id: e.cluster_id,
                x: e.x as i32,
                y: e.y as i32,
                plane: e.plane as i32,
            },
        });
        entrance_index.insert(e.entrance_id, id);
    }

    // Add virtual start/end nodes in deterministic order
    let start_id = NodeId(nodes.len());
    nodes.push(Node { id: start_id, kind: NodeKind::VirtualStart(options.start) });
    let end_id = NodeId(nodes.len());
    nodes.push(Node { id: end_id, kind: NodeKind::VirtualEnd(options.end) });

    let mut edges: Vec<Edge> = Vec::new();

    // Intra connections: sort by (from,to)
    let mut intra: Vec<ClusterIntraConnection> = inputs.intra.to_vec();
    intra.sort_by_key(|c| (c.entrance_from, c.entrance_to));
    for c in intra.into_iter() {
        if let (Some(&from), Some(&to)) = (entrance_index.get(&c.entrance_from), entrance_index.get(&c.entrance_to)) {
            edges.push(Edge {
                from,
                to,
                cost: c.cost,
                kind: EdgeKind::Intra { path_blob: c.path_blob.clone() },
            });
        }
    }

    // Inter connections: sort by (from,to)
    let mut inter: Vec<ClusterInterConnection> = inputs.inter.to_vec();
    inter.sort_by_key(|c| (c.entrance_from, c.entrance_to));
    for c in inter.into_iter() {
        if let (Some(&from), Some(&to)) = (entrance_index.get(&c.entrance_from), entrance_index.get(&c.entrance_to)) {
            edges.push(Edge {
                from,
                to,
                cost: c.cost,
                kind: EdgeKind::Inter,
            });
        }
    }

    // Teleports: filter by requirements and valid endpoints; deterministic sort by (src_entrance,dst_entrance,edge_id)
    let mut teleports: Vec<AbstractTeleportEdge> = inputs.teleports.to_vec();
    teleports.sort_by_key(|t| (t.src_entrance.unwrap_or(i64::MIN), t.dst_entrance.unwrap_or(i64::MIN), t.edge_id));

    // Build a small index of requirements by id to avoid searching the slice each time
    let mut req_index = std::collections::HashMap::new();
    for r in inputs.teleport_requirements.iter() {
        req_index.insert(r.id, r.clone());
    }

    for t in teleports.into_iter() {
        let (Some(src_eid), Some(dst_eid)) = (t.src_entrance, t.dst_entrance) else { continue };
        let (Some(&from), Some(&to)) = (entrance_index.get(&src_eid), entrance_index.get(&dst_eid)) else { continue };
        let allowed = match t.requirement_id.and_then(|id| req_index.get(&id)) {
            None => true,
            Some(req) => evaluator.satisfies_all(std::slice::from_ref(req)),
        };
        if !allowed { continue; }
        edges.push(Edge { from, to, cost: t.cost, kind: EdgeKind::Teleport { edge_id: t.edge_id, requirement_id: t.requirement_id } });
    }

    Graph { nodes, edges }
}
