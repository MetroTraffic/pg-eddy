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
    /// Variable is definitely not a node (from a literal, boolean expr, arithmetic, etc.).
    NotNode,
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
    /// LeftJoin: for each outer row, execute inner (left join / OPTIONAL MATCH semantics).
    /// If inner has results: emit merged rows. If inner is empty: emit outer row with
    /// null_vars set to null. Used for OPTIONAL MATCH where new variables are introduced.
    LeftJoin {
        outer: Box<LogicalPlan>,
        inner: Box<LogicalPlan>,
        null_vars: Vec<String>,
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
        if let (Some(lc), Some(rc)) = (left_cols, right_cols)
            && lc != rc
        {
            return Err(PlanError {
                message: format!(
                    "SyntaxError::DifferentColumnsInUnion: columns {:?} and {:?} differ",
                        lc, rc
                    ),
                });
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

/// Plan a subquery with knowledge of outer-scope bound variables.
/// Used for EXISTS { MATCH...RETURN } subqueries where outer vars like `n` are bound.
pub fn plan_with_outer(query: &Query, outer_vars: &HashSet<String>) -> Result<LogicalPlan, PlanError> {
    let mut bound_vars: HashSet<String> = outer_vars.clone();
    let mut var_kinds: HashMap<String, VarKind> = HashMap::new();
    // Outer vars are treated as Scalar (generic) — the executor provides the actual value.
    for v in outer_vars {
        var_kinds.insert(v.clone(), VarKind::Scalar);
    }
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
                // Pattern predicates in WITH projection are not allowed.
                for item in items.iter() {
                    if expr_has_inline_pattern(&item.expr) {
                        return Err(PlanError {
                            message: "SyntaxError::UnexpectedSyntax: pattern predicates \
                                      cannot be used in WITH projections".into(),
                        });
                    }
                }
                // Check for duplicate column names (ColumnNameConflict) and missing aliases.
                {
                    let mut seen_cols: std::collections::HashSet<String> = std::collections::HashSet::new();
                    for item in items.iter() {
                        // Non-variable/property expressions must have an alias.
                        if item.alias.is_none() {
                            let needs_alias = !matches!(item.expr,
                                Expr::Variable(_) | Expr::Star
                            );
                            if needs_alias {
                                return Err(PlanError {
                                    message: "SyntaxError: NoExpressionAlias — \
                                              expressions in WITH must be aliased".into(),
                                });
                            }
                        }
                        let col_name = if let Some(ref alias) = item.alias {
                            alias.clone()
                        } else if let Expr::Variable(ref v) = item.expr {
                            v.clone()
                        } else {
                            continue;
                        };
                        if !seen_cols.insert(col_name.clone()) {
                            return Err(PlanError {
                                message: format!(
                                    "SyntaxError: ColumnNameConflict — \
                                     multiple WITH columns with name '{col_name}'"
                                ),
                            });
                        }
                    }
                }
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
                // WITH WHERE handling:
                // - If the WITH has aggregation (e.g. count(*) AS c), the WHERE is a
                //   post-aggregation filter (HAVING) and must be applied AFTER projection.
                // - Otherwise, apply the WHERE BEFORE projection so pre-projected variables
                //   are in scope, and substitute projection aliases (e.g. `WITH a+b AS c WHERE c > 0`).
                if let Some(wh) = where_clause {
                    if !proj_has_agg {
                        // Non-aggregating WITH: substitute aliases and filter before projection.
                        let alias_map: HashMap<String, Expr> = items.iter()
                            .filter_map(|item| {
                                item.alias.as_ref().map(|alias| (alias.clone(), item.expr.clone()))
                            })
                            .collect();
                        let rewritten_wh = substitute_aliases_in_expr(wh, &alias_map);
                        current = LogicalPlan::Filter { input: Box::new(current), predicate: rewritten_wh };
                    }
                    // For aggregating WITH: WHERE will be added AFTER projection below.
                }
                current = LogicalPlan::Project {
                    input: Box::new(current),
                    items: items.clone(),
                    distinct: *distinct,
                    order_by: order_by.clone(),
                    skip: skip.clone(),
                    limit: limit.clone(),
                };
                let old_var_kinds = std::mem::take(var_kinds);
                bound_vars.clear();
                for item in items {
                    let exposed = item.alias.clone().or_else(|| {
                        if let Expr::Variable(v) = &item.expr { Some(v.clone()) } else { None }
                    });
                    if let Some(v) = exposed {
                        bound_vars.insert(v.clone());
                        // Infer kind from old_var_kinds (not yet-cleared var_kinds).
                        let kind = expr_var_kind(&item.expr, &old_var_kinds);
                        var_kinds.insert(v, kind);
                    }
                }
                // For aggregating WITH with WHERE: apply the WHERE as a post-aggregation
                // filter (HAVING) now that projected columns are in scope.
                if proj_has_agg {
                    if let Some(wh) = where_clause {
                        current = LogicalPlan::Filter { input: Box::new(current), predicate: wh.clone() };
                    }
                }
            }
            QueryClause::Return { distinct, items, order_by, skip, limit } => {
                // Check for RETURN * with no non-anonymous variables in scope.
                let has_star = items.iter().any(|i| matches!(i.expr, Expr::Star));
                if has_star {
                    let has_visible_vars = bound_vars.iter().any(|v| !v.starts_with('_'));
                    if !has_visible_vars {
                        return Err(PlanError {
                            message: "SyntaxError: NoVariablesInScope — \
                                      RETURN * requires at least one named variable in scope".to_string(),
                        });
                    }
                }
                // Check for undefined variable references in RETURN items.
                for item in items.iter() {
                    if let Expr::Variable(v) = &item.expr {
                        if !bound_vars.contains(v) {
                            return Err(PlanError {
                                message: format!(
                                    "SyntaxError: UndefinedVariable — \
                                     variable '{v}' not defined"
                                ),
                            });
                        }
                    }
                }
                // Check for nested aggregation in RETURN items.
                for item in items.iter() {
                    check_nested_aggregation(&item.expr, false)?;
                    check_quantifier_type_mismatch(&item.expr)?;
                }
                // Check: aggregation in ORDER BY without aggregation in RETURN projection
                // is a SyntaxError (InvalidAggregation). Mirrors the same check for WITH.
                {
                    let ret_has_agg = items.iter().any(|it| expr_contains_aggregation(&it.expr));
                    if !ret_has_agg {
                        for ob_item in order_by {
                            if expr_contains_aggregation(&ob_item.expr) {
                                return Err(PlanError {
                                    message: "SyntaxError: InvalidAggregation — aggregation in ORDER BY \
                                        without corresponding aggregation in RETURN projection".into(),
                                });
                            }
                        }
                    }
                }
                // Check: RETURN DISTINCT + ORDER BY can only reference projected variables.
                if *distinct && !order_by.is_empty() {
                    let mut projected_keys: HashSet<String> = HashSet::new();
                    for item in items.iter() {
                        if let Some(ref alias) = item.alias {
                            projected_keys.insert(alias.clone());
                        }
                        if let Expr::Variable(ref v) = item.expr {
                            projected_keys.insert(v.clone());
                        }
                    }
                    for ob_item in order_by {
                        check_expr_vars(&ob_item.expr, &projected_keys)?;
                    }
                }
                // Pattern predicates in RETURN/WITH projection are not allowed.
                for item in items.iter() {
                    if expr_has_inline_pattern(&item.expr) {
                        return Err(PlanError {
                            message: "SyntaxError::UnexpectedSyntax: pattern predicates \
                                      cannot be used in RETURN projections".into(),
                        });
                    }
                    // size(path_var) is InvalidArgumentType.
                    // labels(path|edge) is InvalidArgumentType.
                    // type(node|path) is InvalidArgumentType.
                    // length(node|edge) is InvalidArgumentType.
                    if let Expr::FunctionCall(ref fname, ref args) = item.expr {
                        let lname = fname.to_ascii_lowercase();
                        if args.len() == 1 {
                            if let Expr::Variable(ref v) = args[0] {
                                let vk = var_kinds.get(v);
                                match lname.as_str() {
                                    "size" if matches!(vk, Some(VarKind::Path)) => {
                                        return Err(PlanError { message: format!(
                                            "SyntaxError::InvalidArgumentType: size() cannot be used on path variable '{v}'"
                                        )});
                                    }
                                    "labels" if matches!(vk, Some(VarKind::Path | VarKind::Rel)) => {
                                        return Err(PlanError { message: format!(
                                            "SyntaxError: InvalidArgumentType — labels() cannot be used on path or relationship variable '{v}'"
                                        )});
                                    }
                                    "type" if matches!(vk, Some(VarKind::Node | VarKind::Path)) => {
                                        return Err(PlanError { message: format!(
                                            "SyntaxError: InvalidArgumentType — type() cannot be used on node or path variable '{v}'"
                                        )});
                                    }
                                    "length" if matches!(vk, Some(VarKind::Node | VarKind::Rel)) => {
                                        return Err(PlanError { message: format!(
                                            "SyntaxError: InvalidArgumentType — length() cannot be used on node or relationship variable '{v}'"
                                        )});
                                    }
                                    _ => {}
                                }
                            }
                        }
                    }
                }
                // Check for duplicate column names (ColumnNameConflict).
                {
                    let mut seen_cols: std::collections::HashSet<String> = std::collections::HashSet::new();
                    for (idx, item) in items.iter().enumerate() {
                        let col_name = if let Some(ref alias) = item.alias {
                            alias.clone()
                        } else if let Expr::Variable(ref v) = item.expr {
                            v.clone()
                        } else if let Expr::Property(ref base, ref k) = item.expr {
                            // Full default name: "base.key" so a.id and b.id don't clash.
                            if let Expr::Variable(ref bv) = **base {
                                format!("{bv}.{k}")
                            } else {
                                format!("_col{idx}")
                            }
                        } else {
                            continue; // unnamed expressions don't conflict
                        };
                        if !seen_cols.insert(col_name.clone()) {
                            return Err(PlanError {
                                message: format!(
                                    "SyntaxError: ColumnNameConflict — \
                                     multiple return columns with name '{col_name}'"
                                ),
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
            }
            // v0.12.0 write clauses
            QueryClause::Create { patterns } => {
                // Validate and bind CREATE patterns.
                for pattern in patterns {
                    // Track vars bound within this pattern (for multi-node chains like
                    // CREATE (a)-[:R]->(b {name: a.name}) where a is used in b's props).
                    let mut pattern_bound = bound_vars.clone();
                    for elem in &pattern.elements {
                        if let PatternElement::Relationship(r) = elem {
                            // Validate relationship type constraints for CREATE.
                            if r.rel_types.is_empty() {
                                return Err(PlanError {
                                    message: "SyntaxError: NoSingleRelationshipType — \
                                              CREATE relationship must have exactly one type".to_string(),
                                });
                            }
                            if r.rel_types.len() > 1 {
                                return Err(PlanError {
                                    message: "SyntaxError: NoSingleRelationshipType — \
                                              CREATE relationship cannot have multiple types".to_string(),
                                });
                            }
                            if r.length.is_some() {
                                return Err(PlanError {
                                    message: "SyntaxError: CreatingVarLength — \
                                              CREATE does not support variable-length relationships".to_string(),
                                });
                            }
                            // Validate inline property expressions use defined vars.
                            for (_, expr) in &r.properties {
                                check_expr_vars(expr, &pattern_bound)?;
                            }
                            if let Some(v) = &r.variable {
                                bound_vars.insert(v.clone());
                                pattern_bound.insert(v.clone());
                            }
                        }
                        if let PatternElement::Node(n) = elem {
                            // Validate inline property expressions use defined vars.
                            for (_, expr) in &n.properties {
                                check_expr_vars(expr, &pattern_bound)?;
                            }
                            if let Some(v) = &n.variable {
                                if bound_vars.contains(v) {
                                    // Already bound: error if node re-declares labels/props/map or is standalone
                                    let has_labels = !n.labels.is_empty();
                                    let has_props = !n.properties.is_empty() || n.has_explicit_map;
                                    let is_standalone = pattern.elements.len() == 1;
                                    if has_labels || has_props || is_standalone {
                                        return Err(PlanError {
                                            message: format!(
                                                "SyntaxError: VariableAlreadyBound — \
                                                 cannot CREATE node '{v}' that is already bound"
                                            ),
                                        });
                                    }
                                } else {
                                    bound_vars.insert(v.clone());
                                    pattern_bound.insert(v.clone());
                                }
                            }
                        }
                    }
                }
                current = LogicalPlan::CreatePattern {
                    input: Box::new(current),
                    patterns: patterns.clone(),
                };
            }
            QueryClause::Set { items } => {
                // Validate that all variable references in SET expressions are bound.
                for item in items {
                    match item {
                        crate::cypher::ast::SetItem::Property(target, value) => {
                            check_expr_vars(target, bound_vars)?;
                            check_expr_vars(value, bound_vars)?;
                            // Pattern predicates (inline pattern expressions) cannot appear
                            // in the right-hand side of a SET clause.
                            if expr_has_inline_pattern(value) {
                                return Err(PlanError {
                                    message: "SyntaxError: UnexpectedSyntax — pattern \
                                              predicates cannot be used in the right-hand \
                                              side of a SET clause".into(),
                                });
                            }
                        }
                        crate::cypher::ast::SetItem::Variable(var, value) => {
                            if !bound_vars.contains(var) {
                                return Err(PlanError { message: format!("SyntaxError: UndefinedVariable — variable '{var}' not defined") });
                            }
                            check_expr_vars(value, bound_vars)?;
                        }
                        crate::cypher::ast::SetItem::MergeMap(var, value) => {
                            if !bound_vars.contains(var) {
                                return Err(PlanError { message: format!("SyntaxError: UndefinedVariable — variable '{var}' not defined") });
                            }
                            check_expr_vars(value, bound_vars)?;
                        }
                        crate::cypher::ast::SetItem::Label(var, _) => {
                            if !bound_vars.contains(var) {
                                return Err(PlanError { message: format!("SyntaxError: UndefinedVariable — variable '{var}' not defined") });
                            }
                        }
                    }
                }
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
                // Validate DELETE expressions: must be variable references.
                for expr in exprs.iter() {
                    match expr {
                        Expr::Variable(v) => {
                            if !bound_vars.contains(v) {
                                return Err(PlanError {
                                    message: format!(
                                        "SyntaxError: UndefinedVariable — \
                                         variable '{v}' is not defined"
                                    ),
                                });
                            }
                        }
                        // Literal numeric/string/boolean/null cannot be nodes — reject at plan time.
                        Expr::IntLit(_) | Expr::FloatLit(_) | Expr::StringLit(_)
                        | Expr::BoolLit(_) | Expr::NullLit | Expr::List(_) | Expr::MapLiteral(_) => {
                            return Err(PlanError {
                                message: "SyntaxError: InvalidArgumentType — \
                                          DELETE expression must be a variable (node or relationship)".into(),
                            });
                        }
                        // HasLabel (e.g. DELETE n:Person) — cannot delete a label predicate.
                        Expr::HasLabel(_, _) => {
                            return Err(PlanError {
                                message: "SyntaxError: InvalidDelete — \
                                          cannot DELETE a label predicate; use REMOVE to remove labels".into(),
                            });
                        }
                        // Property access, function calls, subscript and other expressions
                        // that might yield nodes/rels are evaluated at runtime.
                        Expr::Property(_, _) | Expr::FunctionCall(_, _)
                        | Expr::Subscript(_, _) => {}
                        // Arithmetic, comparisons, and other non-node/rel expressions
                        // that weren't caught by the literal arm above.
                        Expr::Arith(_, _, _)
                        | Expr::Compare(_, _, _) | Expr::And(_, _) | Expr::Or(_, _)
                        | Expr::Not(_) | Expr::Neg(_) => {
                            return Err(PlanError {
                                message: "SyntaxError: InvalidArgumentType — \
                                          DELETE expression must be a node or relationship variable".into(),
                            });
                        }
                        // Other complex expressions are evaluated at runtime.
                        _ => {}
                    }
                }
                current = LogicalPlan::DeleteNodes {
                    input: Box::new(current),
                    exprs: exprs.clone(),
                    detach: *detach,
                };
            }
            QueryClause::Merge { pattern, on_create, on_match } => {
                // Validate MERGE pattern (similar to CREATE validation).
                for elem in &pattern.elements {
                    if let PatternElement::Relationship(r) = elem {
                        if r.rel_types.is_empty() {
                            return Err(PlanError {
                                message: "SyntaxError: NoSingleRelationshipType — \
                                          MERGE relationship must have exactly one type".to_string(),
                            });
                        }
                        if r.rel_types.len() > 1 {
                            return Err(PlanError {
                                message: "SyntaxError: NoSingleRelationshipType — \
                                          MERGE relationship cannot have multiple types".to_string(),
                            });
                        }
                        if r.length.is_some() {
                            return Err(PlanError {
                                message: "SyntaxError: CreatingVarLength — \
                                          MERGE does not support variable-length relationships".to_string(),
                            });
                        }
                        // Check rel var not already bound.
                        if let Some(ref v) = r.variable {
                            if bound_vars.contains(v) {
                                return Err(PlanError {
                                    message: format!(
                                        "SyntaxError: VariableAlreadyBound — \
                                         relationship '{v}' is already bound in MERGE"
                                    ),
                                });
                            }
                        }
                    }
                    // Check node vars: if re-bound standalone or with new labels, it's VariableAlreadyBound.
                    if let PatternElement::Node(n) = elem {
                        if let Some(ref v) = n.variable {
                            let is_standalone = pattern.elements.len() == 1;
                            if bound_vars.contains(v) && (is_standalone || !n.labels.is_empty()) {
                                return Err(PlanError {
                                    message: format!(
                                        "SyntaxError: VariableAlreadyBound — \
                                         cannot MERGE node '{v}' that is already bound"
                                    ),
                                });
                            }
                        }
                    }
                }
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
                // Path variable from MERGE p = (...) comes into scope.
                if let Some(ref pvar) = pattern.variable {
                    bound_vars.insert(pvar.clone());
                    var_kinds.insert(pvar.clone(), VarKind::Path);
                }
                // Validate ON CREATE and ON MATCH SET items.
                // Variables from the MERGE pattern are in scope for these items.
                let merge_bound = bound_vars.clone();
                for item in on_create.iter().chain(on_match.iter()) {
                    match item {
                        crate::cypher::ast::SetItem::Property(target, value) => {
                            check_expr_vars(target, &merge_bound)?;
                            check_expr_vars(value, &merge_bound)?;
                        }
                        crate::cypher::ast::SetItem::Variable(var, value) => {
                            if !merge_bound.contains(var) {
                                return Err(PlanError {
                                    message: format!("SyntaxError: UndefinedVariable — variable '{var}' not defined"),
                                });
                            }
                            check_expr_vars(value, &merge_bound)?;
                        }
                        crate::cypher::ast::SetItem::MergeMap(var, value) => {
                            if !merge_bound.contains(var) {
                                return Err(PlanError {
                                    message: format!("SyntaxError: UndefinedVariable — variable '{var}' not defined"),
                                });
                            }
                            check_expr_vars(value, &merge_bound)?;
                        }
                        crate::cypher::ast::SetItem::Label(var, _) => {
                            if !merge_bound.contains(var) {
                                return Err(PlanError {
                                    message: format!("SyntaxError: UndefinedVariable — variable '{var}' not defined"),
                                });
                            }
                        }
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

/// Build an OPTIONAL MATCH plan using LeftJoin semantics.
/// Used when the optional pattern introduces new unbound node variables.
/// The inner plan is built starting from SingleRow so that each outer row
/// is matched independently, preventing O(n_nodes)×optional null expansion.
fn plan_optional_match_left_join(
    current: LogicalPlan,
    patterns: &[Pattern],
    where_clause: &Option<Expr>,
    bound_vars: &mut HashSet<String>,
    var_kinds: &mut HashMap<String, VarKind>,
) -> Result<LogicalPlan, PlanError> {
    // Collect all variables that will be NEW (introduced by the optional patterns).
    // These are what we must null-fill if the inner plan produces no rows.
    let mut null_vars: Vec<String> = Vec::new();
    let mut inner_bound = bound_vars.clone();
    let mut inner_kinds = var_kinds.clone();
    let mut inner_plan = LogicalPlan::SingleRow;

    for pattern in patterns {
        let (new_plan, new_node_var_list, new_rel_var_list) =
            plan_pattern_onto(pattern, inner_plan, &inner_bound, &mut inner_kinds, false)?;
        for v in &new_node_var_list {
            if !null_vars.contains(v) { null_vars.push(v.clone()); }
            inner_bound.insert(v.clone());
        }
        for v in &new_rel_var_list {
            if !null_vars.contains(v) { null_vars.push(v.clone()); }
            inner_bound.insert(v.clone());
        }
        if let Some(ref pvar) = pattern.variable {
            if !null_vars.contains(pvar) { null_vars.push(pvar.clone()); }
            inner_bound.insert(pvar.clone());
            inner_kinds.insert(pvar.clone(), VarKind::Path);
        }
        inner_plan = new_plan;
    }

    if let Some(wh) = where_clause {
        inner_plan = LogicalPlan::Filter { input: Box::new(inner_plan), predicate: wh.clone() };
    }

    // Register all new vars in the outer bound_vars/var_kinds for subsequent clauses.
    for v in &null_vars {
        bound_vars.insert(v.clone());
        if let Some(k) = inner_kinds.get(v) {
            var_kinds.insert(v.clone(), k.clone());
        }
    }

    Ok(LogicalPlan::LeftJoin {
        outer: Box::new(current),
        inner: Box::new(inner_plan),
        null_vars,
    })
}

/// Return true if the expression contains an inline pattern predicate (Exists wrapping a MATCH).
fn expr_has_inline_pattern(expr: &Expr) -> bool {
    match expr {
        Expr::Exists { subquery, .. } => {
            subquery.clauses.len() == 1 && matches!(subquery.clauses[0], QueryClause::Match { .. })
        }
        Expr::And(a, b) | Expr::Or(a, b) | Expr::Xor(a, b) => {
            expr_has_inline_pattern(a) || expr_has_inline_pattern(b)
        }
        Expr::Not(e) | Expr::IsNull(e) | Expr::IsNotNull(e) | Expr::Neg(e) => {
            expr_has_inline_pattern(e)
        }
        Expr::FunctionCall(_, args) => args.iter().any(expr_has_inline_pattern),
        // Property access: expr.prop — recurse into base expression
        Expr::Property(base, _) => expr_has_inline_pattern(base),
        // Subscript / slice
        Expr::Subscript(base, idx) => expr_has_inline_pattern(base) || expr_has_inline_pattern(idx),
        Expr::ListSlice { list_expr, from, to } => {
            expr_has_inline_pattern(list_expr)
                || from.as_deref().map_or(false, expr_has_inline_pattern)
                || to.as_deref().map_or(false, expr_has_inline_pattern)
        }
        // Comparison chains
        Expr::Compare(base, _, rhs) => {
            expr_has_inline_pattern(base) || expr_has_inline_pattern(rhs)
        }
        // Binary arithmetic / string ops
        Expr::Arith(a, _, b)
        | Expr::InList(a, b)
        | Expr::StartsWith(a, b) | Expr::EndsWith(a, b) | Expr::Contains(a, b)
        | Expr::Regex(a, b) => expr_has_inline_pattern(a) || expr_has_inline_pattern(b),
        _ => false,
    }
}

/// Collect all named (non-anonymous) variables from a pattern.
fn collect_pattern_named_vars(pattern: &Pattern) -> Vec<String> {
    let mut vars = Vec::new();
    if let Some(ref v) = pattern.variable {
        vars.push(v.clone());
    }
    for elem in &pattern.elements {
        match elem {
            PatternElement::Node(n) => {
                if let Some(ref v) = n.variable {
                    if !v.starts_with('_') { vars.push(v.clone()); }
                }
            }
            PatternElement::Relationship(r) => {
                if let Some(ref v) = r.variable {
                    if !v.starts_with('_') { vars.push(v.clone()); }
                }
            }
        }
    }
    vars
}

/// Validate that inline pattern predicates embedded in an expression don't
/// introduce new named variables beyond what's in `bound_vars`.
/// Also validates that single-node-only patterns are rejected (InvalidArgumentType).
fn validate_inline_patterns_in_expr(expr: &Expr, bound_vars: &HashSet<String>) -> Result<(), PlanError> {
    match expr {
        Expr::Exists { subquery, implicit } => {
            // Check each MATCH clause in the subquery.
            for clause in &subquery.clauses {
                // Reject write clauses inside full EXISTS subqueries.
                if !*implicit && matches!(clause, QueryClause::Create { .. }
                    | QueryClause::Delete { .. }
                    | QueryClause::Set { .. }
                    | QueryClause::Remove { .. }
                    | QueryClause::Merge { .. }
                    | QueryClause::Foreach { .. })
                {
                    return Err(PlanError {
                        message: "SyntaxError: InvalidClauseComposition — \
                                  write clauses (CREATE/SET/DELETE/MERGE) are not \
                                  allowed inside an existential subquery".into(),
                    });
                }
                if let QueryClause::Match { patterns, where_clause, .. } = clause {
                    // Collect variables that are locally introduced by the patterns.
                    let mut local_vars: HashSet<String> = bound_vars.clone();
                    for pattern in patterns {
                        if *implicit {
                            // Implicit pattern predicates (inline `WHERE (n)-->(a)` form):
                            // Single-node patterns and new named variables are NOT allowed.
                            let has_rel = pattern.elements.iter().any(|e| matches!(e, PatternElement::Relationship(_)));
                            if !has_rel {
                                return Err(PlanError {
                                    message: "SyntaxError::InvalidArgumentType: a single node pattern \
                                              is not a valid existential predicate".into(),
                                });
                            }
                            // named NEW variables in the pattern are NOT allowed — they must
                            // already be in outer scope (openCypher Pattern1 rule).
                            for var in collect_pattern_named_vars(pattern) {
                                if !bound_vars.contains(&var) {
                                    return Err(PlanError {
                                        message: format!(
                                            "SyntaxError::UndefinedVariable: variable '{var}' \
                                             is not defined in the outer scope of a pattern predicate"
                                        ),
                                    });
                                }
                            }
                        } else {
                            // Explicit EXISTS { MATCH pattern WHERE clause } form:
                            // variables introduced by the pattern ARE in scope for the WHERE.
                            for var in collect_pattern_named_vars(pattern) {
                                local_vars.insert(var);
                            }
                        }
                    }
                    if let Some(wc) = where_clause {
                        validate_inline_patterns_in_expr(wc, &local_vars)?;
                    }
                }
            }
        }
        Expr::And(a, b) | Expr::Or(a, b) | Expr::Xor(a, b) => {
            validate_inline_patterns_in_expr(a, bound_vars)?;
            validate_inline_patterns_in_expr(b, bound_vars)?;
        }
        Expr::Not(e) | Expr::IsNull(e) | Expr::IsNotNull(e) | Expr::Neg(e) => {
            validate_inline_patterns_in_expr(e, bound_vars)?;
        }
        _ => {}
    }
    Ok(())
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
    // For OPTIONAL MATCH: if any pattern introduces a new (unbound) first node variable,
    // we must use LeftJoin semantics instead of CrossProduct+optional-expand.
    // With CrossProduct, each scanned value for the new variable gets its own optional
    // null row if the full pattern doesn't match — producing O(n_nodes) null rows instead
    // of the expected 1 null row per outer row.
    // LeftJoin executes the inner pattern freshly for each outer row, with null for
    // all new variables if the inner produces zero rows.
    //
    // Also use LeftJoin when there is a WHERE clause on the OPTIONAL MATCH. Even if
    // the first node is bound, the WHERE could filter out ALL expanded rows, in which
    // case the outer row must be preserved with nulls — not discarded.
    //
    // Also use LeftJoin for multi-hop patterns (more than one relationship step) so
    // that if any intermediate hop fails, ALL new variables are consistently null'd
    // rather than partially set by the per-hop optional expand logic.
    if optional {
        // Count relationship hops across all patterns in this OPTIONAL MATCH.
        let hop_count: usize = patterns.iter()
            .map(|pat| pat.elements.iter().filter(|e| matches!(e, crate::cypher::ast::PatternElement::Relationship(_))).count())
            .sum();
        let needs_left_join = where_clause.is_some() || hop_count > 1 || patterns.iter().any(|pat| {
            if let Some(crate::cypher::ast::PatternElement::Node(n)) = pat.elements.first() {
                // First node is unbound if it has no variable OR its variable is not in bound_vars.
                match &n.variable {
                    None => true,
                    Some(v) => !bound_vars.contains(v),
                }
            } else {
                false
            }
        });
        if needs_left_join {
            return plan_optional_match_left_join(current, patterns, where_clause, bound_vars, var_kinds);
        }
    }

    let mut plan = current;
    let mut new_node_vars: Vec<String> = Vec::new();
    let mut new_rel_vars: Vec<String> = Vec::new();

    for pattern in patterns {
        // If the pattern has a path variable, check it isn't already bound.
        if pattern.variable.as_ref().is_some_and(|pvar| var_kinds.contains_key(pvar)
            || bound_vars.contains(pvar))
        {
            let pvar = pattern.variable.as_deref().unwrap();
            return Err(PlanError {
                message: format!("SyntaxError: VariableAlreadyBound — '{pvar}' is already bound"),
            });
        }
        // Check that the path variable doesn't conflict with node/rel variables in the same pattern.
        if let Some(ref pvar) = pattern.variable {
            for elem in &pattern.elements {
                let conflicts = match elem {
                    PatternElement::Node(n) => n.variable.as_deref() == Some(pvar.as_str()),
                    PatternElement::Relationship(r) => r.variable.as_deref() == Some(pvar.as_str()),
                };
                if conflicts {
                    return Err(PlanError {
                        message: format!(
                            "SyntaxError: VariableAlreadyBound — path variable '{pvar}' \
                             conflicts with a node/relationship variable in the same pattern"
                        ),
                    });
                }
            }
        }
        let (new_plan, new_node_var_list, new_rel_var_list) =
            plan_pattern_onto(pattern, plan, bound_vars, var_kinds, optional)?;
        for v in &new_node_var_list {
            if !new_node_vars.contains(v) { new_node_vars.push(v.clone()); }
            bound_vars.insert(v.clone());
        }
        for v in &new_rel_var_list {
            if !new_rel_vars.contains(v) { new_rel_vars.push(v.clone()); }
            bound_vars.insert(v.clone());
        }
        if let Some(ref pvar) = pattern.variable {
            bound_vars.insert(pvar.clone());
            var_kinds.insert(pvar.clone(), VarKind::Path);
        }
        plan = new_plan;
    }

    // Validate WHERE clause: no new named vars in inline pattern predicates;
    // also check that all variable references are bound, no aggregations, no path property access.
    if let Some(wc) = where_clause {
        validate_inline_patterns_in_expr(wc, bound_vars)?;
        check_expr_vars(wc, bound_vars)?;
        if contains_aggregation(wc) {
            return Err(PlanError {
                message: "SyntaxError: InvalidAggregation — \
                          aggregation functions are not allowed in WHERE".to_string(),
            });
        }
        check_path_property_access(wc, var_kinds)?;
        check_where_is_boolean(wc, var_kinds)?;
    }

    // Relationship isomorphism: different relationship variables in the same MATCH clause
    // must not be bound to the same relationship (openCypher relationship uniqueness).
    let rel_iso_filter = build_rel_isomorphism_filter(&new_rel_vars);

    // In openCypher, different node variables CAN bind to the same node (no node isomorphism).
    let combined = match (rel_iso_filter, where_clause.clone()) {
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
    // Allow Scalar (could be a node from an expression like coalesce) and Node.
    // Reject explicit non-node kinds: Rel, Path, and NotNode.
    if first_node.variable.is_some()
        && matches!(var_kinds.get(&first_var), Some(VarKind::Rel | VarKind::Path | VarKind::NotNode))
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
        // But apply any label/property constraints from the pattern as a filter.
        let mut p = current;
        if !first_node.labels.is_empty() {
            let filter_expr = Expr::HasLabel(
                Box::new(Expr::Variable(first_var.clone())),
                first_node.labels.clone(),
            );
            p = LogicalPlan::Filter { input: Box::new(p), predicate: filter_expr };
        }
        if !first_node.properties.is_empty() {
            for (key, expr) in &first_node.properties {
                let prop_eq = Expr::Compare(
                    Box::new(Expr::Property(Box::new(Expr::Variable(first_var.clone())), key.clone())),
                    CmpOp::Eq,
                    Box::new(expr.clone()),
                );
                p = LogicalPlan::Filter { input: Box::new(p), predicate: prop_eq };
            }
        }
        p
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
        let mut p = match current {
            LogicalPlan::SingleRow => scan,
            other => LogicalPlan::CrossProduct { left: Box::new(other), right: Box::new(scan) },
        };
        // If the node has multiple labels, add Filter for the extra ones.
        if first_node.labels.len() > 1 {
            for extra_label in &first_node.labels[1..] {
                let filter_expr = Expr::HasLabel(
                    Box::new(Expr::Variable(first_var.clone())),
                    vec![extra_label.clone()],
                );
                p = LogicalPlan::Filter { predicate: filter_expr, input: Box::new(p) };
            }
        }
        p
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
                // Detect same relationship variable used twice in the same pattern.
                if new_rel_vars.contains(rv) {
                    return Err(PlanError {
                        message: format!(
                            "SyntaxError: RelationshipUniquenessViolation — \
                             relationship variable '{rv}' appears more than once in the same pattern"
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
            // Allow Scalar (could be a node from expression). Reject explicit Rel/Path/NotNode.
            if next_node.variable.is_some()
                && matches!(var_kinds.get(&dst_var), Some(VarKind::Rel | VarKind::Path | VarKind::NotNode))
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
                // If the relationship has inline property predicates, we need a
                // rel list variable so we can apply an `all(r IN rels WHERE …)`
                // filter post-BFS. Synthesize one if the pattern does not name
                // the relationship explicitly.
                let synth_rel_var = if !rel.properties.is_empty() && rel.variable.is_none() && !is_named {
                    Some(format!("_anon_vrel_{}", i))
                } else {
                    None
                };
                let effective_rel_var = if is_named {
                    None
                } else {
                    rel.variable.clone().or_else(|| synth_rel_var.clone())
                };
                plan = LogicalPlan::VarLengthExpand {
                    input: Box::new(plan),
                    src_var,
                    rel_var: effective_rel_var.clone(),
                    dst_var: dst_var.clone(),
                    rel_types: rel.rel_types.clone(),
                    direction: rel.direction,
                    min_hops: vl.min,
                    max_hops: vl.max,
                    optional,
                    path_carry_var: path_carry,
                };
                // Apply relationship property predicates: each edge in the path
                // must satisfy the inline property map. Emit as
                // `all(_r IN rel_var WHERE _r.key = value AND ...)`.
                if !rel.properties.is_empty()
                    && let Some(rv) = effective_rel_var.as_ref()
                {
                    let iter_var = format!("_r_{}", i);
                    let mut pred: Option<Expr> = None;
                    for (key, val_expr) in &rel.properties {
                        let cmp = Expr::Compare(
                            Box::new(Expr::Property(
                                Box::new(Expr::Variable(iter_var.clone())),
                                key.clone(),
                            )),
                            CmpOp::Eq,
                            Box::new(val_expr.clone()),
                        );
                        pred = Some(match pred {
                            Some(p) => Expr::And(Box::new(p), Box::new(cmp)),
                            None => cmp,
                        });
                    }
                    if let Some(predicate) = pred {
                        let all_expr = Expr::ListPredicate {
                            kind: crate::cypher::ast::ListPredicateKind::All,
                            variable: iter_var,
                            list_expr: Box::new(Expr::Variable(rv.clone())),
                            predicate: Box::new(predicate),
                        };
                        plan = LogicalPlan::Filter { input: Box::new(plan), predicate: all_expr };
                    }
                }
                // Var-length expand does not apply dst node label/property predicates.
                // Emit explicit filter expressions on the bound destination so they
                // are enforced post-traversal.
                for label in &next_node.labels {
                    let filt = Expr::HasLabel(
                        Box::new(Expr::Variable(dst_var.clone())),
                        vec![label.clone()],
                    );
                    plan = LogicalPlan::Filter { input: Box::new(plan), predicate: filt };
                }
                for (key, val_expr) in &next_node.properties {
                    let filt = Expr::Compare(
                        Box::new(Expr::Property(
                            Box::new(Expr::Variable(dst_var.clone())),
                            key.clone(),
                        )),
                        CmpOp::Eq,
                        Box::new(val_expr.clone()),
                    );
                    plan = LogicalPlan::Filter { input: Box::new(plan), predicate: filt };
                }
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
        | Expr::Not(inner) | Expr::Neg(inner) | Expr::HasLabel(inner, _) => expr_contains_aggregation(inner),
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

/// Substitute projection aliases in an expression. Used for WITH WHERE rewriting:
/// when WHERE references a projection alias defined in the same WITH clause,
/// replace the variable reference with the underlying expression so it can be
/// evaluated in the pre-projection scope.
fn substitute_aliases_in_expr(expr: &Expr, alias_map: &HashMap<String, Expr>) -> Expr {
    use Expr::*;
    match expr {
        Variable(v) => {
            if let Some(replacement) = alias_map.get(v) {
                replacement.clone()
            } else {
                expr.clone()
            }
        }
        Property(base, key) => Property(
            Box::new(substitute_aliases_in_expr(base, alias_map)),
            key.clone(),
        ),
        Compare(l, op, r) => Compare(
            Box::new(substitute_aliases_in_expr(l, alias_map)),
            *op,
            Box::new(substitute_aliases_in_expr(r, alias_map)),
        ),
        And(l, r) => And(
            Box::new(substitute_aliases_in_expr(l, alias_map)),
            Box::new(substitute_aliases_in_expr(r, alias_map)),
        ),
        Or(l, r) => Or(
            Box::new(substitute_aliases_in_expr(l, alias_map)),
            Box::new(substitute_aliases_in_expr(r, alias_map)),
        ),
        Xor(l, r) => Xor(
            Box::new(substitute_aliases_in_expr(l, alias_map)),
            Box::new(substitute_aliases_in_expr(r, alias_map)),
        ),
        Not(e) => Not(Box::new(substitute_aliases_in_expr(e, alias_map))),
        IsNull(e) => IsNull(Box::new(substitute_aliases_in_expr(e, alias_map))),
        IsNotNull(e) => IsNotNull(Box::new(substitute_aliases_in_expr(e, alias_map))),
        Arith(l, op, r) => Arith(
            Box::new(substitute_aliases_in_expr(l, alias_map)),
            *op,
            Box::new(substitute_aliases_in_expr(r, alias_map)),
        ),
        Neg(e) => Neg(Box::new(substitute_aliases_in_expr(e, alias_map))),
        FunctionCall(name, args) => FunctionCall(
            name.clone(),
            args.iter().map(|a| substitute_aliases_in_expr(a, alias_map)).collect(),
        ),
        InList(l, r) => InList(
            Box::new(substitute_aliases_in_expr(l, alias_map)),
            Box::new(substitute_aliases_in_expr(r, alias_map)),
        ),
        StartsWith(l, r) => StartsWith(
            Box::new(substitute_aliases_in_expr(l, alias_map)),
            Box::new(substitute_aliases_in_expr(r, alias_map)),
        ),
        EndsWith(l, r) => EndsWith(
            Box::new(substitute_aliases_in_expr(l, alias_map)),
            Box::new(substitute_aliases_in_expr(r, alias_map)),
        ),
        Contains(l, r) => Contains(
            Box::new(substitute_aliases_in_expr(l, alias_map)),
            Box::new(substitute_aliases_in_expr(r, alias_map)),
        ),
        Regex(l, r) => Regex(
            Box::new(substitute_aliases_in_expr(l, alias_map)),
            Box::new(substitute_aliases_in_expr(r, alias_map)),
        ),
        Subscript(l, r) => Subscript(
            Box::new(substitute_aliases_in_expr(l, alias_map)),
            Box::new(substitute_aliases_in_expr(r, alias_map)),
        ),
        HasLabel(e, labels) => HasLabel(
            Box::new(substitute_aliases_in_expr(e, alias_map)),
            labels.clone(),
        ),
        List(items) => List(
            items.iter().map(|e| substitute_aliases_in_expr(e, alias_map)).collect(),
        ),
        MapLiteral(pairs) => MapLiteral(
            pairs.iter().map(|(k, v)| (k.clone(), substitute_aliases_in_expr(v, alias_map))).collect(),
        ),
        ListSlice { list_expr, from, to } => ListSlice {
            list_expr: Box::new(substitute_aliases_in_expr(list_expr, alias_map)),
            from: from.as_ref().map(|e| Box::new(substitute_aliases_in_expr(e, alias_map))),
            to: to.as_ref().map(|e| Box::new(substitute_aliases_in_expr(e, alias_map))),
        },
        CaseSearched { branches, else_ } => CaseSearched {
            branches: branches.iter().map(|(cond, val)| (
                substitute_aliases_in_expr(cond, alias_map),
                substitute_aliases_in_expr(val, alias_map),
            )).collect(),
            else_: else_.as_ref().map(|e| Box::new(substitute_aliases_in_expr(e, alias_map))),
        },
        CaseSimple { test, branches, else_ } => CaseSimple {
            test: Box::new(substitute_aliases_in_expr(test, alias_map)),
            branches: branches.iter().map(|(cond, val)| (
                substitute_aliases_in_expr(cond, alias_map),
                substitute_aliases_in_expr(val, alias_map),
            )).collect(),
            else_: else_.as_ref().map(|e| Box::new(substitute_aliases_in_expr(e, alias_map))),
        },
        ListComprehension { variable, list_expr, predicate, projection } => ListComprehension {
            variable: variable.clone(),
            list_expr: Box::new(substitute_aliases_in_expr(list_expr, alias_map)),
            predicate: predicate.as_ref().map(|e| Box::new(substitute_aliases_in_expr(e, alias_map))),
            projection: projection.as_ref().map(|e| Box::new(substitute_aliases_in_expr(e, alias_map))),
        },
        ListPredicate { kind, variable, list_expr, predicate } => ListPredicate {
            kind: *kind,
            variable: variable.clone(),
            list_expr: Box::new(substitute_aliases_in_expr(list_expr, alias_map)),
            predicate: Box::new(substitute_aliases_in_expr(predicate, alias_map)),
        },
        // Leaf nodes and complex subqueries: return as-is
        IntLit(_) | FloatLit(_) | StringLit(_) | BoolLit(_) | NullLit | Star | Parameter(_) => expr.clone(),
        ShortestPath { .. } | PatternComprehension { .. } | Exists { .. } => expr.clone(),
    }
}

/// Infer the VarKind of an expression for use after WITH re-scoping.
/// Only Variable passthrough preserves kind; everything else is Scalar.
fn expr_var_kind(expr: &Expr, kinds: &HashMap<String, VarKind>) -> VarKind {
    match expr {
        Expr::Variable(v) => kinds.get(v).cloned().unwrap_or(VarKind::Scalar),
        _ if expr_is_definitely_not_node(expr, kinds) => VarKind::NotNode,
        _ => VarKind::Scalar,
    }
}

/// Returns true if the expression is guaranteed to never produce a graph node.
/// This is used for compile-time VariableTypeConflict detection.
fn expr_is_definitely_not_node(expr: &Expr, kinds: &HashMap<String, VarKind>) -> bool {
    match expr {
        // Literal types that are never nodes (NullLit is excluded: null is valid as a node-var result from OPTIONAL MATCH)
        Expr::IntLit(_) | Expr::FloatLit(_) | Expr::StringLit(_) | Expr::BoolLit(_) => true,
        // Collections — never nodes
        Expr::List(_) | Expr::MapLiteral(_) => true,
        // Boolean expressions — always bool, never node
        Expr::And(_, _) | Expr::Or(_, _) | Expr::Not(_) | Expr::Xor(_, _) => true,
        Expr::Compare(_, _, _) | Expr::IsNull(_) | Expr::IsNotNull(_) | Expr::HasLabel(_, _) => true,
        Expr::InList(_, _) | Expr::StartsWith(_, _) | Expr::EndsWith(_, _)
            | Expr::Contains(_, _) | Expr::Regex(_, _) => true,
        // Arithmetic — always numeric
        Expr::Arith(_, _, _) | Expr::Neg(_) => true,
        // Variables: check their kind
        Expr::Variable(v) => matches!(kinds.get(v), Some(VarKind::NotNode | VarKind::Rel | VarKind::Path)),
        // Everything else (function calls, CASE, property access, coalesce, etc.) might be a node
        _ => false,
    }
}

/// Recursively check that all `Expr::Variable` references in an expression
/// are present in `bound_vars`. Ignores internal anonymous vars (`_anon_*`).
fn check_expr_vars(expr: &Expr, bound_vars: &HashSet<String>) -> Result<(), PlanError> {
    match expr {
        Expr::Variable(v) => {
            if !v.starts_with("_anon_") && !v.starts_with("_pr_") && !bound_vars.contains(v) {
                return Err(PlanError {
                    message: format!("SyntaxError: UndefinedVariable — variable '{v}' not defined"),
                });
            }
            Ok(())
        }
        Expr::Property(base, _) => check_expr_vars(base, bound_vars),
        Expr::Arith(l, _, r) | Expr::Compare(l, _, r) | Expr::And(l, r)
        | Expr::Or(l, r) | Expr::Xor(l, r) | Expr::StartsWith(l, r)
        | Expr::EndsWith(l, r) | Expr::Contains(l, r) => {
            check_expr_vars(l, bound_vars)?;
            check_expr_vars(r, bound_vars)
        }
        Expr::Not(e) | Expr::Neg(e) | Expr::IsNull(e) | Expr::IsNotNull(e)
        | Expr::HasLabel(e, _) => check_expr_vars(e, bound_vars),
        Expr::FunctionCall(_, args) => {
            for a in args { check_expr_vars(a, bound_vars)?; }
            Ok(())
        }
        Expr::List(items) => {
            for i in items { check_expr_vars(i, bound_vars)?; }
            Ok(())
        }
        Expr::MapLiteral(pairs) => {
            for (_, v) in pairs { check_expr_vars(v, bound_vars)?; }
            Ok(())
        }
        Expr::Subscript(base, idx) | Expr::ListSlice { list_expr: base, from: Some(idx), .. } => {
            check_expr_vars(base, bound_vars)?;
            check_expr_vars(idx, bound_vars)
        }
        Expr::ListSlice { list_expr, to: Some(t), .. } => {
            check_expr_vars(list_expr, bound_vars)?;
            check_expr_vars(t, bound_vars)
        }
        Expr::ListSlice { list_expr, .. } => check_expr_vars(list_expr, bound_vars),
        Expr::CaseSearched { branches, else_ } => {
            for (c, t) in branches { check_expr_vars(c, bound_vars)?; check_expr_vars(t, bound_vars)?; }
            if let Some(e) = else_ { check_expr_vars(e, bound_vars)?; }
            Ok(())
        }
        Expr::CaseSimple { test, branches, else_ } => {
            check_expr_vars(test, bound_vars)?;
            for (w, t) in branches { check_expr_vars(w, bound_vars)?; check_expr_vars(t, bound_vars)?; }
            if let Some(e) = else_ { check_expr_vars(e, bound_vars)?; }
            Ok(())
        }
        // Literals and structural keywords — no variables
        _ => Ok(()),
    }
}

/// Return true if `expr` contains an aggregation function call at any depth.
fn contains_aggregation(expr: &Expr) -> bool {
    match expr {
        Expr::FunctionCall(name, args) => {
            if is_aggregate_fn(name) { return true; }
            args.iter().any(contains_aggregation)
        }
        Expr::Arith(l, _, r) | Expr::Compare(l, _, r) | Expr::And(l, r)
        | Expr::Or(l, r) | Expr::Xor(l, r) | Expr::StartsWith(l, r)
        | Expr::EndsWith(l, r) | Expr::Contains(l, r) | Expr::InList(l, r) => {
            contains_aggregation(l) || contains_aggregation(r)
        }
        Expr::Not(e) | Expr::Neg(e) | Expr::IsNull(e) | Expr::IsNotNull(e)
        | Expr::HasLabel(e, _) | Expr::Property(e, _) => contains_aggregation(e),
        Expr::List(items) => items.iter().any(contains_aggregation),
        Expr::MapLiteral(pairs) => pairs.iter().any(|(_, v)| contains_aggregation(v)),
        Expr::Subscript(b, i) => contains_aggregation(b) || contains_aggregation(i),
        _ => false,
    }
}

/// Return whether a function name is an aggregate function.
fn is_aggregate_fn(name: &str) -> bool {
    let lc = name.to_ascii_lowercase();
    matches!(lc.as_str(),
        "count" | "count_distinct" | "sum" | "sum_distinct"
        | "avg" | "avg_distinct" | "min" | "max"
        | "collect" | "collect_distinct" | "stdev" | "stdevp"
        | "percentilecont" | "percentiledisc"
    )
}

/// Check that a property access like `r.name` doesn't target a path variable.
fn check_path_property_access(expr: &Expr, var_kinds: &HashMap<String, VarKind>) -> Result<(), PlanError> {
    match expr {
        Expr::Property(base, _) => {
            if let Expr::Variable(v) = base.as_ref() {
                if matches!(var_kinds.get(v), Some(VarKind::Path)) {
                    return Err(PlanError {
                        message: format!(
                            "SyntaxError: InvalidArgumentType — \
                             property access on path variable '{v}' is not supported"
                        ),
                    });
                }
            }
            check_path_property_access(base, var_kinds)
        }
        Expr::Arith(l, _, r) | Expr::Compare(l, _, r) | Expr::And(l, r)
        | Expr::Or(l, r) | Expr::Xor(l, r) | Expr::StartsWith(l, r)
        | Expr::EndsWith(l, r) | Expr::Contains(l, r) | Expr::InList(l, r) => {
            check_path_property_access(l, var_kinds)?;
            check_path_property_access(r, var_kinds)
        }
        Expr::Not(e) | Expr::Neg(e) | Expr::IsNull(e) | Expr::IsNotNull(e)
        | Expr::HasLabel(e, _) => check_path_property_access(e, var_kinds),
        Expr::FunctionCall(_, args) => {
            for a in args { check_path_property_access(a, var_kinds)?; }
            Ok(())
        }
        Expr::List(items) => {
            for i in items { check_path_property_access(i, var_kinds)?; }
            Ok(())
        }
        Expr::MapLiteral(pairs) => {
            for (_, v) in pairs { check_path_property_access(v, var_kinds)?; }
            Ok(())
        }
        Expr::Subscript(b, i) => {
            check_path_property_access(b, var_kinds)?;
            check_path_property_access(i, var_kinds)
        }
        _ => Ok(()),
    }
}

/// Check for nested aggregation: aggregation inside aggregation args → NestedAggregation.
/// `inside_agg` is true when we're already inside an aggregation function call.
fn check_nested_aggregation(expr: &Expr, inside_agg: bool) -> Result<(), PlanError> {
    match expr {
        Expr::FunctionCall(name, args) => {
            let is_agg = is_aggregate_fn(name);
            if is_agg && inside_agg {
                return Err(PlanError {
                    message: "SyntaxError: NestedAggregation — \
                              aggregation functions cannot be nested".to_string(),
                });
            }
            // Non-deterministic functions (rand()) inside aggregates are forbidden.
            if inside_agg && name.to_ascii_lowercase() == "rand" {
                return Err(PlanError {
                    message: "SyntaxError: NonConstantExpression — \
                              non-deterministic function rand() cannot be used inside \
                              an aggregation function".to_string(),
                });
            }
            for a in args {
                check_nested_aggregation(a, inside_agg || is_agg)?;
            }
            Ok(())
        }
        Expr::Arith(l, _, r) | Expr::Compare(l, _, r) | Expr::And(l, r)
        | Expr::Or(l, r) | Expr::Xor(l, r) | Expr::StartsWith(l, r)
        | Expr::EndsWith(l, r) | Expr::Contains(l, r) | Expr::InList(l, r) => {
            check_nested_aggregation(l, inside_agg)?;
            check_nested_aggregation(r, inside_agg)
        }
        Expr::Not(e) | Expr::Neg(e) | Expr::IsNull(e) | Expr::IsNotNull(e)
        | Expr::HasLabel(e, _) | Expr::Property(e, _) => check_nested_aggregation(e, inside_agg),
        Expr::List(items) => {
            for i in items { check_nested_aggregation(i, inside_agg)?; }
            Ok(())
        }
        Expr::MapLiteral(pairs) => {
            for (_, v) in pairs { check_nested_aggregation(v, inside_agg)?; }
            Ok(())
        }
        Expr::Subscript(b, i) => {
            check_nested_aggregation(b, inside_agg)?;
            check_nested_aggregation(i, inside_agg)
        }
        _ => Ok(()),
    }
}

/// Static type-mismatch check for list quantifiers (any/all/none/single).
///
/// When the list expression is a literal whose elements are all of a single
/// non-numeric primitive type (String or Bool), an arithmetic operation
/// applied to the iteration variable in the predicate is an `InvalidArgumentType`
/// compile-time error per openCypher 9 §6.5.
fn check_quantifier_type_mismatch(expr: &Expr) -> Result<(), PlanError> {
    match expr {
        Expr::ListPredicate { variable, list_expr, predicate, .. } => {
            check_quantifier_type_mismatch(list_expr)?;
            if let Some(elem_ty) = infer_homogeneous_list_type(list_expr)
                && !matches!(elem_ty, PrimTy::Int | PrimTy::Float)
                && predicate_arith_on_var(predicate, variable)
            {
                return Err(PlanError {
                    message: format!(
                        "SyntaxError: InvalidArgumentType — \
                         arithmetic operation on iteration variable '{variable}' \
                         whose list elements are of type {elem_ty:?}"
                    ),
                });
            }
            check_quantifier_type_mismatch(predicate)
        }
        Expr::Arith(l, _, r) | Expr::Compare(l, _, r) | Expr::And(l, r)
        | Expr::Or(l, r) | Expr::Xor(l, r) | Expr::StartsWith(l, r)
        | Expr::EndsWith(l, r) | Expr::Contains(l, r) | Expr::InList(l, r) => {
            check_quantifier_type_mismatch(l)?;
            check_quantifier_type_mismatch(r)
        }
        Expr::Not(e) | Expr::Neg(e) | Expr::IsNull(e) | Expr::IsNotNull(e)
        | Expr::HasLabel(e, _) | Expr::Property(e, _) => check_quantifier_type_mismatch(e),
        Expr::FunctionCall(_, args) | Expr::List(args) => {
            for a in args { check_quantifier_type_mismatch(a)?; }
            Ok(())
        }
        Expr::MapLiteral(pairs) => {
            for (_, v) in pairs { check_quantifier_type_mismatch(v)?; }
            Ok(())
        }
        Expr::Subscript(b, i) => {
            check_quantifier_type_mismatch(b)?;
            check_quantifier_type_mismatch(i)
        }
        _ => Ok(()),
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum PrimTy { Int, Float, String, Bool }

/// If `expr` is a literal list whose elements are all the same primitive type,
/// return that type; otherwise None.
fn infer_homogeneous_list_type(expr: &Expr) -> Option<PrimTy> {
    let items = match expr {
        Expr::List(items) => items,
        _ => return None,
    };
    if items.is_empty() { return None; }
    let mut ty: Option<PrimTy> = None;
    for it in items {
        let t = match it {
            Expr::IntLit(_) => PrimTy::Int,
            Expr::FloatLit(_) => PrimTy::Float,
            Expr::StringLit(_) => PrimTy::String,
            Expr::BoolLit(_) => PrimTy::Bool,
            _ => return None,
        };
        match ty {
            None => ty = Some(t),
            Some(prev) if prev == t => {}
            _ => return None,
        }
    }
    ty
}

/// Returns true if `expr` contains an arithmetic operation whose operand
/// directly references the variable `var`.
fn predicate_arith_on_var(expr: &Expr, var: &str) -> bool {
    match expr {
        Expr::Arith(l, _, r) => {
            is_var_ref(l, var) || is_var_ref(r, var)
                || predicate_arith_on_var(l, var) || predicate_arith_on_var(r, var)
        }
        Expr::Compare(l, _, r) | Expr::And(l, r) | Expr::Or(l, r) | Expr::Xor(l, r)
        | Expr::StartsWith(l, r) | Expr::EndsWith(l, r) | Expr::Contains(l, r)
        | Expr::InList(l, r) => predicate_arith_on_var(l, var) || predicate_arith_on_var(r, var),
        Expr::Not(e) | Expr::Neg(e) | Expr::IsNull(e) | Expr::IsNotNull(e)
        | Expr::HasLabel(e, _) | Expr::Property(e, _) => predicate_arith_on_var(e, var),
        Expr::FunctionCall(_, args) | Expr::List(args) => {
            args.iter().any(|a| predicate_arith_on_var(a, var))
        }
        Expr::MapLiteral(pairs) => pairs.iter().any(|(_, v)| predicate_arith_on_var(v, var)),
        Expr::Subscript(b, i) => predicate_arith_on_var(b, var) || predicate_arith_on_var(i, var),
        Expr::ListPredicate { list_expr, predicate, variable, .. } => {
            // Shadowing: an inner predicate with the same iteration variable
            // doesn't reference the outer one's binding inside its predicate.
            predicate_arith_on_var(list_expr, var)
                || (variable != var && predicate_arith_on_var(predicate, var))
        }
        _ => false,
    }
}

fn is_var_ref(expr: &Expr, var: &str) -> bool {
    matches!(expr, Expr::Variable(v) if v == var)
}

/// Reject a WHERE expression that is statically known to evaluate to a graph
/// entity (Node, Relationship, Path) rather than a boolean. Currently fires
/// when the top-level expression is a bare `Variable` reference to such a
/// binding — e.g., `MATCH (n) WHERE (n) RETURN n` (Pattern1 [11]).
fn check_where_is_boolean(
    expr: &Expr,
    var_kinds: &HashMap<String, VarKind>,
) -> Result<(), PlanError> {
    if let Expr::Variable(name) = expr
        && let Some(kind) = var_kinds.get(name)
        && matches!(kind, VarKind::Node | VarKind::Rel | VarKind::Path)
    {
        return Err(PlanError {
            message: format!(
                "SyntaxError: InvalidArgumentType — \
                 WHERE expression '{name}' refers to a graph entity, not a boolean"
            ),
        });
    }
    Ok(())
}

/// Null-safe: `r1 IS NULL OR r2 IS NULL OR id(r1) != id(r2)`.
/// This enforces openCypher relationship uniqueness within a MATCH clause.
fn build_rel_isomorphism_filter(rel_vars: &[String]) -> Option<Expr> {
    if rel_vars.len() < 2 {
        return None;
    }

    let mut pairs: Vec<Expr> = Vec::new();
    for i in 0..rel_vars.len() {
        for j in (i + 1)..rel_vars.len() {
            let left = Expr::FunctionCall(
                "id".into(),
                vec![Expr::Variable(rel_vars[i].clone())],
            );
            let right = Expr::FunctionCall(
                "id".into(),
                vec![Expr::Variable(rel_vars[j].clone())],
            );
            let neq = Expr::Compare(Box::new(left), CmpOp::Neq, Box::new(right));
            // Null-safe for OPTIONAL MATCH.
            let null_safe = Expr::Or(
                Box::new(Expr::IsNull(Box::new(Expr::Variable(rel_vars[i].clone())))),
                Box::new(Expr::Or(
                    Box::new(Expr::IsNull(Box::new(Expr::Variable(rel_vars[j].clone())))),
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

/// Build an isomorphism filter for node variables.
/// NOTE: openCypher enforces only relationship isomorphism, not node isomorphism.
/// This function is retained for potential future use but is no longer called.
#[allow(dead_code)]
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
        LogicalPlan::LeftJoin { outer, inner, null_vars } => {
            let o = explain(outer, indent + 1);
            let i = explain(inner, indent + 1);
            format!("{prefix}LeftJoin(null_vars=[{}])\n{o}\n{i}", null_vars.join(", "))
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
