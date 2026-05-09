/// Cypher parser — recursive-descent parser for openCypher queries.
///
/// v0.6.0 scope: single-clause MATCH … WHERE … RETURN.
use crate::cypher::ast::*;
use crate::cypher::lexer::{Token, SpannedToken, LexError};

/// Parse error.
#[derive(Debug, Clone)]
pub struct ParseError {
    pub message: String,
    pub offset: usize,
}

impl std::fmt::Display for ParseError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "parse error at offset {}: {}", self.offset, self.message)
    }
}

impl From<LexError> for ParseError {
    fn from(e: LexError) -> Self {
        ParseError {
            message: e.message,
            offset: e.offset,
        }
    }
}

/// Parser state.
struct Parser {
    tokens: Vec<SpannedToken>,
    pos: usize,
}

impl Parser {
    fn new(tokens: Vec<SpannedToken>) -> Self {
        Parser { tokens, pos: 0 }
    }

    fn peek(&self) -> &Token {
        &self.tokens[self.pos].token
    }

    fn offset(&self) -> usize {
        self.tokens[self.pos].offset
    }

    fn advance(&mut self) -> &Token {
        let tok = &self.tokens[self.pos].token;
        if self.pos + 1 < self.tokens.len() {
            self.pos += 1;
        }
        tok
    }

    fn expect(&mut self, expected: &Token) -> Result<(), ParseError> {
        if self.peek() == expected {
            self.advance();
            Ok(())
        } else {
            Err(ParseError {
                message: format!("expected {expected:?}, got {:?}", self.peek()),
                offset: self.offset(),
            })
        }
    }

    fn eat_ident(&mut self) -> Result<String, ParseError> {
        match self.peek().clone() {
            Token::Ident(name) => {
                self.advance();
                Ok(name)
            }
            _ => Err(ParseError {
                message: format!("expected identifier, got {:?}", self.peek()),
                offset: self.offset(),
            }),
        }
    }

    /// Parse: MATCH pattern [WHERE expr] RETURN items
    fn parse_query(&mut self) -> Result<Query, ParseError> {
        self.expect(&Token::Match)?;
        let match_clause = self.parse_match_clause()?;

        let where_clause = if *self.peek() == Token::Where {
            self.advance();
            Some(self.parse_expr()?)
        } else {
            None
        };

        self.expect(&Token::Return)?;
        let return_clause = self.parse_return_clause()?;

        if *self.peek() != Token::Eof {
            return Err(ParseError {
                message: format!("unexpected token after RETURN: {:?}", self.peek()),
                offset: self.offset(),
            });
        }

        Ok(Query {
            match_clause,
            where_clause,
            return_clause,
        })
    }

    /// Parse comma-separated patterns.
    fn parse_match_clause(&mut self) -> Result<MatchClause, ParseError> {
        let mut patterns = vec![self.parse_pattern()?];
        while *self.peek() == Token::Comma {
            self.advance();
            patterns.push(self.parse_pattern()?);
        }
        Ok(MatchClause { patterns })
    }

    /// Parse a single pattern chain: (n)-[r:T]->(m)
    fn parse_pattern(&mut self) -> Result<Pattern, ParseError> {
        let mut elements = Vec::new();

        // Must start with a node
        elements.push(PatternElement::Node(self.parse_node_pattern()?));

        // Continue with optional relationship + node pairs
        loop {
            match self.peek() {
                Token::Dash => {
                    let (rel, node) = self.parse_rel_and_node()?;
                    elements.push(PatternElement::Relationship(rel));
                    elements.push(PatternElement::Node(node));
                }
                Token::LArrow => {
                    let (rel, node) = self.parse_rel_and_node()?;
                    elements.push(PatternElement::Relationship(rel));
                    elements.push(PatternElement::Node(node));
                }
                _ => break,
            }
        }

        Ok(Pattern { elements })
    }

    /// Parse: (variable:Label {key: value})
    fn parse_node_pattern(&mut self) -> Result<NodePattern, ParseError> {
        self.expect(&Token::LParen)?;

        let mut variable = None;
        let mut labels = Vec::new();
        let mut properties = Vec::new();

        // Optional variable name
        if let Token::Ident(_) = self.peek() {
            variable = Some(self.eat_ident()?);
        }

        // Optional labels: :Label1:Label2
        while *self.peek() == Token::Colon {
            self.advance();
            labels.push(self.eat_ident()?);
        }

        // Optional properties: {key: value, ...}
        if *self.peek() == Token::LBrace {
            properties = self.parse_property_map()?;
        }

        self.expect(&Token::RParen)?;

        Ok(NodePattern {
            variable,
            labels,
            properties,
        })
    }

    /// Parse a relationship + the following node from a pattern chain.
    /// Handles: -[r:T]->  <-[r:T]-  -[r:T]-
    fn parse_rel_and_node(&mut self) -> Result<(RelPattern, NodePattern), ParseError> {
        // Determine direction from leading arrow/dash
        let left_arrow = *self.peek() == Token::LArrow;
        if left_arrow {
            self.advance(); // consume <
            self.expect(&Token::Dash)?; // consume -
        } else {
            self.expect(&Token::Dash)?; // consume -
        }

        // Parse relationship detail: [variable:TYPE {props}]
        let mut variable = None;
        let mut rel_types = Vec::new();
        let mut properties = Vec::new();

        if *self.peek() == Token::LBracket {
            self.advance(); // [

            // Optional variable
            if let Token::Ident(_) = self.peek() {
                variable = Some(self.eat_ident()?);
            }

            // Optional types: :TYPE1|TYPE2
            if *self.peek() == Token::Colon {
                self.advance();
                rel_types.push(self.eat_ident()?);
                while *self.peek() == Token::Pipe {
                    self.advance();
                    rel_types.push(self.eat_ident()?);
                }
            }

            // Optional properties
            if *self.peek() == Token::LBrace {
                properties = self.parse_property_map()?;
            }

            self.expect(&Token::RBracket)?; // ]
        }

        // Trailing: -> or - (depends on direction)
        self.expect(&Token::Dash)?; // -
        let right_arrow = *self.peek() == Token::RArrow;
        if right_arrow {
            self.advance(); // >
        }

        let direction = match (left_arrow, right_arrow) {
            (false, true) => RelDirection::Out,
            (true, false) => RelDirection::In,
            (false, false) => RelDirection::Both,
            (true, true) => {
                return Err(ParseError {
                    message: "bidirectional arrow <-[]--> is not valid".into(),
                    offset: self.offset(),
                })
            }
        };

        let node = self.parse_node_pattern()?;

        Ok((
            RelPattern {
                variable,
                rel_types,
                direction,
                properties,
            },
            node,
        ))
    }

    /// Parse: {key: value, key: value}
    fn parse_property_map(&mut self) -> Result<Vec<(String, Expr)>, ParseError> {
        self.expect(&Token::LBrace)?;
        let mut props = Vec::new();

        if *self.peek() != Token::RBrace {
            let key = self.eat_ident()?;
            self.expect(&Token::Colon)?;
            let val = self.parse_expr()?;
            props.push((key, val));

            while *self.peek() == Token::Comma {
                self.advance();
                let key = self.eat_ident()?;
                self.expect(&Token::Colon)?;
                let val = self.parse_expr()?;
                props.push((key, val));
            }
        }

        self.expect(&Token::RBrace)?;
        Ok(props)
    }

    /// Parse RETURN items: expr [AS alias], ...
    fn parse_return_clause(&mut self) -> Result<ReturnClause, ParseError> {
        let distinct = if *self.peek() == Token::Distinct {
            self.advance();
            true
        } else {
            false
        };

        let mut items = vec![self.parse_return_item()?];
        while *self.peek() == Token::Comma {
            self.advance();
            items.push(self.parse_return_item()?);
        }

        Ok(ReturnClause { distinct, items })
    }

    fn parse_return_item(&mut self) -> Result<ReturnItem, ParseError> {
        // Handle star
        if *self.peek() == Token::Star {
            self.advance();
            let alias = if *self.peek() == Token::As {
                self.advance();
                Some(self.eat_ident()?)
            } else {
                None
            };
            return Ok(ReturnItem {
                expr: Expr::Star,
                alias,
            });
        }

        let expr = self.parse_expr()?;
        let alias = if *self.peek() == Token::As {
            self.advance();
            Some(self.eat_ident()?)
        } else {
            None
        };
        Ok(ReturnItem { expr, alias })
    }

    // -----------------------------------------------------------------------
    // Expression parsing (precedence climbing)
    // -----------------------------------------------------------------------

    fn parse_expr(&mut self) -> Result<Expr, ParseError> {
        self.parse_or()
    }

    fn parse_or(&mut self) -> Result<Expr, ParseError> {
        let mut left = self.parse_and()?;
        while *self.peek() == Token::Or {
            self.advance();
            let right = self.parse_and()?;
            left = Expr::Or(Box::new(left), Box::new(right));
        }
        Ok(left)
    }

    fn parse_and(&mut self) -> Result<Expr, ParseError> {
        let mut left = self.parse_not()?;
        while *self.peek() == Token::And {
            self.advance();
            let right = self.parse_not()?;
            left = Expr::And(Box::new(left), Box::new(right));
        }
        Ok(left)
    }

    fn parse_not(&mut self) -> Result<Expr, ParseError> {
        if *self.peek() == Token::Not {
            self.advance();
            let expr = self.parse_not()?;
            Ok(Expr::Not(Box::new(expr)))
        } else {
            self.parse_comparison()
        }
    }

    fn parse_comparison(&mut self) -> Result<Expr, ParseError> {
        let left = self.parse_addition()?;

        let op = match self.peek() {
            Token::Eq => Some(CmpOp::Eq),
            Token::Neq => Some(CmpOp::Neq),
            Token::LArrow => Some(CmpOp::Lt),
            Token::RArrow => Some(CmpOp::Gt),
            Token::Le => Some(CmpOp::Le),
            Token::Ge => Some(CmpOp::Ge),
            Token::Is => {
                self.advance();
                if *self.peek() == Token::Not {
                    self.advance();
                    self.expect(&Token::Null)?;
                    return Ok(Expr::IsNotNull(Box::new(left)));
                }
                self.expect(&Token::Null)?;
                return Ok(Expr::IsNull(Box::new(left)));
            }
            _ => None,
        };

        if let Some(op) = op {
            self.advance();
            let right = self.parse_addition()?;
            Ok(Expr::Compare(Box::new(left), op, Box::new(right)))
        } else {
            Ok(left)
        }
    }

    fn parse_addition(&mut self) -> Result<Expr, ParseError> {
        let mut left = self.parse_multiplication()?;
        loop {
            match self.peek() {
                Token::Plus => {
                    self.advance();
                    let right = self.parse_multiplication()?;
                    left = Expr::Arith(Box::new(left), ArithOp::Add, Box::new(right));
                }
                Token::Dash => {
                    self.advance();
                    let right = self.parse_multiplication()?;
                    left = Expr::Arith(Box::new(left), ArithOp::Sub, Box::new(right));
                }
                _ => break,
            }
        }
        Ok(left)
    }

    fn parse_multiplication(&mut self) -> Result<Expr, ParseError> {
        let mut left = self.parse_unary()?;
        loop {
            match self.peek() {
                Token::Star => {
                    self.advance();
                    let right = self.parse_unary()?;
                    left = Expr::Arith(Box::new(left), ArithOp::Mul, Box::new(right));
                }
                Token::Slash => {
                    self.advance();
                    let right = self.parse_unary()?;
                    left = Expr::Arith(Box::new(left), ArithOp::Div, Box::new(right));
                }
                Token::Percent => {
                    self.advance();
                    let right = self.parse_unary()?;
                    left = Expr::Arith(Box::new(left), ArithOp::Mod, Box::new(right));
                }
                _ => break,
            }
        }
        Ok(left)
    }

    fn parse_unary(&mut self) -> Result<Expr, ParseError> {
        if *self.peek() == Token::Dash {
            self.advance();
            let expr = self.parse_unary()?;
            Ok(Expr::Neg(Box::new(expr)))
        } else {
            self.parse_primary()
        }
    }

    fn parse_primary(&mut self) -> Result<Expr, ParseError> {
        let expr = match self.peek().clone() {
            Token::IntLit(v) => {
                self.advance();
                Expr::IntLit(v)
            }
            Token::FloatLit(v) => {
                self.advance();
                Expr::FloatLit(v)
            }
            Token::StringLit(s) => {
                self.advance();
                Expr::StringLit(s)
            }
            Token::True => {
                self.advance();
                Expr::BoolLit(true)
            }
            Token::False => {
                self.advance();
                Expr::BoolLit(false)
            }
            Token::Null => {
                self.advance();
                Expr::NullLit
            }
            Token::Parameter(name) => {
                self.advance();
                Expr::Parameter(name)
            }
            Token::LParen => {
                self.advance();
                let inner = self.parse_expr()?;
                self.expect(&Token::RParen)?;
                inner
            }
            Token::Ident(name) => {
                self.advance();
                // Check for function call: name(...)
                if *self.peek() == Token::LParen {
                    self.advance(); // (
                    let mut args = Vec::new();
                    if *self.peek() != Token::RParen {
                        // Special case: COUNT(*)
                        if *self.peek() == Token::Star {
                            args.push(Expr::Star);
                            self.advance();
                        } else {
                            args.push(self.parse_expr()?);
                            while *self.peek() == Token::Comma {
                                self.advance();
                                args.push(self.parse_expr()?);
                            }
                        }
                    }
                    self.expect(&Token::RParen)?;
                    Expr::FunctionCall(name, args)
                } else {
                    Expr::Variable(name)
                }
            }
            _ => {
                return Err(ParseError {
                    message: format!("unexpected token in expression: {:?}", self.peek()),
                    offset: self.offset(),
                });
            }
        };

        // Handle property access chains: expr.prop.prop
        self.parse_property_chain(expr)
    }

    fn parse_property_chain(&mut self, mut expr: Expr) -> Result<Expr, ParseError> {
        while *self.peek() == Token::Dot {
            self.advance();
            let prop = self.eat_ident()?;

            // Check for method-style function call: expr.prop(args)
            if *self.peek() == Token::LParen {
                // Not supported in v0.6.0 but let's allow it to produce a
                // clear error later rather than a confusing parse error.
                self.advance();
                let mut args = vec![expr.clone()];
                if *self.peek() != Token::RParen {
                    args.push(self.parse_expr()?);
                    while *self.peek() == Token::Comma {
                        self.advance();
                        args.push(self.parse_expr()?);
                    }
                }
                self.expect(&Token::RParen)?;
                expr = Expr::FunctionCall(prop, args);
            } else {
                expr = Expr::Property(Box::new(expr), prop);
            }
        }
        Ok(expr)
    }
}

/// Parse a Cypher query string into an AST.
pub fn parse(input: &str) -> Result<Query, ParseError> {
    let tokens = crate::cypher::lexer::lex(input)?;
    let mut parser = Parser::new(tokens);
    parser.parse_query()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_simple_match_return() {
        let q = parse("MATCH (n:Person) RETURN n").unwrap();
        assert_eq!(q.match_clause.patterns.len(), 1);
        let p = &q.match_clause.patterns[0];
        assert_eq!(p.elements.len(), 1);
        match &p.elements[0] {
            PatternElement::Node(n) => {
                assert_eq!(n.variable.as_deref(), Some("n"));
                assert_eq!(n.labels, vec!["Person"]);
            }
            _ => panic!("expected node"),
        }
        assert!(q.where_clause.is_none());
        assert_eq!(q.return_clause.items.len(), 1);
    }

    #[test]
    fn test_match_with_relationship() {
        let q = parse("MATCH (a)-[r:KNOWS]->(b) RETURN a, b").unwrap();
        let p = &q.match_clause.patterns[0];
        assert_eq!(p.elements.len(), 3); // node, rel, node
        match &p.elements[1] {
            PatternElement::Relationship(r) => {
                assert_eq!(r.variable.as_deref(), Some("r"));
                assert_eq!(r.rel_types, vec!["KNOWS"]);
                assert_eq!(r.direction, RelDirection::Out);
            }
            _ => panic!("expected relationship"),
        }
    }

    #[test]
    fn test_left_arrow() {
        let q = parse("MATCH (a)<-[r:KNOWS]-(b) RETURN a").unwrap();
        match &q.match_clause.patterns[0].elements[1] {
            PatternElement::Relationship(r) => {
                assert_eq!(r.direction, RelDirection::In);
            }
            _ => panic!("expected relationship"),
        }
    }

    #[test]
    fn test_undirected() {
        let q = parse("MATCH (a)-[r:KNOWS]-(b) RETURN a").unwrap();
        match &q.match_clause.patterns[0].elements[1] {
            PatternElement::Relationship(r) => {
                assert_eq!(r.direction, RelDirection::Both);
            }
            _ => panic!("expected relationship"),
        }
    }

    #[test]
    fn test_where_clause() {
        let q = parse("MATCH (n:Person) WHERE n.age > 30 RETURN n").unwrap();
        assert!(q.where_clause.is_some());
        match &q.where_clause.unwrap() {
            Expr::Compare(_, CmpOp::Gt, _) => {}
            other => panic!("expected Compare(Gt), got {other:?}"),
        }
    }

    #[test]
    fn test_where_and_or() {
        let q = parse("MATCH (n) WHERE n.x = 1 AND n.y = 2 OR n.z = 3 RETURN n").unwrap();
        // OR has lower precedence than AND: (x=1 AND y=2) OR z=3
        match &q.where_clause.unwrap() {
            Expr::Or(_, _) => {}
            other => panic!("expected Or at top, got {other:?}"),
        }
    }

    #[test]
    fn test_is_null() {
        let q = parse("MATCH (n) WHERE n.x IS NULL RETURN n").unwrap();
        match &q.where_clause.unwrap() {
            Expr::IsNull(_) => {}
            other => panic!("expected IsNull, got {other:?}"),
        }
    }

    #[test]
    fn test_is_not_null() {
        let q = parse("MATCH (n) WHERE n.x IS NOT NULL RETURN n").unwrap();
        match &q.where_clause.unwrap() {
            Expr::IsNotNull(_) => {}
            other => panic!("expected IsNotNull, got {other:?}"),
        }
    }

    #[test]
    fn test_return_alias() {
        let q = parse("MATCH (n) RETURN n.name AS name").unwrap();
        assert_eq!(q.return_clause.items[0].alias.as_deref(), Some("name"));
    }

    #[test]
    fn test_return_distinct() {
        let q = parse("MATCH (n) RETURN DISTINCT n").unwrap();
        assert!(q.return_clause.distinct);
    }

    #[test]
    fn test_function_call() {
        let q = parse("MATCH (n) RETURN id(n)").unwrap();
        match &q.return_clause.items[0].expr {
            Expr::FunctionCall(name, args) => {
                assert_eq!(name, "id");
                assert_eq!(args.len(), 1);
            }
            other => panic!("expected FunctionCall, got {other:?}"),
        }
    }

    #[test]
    fn test_multi_pattern() {
        let q = parse("MATCH (a:Person), (b:Company) RETURN a, b").unwrap();
        assert_eq!(q.match_clause.patterns.len(), 2);
    }

    #[test]
    fn test_inline_properties() {
        let q = parse("MATCH (n:Person {name: 'Alice', age: 30}) RETURN n").unwrap();
        match &q.match_clause.patterns[0].elements[0] {
            PatternElement::Node(n) => {
                assert_eq!(n.properties.len(), 2);
                assert_eq!(n.properties[0].0, "name");
                assert_eq!(n.properties[1].0, "age");
            }
            _ => panic!("expected node"),
        }
    }

    #[test]
    fn test_chain_pattern() {
        let q = parse("MATCH (a)-[:KNOWS]->(b)-[:LIKES]->(c) RETURN a, c").unwrap();
        let p = &q.match_clause.patterns[0];
        // a, KNOWS, b, LIKES, c = 5 elements
        assert_eq!(p.elements.len(), 5);
    }

    #[test]
    fn test_error_on_junk() {
        assert!(parse("MATCH (n) RETURN n GARBAGE").is_err());
    }
}
