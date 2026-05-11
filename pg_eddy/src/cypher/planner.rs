/// Logical planner — transforms a Cypher AST into a logical execution plan.
///
/// v0.6.0 scope: LabelScan, Expand, Filter, Project, CrossProduct.
/// Node isomorphism is enforced by adding inequality filters for every
/// distinct pair of node variables.
/// v0.10.0 scope: VarLengthExpand (variable-length paths), NamedPath.
/// v0.17.0 scope: VarKind tracking for VariableTypeConflict; aggregation-in-ORDER-BY check.
use crate::cypher::ast::*;
use std::collections::HashSet;
use std::collections::HashMap;

/// The kind of entity a bound variable represents (used for type-conflict detection).
#[derive(Debug, Clone, PartialEq)]
enum VarKind {
    Node,
    Rel,
    Path,
    Scalar,
}

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
        /// If Some(var), the BFS stores the full path (nodes+rels) under this variable name.
        path_carry_var: Option<String>,
    },
    /// Named path: wraps a plan and packages the matched nodes+rels into a path value.
    NamedPath {
        input: Box<LogicalPlan>,
        path_var: String,
        /// Element variable names in order: [node_var, rel_var, node_var, rel_var, ..., node_var].
        /// For var-length segments, the rel_var slot holds a `path_carry_var` variable that
        /// already contains a Value::Path built by the VarLengthExpand executor.
        element_vars: Vec<String>,
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
    // -----------------------------------------------------------------------
    // v0.12.0: Write plan nodes
    // -----------------------------------------------------------------------
    /// Create nodes and relationships from CREATE patterns.
    CreatePattern {
        input: Box<LogicalPlan>,
        patterns: Vec<Pattern>,
    },
    /// SET properties/labels.
    SetProp {
        input: Box<LogicalPlan>,
        items: Vec<SetItem>,
    },
    /// REMOVE properties/labels.
    RemoveProp {
        input: Box<LogicalPlan>,
        items: Vec<RemoveItem>,
    },
    /// DELETE (or DETACH DELETE) nodes/relationships.
    DeleteNodes {
        input: Box<LogicalPlan>,
        exprs: Vec<Expr>,
        detach: bool,
    },
    /// MERGE pattern [ON CREATE SET ...] [ON MATCH SET ...].
    MergePattern {
        input: Box<LogicalPlan>,
        pattern: Pattern,
        on_create: Vec<SetItem>,
        on_match: Vec<SetItem>,
    },
    /// FOREACH (variable IN list | clauses): iterate a list and execute write clauses per element.
    Foreach {
        input: Box<LogicalPlan>,
        variable: String,
        list_expr: Expr,
        body: Box<LogicalPlan>,
    },
    /// UNION / UNION ALL: execute two plans and combine their results.
    Union {
        left: Box<LogicalPlan>,
        right: Box<LogicalPlan>,
        all: bool, // true = UNION ALL (no dedup), false = UNION (dedup)
    },
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
/// Extract the output column names from a RETURN clause in the given clauses list.
fn return_column_names(clauses: &[QueryClause]) -> Option<Vec<String>> {
    for clause in clauses.iter().rev() {
        if let QueryClause::Return { items, .. } = clause {
            let names: Vec<String> = items
                .iter()
                .map(|item| {
                    if let Some(alias) = &item.alias {
                        alias.clone()
                    } else {
                        match &item.expr {
                            crate::cypher::ast::Expr::Variable(v) => v.clone(),
                            crate::cypher::ast::Expr::Property(_, k) => k.clone(),
                            _ => "_col".to_string(),
                        }
                    }
                })
                .collect();
            return Some(names);
        }
    }
    None
}

pub fn plan(query: &Query) -> Result<LogicalPlan, PlanError> {
    let left = plan_single(query)?;
    if let Some((all, right_q)) = &query.union {
        // Validate that both sides of UNION expose the same column names.
        let left_cols = return_column_names(&query.clauses);
        let right_cols = return_column_names(&right_q.clauses);
        if let (Some(lc), Some(rc)) = (left_cols, right_cols) {
            if lc != rc {
                return Err(PlanError {
                    message: format!(
                        "SyntaxError::DifferentColumnsInUnion: columns {:?} and {:?} differ",
                        lc, rc
                    ),
                });
            }
        }
        // Also recursively validate the right side (in case of chained UNIONs).
        let right = plan(right_q)?;
        Ok(LogicalPlan::Union { left: Box::new(left), right: Box::new(right), all: *all })
    } else {
        Ok(left)
    }
}

fn plan_single(query: &Query) -> Result<LogicalPlan, PlanError> {
    let mut bound_vars: HashSet<String> = HashSet::new();
    let mut var_kinds: HashMap<String, VarKind> = HashMap::new();
    plan_clauses(&query.clauses, LogicalPlan::SingleRow, &mut bound_vars, &mut var_kinds)
}

fn plan_clauses(
    clauses: &[QueryClause],
    seed: LogicalPlan,
    bound_vars: &mut HashSet<String>,
    var_kinds: &mut HashMap<String, VarKind>,
) -> Result<LogicalPlan, PlanError> {
    let mut current = seed;
    for clause in clauses {
        match clause {
            QueryClause::Match { optional, patterns, where_clause } => {
                current = plan_match_clause(current, patterns, where_clause, *optional, bound_vars, var_kinds)?;
            }
            QueryClause::Unwind { expr, alias } => {
                bound_vars.insert(alias.clone());
                var_kinds.insert(alias.clone(), VarKind::Scalar);
                current = LogicalPlan::Unwind {
                    input: Box::new(current),
                    expr: expr.clone(),
                    alias: alias.clone(),
                };
            }
            QueryClause::CallSubquery { subquery } => {
                let inner_plan = plan(subquery)?;
                // Collect variables exposed by inner plan's Project if any
                collect_projected_vars(&inner_plan, bound_vars);
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
                    var_kinds.insert(exposed.clone(), VarKind::Scalar);
                }
                current = LogicalPlan::Apply {
                    outer: Box::new(current),
                    inner: Box::new(LogicalPlan::Empty),
                };
            }
            QueryClause::With { distinct, items, order_by, skip, limit, where_clause } => {
                // Check: aggregation function in ORDER BY without aggregation in projection
                // is a SyntaxError (InvalidAggregation).
                let proj_has_agg = items.iter().any(|it| expr_contains_aggregation(&it.expr));
                if !proj_has_agg {
                    for ob_item in order_by {
                        if expr_contains_aggregation(&ob_item.expr) {
                            return Err(PlanError {
                                message: "SyntaxError: InvalidAggregation — aggregation in ORDER BY \
                                    without corresponding aggregation in WITH projection".into(),
                            });
                        }
                    }
                }
                current = LogicalPlan::Project {
                    input: Box::new(current),
                    items: items.clone(),
                    distinct: *distinct,
                    order_by: order_by.clone(),
                    skip: skip.clone(),
                    limit: limit.clone(),
                };
                // After WITH, only the projected variables are in scope.
                // Re-classify them based on expression type.
                bound_vars.clear();
                var_kinds.clear();
                for item in items {
                    let exposed = item.alias.clone().or_else(|| {
                        if let Expr::Variable(v) = &item.expr { Some(v.clone()) } else { None }
                    });
                    if let Some(v) = exposed {
                        bound_vars.insert(v.clone());
                        // Infer kind: Variable passthrough keeps kind; literals are Scalar.
                        let kind = expr_var_kind(&item.expr, var_kinds);
                        var_kinds.insert(v, kind);
                    }
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
            // v0.12.0 write clauses
            QueryClause::Create { patterns } => {
                // Bind any variables introduced by CREATE patterns.
                for pattern in patterns {
                    for elem in &pattern.elements {
                        if let PatternElement::Node(n) = elem
                            && let Some(v) = &n.variable {
                                bound_vars.insert(v.clone());
                            }
                        if let PatternElement::Relationship(r) = elem
                            && let Some(v) = &r.variable {
                                bound_vars.insert(v.clone());
                            }
                    }
                }
                current = LogicalPlan::CreatePattern {
                    input: Box::new(current),
                    patterns: patterns.clone(),
                };
            }
            QueryClause::Set { items } => {
                current = LogicalPlan::SetProp {
                    input: Box::new(current),
                    items: items.clone(),
                };
            }
            QueryClause::Remove { items } => {
                current = LogicalPlan::RemoveProp {
                    input: Box::new(current),
                    items: items.clone(),
                };
            }
            QueryClause::Delete { exprs, detach } => {
                current = LogicalPlan::DeleteNodes {
                    input: Box::new(current),
                    exprs: exprs.clone(),
                    detach: *detach,
                };
            }
            QueryClause::Merge { pattern, on_create, on_match } => {
                // Variables from the merge pattern come into scope.
                for elem in &pattern.elements {
                    if let PatternElement::Node(n) = elem
                        && let Some(v) = &n.variable {
                            bound_vars.insert(v.clone());
                        }
                    if let PatternElement::Relationship(r) = elem
                        && let Some(v) = &r.variable {
                            bound_vars.insert(v.clone());
                        }
                }
                current = LogicalPlan::MergePattern {
                    input: Box::new(current),
                    pattern: pattern.clone(),
                    on_create: on_create.clone(),
                    on_match: on_match.clone(),
                };
            }
            QueryClause::Foreach { variable, list_expr, clauses } => {
                // Plan the body: SingleRow → chain of write clauses.
                let body_seed = LogicalPlan::SingleRow;
                let mut body_vars: HashSet<String> = bound_vars.clone();
                let mut body_kinds: HashMap<String, VarKind> = var_kinds.clone();
                // The loop variable is bound inside the body.
                body_vars.insert(variable.clone());
                body_kinds.insert(variable.clone(), VarKind::Scalar);
                let body_plan = plan_clauses(clauses, body_seed, &mut body_vars, &mut body_kinds)?;
                current = LogicalPlan::Foreach {
                    input: Box::new(current),
                    variable: variable.clone(),
                    list_expr: list_expr.clone(),
                    body: Box::new(body_plan),
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
    var_kinds: &mut HashMap<String, VarKind>,
) -> Result<LogicalPlan, PlanError> {
    let mut plan = current;
    let mut new_node_vars: Vec<String> = Vec::new();

    for pattern in patterns {
        // If the pattern has a path variable, check it isn't already bound as non-Path.
        if pattern.variable.as_ref().is_some_and(|pvar| var_kinds.contains_key(pvar)) {
            let pvar = pattern.variable.as_deref().unwrap();
            return Err(PlanError {
                message: format!("SyntaxError: VariableAlreadyBound — '{pvar}' is already bound"),
            });
        }
        let (new_plan, new_node_var_list, new_rel_var_list) =
            plan_pattern_onto(pattern, plan, bound_vars, var_kinds, optional)?;
        for v in &new_node_var_list {
            if !new_node_vars.contains(v) { new_node_vars.push(v.clone()); }
            bound_vars.insert(v.clone());
        }
        for v in &new_rel_var_list {
            bound_vars.insert(v.clone());
        }
        if let Some(ref pvar) = pattern.variable {
            bound_vars.insert(pvar.clone());
            var_kinds.insert(pvar.clone(), VarKind::Path);
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
/// Returns (new_plan, new_node_vars_introduced, new_rel_vars_introduced).
fn plan_pattern_onto(
    pattern: &Pattern,
    current: LogicalPlan,
    bound_vars: &HashSet<String>,
    var_kinds: &mut HashMap<String, VarKind>,
    optional: bool,
) -> Result<(LogicalPlan, Vec<String>, Vec<String>), PlanError> {
    let mut new_node_vars: Vec<String> = Vec::new();
    let mut new_rel_vars: Vec<String> = Vec::new();

    // First element must be a node.
    let first_node = match pattern.elements.first() {
        Some(PatternElement::Node(n)) => n,
        _ => return Err(PlanError { message: "pattern must start with a node".into() }),
    };

    let first_var = first_node.variable.clone().unwrap_or_else(|| "_anon_n0".to_string());
    let first_is_bound = first_node.variable.is_some() && bound_vars.contains(&first_var);

    // Type-conflict check for first node.
    if first_node.variable.is_some()
        && matches!(var_kinds.get(&first_var), Some(k) if *k != VarKind::Node)
    {
        return Err(PlanError {
            message: format!(
                "SyntaxError: VariableTypeConflict — '{first_var}' is already bound as a \
                 non-node but used as a node"
            ),
        });
    }

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
        var_kinds.insert(first_var.clone(), VarKind::Node);
        match current {
            LogicalPlan::SingleRow => scan,
            other => LogicalPlan::CrossProduct { left: Box::new(other), right: Box::new(scan) },
        }
    };

    // Track element vars for named-path assembly: alternating [node, rel, node, ...]
    let is_named = pattern.variable.is_some();
    let mut element_vars: Vec<String> = if is_named { vec![first_var.clone()] } else { Vec::new() };
    // Counter for anonymous rel names (used in named paths to ensure rel is stored in row).
    let mut anon_rel_counter: usize = 0;
    // Track the current "last node variable" for src_var of each expand.
    // When first_is_bound=true, plan starts as SingleRow but find_last_node_var would
    // return "_none". We need to track explicitly.
    let mut last_node_var = first_var.clone();

    // Process relationship+node pairs.
    let mut i = 1;
    while i < pattern.elements.len() {
        if let PatternElement::Relationship(rel) = &pattern.elements[i] {
            let next_node = match pattern.elements.get(i + 1) {
                Some(PatternElement::Node(n)) => n,
                _ => return Err(PlanError { message: "relationship must be followed by a node".into() }),
            };

            let src_var = last_node_var.clone();

            // Determine rel variable name (user-provided or internal for named paths).
            let rel_var_name: Option<String> = if let Some(ref rv) = rel.variable {
                // Type-conflict check.
                if matches!(var_kinds.get(rv), Some(k) if *k != VarKind::Rel) {
                    return Err(PlanError {
                        message: format!(
                            "SyntaxError: VariableTypeConflict — '{rv}' is already bound as a \
                             non-relationship but used as a relationship"
                        ),
                    });
                }
                Some(rv.clone())
            } else if is_named {
                // Anonymous rel in a named path: generate internal name so it appears in row.
                let internal = format!("_pr_{}", anon_rel_counter);
                anon_rel_counter += 1;
                Some(internal)
            } else {
                None
            };

            // For named paths: add rel var to element_vars.
            if let Some(ref rv) = rel_var_name {
                if is_named {
                    element_vars.push(rv.clone());
                }
                // Track as Rel kind if not already known.
                if !var_kinds.contains_key(rv) {
                    var_kinds.insert(rv.clone(), VarKind::Rel);
                    if rel.variable.is_some() {
                        new_rel_vars.push(rv.clone());
                    }
                }
            }

            let dst_var = next_node.variable.clone()
                .unwrap_or_else(|| format!("_anon_n{}", i + 1));

            // Type-conflict check for dest node.
            if next_node.variable.is_some()
                && matches!(var_kinds.get(&dst_var), Some(k) if *k != VarKind::Node)
            {
                return Err(PlanError {
                    message: format!(
                        "SyntaxError: VariableTypeConflict — '{dst_var}' is already bound as a \
                         non-node but used as a node"
                    ),
                });
            }

            if !bound_vars.contains(&dst_var) && !new_node_vars.contains(&dst_var) {
                new_node_vars.push(dst_var.clone());
                var_kinds.insert(dst_var.clone(), VarKind::Node);
            }

            if is_named {
                element_vars.push(dst_var.clone());
            }

            if let Some(vl) = &rel.length {
                // Variable-length expand.
                // For a named path, use path_carry_var to store the full traversal.
                let path_carry = if is_named {
                    rel_var_name.clone()
                } else {
                    None
                };
                plan = LogicalPlan::VarLengthExpand {
                    input: Box::new(plan),
                    src_var,
                    rel_var: if is_named { None } else { rel.variable.clone() },
                    dst_var: dst_var.clone(),
                    rel_types: rel.rel_types.clone(),
                    direction: rel.direction,
                    min_hops: vl.min,
                    max_hops: vl.max,
                    optional,
                    path_carry_var: path_carry,
                };
            } else {
                // Fixed single-hop expand.
                plan = LogicalPlan::Expand {
                    input: Box::new(plan),
                    src_var,
                    rel_var: rel_var_name.clone(),
                    dst_var: dst_var.clone(),
                    rel_types: rel.rel_types.clone(),
                    direction: rel.direction,
                    rel_props: rel.properties.clone(),
                    dst_labels: next_node.labels.clone(),
                    dst_props: next_node.properties.clone(),
                    optional,
                };
            }
            last_node_var = dst_var;
            i += 2;
        } else {
            i += 1;
        }
    }

    // If the pattern has a name (p = ...), wrap in NamedPath.
    if let Some(ref path_name) = pattern.variable {
        // For a single node (no rels), element_vars = [node_var] already set.
        plan = LogicalPlan::NamedPath {
            input: Box::new(plan),
            path_var: path_name.clone(),
            element_vars,
        };
    }

    Ok((plan, new_node_vars, new_rel_vars))
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

/// Returns true if the expression contains any aggregation function.
fn expr_contains_aggregation(expr: &Expr) -> bool {
    match expr {
        Expr::FunctionCall(name, args) => {
            matches!(
                name.to_lowercase().as_str(),
                "count" | "sum" | "avg" | "min" | "max" | "collect" | "stdev" | "stdevp"
                | "percentilecont" | "percentiledisc"
            ) || args.iter().any(expr_contains_aggregation)
        }
        Expr::Property(inner, _) | Expr::IsNull(inner) | Expr::IsNotNull(inner)
        | Expr::Not(inner) | Expr::Neg(inner) => expr_contains_aggregation(inner),
        Expr::And(l, r) | Expr::Or(l, r) | Expr::Xor(l, r)
        | Expr::Arith(l, _, r)
        | Expr::Compare(l, _, r) | Expr::InList(l, r) | Expr::Subscript(l, r)
        | Expr::StartsWith(l, r) | Expr::EndsWith(l, r) | Expr::Contains(l, r)
        | Expr::Regex(l, r) => {
            expr_contains_aggregation(l) || expr_contains_aggregation(r)
        }
        Expr::List(elems) => elems.iter().any(expr_contains_aggregation),
        Expr::MapLiteral(pairs) => pairs.iter().any(|(_, v)| expr_contains_aggregation(v)),
        Expr::CaseSearched { branches, else_ } => {
            branches.iter().any(|(c, t)| expr_contains_aggregation(c) || expr_contains_aggregation(t))
                || else_.as_ref().is_some_and(|e| expr_contains_aggregation(e))
        }
        Expr::CaseSimple { test, branches, else_ } => {
            expr_contains_aggregation(test)
                || branches.iter().any(|(c, t)| expr_contains_aggregation(c) || expr_contains_aggregation(t))
                || else_.as_ref().is_some_and(|e| expr_contains_aggregation(e))
        }
        _ => false,
    }
}

/// Infer the VarKind of an expression for use after WITH re-scoping.
/// Only Variable passthrough preserves kind; everything else is Scalar.
fn expr_var_kind(expr: &Expr, kinds: &HashMap<String, VarKind>) -> VarKind {
    match expr {
        Expr::Variable(v) => kinds.get(v).cloned().unwrap_or(VarKind::Scalar),
        _ => VarKind::Scalar,
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
        LogicalPlan::NamedPath { input, path_var, element_vars } => {
            let evars = element_vars.join(", ");
            let child = explain(input, indent + 1);
            format!("{prefix}NamedPath({path_var} = [{evars}])\n{child}")
        }
        LogicalPlan::Apply { outer, inner } => {
            let o = explain(outer, indent + 1);
            let i = explain(inner, indent + 1);
            format!("{prefix}Apply\n{o}\n{i}")
        }
        LogicalPlan::Empty => format!("{prefix}Empty"),
        LogicalPlan::CreatePattern { input, .. } => {
            let child = explain(input, indent + 1);
            format!("{prefix}CreatePattern\n{child}")
        }
        LogicalPlan::SetProp { input, items } => {
            let child = explain(input, indent + 1);
            format!("{prefix}SetProp({} items)\n{child}", items.len())
        }
        LogicalPlan::RemoveProp { input, items } => {
            let child = explain(input, indent + 1);
            format!("{prefix}RemoveProp({} items)\n{child}", items.len())
        }
        LogicalPlan::DeleteNodes { input, detach, .. } => {
            let child = explain(input, indent + 1);
            let d = if *detach { "DETACH " } else { "" };
            format!("{prefix}{d}DeleteNodes\n{child}")
        }
        LogicalPlan::MergePattern { input, .. } => {
            let child = explain(input, indent + 1);
            format!("{prefix}MergePattern\n{child}")
        }
        LogicalPlan::Foreach { input, variable, .. } => {
            let child = explain(input, indent + 1);
            format!("{prefix}Foreach({variable} IN list)\n{child}")
        }
        LogicalPlan::Union { left, right, all } => {
            let l = explain(left, indent + 1);
            let r = explain(right, indent + 1);
            let kind = if *all { "ALL" } else { "" };
            format!("{prefix}Union{kind}\n{l}\n{r}")
        }
    }
}

/// Execute a pattern inline starting from the bound variables in `row`.
/// Used by pattern comprehensions: `[(n)-[:R]->(m) | expr]`.
pub fn exec_pattern_inline(
    pattern: &Pattern,
    row: &crate::cypher::executor::Row,
    params: &HashMap<String, serde_json::Value>,
) -> Result<(Vec<crate::cypher::executor::Row>, Vec<String>), PlanError> {
    use crate::cypher::executor::execute;
    let bound: HashSet<String> = row.keys().cloned().collect();
    let mut dummy_kinds: HashMap<String, VarKind> = HashMap::new();
    let (plan, _, _) = plan_pattern_onto(pattern, LogicalPlan::SingleRow, &bound, &mut dummy_kinds, false)?;
    // Inject outer row bindings into params so pre-bound variables (like `n` in `(n)-->()`)
    // are accessible during execution via the params lookup in exec_expand.
    let mut inner_params = params.clone();
    for (k, v) in row {
        inner_params.insert(k.clone(), v.to_json());
    }
    let inner_rows = execute(&plan, &inner_params)
        .map_err(|e| PlanError { message: e.message })?;
    // For each result row, merge with outer row bindings (outer vars don't overwrite inner).
    let merged: Vec<_> = inner_rows.into_iter().map(|mut r| {
        for (k, v) in row {
            r.entry(k.clone()).or_insert_with(|| v.clone());
        }
        r
    }).collect();
    // Re-derive element_vars using the same naming convention as plan_pattern_onto.
    // is_named = pattern.variable.is_some()
    let is_named = pattern.variable.is_some();
    let mut element_vars: Vec<String> = Vec::new();
    if is_named {
        let first_node = match pattern.elements.first() {
            Some(crate::cypher::ast::PatternElement::Node(n)) => n,
            _ => return Ok((merged, element_vars)),
        };
        let first_var = first_node.variable.clone().unwrap_or_else(|| "_anon_n0".to_string());
        element_vars.push(first_var);
        let mut anon_rel_counter: usize = 0;
        let mut i = 1usize;
        while i < pattern.elements.len() {
            if let crate::cypher::ast::PatternElement::Relationship(rel) = &pattern.elements[i] {
                let rel_var = if let Some(ref rv) = rel.variable {
                    rv.clone()
                } else {
                    let internal = format!("_pr_{}", anon_rel_counter);
                    anon_rel_counter += 1;
                    internal
                };
                element_vars.push(rel_var);
                let next_node = match pattern.elements.get(i + 1) {
                    Some(crate::cypher::ast::PatternElement::Node(n)) => n,
                    _ => break,
                };
                let dst_var = next_node.variable.clone()
                    .unwrap_or_else(|| format!("_anon_n{}", i + 1));
                element_vars.push(dst_var);
                i += 2;
            } else {
                i += 1;
            }
        }
    }
    Ok((merged, element_vars))
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
    let mut dummy_kinds: HashMap<String, VarKind> = HashMap::new();
    let (plan, _, _) = plan_pattern_onto(pattern, LogicalPlan::SingleRow, &bound, &mut dummy_kinds, false)?;
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
