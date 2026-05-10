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

    /// Look ahead `offset` positions beyond the current position.
    fn peek_at(&self, offset: usize) -> &Token {
        let idx = (self.pos + offset).min(self.tokens.len() - 1);
        &self.tokens[idx].token
    }

    /// Parse a full query pipeline: (MATCH | OPTIONAL MATCH | UNWIND | WITH)* RETURN ...
    fn parse_query(&mut self) -> Result<Query, ParseError> {
        let mut clauses: Vec<QueryClause> = Vec::new();

        loop {
            match self.peek().clone() {
                Token::Match => {
                    self.advance();
                    let patterns = self.parse_patterns()?;
                    let where_clause = self.try_parse_where()?;
                    clauses.push(QueryClause::Match { optional: false, patterns, where_clause });
                }
                Token::OptionalMatch => {
                    self.advance();
                    let patterns = self.parse_patterns()?;
                    let where_clause = self.try_parse_where()?;
                    clauses.push(QueryClause::Match { optional: true, patterns, where_clause });
                }
                Token::Unwind => {
                    self.advance();
                    let expr = self.parse_expr()?;
                    if let Token::Ident(ref s) = self.peek().clone() {
                        if !s.eq_ignore_ascii_case("AS") {
                            return Err(ParseError {
                                message: format!("expected AS after UNWIND expression, got {:?}", self.peek()),
                                offset: self.offset(),
                            });
                        }
                        self.advance();
                    } else {
                        self.expect(&Token::As)?;
                    }
                    let alias = self.eat_ident_flexible()?;
                    clauses.push(QueryClause::Unwind { expr, alias });
                }
                Token::With => {
                    self.advance();
                    let (distinct, items) = self.parse_return_items()?;
                    let order_by = self.try_parse_order_by()?;
                    let skip = self.try_parse_skip()?;
                    let limit = self.try_parse_limit()?;
                    let where_clause = self.try_parse_where()?;
                    clauses.push(QueryClause::With { distinct, items, order_by, skip, limit, where_clause });
                }
                Token::Return => {
                    self.advance();
                    let (distinct, items) = self.parse_return_items()?;
                    let order_by = self.try_parse_order_by()?;
                    let skip = self.try_parse_skip()?;
                    let limit = self.try_parse_limit()?;
                    clauses.push(QueryClause::Return { distinct, items, order_by, skip, limit });
                    break;
                }
                Token::Eof => break,
                other => {
                    return Err(ParseError {
                        message: format!("unexpected token in query: {:?}", other),
                        offset: self.offset(),
                    });
                }
            }
        }

        if *self.peek() != Token::Eof {
            return Err(ParseError {
                message: format!("unexpected token after RETURN: {:?}", self.peek()),
                offset: self.offset(),
            });
        }

        Ok(Query { clauses })
    }

    /// Parse a WHERE clause if present.
    fn try_parse_where(&mut self) -> Result<Option<Expr>, ParseError> {
        if *self.peek() == Token::Where {
            self.advance();
            Ok(Some(self.parse_expr()?))
        } else {
            Ok(None)
        }
    }

    /// Parse ORDER BY if present.
    fn try_parse_order_by(&mut self) -> Result<Vec<OrderItem>, ParseError> {
        if *self.peek() != Token::OrderBy {
            return Ok(Vec::new());
        }
        self.advance();
        let mut items = Vec::new();
        loop {
            let expr = self.parse_expr()?;
            let ascending = match self.peek().clone() {
                Token::Ident(ref s) if s.eq_ignore_ascii_case("DESC")
                                   || s.eq_ignore_ascii_case("DESCENDING") => {
                    self.advance();
                    false
                }
                Token::Ident(ref s) if s.eq_ignore_ascii_case("ASC")
                                   || s.eq_ignore_ascii_case("ASCENDING") => {
                    self.advance();
                    true
                }
                _ => true,
            };
            items.push(OrderItem { expr, ascending });
            if *self.peek() != Token::Comma { break; }
            self.advance();
        }
        Ok(items)
    }

    fn try_parse_skip(&mut self) -> Result<Option<Expr>, ParseError> {
        if *self.peek() == Token::Skip {
            self.advance();
            Ok(Some(self.parse_expr()?))
        } else {
            Ok(None)
        }
    }

    fn try_parse_limit(&mut self) -> Result<Option<Expr>, ParseError> {
        if *self.peek() == Token::Limit {
            self.advance();
            Ok(Some(self.parse_expr()?))
        } else {
            Ok(None)
        }
    }

    /// Parse comma-separated patterns.
    fn parse_patterns(&mut self) -> Result<Vec<Pattern>, ParseError> {
        let mut patterns = vec![self.parse_pattern()?];
        while *self.peek() == Token::Comma {
            self.advance();
            patterns.push(self.parse_pattern()?);
        }
        Ok(patterns)
    }

    /// Parse RETURN items: [DISTINCT] expr [AS alias], ...
    fn parse_return_items(&mut self) -> Result<(bool, Vec<ReturnItem>), ParseError> {
        let distinct = if *self.peek() == Token::Distinct {
            self.advance();
            true
        } else {
            false
        };
        let items = self.parse_return_clause_items()?;
        Ok((distinct, items))
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

    /// Parse a comma-separated list of return items (without the DISTINCT prefix).
    fn parse_return_clause_items(&mut self) -> Result<Vec<ReturnItem>, ParseError> {
        let mut items = vec![self.parse_return_item()?];
        while *self.peek() == Token::Comma {
            self.advance();
            items.push(self.parse_return_item()?);
        }
        Ok(items)
    }

    /// Like eat_ident but also accepts keyword tokens that are valid as identifiers
    /// in certain positions (e.g. variable names after UNWIND ... AS).
    fn eat_ident_flexible(&mut self) -> Result<String, ParseError> {
        match self.peek().clone() {
            Token::Ident(name) => { self.advance(); Ok(name) }
            Token::End => { self.advance(); Ok("end".to_string()) }
            other => Err(ParseError {
                message: format!("expected identifier, got {:?}", other),
                offset: self.offset(),
            }),
        }
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
        let mut left = self.parse_xor()?;
        while *self.peek() == Token::Or {
            self.advance();
            let right = self.parse_xor()?;
            left = Expr::Or(Box::new(left), Box::new(right));
        }
        Ok(left)
    }

    fn parse_xor(&mut self) -> Result<Expr, ParseError> {
        let mut left = self.parse_and()?;
        while *self.peek() == Token::Xor {
            self.advance();
            let right = self.parse_and()?;
            left = Expr::Xor(Box::new(left), Box::new(right));
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
        let left = self.parse_inlist()?;

        // String predicates: STARTS WITH, ENDS WITH, CONTAINS
        if let Token::Ident(s) = self.peek().clone() {
            let upper = s.to_ascii_uppercase();
            match upper.as_str() {
                "STARTS" => {
                    self.advance();
                    self.expect(&Token::With)?;
                    let right = self.parse_inlist()?;
                    return Ok(Expr::StartsWith(Box::new(left), Box::new(right)));
                }
                "ENDS" => {
                    self.advance();
                    self.expect(&Token::With)?;
                    let right = self.parse_inlist()?;
                    return Ok(Expr::EndsWith(Box::new(left), Box::new(right)));
                }
                "CONTAINS" => {
                    self.advance();
                    let right = self.parse_inlist()?;
                    return Ok(Expr::Contains(Box::new(left), Box::new(right)));
                }
                _ => {}
            }
        }

        // =~ regex match
        if *self.peek() == Token::RegexMatch {
            self.advance();
            let right = self.parse_inlist()?;
            return Ok(Expr::Regex(Box::new(left), Box::new(right)));
        }

        let op = match self.peek() {
            Token::Eq => Some(CmpOp::Eq),
            Token::Neq => Some(CmpOp::Neq),
            Token::LArrow => Some(CmpOp::Lt),
            Token::RArrow => Some(CmpOp::Gt),
            Token::Le => Some(CmpOp::Le),
            Token::Ge => Some(CmpOp::Ge),
            _ => None,
        };

        if let Some(op) = op {
            self.advance();
            let right = self.parse_inlist()?;
            Ok(Expr::Compare(Box::new(left), op, Box::new(right)))
        } else {
            Ok(left)
        }
    }

    /// Parse IN list membership (higher precedence than comparison operators).
    fn parse_inlist(&mut self) -> Result<Expr, ParseError> {
        let left = self.parse_is_null()?;
        if *self.peek() == Token::In {
            self.advance();
            let list_expr = self.parse_is_null()?;
            return Ok(Expr::InList(Box::new(left), Box::new(list_expr)));
        }
        Ok(left)
    }

    /// Parse IS NULL / IS NOT NULL (higher precedence than IN and comparison).
    fn parse_is_null(&mut self) -> Result<Expr, ParseError> {
        let expr = self.parse_addition()?;
        if *self.peek() == Token::Is {
            self.advance();
            if *self.peek() == Token::Not {
                self.advance();
                self.expect(&Token::Null)?;
                return Ok(Expr::IsNotNull(Box::new(expr)));
            }
            self.expect(&Token::Null)?;
            return Ok(Expr::IsNull(Box::new(expr)));
        }
        Ok(expr)
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
        let mut left = self.parse_power()?;
        loop {
            match self.peek() {
                Token::Star => {
                    self.advance();
                    let right = self.parse_power()?;
                    left = Expr::Arith(Box::new(left), ArithOp::Mul, Box::new(right));
                }
                Token::Slash => {
                    self.advance();
                    let right = self.parse_power()?;
                    left = Expr::Arith(Box::new(left), ArithOp::Div, Box::new(right));
                }
                Token::Percent => {
                    self.advance();
                    let right = self.parse_power()?;
                    left = Expr::Arith(Box::new(left), ArithOp::Mod, Box::new(right));
                }
                _ => break,
            }
        }
        Ok(left)
    }

    /// Parse exponentiation (right-associative): `a ^ b ^ c` = `a ^ (b ^ c)`.
    fn parse_power(&mut self) -> Result<Expr, ParseError> {
        // Left-associative (openCypher TCK specifies left-to-right for ^)
        let mut left = self.parse_unary()?;
        while *self.peek() == Token::Caret {
            self.advance();
            let right = self.parse_unary()?;
            left = Expr::Arith(Box::new(left), ArithOp::Pow, Box::new(right));
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
            Token::LBracket => {
                // List comprehension: [var IN list WHERE? pred? | proj?]
                // vs list literal: [expr, expr, ...]
                // Detect by: Ident followed by Token::In at peek_at(1).
                self.advance();
                if matches!(self.peek(), Token::Ident(_)) && *self.peek_at(1) == Token::In {
                    let var = self.eat_ident()?;
                    self.advance(); // consume IN
                    let list_expr = self.parse_expr()?;
                    let predicate = if *self.peek() == Token::Where {
                        self.advance();
                        Some(Box::new(self.parse_expr()?))
                    } else {
                        None
                    };
                    let projection = if *self.peek() == Token::Pipe {
                        self.advance();
                        Some(Box::new(self.parse_expr()?))
                    } else {
                        None
                    };
                    self.expect(&Token::RBracket)?;
                    Expr::ListComprehension {
                        variable: var,
                        list_expr: Box::new(list_expr),
                        predicate,
                        projection,
                    }
                } else {
                    // List literal: [expr, expr, ...]
                    let mut elems = Vec::new();
                    if *self.peek() != Token::RBracket {
                        elems.push(self.parse_expr()?);
                        while *self.peek() == Token::Comma {
                            self.advance();
                            elems.push(self.parse_expr()?);
                        }
                    }
                    self.expect(&Token::RBracket)?;
                    Expr::List(elems)
                }
            }
            Token::Case => {
                self.advance();
                self.parse_case_expr()?
            }
            Token::Ident(name) => {
                self.advance();
                // Check for function call: name(...)
                if *self.peek() == Token::LParen {
                    self.advance(); // (
                    let lc = name.to_ascii_lowercase();

                    // List predicate functions: any/all/none/single — special syntax
                    // any(x IN list WHERE pred)
                    let list_pred_kind = match lc.as_str() {
                        "any"    => Some(ListPredicateKind::Any),
                        "all"    => Some(ListPredicateKind::All),
                        "none"   => Some(ListPredicateKind::None_),
                        "single" => Some(ListPredicateKind::Single),
                        _ => None,
                    };
                    if let Some(kind) = list_pred_kind {
                        let var = self.eat_ident()?;
                        self.expect(&Token::In)?;
                        let list_expr = self.parse_expr()?;
                        let predicate = if *self.peek() == Token::Where {
                            self.advance();
                            self.parse_expr()?
                        } else {
                            Expr::BoolLit(true)
                        };
                        self.expect(&Token::RParen)?;
                        return self.parse_property_chain(Expr::ListPredicate {
                            kind,
                            variable: var,
                            list_expr: Box::new(list_expr),
                            predicate: Box::new(predicate),
                        }).map(Ok)?;
                    }

                    // filter(x IN list WHERE pred) — list comprehension without projection
                    if lc == "filter" && matches!(self.peek(), Token::Ident(_))
                        && *self.peek_at(1) == Token::In
                    {
                        let var = self.eat_ident()?;
                        self.expect(&Token::In)?;
                        let list_expr = self.parse_expr()?;
                        let predicate = if *self.peek() == Token::Where {
                            self.advance();
                            Some(Box::new(self.parse_expr()?))
                        } else {
                            None
                        };
                        self.expect(&Token::RParen)?;
                        return self.parse_property_chain(Expr::ListComprehension {
                            variable: var,
                            list_expr: Box::new(list_expr),
                            predicate,
                            projection: None,
                        }).map(Ok)?;
                    }

                    // extract(x IN list | expr) — deprecated list comprehension
                    if lc == "extract" && matches!(self.peek(), Token::Ident(_))
                        && *self.peek_at(1) == Token::In
                    {
                        let var = self.eat_ident()?;
                        self.expect(&Token::In)?;
                        let list_expr = self.parse_expr()?;
                        let projection = if *self.peek() == Token::Pipe {
                            self.advance();
                            Some(Box::new(self.parse_expr()?))
                        } else {
                            None
                        };
                        self.expect(&Token::RParen)?;
                        return self.parse_property_chain(Expr::ListComprehension {
                            variable: var,
                            list_expr: Box::new(list_expr),
                            predicate: None,
                            projection,
                        }).map(Ok)?;
                    }

                    // Aggregate functions: handle DISTINCT prefix
                    let is_agg = is_aggregate_fn_name(&lc);
                    let (fn_name, args) = if *self.peek() == Token::RParen {
                        (name.clone(), vec![])
                    } else if is_agg && *self.peek() == Token::Distinct {
                        self.advance(); // consume DISTINCT
                        let mut args = vec![];
                        if *self.peek() != Token::RParen {
                            args.push(self.parse_expr()?);
                            while *self.peek() == Token::Comma {
                                self.advance();
                                args.push(self.parse_expr()?);
                            }
                        }
                        (format!("{lc}_distinct"), args)
                    } else if *self.peek() == Token::Star {
                        self.advance();
                        (name.clone(), vec![Expr::Star])
                    } else {
                        let mut args = vec![self.parse_expr()?];
                        while *self.peek() == Token::Comma {
                            self.advance();
                            args.push(self.parse_expr()?);
                        }
                        (name.clone(), args)
                    };
                    self.expect(&Token::RParen)?;
                    Expr::FunctionCall(fn_name, args)
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

    /// Parse a CASE expression (after consuming CASE token).
    /// CASE [test] WHEN val THEN val ... [ELSE val] END
    fn parse_case_expr(&mut self) -> Result<Expr, ParseError> {
        // Simple CASE: CASE test WHEN ...
        // Searched CASE: CASE WHEN cond THEN ...
        let is_searched = *self.peek() == Token::When;

        let test = if is_searched {
            None
        } else {
            Some(Box::new(self.parse_expr()?))
        };

        let mut branches: Vec<(Expr, Expr)> = Vec::new();
        while *self.peek() == Token::When {
            self.advance();
            let when_expr = self.parse_expr()?;
            self.expect(&Token::Then)?;
            let then_expr = self.parse_expr()?;
            branches.push((when_expr, then_expr));
        }

        let else_ = if *self.peek() == Token::Else {
            self.advance();
            Some(Box::new(self.parse_expr()?))
        } else {
            None
        };

        self.expect(&Token::End)?;

        if let Some(test) = test {
            Ok(Expr::CaseSimple { test, branches, else_ })
        } else {
            Ok(Expr::CaseSearched { branches, else_ })
        }
    }

    fn parse_property_chain(&mut self, mut expr: Expr) -> Result<Expr, ParseError> {
        loop {
            if *self.peek() == Token::Dot {
                self.advance();
                let prop = self.eat_ident()?;

                // Check for method-style function call: expr.prop(args)
                if *self.peek() == Token::LParen {
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
            } else if *self.peek() == Token::LBracket {
                self.advance(); // consume [
                if *self.peek() == Token::DotDot {
                    // [..to]
                    self.advance();
                    let to = if *self.peek() != Token::RBracket {
                        Some(Box::new(self.parse_expr()?))
                    } else {
                        None
                    };
                    self.expect(&Token::RBracket)?;
                    expr = Expr::ListSlice { list_expr: Box::new(expr), from: None, to };
                } else {
                    let inner = self.parse_expr()?;
                    if *self.peek() == Token::DotDot {
                        // [from..to] or [from..]
                        self.advance();
                        let to = if *self.peek() != Token::RBracket {
                            Some(Box::new(self.parse_expr()?))
                        } else {
                            None
                        };
                        self.expect(&Token::RBracket)?;
                        expr = Expr::ListSlice { list_expr: Box::new(expr), from: Some(Box::new(inner)), to };
                    } else {
                        self.expect(&Token::RBracket)?;
                        expr = Expr::Subscript(Box::new(expr), Box::new(inner));
                    }
                }
            } else {
                break;
            }
        }
        Ok(expr)
    }
}

/// Returns true if the given (lowercase) function name is an aggregate.
fn is_aggregate_fn_name(name: &str) -> bool {
    matches!(
        name,
        "count" | "sum" | "avg" | "min" | "max" | "collect"
            | "stdev" | "stdevp" | "percentilecont" | "percentiledisc"
    )
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

    /// Helper: get the first Match clause's patterns.
    fn first_match_patterns(q: &Query) -> &Vec<Pattern> {
        match &q.clauses[0] {
            QueryClause::Match { patterns, .. } => patterns,
            other => panic!("expected Match clause, got {other:?}"),
        }
    }

    /// Helper: get the first Match clause's where_clause.
    fn first_where(q: &Query) -> Option<&Expr> {
        match &q.clauses[0] {
            QueryClause::Match { where_clause, .. } => where_clause.as_ref(),
            other => panic!("expected Match clause, got {other:?}"),
        }
    }

    /// Helper: get the Return clause items.
    fn return_items(q: &Query) -> &Vec<ReturnItem> {
        for c in &q.clauses {
            if let QueryClause::Return { items, .. } = c { return items; }
        }
        panic!("no Return clause found");
    }

    /// Helper: get the Return clause's distinct flag.
    fn return_distinct(q: &Query) -> bool {
        for c in &q.clauses {
            if let QueryClause::Return { distinct, .. } = c { return *distinct; }
        }
        false
    }

    #[test]
    fn test_simple_match_return() {
        let q = parse("MATCH (n:Person) RETURN n").unwrap();
        let patterns = first_match_patterns(&q);
        assert_eq!(patterns.len(), 1);
        let p = &patterns[0];
        assert_eq!(p.elements.len(), 1);
        match &p.elements[0] {
            PatternElement::Node(n) => {
                assert_eq!(n.variable.as_deref(), Some("n"));
                assert_eq!(n.labels, vec!["Person"]);
            }
            _ => panic!("expected node"),
        }
        assert!(first_where(&q).is_none());
        assert_eq!(return_items(&q).len(), 1);
    }

    #[test]
    fn test_match_with_relationship() {
        let q = parse("MATCH (a)-[r:KNOWS]->(b) RETURN a, b").unwrap();
        let p = &first_match_patterns(&q)[0];
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
        match &first_match_patterns(&q)[0].elements[1] {
            PatternElement::Relationship(r) => {
                assert_eq!(r.direction, RelDirection::In);
            }
            _ => panic!("expected relationship"),
        }
    }

    #[test]
    fn test_undirected() {
        let q = parse("MATCH (a)-[r:KNOWS]-(b) RETURN a").unwrap();
        match &first_match_patterns(&q)[0].elements[1] {
            PatternElement::Relationship(r) => {
                assert_eq!(r.direction, RelDirection::Both);
            }
            _ => panic!("expected relationship"),
        }
    }

    #[test]
    fn test_where_clause() {
        let q = parse("MATCH (n:Person) WHERE n.age > 30 RETURN n").unwrap();
        assert!(first_where(&q).is_some());
        match first_where(&q).unwrap() {
            Expr::Compare(_, CmpOp::Gt, _) => {}
            other => panic!("expected Compare(Gt), got {other:?}"),
        }
    }

    #[test]
    fn test_where_and_or() {
        let q = parse("MATCH (n) WHERE n.x = 1 AND n.y = 2 OR n.z = 3 RETURN n").unwrap();
        match first_where(&q).unwrap() {
            Expr::Or(_, _) => {}
            other => panic!("expected Or at top, got {other:?}"),
        }
    }

    #[test]
    fn test_is_null() {
        let q = parse("MATCH (n) WHERE n.x IS NULL RETURN n").unwrap();
        match first_where(&q).unwrap() {
            Expr::IsNull(_) => {}
            other => panic!("expected IsNull, got {other:?}"),
        }
    }

    #[test]
    fn test_is_not_null() {
        let q = parse("MATCH (n) WHERE n.x IS NOT NULL RETURN n").unwrap();
        match first_where(&q).unwrap() {
            Expr::IsNotNull(_) => {}
            other => panic!("expected IsNotNull, got {other:?}"),
        }
    }

    #[test]
    fn test_return_alias() {
        let q = parse("MATCH (n) RETURN n.name AS name").unwrap();
        assert_eq!(return_items(&q)[0].alias.as_deref(), Some("name"));
    }

    #[test]
    fn test_return_distinct() {
        let q = parse("MATCH (n) RETURN DISTINCT n").unwrap();
        assert!(return_distinct(&q));
    }

    #[test]
    fn test_function_call() {
        let q = parse("MATCH (n) RETURN id(n)").unwrap();
        match &return_items(&q)[0].expr {
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
        assert_eq!(first_match_patterns(&q).len(), 2);
    }

    #[test]
    fn test_inline_properties() {
        let q = parse("MATCH (n:Person {name: 'Alice', age: 30}) RETURN n").unwrap();
        match &first_match_patterns(&q)[0].elements[0] {
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
        let p = &first_match_patterns(&q)[0];
        // a, KNOWS, b, LIKES, c = 5 elements
        assert_eq!(p.elements.len(), 5);
    }

    #[test]
    fn test_error_on_junk() {
        assert!(parse("MATCH (n) RETURN n GARBAGE").is_err());
    }

    #[test]
    fn test_optional_match() {
        let q = parse("MATCH (a) OPTIONAL MATCH (a)-[:KNOWS]->(b) RETURN a, b").unwrap();
        assert_eq!(q.clauses.len(), 3); // Match, OptionalMatch, Return
        match &q.clauses[1] {
            QueryClause::Match { optional, .. } => assert!(*optional),
            other => panic!("expected optional Match, got {other:?}"),
        }
    }

    #[test]
    fn test_unwind() {
        let q = parse("UNWIND [1, 2, 3] AS x RETURN x").unwrap();
        match &q.clauses[0] {
            QueryClause::Unwind { alias, .. } => assert_eq!(alias, "x"),
            other => panic!("expected Unwind, got {other:?}"),
        }
    }

    #[test]
    fn test_with_clause() {
        let q = parse("MATCH (n) WITH n RETURN n").unwrap();
        assert!(q.clauses.iter().any(|c| matches!(c, QueryClause::With { .. })));
    }

    #[test]
    fn test_case_searched() {
        let q = parse("MATCH (n) RETURN CASE WHEN n.x = 1 THEN 'one' ELSE 'other' END").unwrap();
        match &return_items(&q)[0].expr {
            Expr::CaseSearched { branches, else_ } => {
                assert_eq!(branches.len(), 1);
                assert!(else_.is_some());
            }
            other => panic!("expected CaseSearched, got {other:?}"),
        }
    }

    #[test]
    fn test_case_simple() {
        let q = parse("MATCH (n) RETURN CASE n.x WHEN 1 THEN 'one' WHEN 2 THEN 'two' END").unwrap();
        match &return_items(&q)[0].expr {
            Expr::CaseSimple { branches, else_, .. } => {
                assert_eq!(branches.len(), 2);
                assert!(else_.is_none());
            }
            other => panic!("expected CaseSimple, got {other:?}"),
        }
    }
}

