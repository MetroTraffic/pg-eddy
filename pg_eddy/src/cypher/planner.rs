/// Logical planner — transforms a Cypher AST into a logical execution plan.
///
/// v0.6.0 scope: LabelScan, Expand, Filter, Project, CrossProduct.
/// Node isomorphism is enforced by adding inequality filters for every
/// distinct pair of node variables.
use crate::cypher::ast::*;
use std::collections::HashSet;

/// A logical plan node.
#[derive(Debug, Clone)]
pub enum LogicalPlan {
    /// Scan all nodes, optionally filtered by a single label.
    LabelScan {
        variable: String,
        label: Option<String>,
        inline_props: Vec<(String, Expr)>,
    },
    /// Expand from a bound node variable along relationships.
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

/// Build a logical plan from a parsed Query.
pub fn plan(query: &Query) -> Result<LogicalPlan, PlanError> {
    // Step 1: build one plan branch per MATCH pattern.
    let mut branches: Vec<LogicalPlan> = Vec::new();
    let mut bound_vars: HashSet<String> = HashSet::new();

    for pattern in &query.match_clause.patterns {
        let branch = plan_pattern(pattern, &mut bound_vars)?;
        branches.push(branch);
    }

    // Step 2: combine branches with CrossProduct.
    let mut plan = branches.remove(0);
    for branch in branches {
        plan = LogicalPlan::CrossProduct {
            left: Box::new(plan),
            right: Box::new(branch),
        };
    }

    // Step 3: node isomorphism — every distinct node variable pair gets <>.
    let node_vars = collect_node_variables(&query.match_clause);
    let iso_filter = build_isomorphism_filter(&node_vars);

    // Step 4: WHERE clause filter.
    let combined_filter = match (&iso_filter, &query.where_clause) {
        (Some(iso), Some(wh)) => Some(Expr::And(Box::new(iso.clone()), Box::new(wh.clone()))),
        (Some(iso), None) => Some(iso.clone()),
        (None, Some(wh)) => Some(wh.clone()),
        (None, None) => None,
    };

    if let Some(predicate) = combined_filter {
        plan = LogicalPlan::Filter {
            input: Box::new(plan),
            predicate,
        };
    }

    // Step 5: Project (RETURN clause + ORDER BY / SKIP / LIMIT).
    plan = LogicalPlan::Project {
        input: Box::new(plan),
        items: query.return_clause.items.clone(),
        distinct: query.return_clause.distinct,
        order_by: query.order_by.clone(),
        skip: query.skip.clone(),
        limit: query.limit.clone(),
    };

    Ok(plan)
}

/// Build a plan for a single pattern chain.
fn plan_pattern(
    pattern: &Pattern,
    bound_vars: &mut HashSet<String>,
) -> Result<LogicalPlan, PlanError> {
    let mut plan: Option<LogicalPlan> = None;

    let mut i = 0;
    while i < pattern.elements.len() {
        match &pattern.elements[i] {
            PatternElement::Node(node) => {
                if plan.is_none() {
                    // First node in the pattern — start with LabelScan.
                    let var = node.variable.clone().unwrap_or_else(|| {
                        format!("_anon_n{}", i)
                    });
                    let label = node.labels.first().cloned();
                    plan = Some(LogicalPlan::LabelScan {
                        variable: var.clone(),
                        label,
                        inline_props: node.properties.clone(),
                    });
                    bound_vars.insert(var);
                }
                i += 1;
            }
            PatternElement::Relationship(rel) => {
                // Must be preceded by a node (plan is Some) and followed by a node.
                let src_plan = plan.take().ok_or_else(|| PlanError {
                    message: "relationship without preceding node".into(),
                })?;

                let next_node = match pattern.elements.get(i + 1) {
                    Some(PatternElement::Node(n)) => n,
                    _ => {
                        return Err(PlanError {
                            message: "relationship must be followed by a node".into(),
                        })
                    }
                };

                // Determine src/dst variable names.
                let src_var = find_last_node_var(&src_plan);
                let dst_var = next_node
                    .variable
                    .clone()
                    .unwrap_or_else(|| format!("_anon_n{}", i + 1));

                let rel_var = rel.variable.clone();

                plan = Some(LogicalPlan::Expand {
                    input: Box::new(src_plan),
                    src_var,
                    rel_var: rel_var.clone(),
                    dst_var: dst_var.clone(),
                    rel_types: rel.rel_types.clone(),
                    direction: rel.direction,
                    rel_props: rel.properties.clone(),
                    dst_labels: next_node.labels.clone(),
                    dst_props: next_node.properties.clone(),
                });

                bound_vars.insert(dst_var);
                if let Some(rv) = &rel_var {
                    bound_vars.insert(rv.clone());
                }

                i += 2; // skip relationship + next node
            }
        }
    }

    plan.ok_or_else(|| PlanError {
        message: "empty pattern".into(),
    })
}

/// Find the last node variable name from a plan branch.
fn find_last_node_var(plan: &LogicalPlan) -> String {
    match plan {
        LogicalPlan::LabelScan { variable, .. } => variable.clone(),
        LogicalPlan::Expand { dst_var, .. } => dst_var.clone(),
        LogicalPlan::CrossProduct { right, .. } => find_last_node_var(right),
        LogicalPlan::Filter { input, .. } => find_last_node_var(input),
        LogicalPlan::Project { input, .. } => find_last_node_var(input),
    }
}

/// Collect all named node variables from a MATCH clause.
fn collect_node_variables(match_clause: &MatchClause) -> Vec<String> {
    let mut vars = Vec::new();
    let mut seen = HashSet::new();
    for pattern in &match_clause.patterns {
        for elem in &pattern.elements {
            if let PatternElement::Node(n) = elem
                && let Some(ref v) = n.variable
                    && seen.insert(v.clone()) {
                        vars.push(v.clone());
                    }
        }
    }
    vars
}

/// Build an isomorphism filter: for N node variables, emit
/// `a <> b AND a <> c AND b <> c` using id() comparisons.
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
            pairs.push(Expr::Compare(Box::new(left), CmpOp::Neq, Box::new(right)));
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
        LogicalPlan::LabelScan { variable, label, inline_props } => {
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
}
