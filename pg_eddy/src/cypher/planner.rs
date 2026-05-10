/// Logical planner — transforms a Cypher AST into a logical execution plan.
///
/// v0.6.0 scope: LabelScan, Expand, Filter, Project, CrossProduct.
/// Node isomorphism is enforced by adding inequality filters for every
/// distinct pair of node variables.
/// v0.10.0 scope: VarLengthExpand (variable-length paths), NamedPath.
use crate::cypher::ast::*;
use std::collections::HashSet;
use std::collections::HashMap;

/// A logical plan node.
#[derive(Debug, Clone)]
pub enum LogicalPlan {
    /// Produces a single empty row (seed for queries with no initial MATCH).
    SingleRow,
    /// Scan all nodes, optionally filtered by a single label.
    LabelScan {
        variable: String,
        label: Option<String>,
        inline_props: Vec<(String, Expr)>,
        optional: bool,
    },
    /// Expand from a bound node variable along relationships (single hop).
    Expand {
        input: Box<LogicalPlan>,
        src_var: String,
        rel_var: Option<String>,
        dst_var: String,
        rel_types: Vec<String>,
        direction: RelDirection,
        rel_props: Vec<(String, Expr)>,
        dst_labels: Vec<String>,
        dst_props: Vec<(String, Expr)>,
        optional: bool,
    },
    /// Variable-length expand: BFS/DFS over min..max hops.
    VarLengthExpand {
        input: Box<LogicalPlan>,
        src_var: String,
        rel_var: Option<String>,
        dst_var: String,
        rel_types: Vec<String>,
        direction: RelDirection,
        min_hops: u32,
        max_hops: Option<u32>,
        optional: bool,
    },
    /// Named path: wraps a plan and packages the matched nodes+rels into a path value.
    NamedPath {
        input: Box<LogicalPlan>,
        path_var: String,
        src_var: String,
        rel_var: Option<String>,
        dst_var: String,
    },
    /// Cross-product of two independent plan branches.
    CrossProduct {
        left: Box<LogicalPlan>,
        right: Box<LogicalPlan>,
    },
    /// Filter rows by a boolean expression.
    Filter {
        input: Box<LogicalPlan>,
        predicate: Expr,
    },
    /// Project: compute output columns (also applies ORDER BY / SKIP / LIMIT).
    Project {
        input: Box<LogicalPlan>,
        items: Vec<ReturnItem>,
        distinct: bool,
        order_by: Vec<crate::cypher::ast::OrderItem>,
        skip: Option<Expr>,
        limit: Option<Expr>,
    },
    /// Unwind: expand a list expression into one row per element.
    Unwind {
        input: Box<LogicalPlan>,
        expr: Expr,
        alias: String,
    },
    /// Apply: run an inner subquery for each outer row (CALL { } subquery).
    Apply {
        outer: Box<LogicalPlan>,
        inner: Box<LogicalPlan>,
    },
    /// Empty plan: produces zero rows (used for unimplemented CALL procedures).
    Empty,
}

/// A plan error.
#[derive(Debug, Clone)]
pub struct PlanError {
    pub message: String,
}

impl std::fmt::Display for PlanError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "plan error: {}", self.message)
    }
}

/// Build a logical plan from a parsed Query pipeline.
pub fn plan(query: &Query) -> Result<LogicalPlan, PlanError> {
    let mut current: LogicalPlan = LogicalPlan::SingleRow;
    let mut bound_vars: HashSet<String> = HashSet::new();

    for clause in &query.clauses {
        match clause {
            QueryClause::Match { optional, patterns, where_clause } => {
                current = plan_match_clause(current, patterns, where_clause, *optional, &mut bound_vars)?;
            }
            QueryClause::Unwind { expr, alias } => {
                bound_vars.insert(alias.clone());
                current = LogicalPlan::Unwind {
                    input: Box::new(current),
                    expr: expr.clone(),
                    alias: alias.clone(),
                };
            }
            QueryClause::CallSubquery { subquery } => {
                let inner_plan = plan(subquery)?;
                // Collect variables exposed by inner plan's Project if any
                collect_projected_vars(&inner_plan, &mut bound_vars);
                current = LogicalPlan::Apply {
                    outer: Box::new(current),
                    inner: Box::new(inner_plan),
                };
            }
            QueryClause::CallProcedure { yield_items, .. } => {
                // Procedure registry not yet implemented: produce empty rows.
                // Bind the YIELD variables so downstream clauses don't error.
                for (col, alias) in yield_items {
                    let exposed = alias.as_ref().unwrap_or(col);
                    bound_vars.insert(exposed.clone());
                }
                current = LogicalPlan::Apply {
                    outer: Box::new(current),
                    inner: Box::new(LogicalPlan::Empty),
                };
            }
            QueryClause::With { distinct, items, order_by, skip, limit, where_clause } => {
                current = LogicalPlan::Project {
                    input: Box::new(current),
                    items: items.clone(),
                    distinct: *distinct,
                    order_by: order_by.clone(),
                    skip: skip.clone(),
                    limit: limit.clone(),
                };
                // After WITH, only the projected variables are in scope.
                bound_vars.clear();
                for item in items {
                    let exposed = item.alias.clone().or_else(|| {
                        if let Expr::Variable(v) = &item.expr { Some(v.clone()) } else { None }
                    });
                    if let Some(v) = exposed { bound_vars.insert(v); }
                }
                if let Some(wh) = where_clause {
                    current = LogicalPlan::Filter { input: Box::new(current), predicate: wh.clone() };
                }
            }
            QueryClause::Return { distinct, items, order_by, skip, limit } => {
                current = LogicalPlan::Project {
                    input: Box::new(current),
                    items: items.clone(),
                    distinct: *distinct,
                    order_by: order_by.clone(),
                    skip: skip.clone(),
                    limit: limit.clone(),
                };
            }
        }
    }

    Ok(current)
}

/// Plan all patterns in one MATCH (or OPTIONAL MATCH) clause.
fn plan_match_clause(
    current: LogicalPlan,
    patterns: &[Pattern],
    where_clause: &Option<Expr>,
    optional: bool,
    bound_vars: &mut HashSet<String>,
) -> Result<LogicalPlan, PlanError> {
    let mut plan = current;
    let mut new_node_vars: Vec<String> = Vec::new();

    for pattern in patterns {
        let (new_plan, new_vars) = plan_pattern_onto(pattern, plan, bound_vars, optional)?;
        for v in &new_vars {
            if !new_node_vars.contains(v) { new_node_vars.push(v.clone()); }
            bound_vars.insert(v.clone());
        }
        plan = new_plan;
    }

    // Node isomorphism: add a <> filter for every new pair of node variables.
    let iso_filter = build_isomorphism_filter(&new_node_vars);

    let combined = match (iso_filter, where_clause.clone()) {
        (Some(iso), Some(wh)) => Some(Expr::And(Box::new(iso), Box::new(wh))),
        (Some(iso), None) => Some(iso),
        (None, Some(wh)) => Some(wh),
        (None, None) => None,
    };

    if let Some(predicate) = combined {
        plan = LogicalPlan::Filter { input: Box::new(plan), predicate };
    }

    Ok(plan)
}

/// Plan a single pattern onto the current pipeline.
/// Returns (new_plan, new_node_vars_introduced).
fn plan_pattern_onto(
    pattern: &Pattern,
    current: LogicalPlan,
    bound_vars: &HashSet<String>,
    optional: bool,
) -> Result<(LogicalPlan, Vec<String>), PlanError> {
    let mut new_node_vars: Vec<String> = Vec::new();

    // First element must be a node.
    let first_node = match pattern.elements.first() {
        Some(PatternElement::Node(n)) => n,
        _ => return Err(PlanError { message: "pattern must start with a node".into() }),
    };

    let first_var = first_node.variable.clone().unwrap_or_else(|| "_anon_n0".to_string());
    let first_is_bound = first_node.variable.is_some() && bound_vars.contains(&first_var);

    let mut plan = if first_is_bound {
        // Start with current pipeline — first node is already in scope.
        current
    } else {
        // New node: LabelScan, cross-product with current.
        let label = first_node.labels.first().cloned();
        let scan = LogicalPlan::LabelScan {
            variable: first_var.clone(),
            label,
            inline_props: first_node.properties.clone(),
            optional,
        };
        new_node_vars.push(first_var.clone());
        match current {
            LogicalPlan::SingleRow => scan,
            other => LogicalPlan::CrossProduct { left: Box::new(other), right: Box::new(scan) },
        }
    };

    // Track src_var for named-path assembly.
    let src_var_for_path = first_var.clone();
    let mut last_rel_var: Option<String> = None;

    // Process relationship+node pairs.
    let mut i = 1;
    while i < pattern.elements.len() {
        if let PatternElement::Relationship(rel) = &pattern.elements[i] {
            let next_node = match pattern.elements.get(i + 1) {
                Some(PatternElement::Node(n)) => n,
                _ => return Err(PlanError { message: "relationship must be followed by a node".into() }),
            };

            let src_var = find_last_node_var(&plan);
            let dst_var = next_node.variable.clone()
                .unwrap_or_else(|| format!("_anon_n{}", i + 1));

            if !bound_vars.contains(&dst_var) && !new_node_vars.contains(&dst_var) {
                new_node_vars.push(dst_var.clone());
            }

            last_rel_var = rel.variable.clone();

            if let Some(vl) = &rel.length {
                // Variable-length expand
                plan = LogicalPlan::VarLengthExpand {
                    input: Box::new(plan),
                    src_var,
                    rel_var: rel.variable.clone(),
                    dst_var,
                    rel_types: rel.rel_types.clone(),
                    direction: rel.direction,
                    min_hops: vl.min,
                    max_hops: vl.max,
                    optional,
                };
            } else {
                // Fixed single-hop expand
                plan = LogicalPlan::Expand {
                    input: Box::new(plan),
                    src_var,
                    rel_var: rel.variable.clone(),
                    dst_var,
                    rel_types: rel.rel_types.clone(),
                    direction: rel.direction,
                    rel_props: rel.properties.clone(),
                    dst_labels: next_node.labels.clone(),
                    dst_props: next_node.properties.clone(),
                    optional,
                };
            }
            i += 2;
        } else {
            i += 1;
        }
    }

    // If the pattern has a name (p = ...), wrap in NamedPath.
    if let Some(ref path_name) = pattern.variable {
        let dst_var = find_last_node_var(&plan);
        plan = LogicalPlan::NamedPath {
            input: Box::new(plan),
            path_var: path_name.clone(),
            src_var: src_var_for_path,
            rel_var: last_rel_var,
            dst_var,
        };
    }

    Ok((plan, new_node_vars))
}

/// Find the last node variable name from a plan branch.
fn find_last_node_var(plan: &LogicalPlan) -> String {
    match plan {
        LogicalPlan::SingleRow => "_none".to_string(),
        LogicalPlan::LabelScan { variable, .. } => variable.clone(),
        LogicalPlan::Expand { dst_var, .. } => dst_var.clone(),
        LogicalPlan::VarLengthExpand { dst_var, .. } => dst_var.clone(),
        LogicalPlan::NamedPath { dst_var, .. } => dst_var.clone(),
        LogicalPlan::CrossProduct { right, .. } => find_last_node_var(right),
        LogicalPlan::Filter { input, .. } => find_last_node_var(input),
        LogicalPlan::Project { input, .. } => find_last_node_var(input),
        LogicalPlan::Unwind { alias, .. } => alias.clone(),
        LogicalPlan::Apply { outer, .. } => find_last_node_var(outer),
        LogicalPlan::Empty => "_none".to_string(),
    }
}

/// Build an isomorphism filter: for N node variables, emit
/// `a <> b AND a <> c AND b <> c` using id() comparisons.
/// Null-safe: if either variable is NULL (from OPTIONAL MATCH), the pair passes.
/// Collect all variables exposed by the topmost Project node of a plan.
fn collect_projected_vars(plan: &LogicalPlan, bound_vars: &mut HashSet<String>) {
    if let LogicalPlan::Project { items, .. } = plan {
        for item in items {
            let exposed = item.alias.clone().or_else(|| {
                if let Expr::Variable(v) = &item.expr { Some(v.clone()) } else { None }
            });
            if let Some(v) = exposed { bound_vars.insert(v); }
        }
    }
}

fn build_isomorphism_filter(node_vars: &[String]) -> Option<Expr> {
    if node_vars.len() < 2 {
        return None;
    }

    let mut pairs: Vec<Expr> = Vec::new();
    for i in 0..node_vars.len() {
        for j in (i + 1)..node_vars.len() {
            let left = Expr::FunctionCall(
                "id".into(),
                vec![Expr::Variable(node_vars[i].clone())],
            );
            let right = Expr::FunctionCall(
                "id".into(),
                vec![Expr::Variable(node_vars[j].clone())],
            );
            let neq = Expr::Compare(Box::new(left), CmpOp::Neq, Box::new(right));
            // Wrap with null guards so that null nodes (from OPTIONAL MATCH) pass through.
            let null_safe = Expr::Or(
                Box::new(Expr::IsNull(Box::new(Expr::Variable(node_vars[i].clone())))),
                Box::new(Expr::Or(
                    Box::new(Expr::IsNull(Box::new(Expr::Variable(node_vars[j].clone())))),
                    Box::new(neq),
                )),
            );
            pairs.push(null_safe);
        }
    }

    let mut result = pairs.remove(0);
    for p in pairs {
        result = Expr::And(Box::new(result), Box::new(p));
    }
    Some(result)
}

/// Format a plan as a human-readable explain string.
pub fn explain(plan: &LogicalPlan, indent: usize) -> String {
    let prefix = "  ".repeat(indent);
    match plan {
        LogicalPlan::LabelScan { variable, label, inline_props, .. } => {
            let label_str = label.as_deref().unwrap_or("*");
            let props_str = if inline_props.is_empty() {
                String::new()
            } else {
                let keys: Vec<&str> = inline_props.iter().map(|(k, _)| k.as_str()).collect();
                format!(" props=[{}]", keys.join(", "))
            };
            format!("{prefix}LabelScan({variable}:{label_str}{props_str})")
        }
        LogicalPlan::Expand {
            input, src_var, rel_var, dst_var, rel_types, direction, ..
        } => {
            let dir_str = match direction {
                RelDirection::Out => "->",
                RelDirection::In => "<-",
                RelDirection::Both => "--",
            };
            let types_str = if rel_types.is_empty() {
                "*".to_string()
            } else {
                rel_types.join("|")
            };
            let rv = rel_var.as_deref().unwrap_or("_");
            let child = explain(input, indent + 1);
            format!(
                "{prefix}Expand({src_var})-[{rv}:{types_str}]{dir_str}({dst_var})\n{child}"
            )
        }
        LogicalPlan::CrossProduct { left, right } => {
            let l = explain(left, indent + 1);
            let r = explain(right, indent + 1);
            format!("{prefix}CrossProduct\n{l}\n{r}")
        }
        LogicalPlan::Filter { input, predicate } => {
            let child = explain(input, indent + 1);
            format!("{prefix}Filter({predicate:?})\n{child}")
        }
        LogicalPlan::Project { input, items, distinct, .. } => {
            let cols: Vec<String> = items.iter().map(|it| {
                let base = format!("{:?}", it.expr);
                match &it.alias {
                    Some(a) => format!("{base} AS {a}"),
                    None => base,
                }
            }).collect();
            let dist = if *distinct { " DISTINCT" } else { "" };
            let child = explain(input, indent + 1);
            format!("{prefix}Project{dist}({})\n{child}", cols.join(", "))
        }
        LogicalPlan::SingleRow => format!("{prefix}SingleRow"),
        LogicalPlan::Unwind { input, expr, alias } => {
            let child = explain(input, indent + 1);
            format!("{prefix}Unwind({expr:?} AS {alias})\n{child}")
        }
        LogicalPlan::VarLengthExpand {
            input, src_var, rel_var, dst_var, rel_types, direction, min_hops, max_hops, ..
        } => {
            let dir_str = match direction {
                RelDirection::Out => "->",
                RelDirection::In => "<-",
                RelDirection::Both => "--",
            };
            let types_str = if rel_types.is_empty() { "*".to_string() } else { rel_types.join("|") };
            let rv = rel_var.as_deref().unwrap_or("_");
            let range = match max_hops {
                Some(max) => format!("*{}..{}", min_hops, max),
                None => format!("*{}..", min_hops),
            };
            let child = explain(input, indent + 1);
            format!("{prefix}VarLengthExpand({src_var})-[{rv}:{types_str} {range}]{dir_str}({dst_var})\n{child}")
        }
        LogicalPlan::NamedPath { input, path_var, src_var, rel_var, dst_var } => {
            let rv = rel_var.as_deref().unwrap_or("_");
            let child = explain(input, indent + 1);
            format!("{prefix}NamedPath({path_var} = {src_var}-[{rv}]-{dst_var})\n{child}")
        }
        LogicalPlan::Apply { outer, inner } => {
            let o = explain(outer, indent + 1);
            let i = explain(inner, indent + 1);
            format!("{prefix}Apply\n{o}\n{i}")
        }
        LogicalPlan::Empty => format!("{prefix}Empty"),
    }
}

/// Execute a pattern inline starting from the bound variables in `row`.
/// Used by pattern comprehensions: `[(n)-[:R]->(m) | expr]`.
pub fn exec_pattern_inline(
    pattern: &Pattern,
    row: &crate::cypher::executor::Row,
    params: &HashMap<String, serde_json::Value>,
) -> Result<Vec<crate::cypher::executor::Row>, PlanError> {
    use crate::cypher::executor::execute;
    let bound: HashSet<String> = row.keys().cloned().collect();
    let (plan, _) = plan_pattern_onto(pattern, LogicalPlan::SingleRow, &bound, false)?;
    // Seed the plan with the current row bindings by using a cross-product seed.
    // We pass params as-is; the row bindings are injected below.
    let inner_rows = execute(&plan, params)
        .map_err(|e| PlanError { message: e.message })?;
    // For each result row, merge with outer row bindings.
    Ok(inner_rows.into_iter().map(|mut r| {
        for (k, v) in row {
            r.entry(k.clone()).or_insert_with(|| v.clone());
        }
        r
    }).collect())
}

/// Execute shortestPath(pattern) — run BFS and return the shortest path as a Value::Path.
/// `all` = true means allShortestPaths (return all paths of minimum length).
pub fn plan_pattern_for_shortest_path(
    pattern: &Pattern,
    all: bool,
    row: &crate::cypher::executor::Row,
    params: &HashMap<String, serde_json::Value>,
) -> Result<crate::cypher::executor::Value, PlanError> {
    use crate::cypher::executor::{execute, Value};
    // Build a plan that finds all paths (var-length with max = some cap).
    // Then pick the shortest.
    let bound: HashSet<String> = row.keys().cloned().collect();
    let (plan, _) = plan_pattern_onto(pattern, LogicalPlan::SingleRow, &bound, false)?;
    let rows = execute(&plan, params)
        .map_err(|e| PlanError { message: e.message })?;
    if rows.is_empty() {
        return Ok(Value::Null);
    }
    if all {
        // Return the first (BFS ordering gives shortest first).
        // For now return the first result node (simplified).
        Ok(Value::Null) // TODO: full path packaging
    } else {
        Ok(Value::Null) // TODO: full path packaging
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::cypher::parser::parse;

    #[test]
    fn test_plan_simple_scan() {
        let q = parse("MATCH (n:Person) RETURN n").unwrap();
        let p = plan(&q).unwrap();
        let s = explain(&p, 0);
        assert!(s.contains("LabelScan(n:Person)"));
        assert!(s.contains("Project"));
    }

    #[test]
    fn test_plan_expand() {
        let q = parse("MATCH (a:Person)-[r:KNOWS]->(b:Person) RETURN a, b").unwrap();
        let p = plan(&q).unwrap();
        let s = explain(&p, 0);
        assert!(s.contains("Expand(a)-[r:KNOWS]->(b)"));
        assert!(s.contains("LabelScan(a:Person)"));
    }

    #[test]
    fn test_plan_isomorphism() {
        let q = parse("MATCH (a:Person)-[:KNOWS]->(b:Person) RETURN a, b").unwrap();
        let p = plan(&q).unwrap();
        let s = explain(&p, 0);
        // Should have a Filter with id(a) <> id(b)
        assert!(s.contains("Filter"));
    }

    #[test]
    fn test_plan_cross_product() {
        let q = parse("MATCH (a:Person), (b:Company) RETURN a, b").unwrap();
        let p = plan(&q).unwrap();
        let s = explain(&p, 0);
        assert!(s.contains("CrossProduct"));
    }

    #[test]
    fn test_plan_where() {
        let q = parse("MATCH (n:Person) WHERE n.age > 30 RETURN n").unwrap();
        let p = plan(&q).unwrap();
        let s = explain(&p, 0);
        assert!(s.contains("Filter"));
    }

    #[test]
    fn test_plan_call_subquery() {
        let q = parse("CALL { MATCH (n:Person) RETURN n } RETURN n").unwrap();
        let p = plan(&q).unwrap();
        let s = explain(&p, 0);
        assert!(s.contains("Apply"), "expected Apply in plan: {s}");
    }

    #[test]
    fn test_plan_call_procedure_yields_empty() {
        let q = parse("CALL test.doNothing() YIELD x RETURN x").unwrap();
        let p = plan(&q).unwrap();
        let s = explain(&p, 0);
        assert!(s.contains("Apply"), "expected Apply in plan: {s}");
        assert!(s.contains("Empty"), "expected Empty in plan: {s}");
    }
}
