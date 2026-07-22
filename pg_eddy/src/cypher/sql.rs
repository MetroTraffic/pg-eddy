//! Read-only Cypher-to-SQL compiler for incrementally maintained graph views.
//!
//! The first supported surface is deterministic fixed-length
//! `MATCH ... RETURN`. Unsupported constructs fail closed so pg_trickle never
//! maintains a query whose SQL semantics differ from the in-process executor.

use std::collections::HashMap;

use crate::cypher::ast::{
    CmpOp, Expr, NodePattern, Pattern, PatternElement, Query, QueryClause, RelDirection,
    RelPattern, ReturnItem,
};
use crate::error::PgEddyError;

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CompiledQuery {
    pub sql: String,
    pub columns: Vec<String>,
}

#[derive(Debug, Clone)]
enum Binding {
    Node(String),
    Edge(String),
}

struct Compiler<'a> {
    params: &'a serde_json::Map<String, serde_json::Value>,
    bindings: HashMap<String, Binding>,
    node_occurrences: Vec<(String, String)>,
    edge_occurrences: Vec<(String, String)>,
    from_sql: String,
    predicates: Vec<String>,
    next_node: usize,
    next_edge: usize,
}

pub fn compile(cypher: &str, params: &pgrx::JsonB) -> Result<CompiledQuery, PgEddyError> {
    let params = params
        .0
        .as_object()
        .ok_or_else(|| PgEddyError::InvalidGraphView("params must be a JSON object".into()))?;
    let query = crate::cypher::parser::parse(cypher)
        .map_err(|error| PgEddyError::InvalidGraphView(error.to_string()))?;
    compile_query(&query, params)
}

fn compile_query(
    query: &Query,
    params: &serde_json::Map<String, serde_json::Value>,
) -> Result<CompiledQuery, PgEddyError> {
    let mut left = compile_branch(&query.clauses, params)?;
    if let Some((all, right_query)) = &query.union {
        let right = compile_query(right_query, params)?;
        if left.columns != right.columns {
            return Err(unsupported(
                "UNION graph-view branches must return identical columns",
            ));
        }
        left.sql = format!(
            "({}) UNION{} ({})",
            left.sql,
            if *all { " ALL" } else { "" },
            right.sql,
        );
    }
    Ok(left)
}

fn compile_branch(
    clauses: &[QueryClause],
    params: &serde_json::Map<String, serde_json::Value>,
) -> Result<CompiledQuery, PgEddyError> {
    if clauses.len() < 2 {
        return Err(unsupported(
            "graph views require one or more MATCH clauses followed by RETURN",
        ));
    }

    let (match_clauses, return_clause) = clauses.split_at(clauses.len() - 1);
    let (distinct, items, order_by, skip, limit) = match &return_clause[0] {
        QueryClause::Return {
            distinct,
            items,
            order_by,
            skip,
            limit,
        } => (*distinct, items, order_by, skip.as_ref(), limit.as_ref()),
        _ => return Err(unsupported("graph views must end with RETURN")),
    };
    if items.is_empty() {
        return Err(unsupported("RETURN must not be empty"));
    }

    let mut compiler = Compiler {
        params,
        bindings: HashMap::new(),
        node_occurrences: Vec::new(),
        edge_occurrences: Vec::new(),
        from_sql: String::new(),
        predicates: Vec::new(),
        next_node: 0,
        next_edge: 0,
    };
    for clause in match_clauses {
        let (patterns, where_clause) = match clause {
            QueryClause::Match {
                optional: false,
                patterns,
                where_clause,
            } => (patterns, where_clause.as_ref()),
            QueryClause::Match { optional: true, .. } => {
                return Err(unsupported(
                    "OPTIONAL MATCH graph views are not implemented yet",
                ));
            }
            QueryClause::With { .. } => {
                return Err(unsupported("WITH graph views are not implemented yet"));
            }
            _ => return Err(unsupported("graph views only support MATCH before RETURN")),
        };
        if patterns.is_empty() {
            return Err(unsupported("MATCH must not be empty"));
        }
        for pattern in patterns {
            compiler.compile_pattern(pattern)?;
        }
        if let Some(predicate) = where_clause {
            compiler
                .predicates
                .push(compiler.compile_boolean(predicate)?);
        }
    }

    let mut columns = Vec::with_capacity(items.len());
    let mut select_items = Vec::with_capacity(items.len());
    for (index, item) in items.iter().enumerate() {
        let alias = output_name(item, index);
        validate_output_name(&alias)?;
        if columns.contains(&alias) {
            return Err(unsupported(format!("duplicate output column `{alias}`")));
        }
        let expression = compiler.compile_json(&item.expr)?;
        select_items.push(format!(
            "({expression})::text AS {}",
            quote_identifier(&alias),
        ));
        columns.push(alias);
    }

    let mut sql = format!(
        "SELECT {}{} FROM {}",
        if distinct { "DISTINCT " } else { "" },
        select_items.join(", "),
        compiler.from_sql,
    );
    if !compiler.predicates.is_empty() {
        sql.push_str(" WHERE ");
        sql.push_str(&compiler.predicates.join(" AND "));
    }
    if !order_by.is_empty() {
        let mut parts = Vec::with_capacity(order_by.len());
        for item in order_by {
            parts.push(format!(
                "{} {}",
                compiler.compile_json(&item.expr)?,
                if item.ascending { "ASC" } else { "DESC" },
            ));
        }
        sql.push_str(" ORDER BY ");
        sql.push_str(&parts.join(", "));
    }
    if let Some(skip) = skip {
        sql.push_str(&format!(
            " OFFSET {}",
            compiler.compile_nonnegative_int(skip)?
        ));
    }
    if let Some(limit) = limit {
        sql.push_str(&format!(
            " LIMIT {}",
            compiler.compile_nonnegative_int(limit)?
        ));
    }

    Ok(CompiledQuery { sql, columns })
}

impl Compiler<'_> {
    fn compile_pattern(&mut self, pattern: &Pattern) -> Result<(), PgEddyError> {
        if pattern.variable.is_some() {
            return Err(unsupported(
                "named path graph views are not implemented yet",
            ));
        }
        let Some(PatternElement::Node(first)) = pattern.elements.first() else {
            return Err(unsupported("patterns must begin with a node"));
        };
        #[allow(clippy::manual_is_multiple_of)]
        if pattern.elements.len() % 2 == 0 {
            return Err(unsupported("malformed pattern chain"));
        }

        let first_alias = self.new_node(first)?;
        if self.from_sql.is_empty() {
            self.from_sql = format!("_pg_eddy.ivm_nodes AS {first_alias}");
        } else {
            self.from_sql
                .push_str(&format!(" CROSS JOIN _pg_eddy.ivm_nodes AS {first_alias}",));
        }
        self.add_node_filters(first, &first_alias)?;

        let mut previous_alias = first_alias;
        for pair in pattern.elements[1..].chunks_exact(2) {
            let PatternElement::Relationship(relationship) = &pair[0] else {
                return Err(unsupported("expected relationship in pattern chain"));
            };
            let PatternElement::Node(node) = &pair[1] else {
                return Err(unsupported("relationship must be followed by a node"));
            };
            if relationship.length.is_some() {
                return Err(unsupported(
                    "variable-length graph views are not implemented yet",
                ));
            }

            let edge_alias = self.new_edge(relationship)?;
            let node_alias = self.new_node(node)?;
            let edge_join = match relationship.direction {
                RelDirection::Out => {
                    format!("{edge_alias}.source_node_id = {previous_alias}.node_id")
                }
                RelDirection::In => {
                    format!("{edge_alias}.target_node_id = {previous_alias}.node_id")
                }
                RelDirection::Both => format!(
                    "({edge_alias}.source_node_id = {previous_alias}.node_id OR \
                      {edge_alias}.target_node_id = {previous_alias}.node_id)",
                ),
            };
            self.from_sql.push_str(&format!(
                " JOIN _pg_eddy.ivm_edges AS {edge_alias} ON {edge_join}",
            ));
            let node_join = match relationship.direction {
                RelDirection::Out => {
                    format!("{node_alias}.node_id = {edge_alias}.target_node_id")
                }
                RelDirection::In => {
                    format!("{node_alias}.node_id = {edge_alias}.source_node_id")
                }
                RelDirection::Both => format!(
                    "(({edge_alias}.source_node_id = {previous_alias}.node_id AND \
                        {node_alias}.node_id = {edge_alias}.target_node_id) OR \
                       ({edge_alias}.target_node_id = {previous_alias}.node_id AND \
                        {node_alias}.node_id = {edge_alias}.source_node_id))",
                ),
            };
            self.from_sql.push_str(&format!(
                " JOIN _pg_eddy.ivm_nodes AS {node_alias} ON {node_join}",
            ));
            self.add_edge_filters(relationship, &edge_alias)?;
            self.add_node_filters(node, &node_alias)?;
            previous_alias = node_alias;
        }
        Ok(())
    }

    fn new_node(&mut self, node: &NodePattern) -> Result<String, PgEddyError> {
        let alias = format!("n{}", self.next_node);
        self.next_node += 1;
        let variable = node
            .variable
            .clone()
            .unwrap_or_else(|| format!("__anonymous_node_{}", self.next_node));
        if let Some(existing) = self.bindings.get(&variable) {
            let Binding::Node(existing_alias) = existing else {
                return Err(unsupported(format!(
                    "variable `{variable}` is used as both node and relationship",
                )));
            };
            self.predicates
                .push(format!("{alias}.node_id = {existing_alias}.node_id"));
        } else if node.variable.is_some() {
            self.bindings
                .insert(variable.clone(), Binding::Node(alias.clone()));
        }
        for (other_variable, other_alias) in &self.node_occurrences {
            if *other_variable != variable {
                self.predicates
                    .push(format!("{alias}.node_id <> {other_alias}.node_id"));
            }
        }
        self.node_occurrences.push((variable, alias.clone()));
        Ok(alias)
    }

    fn new_edge(&mut self, relationship: &RelPattern) -> Result<String, PgEddyError> {
        let alias = format!("r{}", self.next_edge);
        self.next_edge += 1;
        let variable = relationship
            .variable
            .clone()
            .unwrap_or_else(|| format!("__anonymous_edge_{}", self.next_edge));
        if let Some(existing) = self.bindings.get(&variable) {
            let Binding::Edge(existing_alias) = existing else {
                return Err(unsupported(format!(
                    "variable `{variable}` is used as both node and relationship",
                )));
            };
            self.predicates
                .push(format!("{alias}.rel_id = {existing_alias}.rel_id"));
        } else if relationship.variable.is_some() {
            self.bindings
                .insert(variable.clone(), Binding::Edge(alias.clone()));
        }
        for (other_variable, other_alias) in &self.edge_occurrences {
            if *other_variable != variable {
                self.predicates
                    .push(format!("{alias}.rel_id <> {other_alias}.rel_id"));
            }
        }
        self.edge_occurrences.push((variable, alias.clone()));
        Ok(alias)
    }

    fn add_node_filters(&mut self, node: &NodePattern, alias: &str) -> Result<(), PgEddyError> {
        if !node.labels.is_empty() {
            self.predicates.extend(
                node.labels
                    .iter()
                    .map(|label| format!("{} = ANY({alias}.labels)", quote_literal(label))),
            );
        }
        for (key, value) in &node.properties {
            self.predicates.push(format!(
                "{alias}.properties -> {} = {}",
                quote_literal(key),
                self.compile_constant_json(value)?,
            ));
        }
        Ok(())
    }

    fn add_edge_filters(
        &mut self,
        relationship: &RelPattern,
        alias: &str,
    ) -> Result<(), PgEddyError> {
        if !relationship.rel_types.is_empty() {
            let types = relationship
                .rel_types
                .iter()
                .map(|rel_type| quote_literal(rel_type))
                .collect::<Vec<_>>()
                .join(", ");
            self.predicates
                .push(format!("{alias}.rel_type IN ({types})"));
        }
        for (key, value) in &relationship.properties {
            self.predicates.push(format!(
                "{alias}.properties -> {} = {}",
                quote_literal(key),
                self.compile_constant_json(value)?,
            ));
        }
        Ok(())
    }

    fn compile_json(&self, expression: &Expr) -> Result<String, PgEddyError> {
        match expression {
            Expr::Variable(variable) => match self.bindings.get(variable) {
                Some(Binding::Node(alias)) => Ok(node_json(alias)),
                Some(Binding::Edge(alias)) => Ok(edge_json(alias)),
                None => Err(unsupported(format!("unbound variable `{variable}`"))),
            },
            Expr::Property(inner, key) => {
                let Expr::Variable(variable) = inner.as_ref() else {
                    return Err(unsupported("nested property access is not implemented yet"));
                };
                match self.bindings.get(variable) {
                    Some(Binding::Node(alias)) | Some(Binding::Edge(alias)) => Ok(format!(
                        "COALESCE({alias}.properties -> {}, 'null'::jsonb)",
                        quote_literal(key),
                    )),
                    None => Err(unsupported(format!("unbound variable `{variable}`"))),
                }
            }
            Expr::IntLit(_)
            | Expr::FloatLit(_)
            | Expr::StringLit(_)
            | Expr::BoolLit(_)
            | Expr::NullLit
            | Expr::Parameter(_)
            | Expr::List(_)
            | Expr::MapLiteral(_) => self.compile_constant_json(expression),
            Expr::FunctionCall(name, arguments) => self.compile_function(name, arguments),
            _ => Err(unsupported(format!(
                "RETURN expression `{expression:?}` is not implemented yet",
            ))),
        }
    }

    fn compile_function(&self, name: &str, arguments: &[Expr]) -> Result<String, PgEddyError> {
        let lower = name.to_ascii_lowercase();
        if lower == "count" {
            if arguments.len() != 1 {
                return Err(unsupported("count() requires one argument"));
            }
            return if matches!(arguments[0], Expr::Star) {
                Ok("to_jsonb(count(*))".into())
            } else {
                Ok(format!(
                    "to_jsonb(count({}))",
                    self.compile_json(&arguments[0])?
                ))
            };
        }
        if arguments.len() != 1 {
            return Err(unsupported(format!("{name}() requires one argument")));
        }
        let Expr::Variable(variable) = &arguments[0] else {
            return Err(unsupported(format!("{name}() requires an entity variable")));
        };
        match (lower.as_str(), self.bindings.get(variable)) {
            ("id", Some(Binding::Node(alias))) => Ok(format!("to_jsonb({alias}.node_id)")),
            ("id", Some(Binding::Edge(alias))) => Ok(format!("to_jsonb({alias}.rel_id)")),
            ("labels", Some(Binding::Node(alias))) => Ok(format!("to_jsonb({alias}.labels)")),
            ("type", Some(Binding::Edge(alias))) => Ok(format!("to_jsonb({alias}.rel_type)")),
            ("properties", Some(Binding::Node(alias)))
            | ("properties", Some(Binding::Edge(alias))) => Ok(format!("{alias}.properties")),
            _ => Err(unsupported(format!("unsupported function `{name}`"))),
        }
    }

    fn compile_boolean(&self, expression: &Expr) -> Result<String, PgEddyError> {
        match expression {
            Expr::Compare(left, operator, right) => Ok(format!(
                "({} {} {})",
                self.compile_json(left)?,
                cmp_sql(*operator),
                self.compile_json(right)?,
            )),
            Expr::And(left, right) => Ok(format!(
                "({} AND {})",
                self.compile_boolean(left)?,
                self.compile_boolean(right)?,
            )),
            Expr::Or(left, right) => Ok(format!(
                "({} OR {})",
                self.compile_boolean(left)?,
                self.compile_boolean(right)?,
            )),
            Expr::Xor(left, right) => Ok(format!(
                "({} <> {})",
                self.compile_boolean(left)?,
                self.compile_boolean(right)?,
            )),
            Expr::Not(inner) => Ok(format!("NOT ({})", self.compile_boolean(inner)?)),
            Expr::IsNull(inner) => {
                let value = self.compile_json(inner)?;
                Ok(format!("({value} IS NULL OR {value} = 'null'::jsonb)"))
            }
            Expr::IsNotNull(inner) => {
                let value = self.compile_json(inner)?;
                Ok(format!(
                    "({value} IS NOT NULL AND {value} <> 'null'::jsonb)"
                ))
            }
            Expr::HasLabel(inner, labels) => {
                let Expr::Variable(variable) = inner.as_ref() else {
                    return Err(unsupported("label predicates require a node variable"));
                };
                let Some(Binding::Node(alias)) = self.bindings.get(variable) else {
                    return Err(unsupported(format!("unbound node variable `{variable}`")));
                };
                let labels = labels
                    .iter()
                    .map(|label| format!("{} = ANY({alias}.labels)", quote_literal(label)))
                    .collect::<Vec<_>>()
                    .join(" AND ");
                Ok(format!("({labels})"))
            }
            _ => Ok(format!(
                "{} = 'true'::jsonb",
                self.compile_json(expression)?
            )),
        }
    }

    fn compile_constant_json(&self, expression: &Expr) -> Result<String, PgEddyError> {
        let value = constant_value(expression, self.params)?;
        let encoded = serde_json::to_string(&value)
            .map_err(|error| unsupported(format!("cannot encode constant: {error}")))?;
        Ok(format!("{}::jsonb", quote_literal(&encoded)))
    }

    fn compile_nonnegative_int(&self, expression: &Expr) -> Result<i64, PgEddyError> {
        let value = constant_value(expression, self.params)?;
        let value = value
            .as_i64()
            .ok_or_else(|| unsupported("SKIP/LIMIT must be integer constants"))?;
        if value < 0 {
            return Err(unsupported("SKIP/LIMIT must not be negative"));
        }
        Ok(value)
    }
}

fn constant_value(
    expression: &Expr,
    params: &serde_json::Map<String, serde_json::Value>,
) -> Result<serde_json::Value, PgEddyError> {
    match expression {
        Expr::IntLit(value) => Ok((*value).into()),
        Expr::FloatLit(value) => serde_json::Number::from_f64(*value)
            .map(serde_json::Value::Number)
            .ok_or_else(|| unsupported("non-finite float constants are not supported")),
        Expr::StringLit(value) => Ok(value.clone().into()),
        Expr::BoolLit(value) => Ok((*value).into()),
        Expr::NullLit => Ok(serde_json::Value::Null),
        Expr::Parameter(name) => params
            .get(name)
            .cloned()
            .ok_or_else(|| unsupported(format!("missing parameter `${name}`"))),
        Expr::List(items) => items
            .iter()
            .map(|item| constant_value(item, params))
            .collect::<Result<Vec<_>, _>>()
            .map(serde_json::Value::Array),
        Expr::MapLiteral(items) => {
            let mut map = serde_json::Map::new();
            for (key, value) in items {
                map.insert(key.clone(), constant_value(value, params)?);
            }
            Ok(serde_json::Value::Object(map))
        }
        _ => Err(unsupported(
            "pattern properties must be constants or parameters",
        )),
    }
}

fn node_json(alias: &str) -> String {
    format!(
        "jsonb_build_object('node_id', {alias}.node_id, 'labels', \
         to_jsonb({alias}.labels), 'properties', {alias}.properties)",
    )
}

fn edge_json(alias: &str) -> String {
    format!(
        "jsonb_build_object('rel_id', {alias}.rel_id, 'rel_type', {alias}.rel_type, \
         'source_node_id', {alias}.source_node_id, 'target_node_id', \
         {alias}.target_node_id, 'properties', {alias}.properties)",
    )
}

fn output_name(item: &ReturnItem, index: usize) -> String {
    item.alias.clone().unwrap_or_else(|| match &item.expr {
        Expr::Variable(variable) => variable.clone(),
        Expr::Property(_, key) => key.clone(),
        _ => format!("column_{}", index + 1),
    })
}

fn validate_output_name(name: &str) -> Result<(), PgEddyError> {
    if name.is_empty() || name.starts_with("__pgt_") || name.starts_with("__pgs_") {
        return Err(unsupported(format!("reserved output column `{name}`")));
    }
    Ok(())
}

fn cmp_sql(operator: CmpOp) -> &'static str {
    match operator {
        CmpOp::Eq => "=",
        CmpOp::Neq => "<>",
        CmpOp::Lt => "<",
        CmpOp::Gt => ">",
        CmpOp::Le => "<=",
        CmpOp::Ge => ">=",
    }
}

fn quote_literal(value: &str) -> String {
    format!("'{}'", value.replace('\'', "''"))
}

pub(crate) fn quote_identifier(value: &str) -> String {
    format!("\"{}\"", value.replace('"', "\"\""))
}

fn unsupported(message: impl Into<String>) -> PgEddyError {
    PgEddyError::InvalidGraphView(message.into())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn compiles_fixed_relationship_and_parameters() {
        let compiled = compile(
            "MATCH (a:Person {name: $name})-[r:KNOWS]->(b:Person) \
             WHERE b.age > 20 RETURN b.name AS friend, type(r) AS kind",
            &pgrx::JsonB(serde_json::json!({"name": "O'Brien"})),
        )
        .expect("query should compile");

        assert_eq!(compiled.columns, vec!["friend", "kind"]);
        assert!(compiled.sql.contains("_pg_eddy.ivm_nodes AS n0"));
        assert!(compiled.sql.contains("_pg_eddy.ivm_edges AS r0"));
        assert!(compiled.sql.contains("O''Brien"));
        assert!(compiled.sql.contains("r0.rel_type IN"));
        assert!(compiled.sql.contains("AS \"friend\""));
    }

    #[test]
    fn rejects_write_queries() {
        let error = compile("CREATE (:Person)", &pgrx::JsonB(serde_json::json!({})))
            .expect_err("writes must be rejected");
        assert!(error.to_string().starts_with("PE601:"));
    }

    #[test]
    fn compiles_chained_matches() {
        let compiled = compile(
            "MATCH (a:Person)-[:KNOWS]->(b:Person) \
             MATCH (b)-[:KNOWS]->(c:Person) \
             WHERE c.active = true RETURN a.name AS source, c.name AS target",
            &pgrx::JsonB(serde_json::json!({})),
        )
        .expect("chained MATCH should compile");

        assert_eq!(compiled.columns, vec!["source", "target"]);
        assert_eq!(compiled.sql.matches("_pg_eddy.ivm_edges AS").count(), 2);
        assert!(compiled.sql.contains("n2.node_id = n1.node_id"));
    }

    #[test]
    fn compiles_union_all_with_matching_columns() {
        let compiled = compile(
            "MATCH (p:Person) RETURN p.name AS name \
             UNION ALL MATCH (c:Company) RETURN c.name AS name",
            &pgrx::JsonB(serde_json::json!({})),
        )
        .expect("compatible UNION ALL should compile");

        assert_eq!(compiled.columns, vec!["name"]);
        assert!(compiled.sql.contains(" UNION ALL "));
        assert_eq!(compiled.sql.matches("_pg_eddy.ivm_nodes AS n0").count(), 2);
    }

    #[test]
    fn rejects_incompatible_union_columns() {
        let error = compile(
            "MATCH (p:Person) RETURN p.name AS person_name \
             UNION MATCH (c:Company) RETURN c.name AS company_name",
            &pgrx::JsonB(serde_json::json!({})),
        )
        .expect_err("UNION output schemas must match");

        assert!(error.to_string().contains("identical columns"));
    }

    #[test]
    fn rejects_unsupported_read_constructs() {
        for cypher in [
            "MATCH (a:Person) OPTIONAL MATCH (a)-[:KNOWS]->(b) RETURN a, b",
            "MATCH (a:Person)-[:KNOWS*1..3]->(b) RETURN b",
            "MATCH (a:Person) WITH a RETURN a",
        ] {
            let error = compile(cypher, &pgrx::JsonB(serde_json::json!({})))
                .expect_err("unsupported read construct must fail closed");
            assert!(error.to_string().starts_with("PE601:"), "{error}");
        }
    }
}
