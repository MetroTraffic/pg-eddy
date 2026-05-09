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
        LogicalPlan::Project { input, items, distinct } => {
            exec_project(input, items, *distinct, params)
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
    params: &HashMap<String, serde_json::Value>,
) -> Result<Vec<Row>, ExecError> {
    let rows = execute(input, params)?;
    let mut result = Vec::new();
    let mut seen: std::collections::HashSet<String> = std::collections::HashSet::new();

    for row in &rows {
        let mut out_row = Row::new();
        for (idx, item) in items.iter().enumerate() {
            let val = eval_expr(&item.expr, row, params)?;
            let key = item.alias.clone().unwrap_or_else(|| {
                expr_default_name(&item.expr, idx)
            });
            out_row.insert(key, val);
        }

        if distinct {
            let fingerprint = row_fingerprint(&out_row);
            if !seen.insert(fingerprint) {
                continue;
            }
        }

        result.push(out_row);
    }

    Ok(result)
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
            let l = eval_expr(left, row, params)?;
            if !l.is_truthy() {
                return Ok(Value::Bool(false));
            }
            let r = eval_expr(right, row, params)?;
            Ok(Value::Bool(r.is_truthy()))
        }
        Expr::Or(left, right) => {
            let l = eval_expr(left, row, params)?;
            if l.is_truthy() {
                return Ok(Value::Bool(true));
            }
            let r = eval_expr(right, row, params)?;
            Ok(Value::Bool(r.is_truthy()))
        }
        Expr::Not(inner) => {
            let v = eval_expr(inner, row, params)?;
            Ok(Value::Bool(!v.is_truthy()))
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
        _ => Err(ExecError {
            message: format!("unknown function: {name}()"),
        }),
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
