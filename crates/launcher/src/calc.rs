//! Tiny recursive-descent calculator for the "=" search mode.
//!
//! Grammar (standard precedence, lowest to highest):
//!   expr   := term (('+' | '-') term)*
//!   term   := unary (('*' | '/') unary)*
//!   unary  := '-' unary | power
//!   power  := postfix ('^' unary)?          (right-associative)
//!   postfix:= atom '%'?                     ('%' divides by 100)
//!   atom   := number | '(' expr ')'

pub fn eval(input: &str) -> Result<f64, String> {
    let tokens = tokenize(input)?;
    if tokens.is_empty() {
        return Err("empty expression".to_string());
    }
    let mut pos = 0;
    let value = parse_expr(&tokens, &mut pos)?;
    if pos != tokens.len() {
        return Err("unexpected trailing input".to_string());
    }
    Ok(value)
}

#[derive(Clone, Copy, PartialEq, Debug)]
enum Token {
    Number(f64),
    Plus,
    Minus,
    Star,
    Slash,
    Caret,
    Percent,
    LParen,
    RParen,
}

fn tokenize(input: &str) -> Result<Vec<Token>, String> {
    let mut tokens = Vec::new();
    let chars: Vec<char> = input.chars().collect();
    let mut i = 0;
    while i < chars.len() {
        let c = chars[i];
        match c {
            ' ' | '\t' => i += 1,
            '+' => {
                tokens.push(Token::Plus);
                i += 1;
            }
            '-' => {
                tokens.push(Token::Minus);
                i += 1;
            }
            '*' => {
                tokens.push(Token::Star);
                i += 1;
            }
            '/' => {
                tokens.push(Token::Slash);
                i += 1;
            }
            '^' => {
                tokens.push(Token::Caret);
                i += 1;
            }
            '%' => {
                tokens.push(Token::Percent);
                i += 1;
            }
            '(' => {
                tokens.push(Token::LParen);
                i += 1;
            }
            ')' => {
                tokens.push(Token::RParen);
                i += 1;
            }
            '0'..='9' | '.' | ',' => {
                let start = i;
                while i < chars.len() && matches!(chars[i], '0'..='9' | '.' | ',') {
                    i += 1;
                }
                let raw: String = chars[start..i].iter().collect();
                let normalized = raw.replace(',', ".");
                let n = normalized
                    .parse::<f64>()
                    .map_err(|_| format!("bad number: {}", raw))?;
                tokens.push(Token::Number(n));
            }
            _ => return Err(format!("unexpected character: {}", c)),
        }
    }
    Ok(tokens)
}

fn parse_expr(tokens: &[Token], pos: &mut usize) -> Result<f64, String> {
    let mut value = parse_term(tokens, pos)?;
    loop {
        match tokens.get(*pos) {
            Some(Token::Plus) => {
                *pos += 1;
                value += parse_term(tokens, pos)?;
            }
            Some(Token::Minus) => {
                *pos += 1;
                value -= parse_term(tokens, pos)?;
            }
            _ => break,
        }
    }
    Ok(value)
}

fn parse_term(tokens: &[Token], pos: &mut usize) -> Result<f64, String> {
    let mut value = parse_unary(tokens, pos)?;
    loop {
        match tokens.get(*pos) {
            Some(Token::Star) => {
                *pos += 1;
                value *= parse_unary(tokens, pos)?;
            }
            Some(Token::Slash) => {
                *pos += 1;
                let divisor = parse_unary(tokens, pos)?;
                if divisor == 0.0 {
                    return Err("division by zero".to_string());
                }
                value /= divisor;
            }
            _ => break,
        }
    }
    Ok(value)
}

fn parse_unary(tokens: &[Token], pos: &mut usize) -> Result<f64, String> {
    if matches!(tokens.get(*pos), Some(Token::Minus)) {
        *pos += 1;
        return Ok(-parse_unary(tokens, pos)?);
    }
    parse_power(tokens, pos)
}

fn parse_power(tokens: &[Token], pos: &mut usize) -> Result<f64, String> {
    let base = parse_postfix(tokens, pos)?;
    if matches!(tokens.get(*pos), Some(Token::Caret)) {
        *pos += 1;
        let exponent = parse_unary(tokens, pos)?; // right-associative
        return Ok(base.powf(exponent));
    }
    Ok(base)
}

fn parse_postfix(tokens: &[Token], pos: &mut usize) -> Result<f64, String> {
    let mut value = parse_atom(tokens, pos)?;
    while matches!(tokens.get(*pos), Some(Token::Percent)) {
        *pos += 1;
        value /= 100.0;
    }
    Ok(value)
}

fn parse_atom(tokens: &[Token], pos: &mut usize) -> Result<f64, String> {
    match tokens.get(*pos) {
        Some(Token::Number(n)) => {
            *pos += 1;
            Ok(*n)
        }
        Some(Token::LParen) => {
            *pos += 1;
            let value = parse_expr(tokens, pos)?;
            match tokens.get(*pos) {
                Some(Token::RParen) => {
                    *pos += 1;
                    Ok(value)
                }
                _ => Err("expected ')'".to_string()),
            }
        }
        Some(t) => Err(format!("unexpected token: {:?}", t)),
        None => Err("unexpected end of expression".to_string()),
    }
}

/// Formats a result the way a calculator display would: no trailing zeros,
/// no exponent notation for typical magnitudes.
pub fn format_result(value: f64) -> String {
    if !value.is_finite() {
        return "error".to_string();
    }
    if value == value.trunc() && value.abs() < 1e15 {
        return format!("{}", value as i64);
    }
    let s = format!("{:.10}", value);
    let s = s.trim_end_matches('0').trim_end_matches('.');
    s.to_string()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn basic_ops() {
        assert_eq!(eval("3*4").unwrap(), 12.0);
        assert_eq!(eval(" 3 * 4 ").unwrap(), 12.0);
        assert_eq!(eval("2^3").unwrap(), 8.0);
        assert_eq!(eval("50%").unwrap(), 0.5);
        assert_eq!(eval("(2+3)*4").unwrap(), 20.0);
        assert_eq!(eval("10/2-1").unwrap(), 4.0);
        assert_eq!(eval("-5+3").unwrap(), -2.0);
        assert!(eval("1/0").is_err());
        assert!(eval("").is_err());
        assert!(eval("3+").is_err());
    }

    #[test]
    fn formatting() {
        assert_eq!(format_result(12.0), "12");
        assert_eq!(format_result(0.5), "0.5");
        assert_eq!(format_result(4.0), "4");
    }
}
