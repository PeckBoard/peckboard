//! `math` tool — evaluate an arithmetic expression to a number.
//!
//! A small recursive-descent evaluator over `f64`. Supports `+ - * / %`, `^`
//! (right-associative power), unary `+`/`-`, parentheses, the constants `pi`
//! and `e`, and a set of single-/two-argument functions (`sqrt`, `abs`,
//! `sin`, `cos`, `tan`, `ln`, `log`, `log2`, `log10`, `exp`, `floor`, `ceil`,
//! `round`, `min`, `max`, `pow`). No host calls — pure, and unit-testable.

/// Evaluate `{"expression": "..."}` and return `{"result": <number>, "expression": "..."}`.
pub fn eval_tool(args: serde_json::Value) -> Result<serde_json::Value, String> {
    let expr = args
        .get("expression")
        .and_then(|v| v.as_str())
        .ok_or("`expression` (string) is required")?;
    let value = eval(expr)?;
    if !value.is_finite() {
        return Err(format!("result is not finite ({value})"));
    }
    Ok(serde_json::json!({ "expression": expr, "result": value }))
}

/// Parse and evaluate an expression string, or return a human-readable error.
pub fn eval(input: &str) -> Result<f64, String> {
    let tokens = tokenize(input)?;
    let mut p = Parser { tokens, pos: 0 };
    let v = p.parse_expr()?;
    if p.pos != p.tokens.len() {
        return Err(format!("unexpected trailing input near token {}", p.pos));
    }
    Ok(v)
}

#[derive(Debug, Clone, PartialEq)]
enum Tok {
    Num(f64),
    Ident(String),
    Plus,
    Minus,
    Star,
    Slash,
    Percent,
    Caret,
    LParen,
    RParen,
    Comma,
}

fn tokenize(s: &str) -> Result<Vec<Tok>, String> {
    let chars: Vec<char> = s.chars().collect();
    let mut i = 0;
    let mut out = Vec::new();
    while i < chars.len() {
        let c = chars[i];
        if c.is_whitespace() {
            i += 1;
            continue;
        }
        match c {
            '+' => out.push(Tok::Plus),
            '-' => out.push(Tok::Minus),
            '*' => {
                // `**` is an alias for `^` (power).
                if i + 1 < chars.len() && chars[i + 1] == '*' {
                    out.push(Tok::Caret);
                    i += 1;
                } else {
                    out.push(Tok::Star);
                }
            }
            '/' => out.push(Tok::Slash),
            '%' => out.push(Tok::Percent),
            '^' => out.push(Tok::Caret),
            '(' => out.push(Tok::LParen),
            ')' => out.push(Tok::RParen),
            ',' => out.push(Tok::Comma),
            _ if c.is_ascii_digit() || c == '.' => {
                let start = i;
                let mut seen_dot = false;
                let mut seen_exp = false;
                while i < chars.len() {
                    let d = chars[i];
                    if d.is_ascii_digit() {
                        i += 1;
                    } else if d == '.' && !seen_dot && !seen_exp {
                        seen_dot = true;
                        i += 1;
                    } else if (d == 'e' || d == 'E') && !seen_exp {
                        seen_exp = true;
                        i += 1;
                        if i < chars.len() && (chars[i] == '+' || chars[i] == '-') {
                            i += 1;
                        }
                    } else {
                        break;
                    }
                }
                let num: String = chars[start..i].iter().collect();
                let val: f64 = num.parse().map_err(|_| format!("invalid number '{num}'"))?;
                out.push(Tok::Num(val));
                continue; // `i` already advanced past the number
            }
            _ if c.is_alphabetic() || c == '_' => {
                let start = i;
                while i < chars.len() && (chars[i].is_alphanumeric() || chars[i] == '_') {
                    i += 1;
                }
                let ident: String = chars[start..i].iter().collect();
                out.push(Tok::Ident(ident.to_ascii_lowercase()));
                continue;
            }
            _ => return Err(format!("unexpected character '{c}'")),
        }
        i += 1;
    }
    Ok(out)
}

struct Parser {
    tokens: Vec<Tok>,
    pos: usize,
}

impl Parser {
    fn peek(&self) -> Option<&Tok> {
        self.tokens.get(self.pos)
    }

    fn bump(&mut self) -> Option<Tok> {
        let t = self.tokens.get(self.pos).cloned();
        if t.is_some() {
            self.pos += 1;
        }
        t
    }

    // expr := term (("+" | "-") term)*
    fn parse_expr(&mut self) -> Result<f64, String> {
        let mut acc = self.parse_term()?;
        while let Some(op) = self.peek() {
            match op {
                Tok::Plus => {
                    self.bump();
                    acc += self.parse_term()?;
                }
                Tok::Minus => {
                    self.bump();
                    acc -= self.parse_term()?;
                }
                _ => break,
            }
        }
        Ok(acc)
    }

    // term := unary (("*" | "/" | "%") unary)*
    fn parse_term(&mut self) -> Result<f64, String> {
        let mut acc = self.parse_unary()?;
        while let Some(op) = self.peek() {
            match op {
                Tok::Star => {
                    self.bump();
                    acc *= self.parse_unary()?;
                }
                Tok::Slash => {
                    self.bump();
                    let d = self.parse_unary()?;
                    if d == 0.0 {
                        return Err("division by zero".to_string());
                    }
                    acc /= d;
                }
                Tok::Percent => {
                    self.bump();
                    let d = self.parse_unary()?;
                    if d == 0.0 {
                        return Err("modulo by zero".to_string());
                    }
                    acc %= d;
                }
                _ => break,
            }
        }
        Ok(acc)
    }

    // unary := ("+" | "-") unary | power
    // Unary minus binds *looser* than `^`, so `-2^2` is `-(2^2) = -4`, matching
    // mathematical and Python convention.
    fn parse_unary(&mut self) -> Result<f64, String> {
        match self.peek() {
            Some(Tok::Plus) => {
                self.bump();
                self.parse_unary()
            }
            Some(Tok::Minus) => {
                self.bump();
                Ok(-self.parse_unary()?)
            }
            _ => self.parse_power(),
        }
    }

    // power := atom ("^" unary)?   (right-associative; exponent may be signed,
    // so `2^-3` and `2^3^2` both parse correctly).
    fn parse_power(&mut self) -> Result<f64, String> {
        let base = self.parse_atom()?;
        if let Some(Tok::Caret) = self.peek() {
            self.bump();
            let exp = self.parse_unary()?;
            return Ok(base.powf(exp));
        }
        Ok(base)
    }

    // atom := number | constant | func "(" args ")" | "(" expr ")"
    fn parse_atom(&mut self) -> Result<f64, String> {
        match self.bump() {
            Some(Tok::Num(n)) => Ok(n),
            Some(Tok::LParen) => {
                let v = self.parse_expr()?;
                match self.bump() {
                    Some(Tok::RParen) => Ok(v),
                    _ => Err("expected ')'".to_string()),
                }
            }
            Some(Tok::Ident(name)) => {
                // Function call?
                if let Some(Tok::LParen) = self.peek() {
                    self.bump();
                    let mut args = Vec::new();
                    if !matches!(self.peek(), Some(Tok::RParen)) {
                        args.push(self.parse_expr()?);
                        while let Some(Tok::Comma) = self.peek() {
                            self.bump();
                            args.push(self.parse_expr()?);
                        }
                    }
                    match self.bump() {
                        Some(Tok::RParen) => {}
                        _ => return Err(format!("expected ')' after arguments to '{name}'")),
                    }
                    apply_func(&name, &args)
                } else {
                    constant(&name)
                }
            }
            other => Err(format!("unexpected token: {other:?}")),
        }
    }
}

fn constant(name: &str) -> Result<f64, String> {
    match name {
        "pi" => Ok(std::f64::consts::PI),
        "e" => Ok(std::f64::consts::E),
        "tau" => Ok(std::f64::consts::TAU),
        _ => Err(format!("unknown constant '{name}'")),
    }
}

fn apply_func(name: &str, args: &[f64]) -> Result<f64, String> {
    let one = |a: &[f64]| -> Result<f64, String> {
        if a.len() == 1 {
            Ok(a[0])
        } else {
            Err(format!(
                "'{name}' takes exactly 1 argument, got {}",
                a.len()
            ))
        }
    };
    match name {
        "abs" => Ok(one(args)?.abs()),
        "sqrt" => Ok(one(args)?.sqrt()),
        "sin" => Ok(one(args)?.sin()),
        "cos" => Ok(one(args)?.cos()),
        "tan" => Ok(one(args)?.tan()),
        "asin" => Ok(one(args)?.asin()),
        "acos" => Ok(one(args)?.acos()),
        "atan" => Ok(one(args)?.atan()),
        "ln" => Ok(one(args)?.ln()),
        "log10" | "log" => Ok(one(args)?.log10()),
        "log2" => Ok(one(args)?.log2()),
        "exp" => Ok(one(args)?.exp()),
        "floor" => Ok(one(args)?.floor()),
        "ceil" => Ok(one(args)?.ceil()),
        "round" => Ok(one(args)?.round()),
        "sign" => Ok(one(args)?.signum()),
        "pow" => {
            if args.len() == 2 {
                Ok(args[0].powf(args[1]))
            } else {
                Err("'pow' takes 2 arguments".to_string())
            }
        }
        "min" => args
            .iter()
            .copied()
            .reduce(f64::min)
            .ok_or_else(|| "'min' needs at least 1 argument".to_string()),
        "max" => args
            .iter()
            .copied()
            .reduce(f64::max)
            .ok_or_else(|| "'max' needs at least 1 argument".to_string()),
        _ => Err(format!("unknown function '{name}'")),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn close(a: f64, b: f64) -> bool {
        (a - b).abs() < 1e-9
    }

    #[test]
    fn arithmetic_and_precedence() {
        assert!(close(eval("1 + 2 * 3").unwrap(), 7.0));
        assert!(close(eval("(1 + 2) * 3").unwrap(), 9.0));
        assert!(close(eval("2 ^ 3 ^ 2").unwrap(), 512.0)); // right-assoc
        assert!(close(eval("10 % 3").unwrap(), 1.0));
        assert!(close(eval("-2 ^ 2").unwrap(), -4.0)); // unary binds looser than ^
        assert!(close(eval("2 ^ -3").unwrap(), 0.125)); // exponent may be signed
        assert!(close(eval("2 ** 10").unwrap(), 1024.0));
    }

    #[test]
    fn functions_and_constants() {
        assert!(close(eval("sqrt(16)").unwrap(), 4.0));
        assert!(close(eval("max(1, 5, 3)").unwrap(), 5.0));
        assert!(close(eval("pow(2, 8)").unwrap(), 256.0));
        assert!(close(eval("sin(0)").unwrap(), 0.0));
        assert!(close(eval("floor(pi)").unwrap(), 3.0));
    }

    #[test]
    fn errors_are_clean() {
        assert!(eval("1 / 0").is_err());
        assert!(eval("1 +").is_err());
        assert!(eval("foo(1)").is_err());
        assert!(eval("(1 + 2").is_err());
        assert!(eval("2 3").is_err());
    }
}
