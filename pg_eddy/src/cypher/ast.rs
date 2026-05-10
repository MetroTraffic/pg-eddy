/// Cypher AST — typed intermediate representation for parsed Cypher queries.
///
/// v0.7.0 scope: MATCH/WHERE/RETURN + ORDER BY/SKIP/LIMIT, string predicates,
/// IN list membership, list literals, additional built-in functions.
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
}

/// A single pattern: a chain of nodes connected by relationships.
/// `(a)-[r:KNOWS]->(b)-[:LIKES]->(c)` is one pattern with 3 nodes and 2 rels.
#[derive(Debug, Clone)]
pub struct Pattern {
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
}

/// A relationship pattern: `-[variable:TYPE {prop: value}]->`.
#[derive(Debug, Clone)]
pub struct RelPattern {
    pub variable: Option<String>,
    pub rel_types: Vec<String>,
    pub direction: RelDirection,
    pub properties: Vec<(String, Expr)>,
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
