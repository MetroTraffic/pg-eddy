/// Cypher AST — typed intermediate representation for parsed Cypher queries.
///
/// v0.7.0 scope: MATCH/WHERE/RETURN + ORDER BY/SKIP/LIMIT, string predicates,
/// IN list membership, list literals, additional built-in functions.
/// v0.10.0 scope: variable-length paths, named paths, path functions,
/// shortestPath/allShortestPaths, pattern comprehensions.
/// v0.11.0 scope: EXISTS { } predicate, CALL { } subqueries, CALL proc() YIELD.
/// A sort key in ORDER BY.
#[derive(Debug, Clone)]
pub struct OrderItem {
    pub expr: Expr,
    pub ascending: bool,
}

/// A complete Cypher query as an ordered pipeline of clauses.
#[derive(Debug, Clone)]
pub struct Query {
    pub clauses: Vec<QueryClause>,
    /// UNION or UNION ALL with the right-hand query.
    /// `bool` = true → UNION ALL (no deduplication), false → UNION (deduplicate).
    pub union: Option<(bool, Box<Query>)>,
}

/// A single clause in the query pipeline.
#[derive(Debug, Clone)]
pub enum QueryClause {
    /// MATCH or OPTIONAL MATCH clause.
    Match {
        optional: bool,
        patterns: Vec<Pattern>,
        where_clause: Option<Expr>,
    },
    /// UNWIND expr AS alias.
    Unwind {
        expr: Expr,
        alias: String,
    },
    /// CALL { subquery } — uncorrelated or correlated subquery.
    /// `import_vars` lists variables passed in from outer scope.
    CallSubquery {
        subquery: Box<Query>,
    },
    /// CALL proc.name(arg, …) YIELD col1, col2 — procedure call.
    CallProcedure {
        #[allow(dead_code)]
        proc_name: String,
        #[allow(dead_code)]
        args: Vec<Expr>,
        yield_items: Vec<(String, Option<String>)>, // (column, alias)
    },
    /// WITH clause: intermediate projection (may include WHERE after it).
    With {
        distinct: bool,
        items: Vec<ReturnItem>,
        order_by: Vec<OrderItem>,
        skip: Option<Expr>,
        limit: Option<Expr>,
        where_clause: Option<Expr>,
    },
    /// RETURN clause: terminal projection.
    Return {
        distinct: bool,
        items: Vec<ReturnItem>,
        order_by: Vec<OrderItem>,
        skip: Option<Expr>,
        limit: Option<Expr>,
    },
    // -----------------------------------------------------------------------
    // v0.12.0: Write clauses
    // -----------------------------------------------------------------------
    /// CREATE pattern[, pattern] — create nodes and relationships.
    Create {
        patterns: Vec<Pattern>,
    },
    /// MERGE pattern [ON CREATE SET ...] [ON MATCH SET ...]
    Merge {
        pattern: Pattern,
        on_create: Vec<SetItem>,
        on_match: Vec<SetItem>,
    },
    /// SET n.prop = expr | n = {map} | n += {map} | n:Label
    Set {
        items: Vec<SetItem>,
    },
    /// REMOVE n.prop | n:Label
    Remove {
        items: Vec<RemoveItem>,
    },
    /// DELETE n [, m, ...] or DETACH DELETE n [, m, ...]
    Delete {
        exprs: Vec<Expr>,
        detach: bool,
    },
    /// FOREACH (variable IN list | clauses)
    Foreach {
        variable: String,
        list_expr: Expr,
        /// The clauses inside the FOREACH body (only write clauses are valid
        /// per spec: CREATE, SET, REMOVE, DELETE, MERGE, FOREACH).
        clauses: Vec<QueryClause>,
    },
}

/// A SET item: one of four forms.
#[derive(Debug, Clone)]
pub enum SetItem {
    /// `n.prop = expr`
    Property(Expr, Expr),
    /// `n = {map}` — replace all properties
    Variable(String, Expr),
    /// `n += {map}` — merge properties
    MergeMap(String, Expr),
    /// `n:Label[:Label2 ...]` — add labels
    Label(String, Vec<String>),
}

/// A REMOVE item: one of two forms.
#[derive(Debug, Clone)]
pub enum RemoveItem {
    /// `n.prop`
    Property(Expr, String),
    /// `n:Label[:Label2 ...]`
    Label(String, Vec<String>),
}

/// A single pattern: a chain of nodes connected by relationships.
/// `(a)-[r:KNOWS]->(b)-[:LIKES]->(c)` is one pattern with 3 nodes and 2 rels.
/// `variable` names the whole path when present: `p = (a)-[r]->(b)`.
#[derive(Debug, Clone)]
pub struct Pattern {
    pub variable: Option<String>, // named path: p = (a)-[r]->(b)
    pub elements: Vec<PatternElement>,
}

/// A node or relationship in a pattern chain.
#[derive(Debug, Clone)]
pub enum PatternElement {
    Node(NodePattern),
    Relationship(RelPattern),
}

/// A node pattern: `(variable:Label {prop: value})`.
#[derive(Debug, Clone)]
pub struct NodePattern {
    pub variable: Option<String>,
    pub labels: Vec<String>,
    pub properties: Vec<(String, Expr)>,
    /// True if an explicit property map `{}` was written (even if empty).
    pub has_explicit_map: bool,
}

/// A relationship pattern: `-[variable:TYPE {prop: value}]->`.
#[derive(Debug, Clone)]
pub struct RelPattern {
    pub variable: Option<String>,
    pub rel_types: Vec<String>,
    pub direction: RelDirection,
    pub properties: Vec<(String, Expr)>,
    /// Variable-length range: `*`, `*3`, `*1..5`, `*..5`, `*3..`.
    /// `None` = fixed single hop (no `*`).
    pub length: Option<VarLength>,
}

/// Variable-length range for `-[*min..max]-`.
/// Both bounds are optional: `*` = `VarLength { min: 1, max: None }` (unbounded).
#[derive(Debug, Clone)]
pub struct VarLength {
    pub min: u32,          // default 1
    pub max: Option<u32>,  // None = unbounded
}

/// Direction of a relationship in a pattern.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RelDirection {
    Out,   // -[]->(b)
    In,    // <-[]-(b)
    Both,  // -[]-(b)  (undirected)
}

/// A single RETURN item: expression optionally aliased.
#[derive(Debug, Clone)]
pub struct ReturnItem {
    pub expr: Expr,
    pub alias: Option<String>,
}

/// Expression tree — covers comparisons, boolean logic, literals, property
/// access, parameters, and function calls needed for v0.6.0.
#[derive(Debug, Clone)]
pub enum Expr {
    /// Variable reference: `n`
    Variable(String),
    /// Property access: `n.name`
    Property(Box<Expr>, String),
    /// Integer literal
    IntLit(i64),
    /// Float literal
    FloatLit(f64),
    /// String literal
    StringLit(String),
    /// Boolean literal
    BoolLit(bool),
    /// NULL literal
    NullLit,
    /// Parameter reference: `$param`
    Parameter(String),
    /// Binary comparison: =, <>, <, >, <=, >=
    Compare(Box<Expr>, CmpOp, Box<Expr>),
    /// Boolean AND
    And(Box<Expr>, Box<Expr>),
    /// Boolean OR
    Or(Box<Expr>, Box<Expr>),
    /// Boolean XOR
    Xor(Box<Expr>, Box<Expr>),
    /// Boolean NOT
    Not(Box<Expr>),
    /// IS NULL
    IsNull(Box<Expr>),
    /// IS NOT NULL
    IsNotNull(Box<Expr>),
    /// Arithmetic: +, -, *, /, %
    Arith(Box<Expr>, ArithOp, Box<Expr>),
    /// Unary minus
    Neg(Box<Expr>),
    /// Function call: id(n), labels(n), type(r), etc.
    FunctionCall(String, Vec<Expr>),
    /// Star expression (for COUNT(*))
    Star,
    /// List literal: [e1, e2, e3]
    List(Vec<Expr>),
    /// IN list membership: expr IN list_expr
    InList(Box<Expr>, Box<Expr>),
    /// STARTS WITH string predicate
    StartsWith(Box<Expr>, Box<Expr>),
    /// ENDS WITH string predicate
    EndsWith(Box<Expr>, Box<Expr>),
    /// CONTAINS string predicate
    Contains(Box<Expr>, Box<Expr>),
    /// Regular expression match: str =~ pattern
    Regex(Box<Expr>, Box<Expr>),
    /// Searched CASE: CASE WHEN cond THEN val ... [ELSE val] END
    CaseSearched {
        branches: Vec<(Expr, Expr)>,
        else_: Option<Box<Expr>>,
    },
    /// Simple CASE: CASE test WHEN val THEN val ... [ELSE val] END
    CaseSimple {
        test: Box<Expr>,
        branches: Vec<(Expr, Expr)>,
        else_: Option<Box<Expr>>,
    },
    /// List comprehension: [var IN list WHERE? pred? | proj?]
    ListComprehension {
        variable: String,
        list_expr: Box<Expr>,
        predicate: Option<Box<Expr>>,
        projection: Option<Box<Expr>>,
    },
    /// List predicate: any/all/none/single(var IN list WHERE pred)
    ListPredicate {
        kind: ListPredicateKind,
        variable: String,
        list_expr: Box<Expr>,
        predicate: Box<Expr>,
    },
    /// List element access: list[index]
    Subscript(Box<Expr>, Box<Expr>),
    /// List slice: list[from..to]
    ListSlice {
        list_expr: Box<Expr>,
        from: Option<Box<Expr>>,
        to: Option<Box<Expr>>,
    },
    /// shortestPath((a)-[r*]->(b)) or allShortestPaths(...)
    ShortestPath {
        all: bool,
        pattern: Pattern,
    },
    /// Pattern comprehension: [(n)-[:R]->(m) | expr]
    PatternComprehension {
        #[allow(dead_code)]
        path_variable: Option<String>,
        pattern: Pattern,
        predicate: Option<Box<Expr>>,
        projection: Box<Expr>,
    },
    /// EXISTS { pattern } — existential subquery predicate (v0.11.0).
    /// Returns true if at least one result exists for the inner pattern query.
    /// `implicit` = true when this was parsed as an inline pattern predicate
    /// (e.g. `WHERE (n)-->(m)`) rather than an explicit `exists { ... }` block.
    /// Implicit pattern predicates reject new named variables in the pattern.
    Exists {
        subquery: Box<Query>,
        implicit: bool,
    },
    /// Label test: `n:Label` or `n:A:B` — true if node/rel has all listed labels/type
    HasLabel(Box<Expr>, Vec<String>),
    /// Map literal: {key: expr, key2: expr2}
    MapLiteral(Vec<(String, Expr)>),
}

/// Kind of list predicate function.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ListPredicateKind {
    Any,
    All,
    None_,
    Single,
}

/// Comparison operators.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CmpOp {
    Eq,
    Neq,
    Lt,
    Gt,
    Le,
    Ge,
}

/// Arithmetic operators.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ArithOp {
    Add,
    Sub,
    Mul,
    Div,
    Mod,
    Pow,
}

impl std::fmt::Display for CmpOp {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            CmpOp::Eq => write!(f, "="),
            CmpOp::Neq => write!(f, "<>"),
            CmpOp::Lt => write!(f, "<"),
            CmpOp::Gt => write!(f, ">"),
            CmpOp::Le => write!(f, "<="),
            CmpOp::Ge => write!(f, ">="),
        }
    }
}

impl std::fmt::Display for ArithOp {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            ArithOp::Add => write!(f, "+"),
            ArithOp::Sub => write!(f, "-"),
            ArithOp::Mul => write!(f, "*"),
            ArithOp::Div => write!(f, "/"),
            ArithOp::Mod => write!(f, "%"),
            ArithOp::Pow => write!(f, "^"),
        }
    }
}
