/// Cypher executor — interprets a logical plan by calling the pg_eddy
/// storage layer directly (node_store, edge_store, catalog).
///
/// v0.6.0 scope: executes LabelScan + Expand + Filter + Project +
/// CrossProduct plans and returns SETOF JSONB rows.
use crate::cypher::ast::*;
use crate::cypher::planner::LogicalPlan;
use pgrx::prelude::*;
use std::collections::HashMap;

/// A row of bindings: variable name → Value.
pub type Row = HashMap<String, Value>;

/// Runtime value for a binding.
#[derive(Debug, Clone)]
pub enum Value {
    /// A node: its id plus the full JSONB document.
    Node {
        node_id: i64,
        labels: Vec<String>,
        properties: serde_json::Map<String, serde_json::Value>,
    },
    /// An edge: its id plus the full JSONB document.
    Edge {
        edge_id: i64,
        rel_type: String,
        source: i64,
        target: i64,
        properties: serde_json::Map<String, serde_json::Value>,
    },
    Int(i64),
    Float(f64),
    Str(String),
    Bool(bool),
    Null,
    Json(serde_json::Value),
}

impl Value {
    /// Get a property from a Node or Edge value.
    pub fn get_property(&self, key: &str) -> Value {
        match self {
            Value::Node { properties, .. } => match properties.get(key) {
                Some(v) => json_to_value(v),
                None => Value::Null,
            },
            Value::Edge { properties, .. } => match properties.get(key) {
                Some(v) => json_to_value(v),
                None => Value::Null,
            },
            _ => Value::Null,
        }
    }

    /// Convert to serde_json::Value for output.
    pub fn to_json(&self) -> serde_json::Value {
        match self {
            Value::Node { node_id, labels, properties } => {
                let mut m = serde_json::Map::new();
                m.insert("node_id".into(), (*node_id).into());
                m.insert("labels".into(), serde_json::Value::Array(
                    labels.iter().map(|l| serde_json::Value::String(l.clone())).collect()
                ));
                m.insert("properties".into(), serde_json::Value::Object(properties.clone()));
                serde_json::Value::Object(m)
            }
            Value::Edge { edge_id, rel_type, source, target, properties } => {
                let mut m = serde_json::Map::new();
                m.insert("rel_id".into(), (*edge_id).into());
                m.insert("rel_type".into(), rel_type.clone().into());
                m.insert("source_node_id".into(), (*source).into());
                m.insert("target_node_id".into(), (*target).into());
                m.insert("properties".into(), serde_json::Value::Object(properties.clone()));
                serde_json::Value::Object(m)
            }
            Value::Int(v) => (*v).into(),
            Value::Float(v) => serde_json::Number::from_f64(*v)
                .map(serde_json::Value::Number)
                .unwrap_or(serde_json::Value::Null),
            Value::Str(s) => s.clone().into(),
            Value::Bool(b) => (*b).into(),
            Value::Null => serde_json::Value::Null,
            Value::Json(v) => v.clone(),
        }
    }

    /// Get the node_id (for id() function and isomorphism checks).
    pub fn node_id(&self) -> Option<i64> {
        match self {
            Value::Node { node_id, .. } => Some(*node_id),
            _ => None,
        }
    }

    /// Get the edge_id.
    #[allow(dead_code)]
    pub fn edge_id(&self) -> Option<i64> {
        match self {
            Value::Edge { edge_id, .. } => Some(*edge_id),
            _ => None,
        }
    }
}

fn json_to_value(v: &serde_json::Value) -> Value {
    match v {
        serde_json::Value::Null => Value::Null,
        serde_json::Value::Bool(b) => Value::Bool(*b),
        serde_json::Value::Number(n) => {
            if let Some(i) = n.as_i64() {
                Value::Int(i)
            } else {
                Value::Float(n.as_f64().unwrap_or(0.0))
            }
        }
        serde_json::Value::String(s) => Value::Str(s.clone()),
        _ => Value::Json(v.clone()),
    }
}

/// Execute error.
#[derive(Debug)]
pub struct ExecError {
    pub message: String,
}

impl std::fmt::Display for ExecError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "exec error: {}", self.message)
    }
}

/// Execute a logical plan and return result rows.
///
/// `params` — query parameters ($name → value).
pub fn execute(
    plan: &LogicalPlan,
    params: &HashMap<String, serde_json::Value>,
) -> Result<Vec<Row>, ExecError> {
    match plan {
        LogicalPlan::LabelScan { variable, label, inline_props } => {
            exec_label_scan(variable, label.as_deref(), inline_props, params)
        }
        LogicalPlan::Expand {
            input, src_var, rel_var, dst_var,
            rel_types, direction, rel_props,
            dst_labels, dst_props,
        } => {
            exec_expand(
                input, src_var, rel_var.as_deref(), dst_var,
                rel_types, *direction, rel_props, dst_labels, dst_props,
                params,
            )
        }
        LogicalPlan::CrossProduct { left, right } => {
            exec_cross_product(left, right, params)
        }
        LogicalPlan::Filter { input, predicate } => {
            exec_filter(input, predicate, params)
        }
        LogicalPlan::Project { input, items, distinct, order_by, skip, limit } => {
            exec_project(input, items, *distinct, order_by, skip, limit, params)
        }
    }
}

/// Scan all nodes with an optional label filter.
fn exec_label_scan(
    variable: &str,
    label: Option<&str>,
    inline_props: &[(String, Expr)],
    params: &HashMap<String, serde_json::Value>,
) -> Result<Vec<Row>, ExecError> {
    use crate::catalog::labels::{label_name, label_id_by_name, prop_key_name};
    use crate::storage::prop_store;

    // Get candidate node IDs.
    let node_ids: Vec<i64> = if let Some(lname) = label {
        let lid = label_id_by_name(lname);
        match lid {
            None => return Ok(Vec::new()),
            Some(lid) => {
                Spi::connect(|client| {
                    client
                        .select(
                            "SELECT node_id FROM _pg_eddy.label_index WHERE label_id = $1",
                            None,
                            &[pgrx::datum::DatumWithOid::from(lid)],
                        )
                        .unwrap_or_else(|e| pgrx::error!("cypher label scan SPI: {e}"))
                        .filter_map(|row| row.get::<i64>(1).ok().flatten())
                        .collect()
                })
            }
        }
    } else {
        // Full scan
        unsafe {
            let rel = crate::open_nodes_relation();
            let snapshot = pgrx::pg_sys::GetActiveSnapshot();
            let mut state = crate::storage::node_store::NodeScanState::begin(rel, snapshot);
            let mut ids = Vec::new();
            while let Some(r) = state.next() {
                ids.push(r.node_id);
            }
            pgrx::pg_sys::table_close(rel, pgrx::pg_sys::NoLock as pgrx::pg_sys::LOCKMODE);
            ids
        }
    };

    let mut rows = Vec::new();

    for nid in node_ids {
        let record = unsafe {
            let rel = crate::open_nodes_relation();
            let snapshot = pgrx::pg_sys::GetActiveSnapshot();
            let r = crate::storage::node_store::find_node_by_id(rel, nid, snapshot);
            pgrx::pg_sys::table_close(rel, pgrx::pg_sys::NoLock as pgrx::pg_sys::LOCKMODE);
            r
        };

        if let Some(mut r) = record {
            // Resolve overflow properties.
            if r.overflow_blkno != 0 && r.prop_bytes.is_empty() {
                r.prop_bytes = unsafe {
                    let rel = crate::open_nodes_relation();
                    let bytes = crate::storage::node_store::read_overflow_block(rel, r.overflow_blkno);
                    pgrx::pg_sys::table_close(rel, pgrx::pg_sys::NoLock as pgrx::pg_sys::LOCKMODE);
                    bytes
                };
            }

            let labels: Vec<String> = r.label_ids.iter().map(|id| label_name(*id)).collect();
            let properties = prop_store::decode(&r.prop_bytes, prop_key_name);

            let val = Value::Node {
                node_id: nid,
                labels,
                properties,
            };

            // Check inline property filters.
            if !inline_props.is_empty() {
                let mut matches = true;
                for (key, expr) in inline_props {
                    let prop_val = val.get_property(key);
                    let expected = eval_expr(expr, &HashMap::new(), params)?;
                    if !values_equal(&prop_val, &expected) {
                        matches = false;
                        break;
                    }
                }
                if !matches {
                    continue;
                }
            }

            let mut row = Row::new();
            row.insert(variable.to_string(), val);
            rows.push(row);
        }
    }

    Ok(rows)
}

/// Expand from bound nodes along relationships.
#[allow(clippy::too_many_arguments)]
fn exec_expand(
    input: &LogicalPlan,
    src_var: &str,
    rel_var: Option<&str>,
    dst_var: &str,
    rel_types: &[String],
    direction: RelDirection,
    rel_props: &[(String, Expr)],
    dst_labels: &[String],
    dst_props: &[(String, Expr)],
    params: &HashMap<String, serde_json::Value>,
) -> Result<Vec<Row>, ExecError> {
    use crate::catalog::labels::{ensure_rel_type, label_name, prop_key_name, rel_type_name, label_id_by_name};
    use crate::storage::edge_store::{Direction, adjacency_follow};
    use crate::storage::prop_store;

    let input_rows = execute(input, params)?;
    let mut result = Vec::new();

    let dir = match direction {
        RelDirection::Out => Direction::Out,
        RelDirection::In => Direction::In,
        RelDirection::Both => Direction::Both,
    };

    let type_filter: Option<i32> = if rel_types.len() == 1 {
        Some(ensure_rel_type(&rel_types[0]))
    } else {
        None
    };

    // Pre-resolve dst label IDs for filtering.
    let dst_label_ids: Vec<i32> = dst_labels.iter().filter_map(|l| label_id_by_name(l)).collect();

    for input_row in &input_rows {
        let src_val = input_row.get(src_var).ok_or_else(|| ExecError {
            message: format!("unbound variable: {src_var}"),
        })?;

        let src_node_id = src_val.node_id().ok_or_else(|| ExecError {
            message: format!("{src_var} is not a node"),
        })?;

        // Follow adjacency chains.
        let edges = unsafe {
            let node_rel = crate::open_nodes_relation();
            let edge_rel = crate::open_edges_relation();
            let snapshot = pgrx::pg_sys::GetActiveSnapshot();
            let result = adjacency_follow(
                node_rel, edge_rel, src_node_id, dir, type_filter, snapshot,
            );
            pgrx::pg_sys::table_close(edge_rel, pgrx::pg_sys::NoLock as pgrx::pg_sys::LOCKMODE);
            pgrx::pg_sys::table_close(node_rel, pgrx::pg_sys::NoLock as pgrx::pg_sys::LOCKMODE);
            result
        };

        // Multi-type filter (when more than 1 type specified).
        let type_ids: Vec<i32> = if rel_types.len() > 1 {
            rel_types.iter().map(|t| ensure_rel_type(t)).collect()
        } else {
            Vec::new()
        };

        for edge in &edges {
            // Type filter for multi-type patterns.
            if !type_ids.is_empty() && !type_ids.contains(&edge.rel_type_id) {
                continue;
            }

            let other_id = match direction {
                RelDirection::Out => edge.target_node_id,
                RelDirection::In => edge.source_node_id,
                RelDirection::Both => {
                    if edge.source_node_id == src_node_id {
                        edge.target_node_id
                    } else {
                        edge.source_node_id
                    }
                }
            };

            // Load the destination node.
            let dst_record = unsafe {
                let rel = crate::open_nodes_relation();
                let snapshot = pgrx::pg_sys::GetActiveSnapshot();
                let r = crate::storage::node_store::find_node_by_id(rel, other_id, snapshot);
                pgrx::pg_sys::table_close(rel, pgrx::pg_sys::NoLock as pgrx::pg_sys::LOCKMODE);
                r
            };

            let dst_record = match dst_record {
                Some(r) => r,
                None => continue, // invisible or deleted
            };

            // Resolve overflow.
            let mut dst_r = dst_record;
            if dst_r.overflow_blkno != 0 && dst_r.prop_bytes.is_empty() {
                dst_r.prop_bytes = unsafe {
                    let rel = crate::open_nodes_relation();
                    let bytes = crate::storage::node_store::read_overflow_block(rel, dst_r.overflow_blkno);
                    pgrx::pg_sys::table_close(rel, pgrx::pg_sys::NoLock as pgrx::pg_sys::LOCKMODE);
                    bytes
                };
            }

            // Label filter on destination.
            if !dst_label_ids.is_empty() {
                let has_all = dst_label_ids.iter().all(|lid| dst_r.label_ids.contains(lid));
                if !has_all {
                    continue;
                }
            }

            let dst_labels_resolved: Vec<String> = dst_r.label_ids.iter().map(|id| label_name(*id)).collect();
            let dst_properties = prop_store::decode(&dst_r.prop_bytes, prop_key_name);

            let dst_val = Value::Node {
                node_id: other_id,
                labels: dst_labels_resolved,
                properties: dst_properties,
            };

            // Destination inline property filter.
            if !dst_props.is_empty() {
                let mut matches = true;
                for (key, expr) in dst_props {
                    let prop_val = dst_val.get_property(key);
                    let expected = eval_expr(expr, input_row, params)?;
                    if !values_equal(&prop_val, &expected) {
                        matches = false;
                        break;
                    }
                }
                if !matches {
                    continue;
                }
            }

            // Build edge value.
            let edge_props = prop_store::decode(&edge.prop_bytes, prop_key_name);
            let edge_type_name = rel_type_name(edge.rel_type_id);

            let edge_val = Value::Edge {
                edge_id: edge.edge_id,
                rel_type: edge_type_name,
                source: edge.source_node_id,
                target: edge.target_node_id,
                properties: edge_props,
            };

            // Relationship inline property filter.
            if !rel_props.is_empty() {
                let mut matches = true;
                for (key, expr) in rel_props {
                    let prop_val = edge_val.get_property(key);
                    let expected = eval_expr(expr, input_row, params)?;
                    if !values_equal(&prop_val, &expected) {
                        matches = false;
                        break;
                    }
                }
                if !matches {
                    continue;
                }
            }

            let mut row = input_row.clone();
            row.insert(dst_var.to_string(), dst_val);
            if let Some(rv) = rel_var {
                row.insert(rv.to_string(), edge_val);
            }
            result.push(row);
        }
    }

    Ok(result)
}

fn exec_cross_product(
    left: &LogicalPlan,
    right: &LogicalPlan,
    params: &HashMap<String, serde_json::Value>,
) -> Result<Vec<Row>, ExecError> {
    let left_rows = execute(left, params)?;
    let right_rows = execute(right, params)?;
    let mut result = Vec::new();

    for lr in &left_rows {
        for rr in &right_rows {
            let mut row = lr.clone();
            row.extend(rr.clone());
            result.push(row);
        }
    }

    Ok(result)
}

fn exec_filter(
    input: &LogicalPlan,
    predicate: &Expr,
    params: &HashMap<String, serde_json::Value>,
) -> Result<Vec<Row>, ExecError> {
    let rows = execute(input, params)?;
    let mut result = Vec::new();

    for row in &rows {
        let val = eval_expr(predicate, row, params)?;
        if val.is_truthy() {
            result.push(row.clone());
        }
    }

    Ok(result)
}

fn exec_project(
    input: &LogicalPlan,
    items: &[ReturnItem],
    distinct: bool,
    order_by: &[crate::cypher::ast::OrderItem],
    skip: &Option<Expr>,
    limit: &Option<Expr>,
    params: &HashMap<String, serde_json::Value>,
) -> Result<Vec<Row>, ExecError> {
    let rows = execute(input, params)?;

    // Build (projected_row, input_row) pairs; keep input rows for ORDER BY alias resolution.
    let mut projected: Vec<(Row, Row)> = Vec::with_capacity(rows.len());
    let mut seen: std::collections::HashSet<String> = std::collections::HashSet::new();

    for row in rows {
        let mut out_row = Row::new();
        for (idx, item) in items.iter().enumerate() {
            let val = eval_expr(&item.expr, &row, params)?;
            let key = item.alias.clone().unwrap_or_else(|| {
                expr_default_name(&item.expr, idx)
            });
            out_row.insert(key, val);
        }
        projected.push((out_row, row));
    }

    // ORDER BY — sort before DISTINCT/SKIP/LIMIT
    if !order_by.is_empty() {
        projected.sort_by(|(proj_a, in_a), (proj_b, in_b)| {
            for item in order_by {
                // Try projected (alias) first, then input (pattern variable)
                let av = eval_expr(&item.expr, proj_a, params)
                    .or_else(|_| eval_expr(&item.expr, in_a, params))
                    .unwrap_or(Value::Null);
                let bv = eval_expr(&item.expr, proj_b, params)
                    .or_else(|_| eval_expr(&item.expr, in_b, params))
                    .unwrap_or(Value::Null);
                let cmp = value_ordering(&av, &bv);
                if cmp != std::cmp::Ordering::Equal {
                    return if item.ascending { cmp } else { cmp.reverse() };
                }
            }
            std::cmp::Ordering::Equal
        });
    }

    // DISTINCT
    if distinct {
        projected.retain(|(out_row, _)| {
            let fp = row_fingerprint(out_row);
            seen.insert(fp)
        });
    }

    // SKIP
    if let Some(skip_expr) = skip {
        let n = eval_const_usize(skip_expr, params);
        if n >= projected.len() {
            projected.clear();
        } else {
            projected.drain(0..n);
        }
    }

    // LIMIT
    if let Some(limit_expr) = limit {
        let n = eval_const_usize(limit_expr, params);
        projected.truncate(n);
    }

    Ok(projected.into_iter().map(|(out_row, _)| out_row).collect())
}

/// Evaluate an expression against a row of bindings.
pub fn eval_expr(
    expr: &Expr,
    row: &Row,
    params: &HashMap<String, serde_json::Value>,
) -> Result<Value, ExecError> {
    match expr {
        Expr::Variable(name) => {
            row.get(name).cloned().ok_or_else(|| ExecError {
                message: format!("unbound variable: {name}"),
            })
        }
        Expr::Property(base_expr, key) => {
            let base = eval_expr(base_expr, row, params)?;
            Ok(base.get_property(key))
        }
        Expr::IntLit(v) => Ok(Value::Int(*v)),
        Expr::FloatLit(v) => Ok(Value::Float(*v)),
        Expr::StringLit(s) => Ok(Value::Str(s.clone())),
        Expr::BoolLit(b) => Ok(Value::Bool(*b)),
        Expr::NullLit => Ok(Value::Null),
        Expr::Parameter(name) => {
            match params.get(name) {
                Some(v) => Ok(json_to_value(v)),
                None => Err(ExecError {
                    message: format!("missing parameter: ${name}"),
                }),
            }
        }
        Expr::Compare(left, op, right) => {
            let l = eval_expr(left, row, params)?;
            let r = eval_expr(right, row, params)?;
            Ok(Value::Bool(compare_values(&l, op, &r)))
        }
        Expr::And(left, right) => {
            // openCypher 3-valued logic: null AND false = false; null AND true = null
            let l = eval_expr(left, row, params)?;
            match truthy3(&l) {
                Some(false) => Ok(Value::Bool(false)),
                Some(true) => {
                    let r = eval_expr(right, row, params)?;
                    match truthy3(&r) {
                        Some(b) => Ok(Value::Bool(b)),
                        None => Ok(Value::Null),
                    }
                }
                None => {
                    let r = eval_expr(right, row, params)?;
                    match truthy3(&r) {
                        Some(false) => Ok(Value::Bool(false)),
                        _ => Ok(Value::Null),
                    }
                }
            }
        }
        Expr::Or(left, right) => {
            // openCypher 3-valued logic: null OR true = true; null OR false = null
            let l = eval_expr(left, row, params)?;
            match truthy3(&l) {
                Some(true) => Ok(Value::Bool(true)),
                Some(false) => {
                    let r = eval_expr(right, row, params)?;
                    match truthy3(&r) {
                        Some(b) => Ok(Value::Bool(b)),
                        None => Ok(Value::Null),
                    }
                }
                None => {
                    let r = eval_expr(right, row, params)?;
                    match truthy3(&r) {
                        Some(true) => Ok(Value::Bool(true)),
                        _ => Ok(Value::Null),
                    }
                }
            }
        }
        Expr::Not(inner) => {
            let v = eval_expr(inner, row, params)?;
            match truthy3(&v) {
                Some(b) => Ok(Value::Bool(!b)),
                None => Ok(Value::Null),
            }
        }
        Expr::IsNull(inner) => {
            let v = eval_expr(inner, row, params)?;
            Ok(Value::Bool(matches!(v, Value::Null)))
        }
        Expr::IsNotNull(inner) => {
            let v = eval_expr(inner, row, params)?;
            Ok(Value::Bool(!matches!(v, Value::Null)))
        }
        Expr::Arith(left, op, right) => {
            let l = eval_expr(left, row, params)?;
            let r = eval_expr(right, row, params)?;
            eval_arith(&l, op, &r)
        }
        Expr::Neg(inner) => {
            let v = eval_expr(inner, row, params)?;
            match v {
                Value::Int(i) => Ok(Value::Int(-i)),
                Value::Float(f) => Ok(Value::Float(-f)),
                _ => Ok(Value::Null),
            }
        }
        Expr::FunctionCall(name, args) => {
            eval_function(name, args, row, params)
        }
        Expr::Star => {
            // Star in projection: return all bound variables as a JSON object.
            let mut m = serde_json::Map::new();
            for (k, v) in row {
                m.insert(k.clone(), v.to_json());
            }
            Ok(Value::Json(serde_json::Value::Object(m)))
        }
        Expr::List(exprs) => {
            let vals: Vec<serde_json::Value> = exprs
                .iter()
                .map(|e| eval_expr(e, row, params).map(|v| v.to_json()))
                .collect::<Result<_, _>>()?;
            Ok(Value::Json(serde_json::Value::Array(vals)))
        }
        Expr::InList(left, right_list) => {
            let val = eval_expr(left, row, params)?;
            if matches!(val, Value::Null) {
                return Ok(Value::Null);
            }
            let list = eval_expr(right_list, row, params)?;
            let arr = match &list {
                Value::Json(serde_json::Value::Array(a)) => a.clone(),
                Value::Null => return Ok(Value::Null),
                _ => return Ok(Value::Bool(false)),
            };
            let mut has_null = false;
            for item in &arr {
                let item_val = json_to_value(item);
                if matches!(item_val, Value::Null) {
                    has_null = true;
                } else if values_equal(&val, &item_val) {
                    return Ok(Value::Bool(true));
                }
            }
            if has_null { Ok(Value::Null) } else { Ok(Value::Bool(false)) }
        }
        Expr::StartsWith(left, right) => {
            let l = eval_expr(left, row, params)?;
            let r = eval_expr(right, row, params)?;
            match (&l, &r) {
                (Value::Null, _) | (_, Value::Null) => Ok(Value::Null),
                (Value::Str(s), Value::Str(prefix)) => Ok(Value::Bool(s.starts_with(prefix.as_str()))),
                _ => Ok(Value::Null),
            }
        }
        Expr::EndsWith(left, right) => {
            let l = eval_expr(left, row, params)?;
            let r = eval_expr(right, row, params)?;
            match (&l, &r) {
                (Value::Null, _) | (_, Value::Null) => Ok(Value::Null),
                (Value::Str(s), Value::Str(suffix)) => Ok(Value::Bool(s.ends_with(suffix.as_str()))),
                _ => Ok(Value::Null),
            }
        }
        Expr::Contains(left, right) => {
            let l = eval_expr(left, row, params)?;
            let r = eval_expr(right, row, params)?;
            match (&l, &r) {
                (Value::Null, _) | (_, Value::Null) => Ok(Value::Null),
                (Value::Str(s), Value::Str(sub)) => Ok(Value::Bool(s.contains(sub.as_str()))),
                _ => Ok(Value::Null),
            }
        }
        Expr::Regex(left, right) => {
            let l = eval_expr(left, row, params)?;
            let r = eval_expr(right, row, params)?;
            match (&l, &r) {
                (Value::Null, _) | (_, Value::Null) => Ok(Value::Null),
                (Value::Str(s), Value::Str(pattern)) => {
                    // Use PostgreSQL's regexp_like via SPI for full POSIX regex support.
                    let result = Spi::get_one_with_args::<bool>(
                        "SELECT $1 ~ $2",
                        &[
                            pgrx::datum::DatumWithOid::from(s.as_str()),
                            pgrx::datum::DatumWithOid::from(pattern.as_str()),
                        ],
                    ).unwrap_or(Some(false)).unwrap_or(false);
                    Ok(Value::Bool(result))
                }
                _ => Ok(Value::Null),
            }
        }
    }
}

impl Value {
    fn is_truthy(&self) -> bool {
        match self {
            Value::Bool(b) => *b,
            Value::Null => false,
            Value::Int(0) => false,
            Value::Str(s) => !s.is_empty(),
            _ => true,
        }
    }
}

/// Three-valued logic for openCypher: returns None for NULL.
fn truthy3(v: &Value) -> Option<bool> {
    match v {
        Value::Null => None,
        other => Some(other.is_truthy()),
    }
}

fn compare_values(left: &Value, op: &CmpOp, right: &Value) -> bool {
    // NULL comparisons always return false (except IS NULL/IS NOT NULL).
    if matches!(left, Value::Null) || matches!(right, Value::Null) {
        return false;
    }

    match (left, right) {
        (Value::Int(a), Value::Int(b)) => int_cmp(*a, op, *b),
        (Value::Float(a), Value::Float(b)) => float_cmp(*a, op, *b),
        (Value::Int(a), Value::Float(b)) => float_cmp(*a as f64, op, *b),
        (Value::Float(a), Value::Int(b)) => float_cmp(*a, op, *b as f64),
        (Value::Str(a), Value::Str(b)) => str_cmp(a, op, b),
        (Value::Bool(a), Value::Bool(b)) => match op {
            CmpOp::Eq => a == b,
            CmpOp::Neq => a != b,
            _ => false,
        },
        (Value::Node { node_id: a, .. }, Value::Node { node_id: b, .. }) => int_cmp(*a, op, *b),
        _ => false,
    }
}

fn int_cmp(a: i64, op: &CmpOp, b: i64) -> bool {
    match op {
        CmpOp::Eq => a == b,
        CmpOp::Neq => a != b,
        CmpOp::Lt => a < b,
        CmpOp::Gt => a > b,
        CmpOp::Le => a <= b,
        CmpOp::Ge => a >= b,
    }
}

fn float_cmp(a: f64, op: &CmpOp, b: f64) -> bool {
    match op {
        CmpOp::Eq => (a - b).abs() < f64::EPSILON,
        CmpOp::Neq => (a - b).abs() >= f64::EPSILON,
        CmpOp::Lt => a < b,
        CmpOp::Gt => a > b,
        CmpOp::Le => a <= b,
        CmpOp::Ge => a >= b,
    }
}

fn str_cmp(a: &str, op: &CmpOp, b: &str) -> bool {
    match op {
        CmpOp::Eq => a == b,
        CmpOp::Neq => a != b,
        CmpOp::Lt => a < b,
        CmpOp::Gt => a > b,
        CmpOp::Le => a <= b,
        CmpOp::Ge => a >= b,
    }
}

fn eval_arith(left: &Value, op: &ArithOp, right: &Value) -> Result<Value, ExecError> {
    match (left, right) {
        (Value::Int(a), Value::Int(b)) => match op {
            ArithOp::Add => Ok(Value::Int(a + b)),
            ArithOp::Sub => Ok(Value::Int(a - b)),
            ArithOp::Mul => Ok(Value::Int(a * b)),
            ArithOp::Div => {
                if *b == 0 { Ok(Value::Null) } else { Ok(Value::Int(a / b)) }
            }
            ArithOp::Mod => {
                if *b == 0 { Ok(Value::Null) } else { Ok(Value::Int(a % b)) }
            }
        },
        (Value::Float(a), Value::Float(b)) => float_arith(*a, op, *b),
        (Value::Int(a), Value::Float(b)) => float_arith(*a as f64, op, *b),
        (Value::Float(a), Value::Int(b)) => float_arith(*a, op, *b as f64),
        (Value::Str(a), Value::Str(b)) if matches!(op, ArithOp::Add) => {
            Ok(Value::Str(format!("{a}{b}")))
        }
        _ => Ok(Value::Null),
    }
}

fn float_arith(a: f64, op: &ArithOp, b: f64) -> Result<Value, ExecError> {
    match op {
        ArithOp::Add => Ok(Value::Float(a + b)),
        ArithOp::Sub => Ok(Value::Float(a - b)),
        ArithOp::Mul => Ok(Value::Float(a * b)),
        ArithOp::Div => {
            if b == 0.0 { Ok(Value::Null) } else { Ok(Value::Float(a / b)) }
        }
        ArithOp::Mod => {
            if b == 0.0 { Ok(Value::Null) } else { Ok(Value::Float(a % b)) }
        }
    }
}

fn eval_function(
    name: &str,
    args: &[Expr],
    row: &Row,
    params: &HashMap<String, serde_json::Value>,
) -> Result<Value, ExecError> {
    let lower = name.to_ascii_lowercase();
    match lower.as_str() {
        "id" => {
            if args.len() != 1 {
                return Err(ExecError { message: "id() takes exactly 1 argument".into() });
            }
            let val = eval_expr(&args[0], row, params)?;
            match val {
                Value::Node { node_id, .. } => Ok(Value::Int(node_id)),
                Value::Edge { edge_id, .. } => Ok(Value::Int(edge_id)),
                _ => Ok(Value::Null),
            }
        }
        "labels" => {
            if args.len() != 1 {
                return Err(ExecError { message: "labels() takes exactly 1 argument".into() });
            }
            let val = eval_expr(&args[0], row, params)?;
            match val {
                Value::Node { labels, .. } => {
                    let arr: Vec<serde_json::Value> = labels.into_iter().map(|l| l.into()).collect();
                    Ok(Value::Json(serde_json::Value::Array(arr)))
                }
                _ => Ok(Value::Null),
            }
        }
        "type" => {
            if args.len() != 1 {
                return Err(ExecError { message: "type() takes exactly 1 argument".into() });
            }
            let val = eval_expr(&args[0], row, params)?;
            match val {
                Value::Edge { rel_type, .. } => Ok(Value::Str(rel_type)),
                _ => Ok(Value::Null),
            }
        }
        "properties" => {
            if args.len() != 1 {
                return Err(ExecError { message: "properties() takes exactly 1 argument".into() });
            }
            let val = eval_expr(&args[0], row, params)?;
            match val {
                Value::Node { properties, .. } | Value::Edge { properties, .. } => {
                    Ok(Value::Json(serde_json::Value::Object(properties)))
                }
                _ => Ok(Value::Null),
            }
        }
        "keys" => {
            if args.len() != 1 {
                return Err(ExecError { message: "keys() takes exactly 1 argument".into() });
            }
            let val = eval_expr(&args[0], row, params)?;
            match val {
                Value::Node { properties, .. } | Value::Edge { properties, .. } => {
                    let keys: Vec<serde_json::Value> = properties.keys().map(|k| k.clone().into()).collect();
                    Ok(Value::Json(serde_json::Value::Array(keys)))
                }
                _ => Ok(Value::Null),
            }
        }
        "tostring" => {
            if args.len() != 1 {
                return Err(ExecError { message: "toString() takes exactly 1 argument".into() });
            }
            let val = eval_expr(&args[0], row, params)?;
            match val {
                Value::Int(i) => Ok(Value::Str(i.to_string())),
                Value::Float(f) => Ok(Value::Str(f.to_string())),
                Value::Str(s) => Ok(Value::Str(s)),
                Value::Bool(b) => Ok(Value::Str(b.to_string())),
                Value::Null => Ok(Value::Null),
                _ => Ok(Value::Null),
            }
        }
        "tointeger" => {
            if args.len() != 1 {
                return Err(ExecError { message: "toInteger() takes exactly 1 argument".into() });
            }
            let val = eval_expr(&args[0], row, params)?;
            match val {
                Value::Int(i) => Ok(Value::Int(i)),
                Value::Float(f) => Ok(Value::Int(f as i64)),
                Value::Str(s) => match s.parse::<i64>() {
                    Ok(i) => Ok(Value::Int(i)),
                    Err(_) => Ok(Value::Null),
                },
                Value::Null => Ok(Value::Null),
                _ => Ok(Value::Null),
            }
        }
        "tofloat" => {
            if args.len() != 1 {
                return Err(ExecError { message: "toFloat() takes exactly 1 argument".into() });
            }
            let val = eval_expr(&args[0], row, params)?;
            match val {
                Value::Int(i) => Ok(Value::Float(i as f64)),
                Value::Float(f) => Ok(Value::Float(f)),
                Value::Str(s) => match s.parse::<f64>() {
                    Ok(f) => Ok(Value::Float(f)),
                    Err(_) => Ok(Value::Null),
                },
                Value::Null => Ok(Value::Null),
                _ => Ok(Value::Null),
            }
        }
        "coalesce" => {
            for arg in args {
                let val = eval_expr(arg, row, params)?;
                if !matches!(val, Value::Null) {
                    return Ok(val);
                }
            }
            Ok(Value::Null)
        }
        // --- Type conversion ---
        "toboolean" => {
            if args.len() != 1 {
                return Err(ExecError { message: "toBoolean() takes exactly 1 argument".into() });
            }
            let val = eval_expr(&args[0], row, params)?;
            match val {
                Value::Bool(b) => Ok(Value::Bool(b)),
                Value::Str(s) => match s.to_ascii_lowercase().as_str() {
                    "true" => Ok(Value::Bool(true)),
                    "false" => Ok(Value::Bool(false)),
                    _ => Ok(Value::Null),
                },
                Value::Null => Ok(Value::Null),
                _ => Ok(Value::Null),
            }
        }
        // --- Size / length ---
        "size" => {
            if args.len() != 1 {
                return Err(ExecError { message: "size() takes exactly 1 argument".into() });
            }
            let val = eval_expr(&args[0], row, params)?;
            match val {
                Value::Str(s) => Ok(Value::Int(s.chars().count() as i64)),
                Value::Json(serde_json::Value::Array(a)) => Ok(Value::Int(a.len() as i64)),
                Value::Null => Ok(Value::Null),
                _ => Ok(Value::Null),
            }
        }
        "length" => {
            if args.len() != 1 {
                return Err(ExecError { message: "length() takes exactly 1 argument".into() });
            }
            let val = eval_expr(&args[0], row, params)?;
            match val {
                Value::Str(s) => Ok(Value::Int(s.chars().count() as i64)),
                Value::Json(serde_json::Value::Array(a)) => Ok(Value::Int(a.len() as i64)),
                Value::Null => Ok(Value::Null),
                _ => Ok(Value::Null),
            }
        }
        // --- List functions ---
        "head" => {
            if args.len() != 1 {
                return Err(ExecError { message: "head() takes exactly 1 argument".into() });
            }
            let val = eval_expr(&args[0], row, params)?;
            match val {
                Value::Json(serde_json::Value::Array(mut a)) => {
                    if a.is_empty() { Ok(Value::Null) } else { Ok(json_to_value(&a.remove(0))) }
                }
                Value::Null => Ok(Value::Null),
                _ => Ok(Value::Null),
            }
        }
        "tail" => {
            if args.len() != 1 {
                return Err(ExecError { message: "tail() takes exactly 1 argument".into() });
            }
            let val = eval_expr(&args[0], row, params)?;
            match val {
                Value::Json(serde_json::Value::Array(a)) => {
                    if a.is_empty() {
                        Ok(Value::Json(serde_json::Value::Array(vec![])))
                    } else {
                        Ok(Value::Json(serde_json::Value::Array(a[1..].to_vec())))
                    }
                }
                Value::Null => Ok(Value::Null),
                _ => Ok(Value::Null),
            }
        }
        "last" => {
            if args.len() != 1 {
                return Err(ExecError { message: "last() takes exactly 1 argument".into() });
            }
            let val = eval_expr(&args[0], row, params)?;
            match val {
                Value::Json(serde_json::Value::Array(a)) => {
                    match a.last() {
                        Some(v) => Ok(json_to_value(v)),
                        None => Ok(Value::Null),
                    }
                }
                Value::Null => Ok(Value::Null),
                _ => Ok(Value::Null),
            }
        }
        "reverse" => {
            if args.len() != 1 {
                return Err(ExecError { message: "reverse() takes exactly 1 argument".into() });
            }
            let val = eval_expr(&args[0], row, params)?;
            match val {
                Value::Str(s) => Ok(Value::Str(s.chars().rev().collect())),
                Value::Json(serde_json::Value::Array(mut a)) => {
                    a.reverse();
                    Ok(Value::Json(serde_json::Value::Array(a)))
                }
                Value::Null => Ok(Value::Null),
                _ => Ok(Value::Null),
            }
        }
        "range" => {
            let (start, end, step) = match args.len() {
                2 => {
                    let s = eval_expr(&args[0], row, params)?;
                    let e = eval_expr(&args[1], row, params)?;
                    (s, e, Value::Int(1))
                }
                3 => {
                    let s = eval_expr(&args[0], row, params)?;
                    let e = eval_expr(&args[1], row, params)?;
                    let st = eval_expr(&args[2], row, params)?;
                    (s, e, st)
                }
                _ => return Err(ExecError { message: "range() takes 2 or 3 arguments".into() }),
            };
            let (s, e, st) = match (start, end, step) {
                (Value::Int(s), Value::Int(e), Value::Int(st)) => (s, e, st),
                _ => return Ok(Value::Null),
            };
            if st == 0 {
                return Err(ExecError { message: "range() step cannot be 0".into() });
            }
            let mut result = Vec::new();
            let mut cur = s;
            while (st > 0 && cur <= e) || (st < 0 && cur >= e) {
                result.push(serde_json::Value::Number(cur.into()));
                cur += st;
            }
            Ok(Value::Json(serde_json::Value::Array(result)))
        }
        // --- String functions ---
        "trim" => {
            if args.len() != 1 { return Err(ExecError { message: "trim() takes exactly 1 argument".into() }); }
            let v = eval_expr(&args[0], row, params)?;
            match v {
                Value::Str(s) => Ok(Value::Str(s.trim().to_string())),
                Value::Null => Ok(Value::Null),
                _ => Ok(Value::Null),
            }
        }
        "ltrim" => {
            if args.len() != 1 { return Err(ExecError { message: "ltrim() takes exactly 1 argument".into() }); }
            let v = eval_expr(&args[0], row, params)?;
            match v {
                Value::Str(s) => Ok(Value::Str(s.trim_start().to_string())),
                Value::Null => Ok(Value::Null),
                _ => Ok(Value::Null),
            }
        }
        "rtrim" => {
            if args.len() != 1 { return Err(ExecError { message: "rtrim() takes exactly 1 argument".into() }); }
            let v = eval_expr(&args[0], row, params)?;
            match v {
                Value::Str(s) => Ok(Value::Str(s.trim_end().to_string())),
                Value::Null => Ok(Value::Null),
                _ => Ok(Value::Null),
            }
        }
        "upper" | "toupper" => {
            if args.len() != 1 { return Err(ExecError { message: "upper() takes exactly 1 argument".into() }); }
            let v = eval_expr(&args[0], row, params)?;
            match v {
                Value::Str(s) => Ok(Value::Str(s.to_uppercase())),
                Value::Null => Ok(Value::Null),
                _ => Ok(Value::Null),
            }
        }
        "lower" | "tolower" => {
            if args.len() != 1 { return Err(ExecError { message: "lower() takes exactly 1 argument".into() }); }
            let v = eval_expr(&args[0], row, params)?;
            match v {
                Value::Str(s) => Ok(Value::Str(s.to_lowercase())),
                Value::Null => Ok(Value::Null),
                _ => Ok(Value::Null),
            }
        }
        "substring" => {
            if args.len() < 2 { return Err(ExecError { message: "substring() takes 2 or 3 arguments".into() }); }
            let s = eval_expr(&args[0], row, params)?;
            let start = eval_expr(&args[1], row, params)?;
            let len_val = if args.len() >= 3 { Some(eval_expr(&args[2], row, params)?) } else { None };
            match (s, start) {
                (Value::Null, _) | (_, Value::Null) => Ok(Value::Null),
                (Value::Str(s), Value::Int(start)) => {
                    let chars: Vec<char> = s.chars().collect();
                    let start = start.max(0) as usize;
                    let start = start.min(chars.len());
                    let result: String = match len_val {
                        Some(Value::Int(len)) => chars[start..].iter().take(len.max(0) as usize).collect(),
                        Some(Value::Null) => return Ok(Value::Null),
                        None => chars[start..].iter().collect(),
                        _ => chars[start..].iter().collect(),
                    };
                    Ok(Value::Str(result))
                }
                _ => Ok(Value::Null),
            }
        }
        "replace" => {
            if args.len() != 3 { return Err(ExecError { message: "replace() takes exactly 3 arguments".into() }); }
            let original = eval_expr(&args[0], row, params)?;
            let search = eval_expr(&args[1], row, params)?;
            let replacement = eval_expr(&args[2], row, params)?;
            match (original, search, replacement) {
                (Value::Null, _, _) | (_, Value::Null, _) | (_, _, Value::Null) => Ok(Value::Null),
                (Value::Str(s), Value::Str(from), Value::Str(to)) => {
                    Ok(Value::Str(s.replace(&from as &str, &to as &str)))
                }
                _ => Ok(Value::Null),
            }
        }
        "split" => {
            if args.len() != 2 { return Err(ExecError { message: "split() takes exactly 2 arguments".into() }); }
            let s = eval_expr(&args[0], row, params)?;
            let delim = eval_expr(&args[1], row, params)?;
            match (s, delim) {
                (Value::Null, _) | (_, Value::Null) => Ok(Value::Null),
                (Value::Str(s), Value::Str(d)) => {
                    let parts: Vec<serde_json::Value> = s.split(&d as &str)
                        .map(|p| serde_json::Value::String(p.to_string()))
                        .collect();
                    Ok(Value::Json(serde_json::Value::Array(parts)))
                }
                _ => Ok(Value::Null),
            }
        }
        // --- Math functions ---
        "abs" => math1(args, row, params, |x| x.abs(), |x: f64| x.abs()),
        "ceil" | "ceiling" => math1(args, row, params, |x| x, |x: f64| x.ceil()),
        "floor" => math1(args, row, params, |x| x, |x: f64| x.floor()),
        "round" => math1(args, row, params, |x| x, |x: f64| x.round()),
        "sign" => math1(args, row, params, |x: i64| x.signum(), |x: f64| x.signum()),
        "sqrt" => {
            if args.len() != 1 { return Err(ExecError { message: "sqrt() takes exactly 1 argument".into() }); }
            let v = eval_expr(&args[0], row, params)?;
            match v {
                Value::Int(i) => Ok(Value::Float((i as f64).sqrt())),
                Value::Float(f) => Ok(Value::Float(f.sqrt())),
                Value::Null => Ok(Value::Null),
                _ => Ok(Value::Null),
            }
        }
        "log" => math1f(args, row, params, |x: f64| x.ln()),
        "log10" => math1f(args, row, params, |x: f64| x.log10()),
        "exp" => math1f(args, row, params, |x: f64| x.exp()),
        "sin" => math1f(args, row, params, |x: f64| x.sin()),
        "cos" => math1f(args, row, params, |x: f64| x.cos()),
        "tan" => math1f(args, row, params, |x: f64| x.tan()),
        "asin" => math1f(args, row, params, |x: f64| x.asin()),
        "acos" => math1f(args, row, params, |x: f64| x.acos()),
        "atan" => math1f(args, row, params, |x: f64| x.atan()),
        "atan2" => {
            if args.len() != 2 { return Err(ExecError { message: "atan2() takes exactly 2 arguments".into() }); }
            let y = to_f64(&eval_expr(&args[0], row, params)?);
            let x = to_f64(&eval_expr(&args[1], row, params)?);
            match (y, x) {
                (Some(y), Some(x)) => Ok(Value::Float(y.atan2(x))),
                _ => Ok(Value::Null),
            }
        }
        "pi" => {
            if !args.is_empty() { return Err(ExecError { message: "pi() takes no arguments".into() }); }
            Ok(Value::Float(std::f64::consts::PI))
        }
        "e" => {
            if !args.is_empty() { return Err(ExecError { message: "e() takes no arguments".into() }); }
            Ok(Value::Float(std::f64::consts::E))
        }
        _ => Err(ExecError {
            message: format!("unknown function: {name}()"),
        }),
    }
}

fn to_f64(v: &Value) -> Option<f64> {
    match v {
        Value::Int(i) => Some(*i as f64),
        Value::Float(f) => Some(*f),
        _ => None,
    }
}

/// Helper for math functions that accept int or float and return int for int input.
fn math1(
    args: &[Expr],
    row: &Row,
    params: &HashMap<String, serde_json::Value>,
    int_fn: impl Fn(i64) -> i64,
    float_fn: impl Fn(f64) -> f64,
) -> Result<Value, ExecError> {
    if args.len() != 1 {
        return Err(ExecError { message: "math function takes exactly 1 argument".into() });
    }
    let v = eval_expr(&args[0], row, params)?;
    match v {
        Value::Int(i) => Ok(Value::Int(int_fn(i))),
        Value::Float(f) => Ok(Value::Float(float_fn(f))),
        Value::Null => Ok(Value::Null),
        _ => Ok(Value::Null),
    }
}

/// Helper for math functions that always return float.
fn math1f(
    args: &[Expr],
    row: &Row,
    params: &HashMap<String, serde_json::Value>,
    float_fn: impl Fn(f64) -> f64,
) -> Result<Value, ExecError> {
    if args.len() != 1 {
        return Err(ExecError { message: "math function takes exactly 1 argument".into() });
    }
    let v = eval_expr(&args[0], row, params)?;
    match v {
        Value::Int(i) => Ok(Value::Float(float_fn(i as f64))),
        Value::Float(f) => Ok(Value::Float(float_fn(f))),
        Value::Null => Ok(Value::Null),
        _ => Ok(Value::Null),
    }
}

fn values_equal(a: &Value, b: &Value) -> bool {
    compare_values(a, &CmpOp::Eq, b)
}

fn expr_default_name(expr: &Expr, idx: usize) -> String {
    match expr {
        Expr::Variable(name) => name.clone(),
        Expr::Property(base, prop) => {
            format!("{}.{prop}", expr_default_name(base, idx))
        }
        Expr::FunctionCall(name, _) => name.clone(),
        Expr::Star => "*".into(),
        _ => format!("_col{idx}"),
    }
}

fn row_fingerprint(row: &Row) -> String {
    let mut parts: Vec<String> = row.iter().map(|(k, v)| {
        format!("{k}={}", serde_json::to_string(&v.to_json()).unwrap_or_default())
    }).collect();
    parts.sort();
    parts.join("|")
}

/// Compare two Values for ordering (used by ORDER BY).
/// NULL sorts last (greater than any non-null).
fn value_ordering(a: &Value, b: &Value) -> std::cmp::Ordering {
    match (a, b) {
        (Value::Null, Value::Null) => std::cmp::Ordering::Equal,
        (Value::Null, _) => std::cmp::Ordering::Greater,
        (_, Value::Null) => std::cmp::Ordering::Less,
        (Value::Int(x), Value::Int(y)) => x.cmp(y),
        (Value::Float(x), Value::Float(y)) => x.partial_cmp(y).unwrap_or(std::cmp::Ordering::Equal),
        (Value::Int(x), Value::Float(y)) => (*x as f64).partial_cmp(y).unwrap_or(std::cmp::Ordering::Equal),
        (Value::Float(x), Value::Int(y)) => x.partial_cmp(&(*y as f64)).unwrap_or(std::cmp::Ordering::Equal),
        (Value::Str(x), Value::Str(y)) => x.cmp(y),
        (Value::Bool(x), Value::Bool(y)) => x.cmp(y),
        _ => std::cmp::Ordering::Equal,
    }
}

/// Evaluate a SKIP/LIMIT expression to a usize (params available, no row context needed).
fn eval_const_usize(expr: &Expr, params: &HashMap<String, serde_json::Value>) -> usize {
    let dummy = Row::new();
    match eval_expr(expr, &dummy, params).unwrap_or(Value::Null) {
        Value::Int(n) => n.max(0) as usize,
        _ => 0,
    }
}

/// Convert result rows to JSONB output format.
pub fn rows_to_jsonb(rows: Vec<Row>) -> Vec<pgrx::JsonB> {
    rows.into_iter().map(|row| {
        if row.len() == 1 {
            let (_, val) = row.into_iter().next().unwrap();
            pgrx::JsonB(val.to_json())
        } else {
            let mut m = serde_json::Map::new();
            for (k, v) in &row {
                m.insert(k.clone(), v.to_json());
            }
            pgrx::JsonB(serde_json::Value::Object(m))
        }
    }).collect()
}
