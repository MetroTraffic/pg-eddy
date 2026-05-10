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
        LogicalPlan::SingleRow => {
            Ok(vec![HashMap::new()])
        }
        LogicalPlan::LabelScan { variable, label, inline_props, optional } => {
            exec_label_scan(variable, label.as_deref(), inline_props, *optional, params)
        }
        LogicalPlan::Expand {
            input, src_var, rel_var, dst_var,
            rel_types, direction, rel_props,
            dst_labels, dst_props, optional,
        } => {
            exec_expand(
                input, src_var, rel_var.as_deref(), dst_var,
                rel_types, *direction, rel_props, dst_labels, dst_props,
                *optional, params,
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
        LogicalPlan::Unwind { input, expr, alias } => {
            exec_unwind(input, expr, alias, params)
        }
    }
}

/// Scan all nodes with an optional label filter.
fn exec_label_scan(
    variable: &str,
    label: Option<&str>,
    inline_props: &[(String, Expr)],
    optional: bool,
    params: &HashMap<String, serde_json::Value>,
) -> Result<Vec<Row>, ExecError> {
    use crate::catalog::labels::{label_name, label_id_by_name, prop_key_name};
    use crate::storage::prop_store;

    // Get candidate node IDs.
    let node_ids: Vec<i64> = if let Some(lname) = label {
        let lid = label_id_by_name(lname);
        match lid {
            None => {
                // Label doesn't exist at all.
                if optional {
                    let mut null_row = Row::new();
                    null_row.insert(variable.to_string(), Value::Null);
                    return Ok(vec![null_row]);
                }
                return Ok(Vec::new());
            }
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

    // If optional and no rows matched, return one null-filled row.
    if optional && rows.is_empty() {
        let mut null_row = Row::new();
        null_row.insert(variable.to_string(), Value::Null);
        return Ok(vec![null_row]);
    }

    Ok(rows)
}
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
    optional: bool,
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

        // If src is NULL (propagated from upstream optional expand), emit a null row.
        if matches!(src_val, Value::Null) {
            if optional {
                let mut row = input_row.clone();
                row.insert(dst_var.to_string(), Value::Null);
                if let Some(rv) = rel_var { row.insert(rv.to_string(), Value::Null); }
                result.push(row);
            }
            continue;
        }

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

        let mut matched_any = false;

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
            matched_any = true;
        }

        // OPTIONAL: if no edges matched, emit a null row.
        if optional && !matched_any {
            let mut row = input_row.clone();
            row.insert(dst_var.to_string(), Value::Null);
            if let Some(rv) = rel_var { row.insert(rv.to_string(), Value::Null); }
            result.push(row);
        }
    }

    // OPTIONAL MATCH with no source rows at all → one null row
    if optional && result.is_empty() {
        let mut null_row = Row::new();
        null_row.insert(src_var.to_string(), Value::Null);
        null_row.insert(dst_var.to_string(), Value::Null);
        if let Some(rv) = rel_var { null_row.insert(rv.to_string(), Value::Null); }
        result.push(null_row);
    }

    Ok(result)
}

fn exec_unwind(
    input: &LogicalPlan,
    expr: &Expr,
    alias: &str,
    params: &HashMap<String, serde_json::Value>,
) -> Result<Vec<Row>, ExecError> {
    let input_rows = execute(input, params)?;
    let mut result = Vec::new();

    for input_row in &input_rows {
        let val = eval_expr(expr, input_row, params)?;
        let items = match val {
            Value::Json(serde_json::Value::Array(arr)) => arr,
            Value::Null => continue, // UNWIND null → no rows
            other => {
                // Single scalar: wrap in a one-element array.
                vec![other.to_json()]
            }
        };
        for item in items {
            let mut row = input_row.clone();
            row.insert(alias.to_string(), json_to_value(&item));
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

    // If any return item contains an aggregate function, use grouping.
    if items.iter().any(|i| expr_has_aggregate(&i.expr)) {
        return exec_project_aggregate(rows, items, distinct, order_by, skip, limit, params);
    }

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

/// Aggregating version of exec_project: groups rows by non-aggregate key items,
/// computes aggregates per group, then applies ORDER BY / DISTINCT / SKIP / LIMIT.
fn exec_project_aggregate(
    rows: Vec<Row>,
    items: &[ReturnItem],
    distinct: bool,
    order_by: &[crate::cypher::ast::OrderItem],
    skip: &Option<Expr>,
    limit: &Option<Expr>,
    params: &HashMap<String, serde_json::Value>,
) -> Result<Vec<Row>, ExecError> {
    // Semantic check: AmbiguousAggregationExpression.
    //
    // Collect grouping key expressions (non-aggregate RETURN/WITH items).
    // Also include aliases as Variable("alias") so ORDER BY can reference them.
    let mut key_exprs: Vec<Expr> = Vec::new();
    for item in items {
        if !expr_has_aggregate(&item.expr) {
            key_exprs.push(item.expr.clone());
            if let Some(alias) = &item.alias {
                key_exprs.push(Expr::Variable(alias.clone()));
            }
        }
    }

    // For each aggregate-containing expression (RETURN items and ORDER BY items),
    // every "free" variable/property reference (not nested inside an aggregate call)
    // must appear as an exact key expression.
    for item in items {
        if expr_has_aggregate(&item.expr) {
            let mut free_refs: Vec<Expr> = Vec::new();
            collect_free_var_refs(&item.expr, &mut free_refs);
            for fref in &free_refs {
                if !key_exprs.iter().any(|k| expr_structural_eq(k, fref)) {
                    return Err(ExecError {
                        message: "SyntaxError: AmbiguousAggregationExpression: expression \
                                   mixes aggregate function calls and non-aggregated \
                                   variable references"
                            .into(),
                    });
                }
            }
        }
    }
    for ob in order_by {
        if expr_has_aggregate(&ob.expr) {
            let mut free_refs: Vec<Expr> = Vec::new();
            collect_free_var_refs(&ob.expr, &mut free_refs);
            for fref in &free_refs {
                if !key_exprs.iter().any(|k| expr_structural_eq(k, fref)) {
                    return Err(ExecError {
                        message: "SyntaxError: AmbiguousAggregationExpression in ORDER BY: \
                                   expression mixes aggregate and non-aggregated \
                                   variable references"
                            .into(),
                    });
                }
            }
        }
    }

    // Separate key items (no aggregate) from aggregate items.
    let has_key = items.iter().any(|i| !expr_has_aggregate(&i.expr));

    // Groups: ordered by first seen fingerprint.
    // Each entry: (fingerprint, key_row, group_rows)
    let mut group_order: Vec<String> = Vec::new();
    let mut groups: std::collections::HashMap<String, (Row, Vec<Row>)> =
        std::collections::HashMap::new();

    for row in &rows {
        // Build key row from non-aggregate items.
        let mut key_row = Row::new();
        let mut key_parts: Vec<String> = Vec::new();
        for (idx, item) in items.iter().enumerate() {
            if !expr_has_aggregate(&item.expr) {
                let val = eval_expr(&item.expr, row, params).unwrap_or(Value::Null);
                let col = item.alias.clone()
                    .unwrap_or_else(|| expr_default_name(&item.expr, idx));
                key_parts.push(format!("{}={}", col,
                    serde_json::to_string(&val.to_json()).unwrap_or_default()));
                key_row.insert(col, val);
            }
        }
        let fp = key_parts.join("\x00");
        if !groups.contains_key(&fp) {
            group_order.push(fp.clone());
            groups.insert(fp.clone(), (key_row, Vec::new()));
        }
        groups.get_mut(&fp).unwrap().1.push(row.clone());
    }

    // If no rows and no grouping keys (e.g. RETURN count(*) on empty graph),
    // produce one empty group so COUNT returns 0.
    if groups.is_empty() && !has_key {
        group_order.push(String::new());
        groups.insert(String::new(), (Row::new(), Vec::new()));
    }

    let mut projected: Vec<(Row, Row)> = Vec::new();
    for fp in &group_order {
        let (key_row, group_rows) = groups.get(fp).unwrap();
        let mut out_row = Row::new();
        for (idx, item) in items.iter().enumerate() {
            let val = eval_with_agg(&item.expr, group_rows, key_row, params)?;
            let col = item.alias.clone()
                .unwrap_or_else(|| expr_default_name(&item.expr, idx));
            out_row.insert(col, val);
        }
        projected.push((out_row, key_row.clone()));
    }

    // ORDER BY
    if !order_by.is_empty() {
        projected.sort_by(|(proj_a, in_a), (proj_b, in_b)| {
            for item in order_by {
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
        let mut seen = std::collections::HashSet::new();
        projected.retain(|(out_row, _)| seen.insert(row_fingerprint(out_row)));
    }

    // SKIP
    if let Some(skip_expr) = skip {
        let n = eval_const_usize(skip_expr, params);
        if n >= projected.len() { projected.clear(); } else { projected.drain(0..n); }
    }

    // LIMIT
    if let Some(limit_expr) = limit {
        projected.truncate(eval_const_usize(limit_expr, params));
    }

    Ok(projected.into_iter().map(|(out_row, _)| out_row).collect())
}

/// Returns true if the expression contains any aggregate function call.
fn expr_has_aggregate(expr: &Expr) -> bool {
    match expr {
        Expr::FunctionCall(name, args) => {
            is_aggregate_name(name) || args.iter().any(expr_has_aggregate)
        }
        Expr::Arith(l, _, r) | Expr::Compare(l, _, r) | Expr::And(l, r) | Expr::Or(l, r)
        | Expr::Xor(l, r)
        | Expr::StartsWith(l, r) | Expr::EndsWith(l, r) | Expr::Contains(l, r)
        | Expr::Regex(l, r) | Expr::InList(l, r) => {
            expr_has_aggregate(l) || expr_has_aggregate(r)
        }
        Expr::Not(e) | Expr::Neg(e) | Expr::IsNull(e) | Expr::IsNotNull(e)
        | Expr::Property(e, _) => expr_has_aggregate(e),
        Expr::Subscript(l, r) => expr_has_aggregate(l) || expr_has_aggregate(r),
        Expr::ListSlice { list_expr, from, to, .. } => {
            expr_has_aggregate(list_expr)
                || from.as_deref().map_or(false, expr_has_aggregate)
                || to.as_deref().map_or(false, expr_has_aggregate)
        }
        Expr::List(exprs) => exprs.iter().any(expr_has_aggregate),
        Expr::CaseSearched { branches, else_ } => {
            branches.iter().any(|(c, t)| expr_has_aggregate(c) || expr_has_aggregate(t))
                || else_.as_deref().map_or(false, expr_has_aggregate)
        }
        Expr::CaseSimple { test, branches, else_ } => {
            expr_has_aggregate(test)
                || branches.iter().any(|(w, t)| expr_has_aggregate(w) || expr_has_aggregate(t))
                || else_.as_deref().map_or(false, expr_has_aggregate)
        }
        Expr::ListComprehension { list_expr, predicate, projection, .. } => {
            expr_has_aggregate(list_expr)
                || predicate.as_deref().map_or(false, expr_has_aggregate)
                || projection.as_deref().map_or(false, expr_has_aggregate)
        }
        Expr::ListPredicate { list_expr, predicate, .. } => {
            expr_has_aggregate(list_expr) || expr_has_aggregate(predicate)
        }
        _ => false,
    }
}

/// Returns true if the function name is an aggregate.
fn is_aggregate_name(name: &str) -> bool {
    let lc = name.to_ascii_lowercase();
    matches!(
        lc.as_str(),
        "count" | "count_distinct"
            | "sum" | "sum_distinct"
            | "avg" | "avg_distinct"
            | "min" | "max"
            | "collect" | "collect_distinct"
            | "stdev" | "stdevp"
            | "percentilecont" | "percentiledisc"
    )
}

/// Evaluate an expression that may contain aggregates over a group of rows.
/// Non-aggregate sub-expressions are evaluated on `fallback_row`.
fn eval_with_agg(
    expr: &Expr,
    group_rows: &[Row],
    fallback_row: &Row,
    params: &HashMap<String, serde_json::Value>,
) -> Result<Value, ExecError> {
    if !expr_has_aggregate(expr) {
        // Use first group row if available (they all have the same key values).
        let row = group_rows.first().unwrap_or(fallback_row);
        return eval_expr(expr, row, params);
    }
    match expr {
        Expr::FunctionCall(name, args) if is_aggregate_name(name) => {
            eval_aggregate_call(name, args, group_rows, params)
        }
        Expr::FunctionCall(name, args) => {
            // Non-aggregate function: evaluate each arg (which may contain aggregates) then call function
            let vals: Vec<Value> = args.iter()
                .map(|a| eval_with_agg(a, group_rows, fallback_row, params))
                .collect::<Result<_, _>>()?;
            // Build a synthetic row that maps positional argument slots to values
            // and call eval_function directly.
            eval_function_with_vals(name, &vals)
        }
        Expr::Arith(l, op, r) => {
            let lv = eval_with_agg(l, group_rows, fallback_row, params)?;
            let rv = eval_with_agg(r, group_rows, fallback_row, params)?;
            eval_arith(&lv, op, &rv)
        }
        Expr::Compare(l, op, r) => {
            let lv = eval_with_agg(l, group_rows, fallback_row, params)?;
            let rv = eval_with_agg(r, group_rows, fallback_row, params)?;
            match compare_values(&lv, op, &rv) {
                None => Ok(Value::Null),
                Some(b) => Ok(Value::Bool(b)),
            }
        }
        Expr::And(l, r) => {
            let lv = eval_with_agg(l, group_rows, fallback_row, params)?;
            match truthy3(&lv) {
                Some(false) => Ok(Value::Bool(false)),
                Some(true) => {
                    let rv = eval_with_agg(r, group_rows, fallback_row, params)?;
                    Ok(match truthy3(&rv) { Some(b) => Value::Bool(b), None => Value::Null })
                }
                None => {
                    let rv = eval_with_agg(r, group_rows, fallback_row, params)?;
                    Ok(match truthy3(&rv) { Some(false) => Value::Bool(false), _ => Value::Null })
                }
            }
        }
        Expr::Or(l, r) => {
            let lv = eval_with_agg(l, group_rows, fallback_row, params)?;
            match truthy3(&lv) {
                Some(true) => Ok(Value::Bool(true)),
                Some(false) => {
                    let rv = eval_with_agg(r, group_rows, fallback_row, params)?;
                    Ok(match truthy3(&rv) { Some(b) => Value::Bool(b), None => Value::Null })
                }
                None => {
                    let rv = eval_with_agg(r, group_rows, fallback_row, params)?;
                    Ok(match truthy3(&rv) { Some(true) => Value::Bool(true), _ => Value::Null })
                }
            }
        }
        Expr::Xor(l, r) => {
            let lv = eval_with_agg(l, group_rows, fallback_row, params)?;
            let rv = eval_with_agg(r, group_rows, fallback_row, params)?;
            Ok(match (truthy3(&lv), truthy3(&rv)) {
                (Some(a), Some(b)) => Value::Bool(a ^ b),
                _ => Value::Null,
            })
        }
        Expr::Not(e) => {
            let v = eval_with_agg(e, group_rows, fallback_row, params)?;
            Ok(match truthy3(&v) { Some(b) => Value::Bool(!b), None => Value::Null })
        }
        Expr::IsNull(e) => {
            let v = eval_with_agg(e, group_rows, fallback_row, params)?;
            Ok(Value::Bool(matches!(v, Value::Null)))
        }
        Expr::IsNotNull(e) => {
            let v = eval_with_agg(e, group_rows, fallback_row, params)?;
            Ok(Value::Bool(!matches!(v, Value::Null)))
        }
        Expr::Neg(e) => {
            let v = eval_with_agg(e, group_rows, fallback_row, params)?;
            match v {
                Value::Int(i) => Ok(Value::Int(-i)),
                Value::Float(f) => Ok(Value::Float(-f)),
                _ => Ok(Value::Null),
            }
        }
        Expr::CaseSearched { branches, else_ } => {
            for (cond, then) in branches {
                let cv = eval_with_agg(cond, group_rows, fallback_row, params)?;
                if matches!(truthy3(&cv), Some(true)) {
                    return eval_with_agg(then, group_rows, fallback_row, params);
                }
            }
            match else_ {
                Some(e) => eval_with_agg(e, group_rows, fallback_row, params),
                None => Ok(Value::Null),
            }
        }
        Expr::ListComprehension { variable, list_expr, predicate, projection } => {
            // Evaluate the list source with potential aggregation
            let list_val = eval_with_agg(list_expr, group_rows, fallback_row, params)?;
            let items = match &list_val {
                Value::Json(serde_json::Value::Array(arr)) => arr.clone(),
                Value::Null => return Ok(Value::Json(serde_json::Value::Array(vec![]))),
                _ => return Ok(Value::Null),
            };
            let base_row = group_rows.first().unwrap_or(fallback_row);
            let mut result = Vec::new();
            for item in &items {
                let item_val = json_to_value(item);
                let mut iter_row = base_row.clone();
                iter_row.insert(variable.clone(), item_val);
                if let Some(pred) = predicate {
                    let pv = eval_expr(pred, &iter_row, params).unwrap_or(Value::Null);
                    if !matches!(truthy3(&pv), Some(true)) {
                        continue;
                    }
                }
                let proj_val = if let Some(proj) = projection {
                    eval_expr(proj, &iter_row, params)?
                } else {
                    iter_row.get(variable.as_str()).cloned().unwrap_or(Value::Null)
                };
                result.push(proj_val.to_json());
            }
            Ok(Value::Json(serde_json::Value::Array(result)))
        }
        // For other compound expressions fall back to evaluating on first row
        other => {
            let row = group_rows.first().unwrap_or(fallback_row);
            eval_expr(other, row, params)
        }
    }
}

/// Call a non-aggregate function given already-evaluated argument values.
fn eval_function_with_vals(name: &str, vals: &[Value]) -> Result<Value, ExecError> {
    // Build a synthetic row mapping placeholder keys to the pre-evaluated values,
    // then call eval_function with Variable args pointing to those keys.
    let mut synthetic_row = Row::new();
    let mut synthetic_args: Vec<Expr> = Vec::with_capacity(vals.len());
    for (i, val) in vals.iter().enumerate() {
        let key = format!("__fn_arg_{i}");
        synthetic_row.insert(key.clone(), val.clone());
        synthetic_args.push(Expr::Variable(key));
    }
    let empty_params = HashMap::new();
    eval_function(name, &synthetic_args, &synthetic_row, &empty_params)
}

/// Evaluate an aggregate function call over a group of rows.
fn eval_aggregate_call(
    name: &str,
    args: &[Expr],
    group_rows: &[Row],
    params: &HashMap<String, serde_json::Value>,
) -> Result<Value, ExecError> {
    let lc = name.to_ascii_lowercase();
    let distinct = lc.ends_with("_distinct");
    let base = if distinct { &lc[..lc.len() - 9] } else { lc.as_str() };

    match base {
        "count" => {
            if args.len() == 1 && matches!(args[0], Expr::Star) {
                return Ok(Value::Int(group_rows.len() as i64));
            }
            if args.is_empty() {
                return Ok(Value::Int(group_rows.len() as i64));
            }
            let vals: Vec<Value> = group_rows.iter()
                .filter_map(|row| eval_expr(&args[0], row, params).ok())
                .filter(|v| !matches!(v, Value::Null))
                .collect();
            if distinct {
                let mut seen = std::collections::HashSet::new();
                let count = vals.iter()
                    .filter(|v| seen.insert(value_fingerprint(v)))
                    .count();
                Ok(Value::Int(count as i64))
            } else {
                Ok(Value::Int(vals.len() as i64))
            }
        }
        "sum" => {
            if args.is_empty() {
                return Err(ExecError { message: "sum() requires an argument".into() });
            }
            let mut sum_i = 0i64;
            let mut sum_f = 0.0f64;
            let mut is_float = false;
            for row in group_rows {
                match eval_expr(&args[0], row, params).unwrap_or(Value::Null) {
                    Value::Int(i) => sum_i += i,
                    Value::Float(f) => { sum_f += f; is_float = true; }
                    Value::Null => {}
                    _ => {}
                }
            }
            if is_float {
                Ok(Value::Float(sum_f + sum_i as f64))
            } else {
                Ok(Value::Int(sum_i))
            }
        }
        "avg" => {
            if args.is_empty() {
                return Err(ExecError { message: "avg() requires an argument".into() });
            }
            let mut sum = 0.0f64;
            let mut count = 0usize;
            for row in group_rows {
                match eval_expr(&args[0], row, params).unwrap_or(Value::Null) {
                    Value::Int(i) => { sum += i as f64; count += 1; }
                    Value::Float(f) => { sum += f; count += 1; }
                    Value::Null => {}
                    _ => {}
                }
            }
            if count == 0 {
                Ok(Value::Null)
            } else {
                Ok(Value::Float(sum / count as f64))
            }
        }
        "min" => {
            if args.is_empty() {
                return Err(ExecError { message: "min() requires an argument".into() });
            }
            let mut result: Option<Value> = None;
            for row in group_rows {
                let v = eval_expr(&args[0], row, params).unwrap_or(Value::Null);
                if matches!(v, Value::Null) { continue; }
                result = Some(match result {
                    None => v,
                    Some(cur) => if value_ordering(&v, &cur) == std::cmp::Ordering::Less { v } else { cur },
                });
            }
            Ok(result.unwrap_or(Value::Null))
        }
        "max" => {
            if args.is_empty() {
                return Err(ExecError { message: "max() requires an argument".into() });
            }
            let mut result: Option<Value> = None;
            for row in group_rows {
                let v = eval_expr(&args[0], row, params).unwrap_or(Value::Null);
                if matches!(v, Value::Null) { continue; }
                result = Some(match result {
                    None => v,
                    Some(cur) => if value_ordering(&v, &cur) == std::cmp::Ordering::Greater { v } else { cur },
                });
            }
            Ok(result.unwrap_or(Value::Null))
        }
        "collect" => {
            if args.is_empty() {
                return Err(ExecError { message: "collect() requires an argument".into() });
            }
            let mut vals: Vec<serde_json::Value> = Vec::new();
            for row in group_rows {
                let v = eval_expr(&args[0], row, params).unwrap_or(Value::Null);
                if !matches!(v, Value::Null) {
                    if distinct {
                        let j = v.to_json();
                        let fp = serde_json::to_string(&j).unwrap_or_default();
                        if !vals.iter().any(|x| serde_json::to_string(x).unwrap_or_default() == fp) {
                            vals.push(j);
                        }
                    } else {
                        vals.push(v.to_json());
                    }
                }
            }
            Ok(Value::Json(serde_json::Value::Array(vals)))
        }
        "stdev" => {
            // Sample standard deviation (Bessel's correction)
            eval_stdev(args, group_rows, params, true)
        }
        "stdevp" => {
            // Population standard deviation
            eval_stdev(args, group_rows, params, false)
        }
        "percentilecont" => {
            eval_percentile(args, group_rows, params, true)
        }
        "percentiledisc" => {
            eval_percentile(args, group_rows, params, false)
        }
        _ => Err(ExecError { message: format!("unknown aggregate: {name}") }),
    }
}

fn eval_stdev(
    args: &[Expr],
    group_rows: &[Row],
    params: &HashMap<String, serde_json::Value>,
    sample: bool,
) -> Result<Value, ExecError> {
    if args.is_empty() {
        return Err(ExecError { message: "stdev() requires an argument".into() });
    }
    let vals: Vec<f64> = group_rows.iter()
        .filter_map(|row| eval_expr(&args[0], row, params).ok())
        .filter_map(|v| match v { Value::Int(i) => Some(i as f64), Value::Float(f) => Some(f), _ => None })
        .collect();
    let n = vals.len();
    if n == 0 || (sample && n == 1) { return Ok(Value::Float(0.0)); }
    let mean = vals.iter().sum::<f64>() / n as f64;
    let variance = vals.iter().map(|x| (x - mean).powi(2)).sum::<f64>()
        / if sample { (n - 1) as f64 } else { n as f64 };
    Ok(Value::Float(variance.sqrt()))
}

fn eval_percentile(
    args: &[Expr],
    group_rows: &[Row],
    params: &HashMap<String, serde_json::Value>,
    interpolate: bool,
) -> Result<Value, ExecError> {
    if args.len() < 2 {
        return Err(ExecError { message: "percentile functions require 2 arguments".into() });
    }
    let pct_val = eval_expr(&args[1], &Row::new(), params).unwrap_or(Value::Null);
    let pct = match pct_val {
        Value::Float(f) => f,
        Value::Int(i) => i as f64,
        _ => return Ok(Value::Null),
    };
    if pct < 0.0 || pct > 1.0 {
        return Err(ExecError { message: "percentile must be between 0.0 and 1.0".into() });
    }
    let mut vals: Vec<f64> = group_rows.iter()
        .filter_map(|row| eval_expr(&args[0], row, params).ok())
        .filter_map(|v| match v { Value::Int(i) => Some(i as f64), Value::Float(f) => Some(f), _ => None })
        .collect();
    if vals.is_empty() { return Ok(Value::Null); }
    vals.sort_by(|a, b| a.partial_cmp(b).unwrap_or(std::cmp::Ordering::Equal));
    let n = vals.len();
    if interpolate {
        let idx = pct * (n - 1) as f64;
        let lo = idx.floor() as usize;
        let hi = (lo + 1).min(n - 1);
        let frac = idx - lo as f64;
        Ok(Value::Float(vals[lo] * (1.0 - frac) + vals[hi] * frac))
    } else {
        let idx = (pct * n as f64).ceil() as usize;
        let idx = idx.saturating_sub(1).min(n - 1);
        Ok(Value::Float(vals[idx]))
    }
}

/// Produce a stable string fingerprint for a Value (used in DISTINCT deduplication).
fn value_fingerprint(v: &Value) -> String {
    serde_json::to_string(&v.to_json()).unwrap_or_default()
}

/// Returns true if the expression contains a Variable or Property reference that is
/// NOT nested inside an aggregate function call.  Used to detect AmbiguousAggregationExpression.
fn expr_has_direct_var_ref(expr: &Expr) -> bool {
    match expr {
        Expr::Variable(_) => true,
        Expr::Property(base, _) => expr_has_direct_var_ref(base),
        Expr::FunctionCall(name, args) => {
            if is_aggregate_name(name) {
                false // refs inside aggregates are correctly aggregated
            } else {
                args.iter().any(expr_has_direct_var_ref)
            }
        }
        Expr::Arith(l, _, r) | Expr::Compare(l, _, r) | Expr::And(l, r) | Expr::Or(l, r)
        | Expr::Xor(l, r)
        | Expr::StartsWith(l, r) | Expr::EndsWith(l, r) | Expr::Contains(l, r)
        | Expr::Regex(l, r) | Expr::InList(l, r) => {
            expr_has_direct_var_ref(l) || expr_has_direct_var_ref(r)
        }
        Expr::Not(e) | Expr::Neg(e) | Expr::IsNull(e) | Expr::IsNotNull(e)
        | Expr::Property(e, _) => expr_has_direct_var_ref(e),
        Expr::Subscript(l, r) => expr_has_direct_var_ref(l) || expr_has_direct_var_ref(r),
        Expr::ListSlice { list_expr, from, to, .. } => {
            expr_has_direct_var_ref(list_expr)
                || from.as_deref().map_or(false, expr_has_direct_var_ref)
                || to.as_deref().map_or(false, expr_has_direct_var_ref)
        }
        Expr::List(es) => es.iter().any(expr_has_direct_var_ref),
        Expr::CaseSearched { branches, else_ } => {
            branches.iter().any(|(c, t)| expr_has_direct_var_ref(c) || expr_has_direct_var_ref(t))
                || else_.as_deref().map_or(false, expr_has_direct_var_ref)
        }
        Expr::CaseSimple { test, branches, else_ } => {
            expr_has_direct_var_ref(test)
                || branches.iter().any(|(w, t)| expr_has_direct_var_ref(w) || expr_has_direct_var_ref(t))
                || else_.as_deref().map_or(false, expr_has_direct_var_ref)
        }
        Expr::ListComprehension { list_expr, predicate, projection, .. } => {
            expr_has_direct_var_ref(list_expr)
                || predicate.as_deref().map_or(false, expr_has_direct_var_ref)
                || projection.as_deref().map_or(false, expr_has_direct_var_ref)
        }
        Expr::ListPredicate { list_expr, predicate, .. } => {
            expr_has_direct_var_ref(list_expr) || expr_has_direct_var_ref(predicate)
        }
        // Literals, Star, Parameter, NullLit don't reference variables
        _ => false,
    }
}

/// Collect all Variable/Property leaf expressions that are NOT nested inside an
/// aggregate function call.  These are the "free variable references" that must
/// be covered by a grouping-key expression.
fn collect_free_var_refs(expr: &Expr, acc: &mut Vec<Expr>) {
    match expr {
        Expr::Variable(_) => acc.push(expr.clone()),
        Expr::Property(base, _) => {
            // Treat the whole `node.prop` as a single unit when the base is a Variable.
            if matches!(**base, Expr::Variable(_)) {
                acc.push(expr.clone());
            } else {
                collect_free_var_refs(base, acc);
            }
        }
        Expr::FunctionCall(name, args) => {
            if is_aggregate_name(name) {
                // Do NOT recurse into aggregate call arguments.
            } else {
                for a in args {
                    collect_free_var_refs(a, acc);
                }
            }
        }
        Expr::Arith(l, _, r) | Expr::Compare(l, _, r) | Expr::And(l, r) | Expr::Or(l, r)
        | Expr::Xor(l, r)
        | Expr::StartsWith(l, r) | Expr::EndsWith(l, r) | Expr::Contains(l, r)
        | Expr::Regex(l, r) | Expr::InList(l, r) => {
            collect_free_var_refs(l, acc);
            collect_free_var_refs(r, acc);
        }
        Expr::Not(e) | Expr::Neg(e) | Expr::IsNull(e) | Expr::IsNotNull(e) => {
            collect_free_var_refs(e, acc);
        }
        Expr::Subscript(l, r) => { collect_free_var_refs(l, acc); collect_free_var_refs(r, acc); }
        Expr::List(es) => {
            for e in es { collect_free_var_refs(e, acc); }
        }
        Expr::CaseSearched { branches, else_ } => {
            for (c, t) in branches { collect_free_var_refs(c, acc); collect_free_var_refs(t, acc); }
            if let Some(e) = else_ { collect_free_var_refs(e, acc); }
        }
        Expr::CaseSimple { test, branches, else_ } => {
            collect_free_var_refs(test, acc);
            for (w, t) in branches { collect_free_var_refs(w, acc); collect_free_var_refs(t, acc); }
            if let Some(e) = else_ { collect_free_var_refs(e, acc); }
        }
        // Literals, Param, Star, list comprehensions/predicates — no plain var refs
        _ => {}
    }
}

/// Structural equality for Variable and Property expressions (case-insensitive names).
/// Used to check whether a free variable reference is covered by a grouping key.
fn expr_structural_eq(a: &Expr, b: &Expr) -> bool {
    match (a, b) {
        (Expr::Variable(x), Expr::Variable(y)) => x.to_lowercase() == y.to_lowercase(),
        (Expr::Property(ba, fa), Expr::Property(bb, fb)) => {
            fa.to_lowercase() == fb.to_lowercase() && expr_structural_eq(ba, bb)
        }
        _ => false,
    }
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
            match compare_values(&l, op, &r) {
                None => Ok(Value::Null),
                Some(b) => Ok(Value::Bool(b)),
            }
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
        Expr::CaseSearched { branches, else_ } => {
            for (cond, then) in branches {
                let cv = eval_expr(cond, row, params)?;
                if matches!(truthy3(&cv), Some(true)) {
                    return eval_expr(then, row, params);
                }
            }
            match else_ {
                Some(e) => eval_expr(e, row, params),
                None => Ok(Value::Null),
            }
        }
        Expr::CaseSimple { test, branches, else_ } => {
            let test_val = eval_expr(test, row, params)?;
            for (when, then) in branches {
                let wv = eval_expr(when, row, params)?;
                if values_equal(&test_val, &wv) {
                    return eval_expr(then, row, params);
                }
            }
            match else_ {
                Some(e) => eval_expr(e, row, params),
                None => Ok(Value::Null),
            }
        }
        Expr::ListComprehension { variable, list_expr, predicate, projection } => {
            let list_val = eval_expr(list_expr, row, params)?;
            let arr = match list_val {
                Value::Json(serde_json::Value::Array(a)) => a,
                Value::Null => return Ok(Value::Json(serde_json::Value::Array(vec![]))),
                _ => return Ok(Value::Json(serde_json::Value::Array(vec![]))),
            };
            let mut result = Vec::new();
            for item in arr {
                let mut inner_row = row.clone();
                inner_row.insert(variable.clone(), json_to_value(&item));
                // Apply WHERE predicate if present.
                if let Some(pred) = predicate {
                    let pv = eval_expr(pred, &inner_row, params)?;
                    if !matches!(truthy3(&pv), Some(true)) {
                        continue;
                    }
                }
                // Apply projection if present, else return element.
                let out = if let Some(proj) = projection {
                    eval_expr(proj, &inner_row, params)?
                } else {
                    json_to_value(&item)
                };
                result.push(out.to_json());
            }
            Ok(Value::Json(serde_json::Value::Array(result)))
        }
        Expr::ListPredicate { kind, variable, list_expr, predicate } => {
            use crate::cypher::ast::ListPredicateKind;
            let list_val = eval_expr(list_expr, row, params)?;
            let arr = match list_val {
                Value::Json(serde_json::Value::Array(a)) => a,
                Value::Null => return Ok(Value::Null),
                _ => return Ok(Value::Bool(false)),
            };
            let mut true_count = 0usize;
            let mut has_null = false;
            for item in &arr {
                let mut inner_row = row.clone();
                inner_row.insert(variable.clone(), json_to_value(item));
                let pv = eval_expr(predicate, &inner_row, params)?;
                match truthy3(&pv) {
                    Some(true) => true_count += 1,
                    None => has_null = true,
                    Some(false) => {}
                }
            }
            let total = arr.len();
            match kind {
                ListPredicateKind::Any => {
                    if true_count > 0 { Ok(Value::Bool(true)) }
                    else if has_null { Ok(Value::Null) }
                    else { Ok(Value::Bool(false)) }
                }
                ListPredicateKind::All => {
                    if true_count == total { Ok(Value::Bool(true)) }
                    else if has_null && true_count + 1 == total { Ok(Value::Null) }
                    else { Ok(Value::Bool(false)) }
                }
                ListPredicateKind::None_ => {
                    if true_count == 0 && !has_null { Ok(Value::Bool(true)) }
                    else if true_count > 0 { Ok(Value::Bool(false)) }
                    else { Ok(Value::Null) }
                }
                ListPredicateKind::Single => {
                    if true_count == 1 && !has_null { Ok(Value::Bool(true)) }
                    else if true_count > 1 { Ok(Value::Bool(false)) }
                    else if true_count == 0 && !has_null { Ok(Value::Bool(false)) }
                    else { Ok(Value::Null) }
                }
            }
        }
        Expr::Xor(left, right) => {
            let l = eval_expr(left, row, params)?;
            let r = eval_expr(right, row, params)?;
            Ok(match (truthy3(&l), truthy3(&r)) {
                (Some(a), Some(b)) => Value::Bool(a ^ b),
                _ => Value::Null,
            })
        }
        Expr::Subscript(list_expr, index_expr) => {
            let list_val = eval_expr(list_expr, row, params)?;
            let idx_val = eval_expr(index_expr, row, params)?;
            let arr = match &list_val {
                Value::Json(serde_json::Value::Array(a)) => a,
                Value::Null => return Ok(Value::Null),
                _ => return Ok(Value::Null),
            };
            let idx = match idx_val {
                Value::Int(i) => i,
                Value::Float(f) => f as i64,
                _ => return Ok(Value::Null),
            };
            let len = arr.len() as i64;
            let actual = if idx < 0 { len + idx } else { idx };
            if actual < 0 || actual >= len {
                Ok(Value::Null)
            } else {
                Ok(json_to_value(&arr[actual as usize]))
            }
        }
        Expr::ListSlice { list_expr, from, to } => {
            let list_val = eval_expr(list_expr, row, params)?;
            let arr = match list_val {
                Value::Json(serde_json::Value::Array(a)) => a,
                Value::Null => return Ok(Value::Json(serde_json::Value::Array(vec![]))),
                _ => return Ok(Value::Json(serde_json::Value::Array(vec![]))),
            };
            let len = arr.len() as i64;
            let start = if let Some(f) = from {
                match eval_expr(f, row, params)? {
                    Value::Int(i) => if i < 0 { (len + i).max(0) } else { i.min(len) },
                    Value::Float(f) => { let i = f as i64; if i < 0 { (len + i).max(0) } else { i.min(len) } }
                    _ => 0,
                }
            } else { 0 };
            let end = if let Some(t) = to {
                match eval_expr(t, row, params)? {
                    Value::Int(i) => if i < 0 { (len + i).max(0) } else { i.min(len) },
                    Value::Float(f) => { let i = f as i64; if i < 0 { (len + i).max(0) } else { i.min(len) } }
                    _ => len,
                }
            } else { len };
            let sliced: Vec<serde_json::Value> = arr
                .into_iter()
                .skip(start as usize)
                .take((end - start).max(0) as usize)
                .collect();
            Ok(Value::Json(serde_json::Value::Array(sliced)))
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

fn compare_values(left: &Value, op: &CmpOp, right: &Value) -> Option<bool> {
    // Null comparisons always return null.
    if matches!(left, Value::Null) || matches!(right, Value::Null) {
        return None;
    }
    match (left, right) {
        (Value::Int(a), Value::Int(b)) => Some(int_cmp(*a, op, *b)),
        (Value::Float(a), Value::Float(b)) => Some(float_cmp(*a, op, *b)),
        (Value::Int(a), Value::Float(b)) => Some(float_cmp(*a as f64, op, *b)),
        (Value::Float(a), Value::Int(b)) => Some(float_cmp(*a, op, *b as f64)),
        (Value::Str(a), Value::Str(b)) => Some(str_cmp(a, op, b)),
        // Boolean ordering: false < true (treated as 0/1)
        (Value::Bool(a), Value::Bool(b)) => {
            let av = if *a { 1i32 } else { 0 };
            let bv = if *b { 1i32 } else { 0 };
            Some(match op {
                CmpOp::Eq => av == bv,
                CmpOp::Neq => av != bv,
                CmpOp::Lt => av < bv,
                CmpOp::Gt => av > bv,
                CmpOp::Le => av <= bv,
                CmpOp::Ge => av >= bv,
            })
        }
        (Value::Node { node_id: a, .. }, Value::Node { node_id: b, .. }) => Some(int_cmp(*a, op, *b)),
        // List comparison (lexicographic)
        (Value::Json(serde_json::Value::Array(a)), Value::Json(serde_json::Value::Array(b))) => {
            compare_lists(a, b, op)
        }
        // Type mismatch: = and <> are defined (different types are not equal),
        // ordering operators return null.
        _ => match op {
            CmpOp::Eq => Some(false),
            CmpOp::Neq => Some(true),
            _ => None,
        },
    }
}

/// Lexicographic list comparison returning Option<bool> (None = null).
fn compare_lists(a: &[serde_json::Value], b: &[serde_json::Value], op: &CmpOp) -> Option<bool> {
    match op {
        CmpOp::Eq | CmpOp::Neq => {
            // Different lengths → definitively not equal
            if a.len() != b.len() {
                return Some(matches!(op, CmpOp::Neq));
            }
            // Same length: compare element by element recursively
            let mut has_null = false;
            for (ai, bi) in a.iter().zip(b.iter()) {
                let av = json_to_value(ai);
                let bv = json_to_value(bi);
                match compare_values(&av, &CmpOp::Eq, &bv) {
                    None => { has_null = true; }
                    Some(false) => return Some(matches!(op, CmpOp::Neq)), // definitely ≠
                    Some(true) => {}  // equal, continue
                }
            }
            if has_null { None } else { Some(matches!(op, CmpOp::Eq)) }
        }
        _ => {
            // Ordering comparison: lexicographic using elem_ordering
            let min_len = a.len().min(b.len());
            let mut has_null = false;
            for i in 0..min_len {
                let av = json_to_value(&a[i]);
                let bv = json_to_value(&b[i]);
                match elem_ordering(&av, &bv) {
                    None => { has_null = true; }
                    Some(std::cmp::Ordering::Equal) => {}
                    Some(std::cmp::Ordering::Less) => {
                        return Some(matches!(op, CmpOp::Lt | CmpOp::Le | CmpOp::Neq));
                    }
                    Some(std::cmp::Ordering::Greater) => {
                        return Some(matches!(op, CmpOp::Gt | CmpOp::Ge | CmpOp::Neq));
                    }
                }
            }
            // All compared elements equal (or null-indeterminate)
            match a.len().cmp(&b.len()) {
                std::cmp::Ordering::Greater => Some(matches!(op, CmpOp::Gt | CmpOp::Ge | CmpOp::Neq)),
                std::cmp::Ordering::Less    => Some(matches!(op, CmpOp::Lt | CmpOp::Le | CmpOp::Neq)),
                std::cmp::Ordering::Equal   => if has_null { None } else {
                    Some(matches!(op, CmpOp::Eq | CmpOp::Le | CmpOp::Ge))
                },
            }
        }
    }
}

/// Element-level ordering for list comparison. None = null/incomparable.
fn elem_ordering(a: &Value, b: &Value) -> Option<std::cmp::Ordering> {
    match (a, b) {
        (Value::Null, _) | (_, Value::Null) => None,
        (Value::Int(x), Value::Int(y)) => Some(x.cmp(y)),
        (Value::Float(x), Value::Float(y)) => x.partial_cmp(y),
        (Value::Int(x), Value::Float(y)) => (*x as f64).partial_cmp(y),
        (Value::Float(x), Value::Int(y)) => x.partial_cmp(&(*y as f64)),
        (Value::Str(x), Value::Str(y)) => Some(x.cmp(y)),
        (Value::Bool(x), Value::Bool(y)) => {
            Some((if *x { 1i32 } else { 0 }).cmp(&(if *y { 1i32 } else { 0 })))
        }
        _ => None,
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
            ArithOp::Pow => Ok(Value::Float((*a as f64).powf(*b as f64))),
        },
        (Value::Float(a), Value::Float(b)) => float_arith(*a, op, *b),
        (Value::Int(a), Value::Float(b)) => float_arith(*a as f64, op, *b),
        (Value::Float(a), Value::Int(b)) => float_arith(*a, op, *b as f64),
        (Value::Str(a), Value::Str(b)) if matches!(op, ArithOp::Add) => {
            Ok(Value::Str(format!("{a}{b}")))
        }
        // List concatenation: list + list
        (Value::Json(serde_json::Value::Array(a)), Value::Json(serde_json::Value::Array(b)))
            if matches!(op, ArithOp::Add) => {
            let mut result = a.clone();
            result.extend(b.iter().cloned());
            Ok(Value::Json(serde_json::Value::Array(result)))
        }
        // List append: list + scalar
        (Value::Json(serde_json::Value::Array(a)), _)
            if matches!(op, ArithOp::Add) => {
            let mut result = a.clone();
            result.push(right.to_json());
            Ok(Value::Json(serde_json::Value::Array(result)))
        }
        // List prepend: scalar + list
        (_, Value::Json(serde_json::Value::Array(b)))
            if matches!(op, ArithOp::Add) => {
            let mut result = vec![left.to_json()];
            result.extend(b.iter().cloned());
            Ok(Value::Json(serde_json::Value::Array(result)))
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
        ArithOp::Pow => Ok(Value::Float(a.powf(b))),
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
        // --- Remaining string functions ---
        "left" => {
            if args.len() != 2 { return Err(ExecError { message: "left() takes exactly 2 arguments".into() }); }
            let s = eval_expr(&args[0], row, params)?;
            let n = eval_expr(&args[1], row, params)?;
            match (s, n) {
                (Value::Null, _) | (_, Value::Null) => Ok(Value::Null),
                (Value::Str(s), Value::Int(n)) => {
                    let n = n.max(0) as usize;
                    Ok(Value::Str(s.chars().take(n).collect()))
                }
                _ => Ok(Value::Null),
            }
        }
        "right" => {
            if args.len() != 2 { return Err(ExecError { message: "right() takes exactly 2 arguments".into() }); }
            let s = eval_expr(&args[0], row, params)?;
            let n = eval_expr(&args[1], row, params)?;
            match (s, n) {
                (Value::Null, _) | (_, Value::Null) => Ok(Value::Null),
                (Value::Str(s), Value::Int(n)) => {
                    let n = n.max(0) as usize;
                    let chars: Vec<char> = s.chars().collect();
                    let start = chars.len().saturating_sub(n);
                    Ok(Value::Str(chars[start..].iter().collect()))
                }
                _ => Ok(Value::Null),
            }
        }
        // --- Remaining math functions ---
        "toradians" => math1f(args, row, params, |x| x.to_radians()),
        "todegrees" => math1f(args, row, params, |x| x.to_degrees()),
        "rand" => {
            if !args.is_empty() { return Err(ExecError { message: "rand() takes no arguments".into() }); }
            let f = Spi::get_one::<f64>("SELECT random()").unwrap_or(Some(0.0)).unwrap_or(0.0);
            Ok(Value::Float(f))
        }
        "randomuuid" => {
            if !args.is_empty() { return Err(ExecError { message: "randomUUID() takes no arguments".into() }); }
            let s = Spi::get_one::<String>("SELECT gen_random_uuid()::text")
                .unwrap_or(Some(String::new())).unwrap_or_default();
            Ok(Value::Str(s))
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
    match (a, b) {
        // JSON array equality: element-wise
        (Value::Json(serde_json::Value::Array(aa)), Value::Json(serde_json::Value::Array(bb))) => {
            if aa.len() != bb.len() { return false; }
            aa.iter().zip(bb.iter()).all(|(x, y)| {
                let xv = json_to_value(x);
                let yv = json_to_value(y);
                values_equal(&xv, &yv)
            })
        }
        _ => matches!(compare_values(a, &CmpOp::Eq, b), Some(true)),
    }
}

fn cmp_op_str(op: &CmpOp) -> &'static str {
    match op {
        CmpOp::Eq  => "=",
        CmpOp::Neq => "<>",
        CmpOp::Lt  => "<",
        CmpOp::Gt  => ">",
        CmpOp::Le  => "<=",
        CmpOp::Ge  => ">=",
    }
}

fn arith_op_str(op: &ArithOp) -> &'static str {
    match op {
        ArithOp::Add => "+",
        ArithOp::Sub => "-",
        ArithOp::Mul => "*",
        ArithOp::Div => "/",
        ArithOp::Mod => "%",
        ArithOp::Pow => "^",
    }
}

fn expr_default_name(expr: &Expr, idx: usize) -> String {
    match expr {
        Expr::Variable(name) => name.clone(),
        Expr::Property(base, prop) => {
            format!("{}.{prop}", expr_default_name(base, idx))
        }
        Expr::FunctionCall(name, args) => {
            // count_distinct(x) → "count(DISTINCT x)"
            if let Some(base) = name.strip_suffix("_distinct") {
                let arg_names: Vec<String> = args.iter()
                    .enumerate()
                    .map(|(i, a)| expr_default_name(a, i))
                    .collect();
                return format!("{base}(DISTINCT {})", arg_names.join(", "));
            }
            let arg_names: Vec<String> = args.iter()
                .enumerate()
                .map(|(i, a)| expr_default_name(a, i))
                .collect();
            format!("{name}({})", arg_names.join(", "))
        }
        Expr::Star => "*".into(),
        Expr::NullLit => "null".into(),
        Expr::IntLit(v) => v.to_string(),
        Expr::FloatLit(v) => format!("{v}"),
        Expr::BoolLit(b) => b.to_string(),
        Expr::StringLit(s) => format!("'{s}'"),
        Expr::Parameter(name) => format!("${name}"),
        Expr::IsNull(e) => format!("{} IS NULL", expr_default_name(e, idx)),
        Expr::IsNotNull(e) => format!("{} IS NOT NULL", expr_default_name(e, idx)),
        Expr::Not(e) => format!("NOT ({})", expr_default_name(e, idx)),
        Expr::Neg(e) => format!("-({})", expr_default_name(e, idx)),
        Expr::Compare(l, op, r) => format!(
            "{} {} {}",
            expr_default_name(l, idx),
            cmp_op_str(op),
            expr_default_name(r, idx),
        ),
        Expr::Arith(l, op, r) => format!(
            "{} {} {}",
            expr_default_name(l, idx),
            arith_op_str(op),
            expr_default_name(r, idx),
        ),
        Expr::And(l, r) => format!(
            "{} AND {}",
            expr_default_name(l, idx),
            expr_default_name(r, idx),
        ),
        Expr::Or(l, r) => format!(
            "{} OR {}",
            expr_default_name(l, idx),
            expr_default_name(r, idx),
        ),
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
/// Cross-type ordering (openCypher): null > list > number > string > boolean
fn value_ordering(a: &Value, b: &Value) -> std::cmp::Ordering {
    use std::cmp::Ordering::{Equal, Greater, Less};
    match (a, b) {
        (Value::Null, Value::Null) => Equal,
        (Value::Null, _) => Greater,
        (_, Value::Null) => Less,
        (Value::Int(x), Value::Int(y)) => x.cmp(y),
        (Value::Float(x), Value::Float(y)) => x.partial_cmp(y).unwrap_or(Equal),
        (Value::Int(x), Value::Float(y)) => (*x as f64).partial_cmp(y).unwrap_or(Equal),
        (Value::Float(x), Value::Int(y)) => x.partial_cmp(&(*y as f64)).unwrap_or(Equal),
        (Value::Str(x), Value::Str(y)) => x.cmp(y),
        (Value::Bool(x), Value::Bool(y)) => x.cmp(y),
        // List ordering: lexicographic, with null elements sorting highest within a list.
        (Value::Json(serde_json::Value::Array(xa)), Value::Json(serde_json::Value::Array(ya))) => {
            list_ordering(xa, ya)
        }
        // Cross-type: lists > numbers > strings > booleans (matching Neo4j/openCypher TCK)
        (Value::Json(serde_json::Value::Array(_)), _) => Greater,
        (_, Value::Json(serde_json::Value::Array(_))) => Less,
        (Value::Int(_) | Value::Float(_), Value::Str(_) | Value::Bool(_)) => Greater,
        (Value::Str(_) | Value::Bool(_), Value::Int(_) | Value::Float(_)) => Less,
        (Value::Str(_), Value::Bool(_)) => Greater,
        (Value::Bool(_), Value::Str(_)) => Less,
        _ => Equal,
    }
}

/// Lexicographic ordering of two JSON arrays (for ORDER BY).
/// Nulls within a list sort as Greater than any value (i.e., highest).
fn list_ordering(a: &[serde_json::Value], b: &[serde_json::Value]) -> std::cmp::Ordering {
    let min_len = a.len().min(b.len());
    for i in 0..min_len {
        let av = json_to_value(&a[i]);
        let bv = json_to_value(&b[i]);
        let cmp = value_ordering(&av, &bv);
        if cmp != std::cmp::Ordering::Equal {
            return cmp;
        }
    }
    a.len().cmp(&b.len())
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
        let mut m = serde_json::Map::new();
        for (k, v) in &row {
            m.insert(k.clone(), v.to_json());
        }
        pgrx::JsonB(serde_json::Value::Object(m))
    }).collect()
}
