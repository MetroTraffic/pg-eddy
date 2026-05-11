// Cypher lexer — tokenises an openCypher query string.
//
// All openCypher token types for v0.6.0 scope: identifiers, keywords,
// string/integer/float literals, operators, punctuation, and parameters.

/// Token types produced by the lexer.
#[allow(dead_code)]
#[derive(Debug, Clone, PartialEq)]
pub enum Token {
    // Keywords (case-insensitive)
    Match,
    OptionalMatch,
    Where,
    Return,
    As,
    Distinct,
    OrderBy,  // reserved for v0.7.0
    Limit,    // reserved for v0.7.0
    Skip,     // reserved for v0.7.0
    And,
    Or,
    Xor,
    Not,
    In,
    Is,
    Null,
    True,
    False,
    Create,   // v0.12.0 write clause
    Delete,   // v0.12.0 write clause
    Detach,   // v0.12.0 write clause
    Set,      // v0.12.0 write clause
    Merge,    // v0.12.0 write clause
    Remove,   // v0.12.0 write clause
    Foreach,  // FOREACH clause (v0.14.0)
    On,       // ON CREATE / ON MATCH in MERGE (v0.12.0)
    Call,     // CALL clause (v0.11.0)
    Yield,    // YIELD in CALL (v0.11.0)
    With,     // WITH clause (also used in STARTS WITH / ENDS WITH)
    Unwind,   // UNWIND clause
    Case,     // CASE expression
    When,     // WHEN branch
    Then,     // THEN result
    Else,     // ELSE default
    End,      // END of CASE

    // Identifiers and literals
    Ident(String),
    StringLit(String),
    IntLit(i64),
    FloatLit(f64),
    Parameter(String), // $paramName

    // Punctuation
    LParen,     // (
    RParen,     // )
    LBracket,   // [
    RBracket,   // ]
    LBrace,     // {
    RBrace,     // }
    Colon,      // :
    Comma,      // ,
    Dot,        // .
    Pipe,       // |
    DotDot,     // ..

    // Arrows / dashes
    Dash,       // -
    LArrow,     // <
    RArrow,     // >

    // Operators
    Eq,         // =
    Neq,        // <>
    Lt,         // <  (context-dependent with LArrow)
    Gt,         // >  (context-dependent with RArrow)
    Le,         // <=
    Ge,         // >=
    RegexMatch, // =~
    Plus,       // +
    PlusEq,     // +=
    Star,       // *
    Slash,      // /
    Percent,    // %
    Caret,      // ^

    // End of input
    Eof,
}

/// A token with its position in the source string.
#[derive(Debug, Clone)]
pub struct SpannedToken {
    pub token: Token,
    pub offset: usize,
}

/// Lexer error.
#[derive(Debug, Clone)]
pub struct LexError {
    pub message: String,
    pub offset: usize,
}

impl std::fmt::Display for LexError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "lex error at offset {}: {}", self.offset, self.message)
    }
}

/// Tokenise a Cypher query string into a vector of spanned tokens.
pub fn lex(input: &str) -> Result<Vec<SpannedToken>, LexError> {
    let mut tokens = Vec::new();
    let bytes = input.as_bytes();
    let len = bytes.len();
    let mut pos = 0;

    while pos < len {
        // Skip whitespace
        if bytes[pos].is_ascii_whitespace() {
            pos += 1;
            continue;
        }

        // Skip line comments: //
        if pos + 1 < len && bytes[pos] == b'/' && bytes[pos + 1] == b'/' {
            while pos < len && bytes[pos] != b'\n' {
                pos += 1;
            }
            continue;
        }

        // Skip block comments: /* ... */
        if pos + 1 < len && bytes[pos] == b'/' && bytes[pos + 1] == b'*' {
            pos += 2;
            let start = pos;
            while pos + 1 < len && !(bytes[pos] == b'*' && bytes[pos + 1] == b'/') {
                pos += 1;
            }
            if pos + 1 >= len {
                return Err(LexError {
                    message: "unterminated block comment".into(),
                    offset: start - 2,
                });
            }
            pos += 2; // skip */
            continue;
        }

        let start = pos;

        // String literals: single-quoted
        if bytes[pos] == b'\'' {
            pos += 1;
            let mut s = String::new();
            while pos < len {
                if bytes[pos] == b'\'' {
                    if pos + 1 < len && bytes[pos + 1] == b'\'' {
                        // escaped quote ''
                        s.push('\'');
                        pos += 2;
                    } else {
                        pos += 1; // closing quote
                        break;
                    }
                } else if bytes[pos] == b'\\' && pos + 1 < len {
                    pos += 1;
                    match bytes[pos] {
                        b'n' => s.push('\n'),
                        b't' => s.push('\t'),
                        b'r' => s.push('\r'),
                        b'\\' => s.push('\\'),
                        b'\'' => s.push('\''),
                        b'"' => s.push('"'),
                        _ => {
                            s.push('\\');
                            s.push(bytes[pos] as char);
                        }
                    }
                    pos += 1;
                } else {
                    s.push(bytes[pos] as char);
                    pos += 1;
                }
            }
            tokens.push(SpannedToken { token: Token::StringLit(s), offset: start });
            continue;
        }

        // Double-quoted string literals
        if bytes[pos] == b'"' {
            pos += 1;
            let mut s = String::new();
            while pos < len && bytes[pos] != b'"' {
                if bytes[pos] == b'\\' && pos + 1 < len {
                    pos += 1;
                    match bytes[pos] {
                        b'n' => s.push('\n'),
                        b't' => s.push('\t'),
                        b'r' => s.push('\r'),
                        b'\\' => s.push('\\'),
                        b'"' => s.push('"'),
                        _ => {
                            s.push('\\');
                            s.push(bytes[pos] as char);
                        }
                    }
                } else {
                    s.push(bytes[pos] as char);
                }
                pos += 1;
            }
            if pos < len {
                pos += 1; // closing "
            }
            tokens.push(SpannedToken { token: Token::StringLit(s), offset: start });
            continue;
        }

        // Backtick-quoted identifiers
        if bytes[pos] == b'`' {
            pos += 1;
            let mut s = String::new();
            while pos < len && bytes[pos] != b'`' {
                s.push(bytes[pos] as char);
                pos += 1;
            }
            if pos < len {
                pos += 1; // closing `
            }
            tokens.push(SpannedToken { token: Token::Ident(s), offset: start });
            continue;
        }

        // Numbers: integers and floats
        if bytes[pos].is_ascii_digit() || (bytes[pos] == b'.' && pos + 1 < len && bytes[pos + 1].is_ascii_digit()) {
            // Hex literal: 0x1A2B or 0X1a2b
            if bytes[pos] == b'0' && pos + 1 < len && (bytes[pos + 1] == b'x' || bytes[pos + 1] == b'X') {
                pos += 2; // consume "0x"
                let hex_start = pos;
                while pos < len && bytes[pos].is_ascii_hexdigit() {
                    pos += 1;
                }
                let hex_str = &input[hex_start..pos];
                if hex_str.is_empty() {
                    return Err(LexError { message: "incomplete hexadecimal integer".into(), offset: start });
                }
                // Parse as u64 first to handle 0x8000000000000000 (i64::MIN after negation).
                let val = if let Ok(v) = u64::from_str_radix(hex_str, 16) {
                    v as i64 // wrapping cast: handles i64::MIN case (0x8000000000000000)
                } else {
                    return Err(LexError { message: "hexadecimal integer overflow".into(), offset: start });
                };
                tokens.push(SpannedToken { token: Token::IntLit(val), offset: start });
                continue;
            }
            // Octal literal: 0o777 or 0O777
            if bytes[pos] == b'0' && pos + 1 < len && (bytes[pos + 1] == b'o' || bytes[pos + 1] == b'O') {
                pos += 2; // consume "0o"
                let oct_start = pos;
                while pos < len && bytes[pos] >= b'0' && bytes[pos] <= b'7' {
                    pos += 1;
                }
                let oct_str = &input[oct_start..pos];
                if oct_str.is_empty() {
                    return Err(LexError { message: "incomplete octal integer".into(), offset: start });
                }
                let val = if let Ok(v) = u64::from_str_radix(oct_str, 8) {
                    v as i64 // wrapping cast: handles i64::MIN case
                } else {
                    return Err(LexError { message: "octal integer overflow".into(), offset: start });
                };
                tokens.push(SpannedToken { token: Token::IntLit(val), offset: start });
                continue;
            }
            let mut num_str = String::new();
            let mut has_dot = false;
            let mut has_e = false;
            while pos < len {
                if bytes[pos].is_ascii_digit() {
                    num_str.push(bytes[pos] as char);
                    pos += 1;
                } else if bytes[pos] == b'.' && !has_dot && !has_e {
                    // Check it's not ".." (range operator)
                    if pos + 1 < len && bytes[pos + 1] == b'.' {
                        break;
                    }
                    has_dot = true;
                    num_str.push('.');
                    pos += 1;
                } else if (bytes[pos] == b'e' || bytes[pos] == b'E') && !has_e {
                    has_e = true;
                    has_dot = true; // treat as float
                    num_str.push('e');
                    pos += 1;
                    if pos < len && (bytes[pos] == b'+' || bytes[pos] == b'-') {
                        num_str.push(bytes[pos] as char);
                        pos += 1;
                    }
                } else {
                    break;
                }
            }
            if has_dot {
                let val: f64 = num_str.parse().map_err(|_| LexError {
                    message: format!("invalid float literal: {num_str}"),
                    offset: start,
                })?;
                tokens.push(SpannedToken { token: Token::FloatLit(val), offset: start });
            } else {
                // Parse as i64; overflow (e.g. 9223372036854775808 = i64::MAX+1)
                // is stored as FloatLit to avoid a hard parse error.
                match num_str.parse::<i64>() {
                    Ok(val) => tokens.push(SpannedToken { token: Token::IntLit(val), offset: start }),
                    Err(_) => {
                        // Try as u64 first (for very large positives), then as f64.
                        let fval: f64 = num_str.parse().unwrap_or(f64::INFINITY);
                        tokens.push(SpannedToken { token: Token::FloatLit(fval), offset: start });
                    }
                }
            }
            continue;
        }

        // Parameter: $name
        if bytes[pos] == b'$' {
            pos += 1;
            let pstart = pos;
            while pos < len && (bytes[pos].is_ascii_alphanumeric() || bytes[pos] == b'_') {
                pos += 1;
            }
            let name = &input[pstart..pos];
            tokens.push(SpannedToken {
                token: Token::Parameter(name.to_string()),
                offset: start,
            });
            continue;
        }

        // Identifiers and keywords
        if bytes[pos].is_ascii_alphabetic() || bytes[pos] == b'_' {
            while pos < len && (bytes[pos].is_ascii_alphanumeric() || bytes[pos] == b'_') {
                pos += 1;
            }
            let word = &input[start..pos];
            let token = match word.to_ascii_uppercase().as_str() {
                "MATCH" => Token::Match,
                "OPTIONAL" => {
                    // peek for "MATCH"
                    let saved = pos;
                    while pos < len && bytes[pos].is_ascii_whitespace() {
                        pos += 1;
                    }
                    let kw_start = pos;
                    while pos < len && bytes[pos].is_ascii_alphabetic() {
                        pos += 1;
                    }
                    if input[kw_start..pos].eq_ignore_ascii_case("MATCH") {
                        Token::OptionalMatch
                    } else {
                        pos = saved;
                        Token::Ident(word.to_string())
                    }
                }
                "WHERE" => Token::Where,
                "RETURN" => Token::Return,
                "AS" => Token::As,
                "DISTINCT" => Token::Distinct,
                "ORDER" => {
                    // peek for "BY"
                    let saved = pos;
                    while pos < len && bytes[pos].is_ascii_whitespace() {
                        pos += 1;
                    }
                    let kw_start = pos;
                    while pos < len && bytes[pos].is_ascii_alphabetic() {
                        pos += 1;
                    }
                    if input[kw_start..pos].eq_ignore_ascii_case("BY") {
                        Token::OrderBy
                    } else {
                        pos = saved;
                        Token::Ident(word.to_string())
                    }
                }
                "LIMIT" => Token::Limit,
                "SKIP" => Token::Skip,
                "AND" => Token::And,
                "OR" => Token::Or,
                "XOR" => Token::Xor,
                "NOT" => Token::Not,
                "IN" => Token::In,
                "IS" => Token::Is,
                "NULL" => Token::Null,
                "TRUE" => Token::True,
                "FALSE" => Token::False,
                "CREATE" => Token::Create,
                "DELETE" => Token::Delete,
                "DETACH" => Token::Detach,
                "SET" => Token::Set,
                "MERGE" => Token::Merge,
                "REMOVE" => Token::Remove,
                "FOREACH" => Token::Foreach,
                "ON" => Token::On,
                "CALL" => Token::Call,
                "YIELD" => Token::Yield,
                "WITH" => Token::With,
                "UNWIND" => Token::Unwind,
                "CASE" => Token::Case,
                "WHEN" => Token::When,
                "THEN" => Token::Then,
                "ELSE" => Token::Else,
                "END" => Token::End,
                _ => Token::Ident(word.to_string()),
            };
            tokens.push(SpannedToken { token, offset: start });
            continue;
        }

        // Multi-character operators and punctuation
        match bytes[pos] {
            b'(' => { tokens.push(SpannedToken { token: Token::LParen, offset: start }); pos += 1; }
            b')' => { tokens.push(SpannedToken { token: Token::RParen, offset: start }); pos += 1; }
            b'[' => { tokens.push(SpannedToken { token: Token::LBracket, offset: start }); pos += 1; }
            b']' => { tokens.push(SpannedToken { token: Token::RBracket, offset: start }); pos += 1; }
            b'{' => { tokens.push(SpannedToken { token: Token::LBrace, offset: start }); pos += 1; }
            b'}' => { tokens.push(SpannedToken { token: Token::RBrace, offset: start }); pos += 1; }
            b':' => { tokens.push(SpannedToken { token: Token::Colon, offset: start }); pos += 1; }
            b',' => { tokens.push(SpannedToken { token: Token::Comma, offset: start }); pos += 1; }
            b'|' => { tokens.push(SpannedToken { token: Token::Pipe, offset: start }); pos += 1; }
            b'+' => {
                if pos + 1 < bytes.len() && bytes[pos + 1] == b'=' {
                    tokens.push(SpannedToken { token: Token::PlusEq, offset: start });
                    pos += 2;
                } else {
                    tokens.push(SpannedToken { token: Token::Plus, offset: start });
                    pos += 1;
                }
            }
            b'*' => { tokens.push(SpannedToken { token: Token::Star, offset: start }); pos += 1; }
            b'%' => { tokens.push(SpannedToken { token: Token::Percent, offset: start }); pos += 1; }
            b'^' => { tokens.push(SpannedToken { token: Token::Caret, offset: start }); pos += 1; }
            b'-' => { tokens.push(SpannedToken { token: Token::Dash, offset: start }); pos += 1; }
            b'/' => { tokens.push(SpannedToken { token: Token::Slash, offset: start }); pos += 1; }
            b'.' => {
                if pos + 1 < len && bytes[pos + 1] == b'.' {
                    tokens.push(SpannedToken { token: Token::DotDot, offset: start });
                    pos += 2;
                } else {
                    tokens.push(SpannedToken { token: Token::Dot, offset: start });
                    pos += 1;
                }
            }
            b'=' => {
                if pos + 1 < len && bytes[pos + 1] == b'~' {
                    tokens.push(SpannedToken { token: Token::RegexMatch, offset: start });
                    pos += 2;
                } else {
                    tokens.push(SpannedToken { token: Token::Eq, offset: start });
                    pos += 1;
                }
            }
            b'<' => {
                if pos + 1 < len && bytes[pos + 1] == b'>' {
                    tokens.push(SpannedToken { token: Token::Neq, offset: start });
                    pos += 2;
                } else if pos + 1 < len && bytes[pos + 1] == b'=' {
                    tokens.push(SpannedToken { token: Token::Le, offset: start });
                    pos += 2;
                } else {
                    tokens.push(SpannedToken { token: Token::LArrow, offset: start });
                    pos += 1;
                }
            }
            b'>' => {
                if pos + 1 < len && bytes[pos + 1] == b'=' {
                    tokens.push(SpannedToken { token: Token::Ge, offset: start });
                    pos += 2;
                } else {
                    tokens.push(SpannedToken { token: Token::RArrow, offset: start });
                    pos += 1;
                }
            }
            _ => {
                return Err(LexError {
                    message: format!("unexpected character: '{}'", bytes[pos] as char),
                    offset: pos,
                });
            }
        }
    }

    tokens.push(SpannedToken { token: Token::Eof, offset: pos });
    Ok(tokens)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_simple_match() {
        let tokens = lex("MATCH (n:Person) RETURN n").unwrap();
        let types: Vec<&Token> = tokens.iter().map(|t| &t.token).collect();
        assert_eq!(types, vec![
            &Token::Match,
            &Token::LParen,
            &Token::Ident("n".into()),
            &Token::Colon,
            &Token::Ident("Person".into()),
            &Token::RParen,
            &Token::Return,
            &Token::Ident("n".into()),
            &Token::Eof,
        ]);
    }

    #[test]
    fn test_relationship_pattern() {
        let tokens = lex("MATCH (a)-[r:KNOWS]->(b) RETURN a, b").unwrap();
        let types: Vec<&Token> = tokens.iter().map(|t| &t.token).collect();
        assert!(types.contains(&&Token::LBracket));
        assert!(types.contains(&&Token::RBracket));
        assert!(types.contains(&&Token::Dash));
        assert!(types.contains(&&Token::RArrow));
    }

    #[test]
    fn test_where_clause() {
        let tokens = lex("MATCH (n) WHERE n.age > 30 AND n.name = 'Alice' RETURN n").unwrap();
        assert!(tokens.iter().any(|t| t.token == Token::Where));
        assert!(tokens.iter().any(|t| t.token == Token::And));
        assert!(tokens.iter().any(|t| t.token == Token::IntLit(30)));
        assert!(tokens.iter().any(|t| t.token == Token::StringLit("Alice".into())));
    }

    #[test]
    fn test_string_escapes() {
        let tokens = lex(r"'hello\'s world'").unwrap();
        match &tokens[0].token {
            Token::StringLit(s) => assert_eq!(s, "hello's world"),
            other => panic!("expected StringLit, got {other:?}"),
        }
    }

    #[test]
    fn test_parameter() {
        let tokens = lex("$name").unwrap();
        assert_eq!(tokens[0].token, Token::Parameter("name".into()));
    }

    #[test]
    fn test_float_literal() {
        let tokens = lex("3.14").unwrap();
        match &tokens[0].token {
            Token::FloatLit(v) => assert!((*v - 3.14).abs() < 1e-10),
            other => panic!("expected FloatLit, got {other:?}"),
        }
    }

    #[test]
    fn test_case_insensitive_keywords() {
        let tokens = lex("match WHERE Return").unwrap();
        assert_eq!(tokens[0].token, Token::Match);
        assert_eq!(tokens[1].token, Token::Where);
        assert_eq!(tokens[2].token, Token::Return);
    }
}
