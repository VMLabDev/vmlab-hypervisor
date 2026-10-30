//! Textual Jib syntax. Identifiers retain Sail's mangling; widths are not truncated.
use serde::{Deserialize, Serialize};
use std::fmt;

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub enum Type {
    Integer(Option<u32>),
    Bits(Option<u32>),
    Float(u32),
    Unit,
    Bool,
    Bit,
    String,
    Real,
    RoundingMode,
    Enum(String),
    Struct(String),
    Union(String),
    Vector(Box<Type>),
    FixedVector(u32, Box<Type>),
    List(Box<Type>),
    Ref(Box<Type>),
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub enum Expr {
    Id(String),
    Integer(String, Option<u32>),
    Bits(String),
    Bool(bool),
    String(String),
    Unit,
    Undefined(Type),
    Ref(String),
    Struct(String, Vec<(String, Expr)>),
    Field(Box<Expr>, String),
    Is(Box<Expr>, String),
    As(Box<Expr>, String),
    Op(String, Option<u32>, Vec<Expr>),
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub enum Location {
    Id(String),
    Field(Box<Location>, String),
    Deref(Box<Location>),
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub enum Statement {
    Declare(String, Type, Option<Expr>),
    Copy(Location, Expr),
    Call(Location, bool, String, Vec<Expr>),
    Jump(Expr, usize),
    Goto(usize),
    Monomorphize(String, Type),
    Exit(String),
    Arbitrary,
    End,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct Instruction {
    pub statement: Statement,
    /// Tokens after the backtick, retained for diagnostics and regeneration.
    pub source: Vec<String>,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub enum Definition {
    Enum(String, Vec<String>),
    Struct(String, Vec<(String, Type)>),
    Union(String, Vec<(String, Type)>),
    Register(String, Type, Vec<Instruction>),
    Let(Vec<(String, Type)>, Vec<Instruction>),
    Signature {
        name: String,
        external: Option<String>,
        abstract_: bool,
        args: Vec<Type>,
        result: Type,
    },
    Function(String, Vec<String>, Vec<Instruction>),
    Pragma(String),
    Files(Vec<String>),
}

/// A parsed document retains its original bytes as well as its syntax tree.
#[derive(Clone, Debug)]
pub struct Document<'a> {
    pub definitions: Vec<Definition>,
    source: &'a str,
}

impl fmt::Display for Document<'_> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.source)
    }
}

#[derive(Clone, Debug, PartialEq, Eq, thiserror::Error)]
#[error("Jib at byte {offset}: {message}")]
pub struct Error {
    pub offset: usize,
    pub message: String,
}

#[derive(Debug)]
struct Token<'a> {
    text: &'a str,
    offset: usize,
}

fn lex(source: &str) -> Result<Vec<Token<'_>>, Error> {
    let mut tokens = Vec::new();
    let mut i = 0;
    let bytes = source.as_bytes();
    while i < bytes.len() {
        let start = i;
        if bytes[i].is_ascii_whitespace() {
            i += 1;
            continue;
        }
        if source[i..].starts_with("//") {
            i += source[i..].find('\n').unwrap_or(bytes.len() - i);
            continue;
        }
        if bytes[i] == b'#' && !matches!(bytes.get(i + 1), Some(b'x' | b'b')) {
            i += source[i..].find('\n').unwrap_or(bytes.len() - i);
        } else if bytes[i] == b'"' {
            i += 1;
            loop {
                match bytes.get(i) {
                    Some(b'"') => {
                        i += 1;
                        break;
                    }
                    Some(b'\\') => i += 2,
                    Some(_) => i += 1,
                    None => {
                        return Err(Error {
                            offset: start,
                            message: "unterminated string".into(),
                        });
                    }
                }
                if i > bytes.len() {
                    return Err(Error {
                        offset: start,
                        message: "unterminated string escape".into(),
                    });
                }
            }
        } else if source[i..].starts_with("::<") {
            i += 3;
        } else if source[i..].starts_with("->") {
            i += 2;
        } else if bytes[i].is_ascii_alphanumeric() || matches!(bytes[i], b'_' | b'%' | b'@' | b'#')
        {
            i += 1;
            while i < bytes.len() && (bytes[i].is_ascii_alphanumeric() || bytes[i] == b'_') {
                i += 1;
            }
        } else if b"(){}[].*:=,;$-&>`".contains(&bytes[i]) {
            i += 1;
        } else {
            return Err(Error {
                offset: i,
                message: "unexpected character".into(),
            });
        }
        tokens.push(Token {
            text: &source[start..i],
            offset: start,
        });
    }
    Ok(tokens)
}

struct Parser<'a> {
    tokens: Vec<Token<'a>>,
    pos: usize,
    end: usize,
    depth: usize,
}

impl Parser<'_> {
    fn error(&self, message: impl Into<String>) -> Error {
        Error {
            offset: self.tokens.get(self.pos).map_or(self.end, |t| t.offset),
            message: message.into(),
        }
    }

    fn peek(&self) -> &str {
        self.tokens.get(self.pos).map_or("", |t| t.text)
    }

    fn take(&mut self) -> Result<String, Error> {
        let token = self
            .tokens
            .get(self.pos)
            .ok_or_else(|| self.error("unexpected end of input"))?;
        self.pos += 1;
        Ok(token.text.to_owned())
    }

    fn eat(&mut self, text: &str) -> bool {
        if self.peek() == text {
            self.pos += 1;
            true
        } else {
            false
        }
    }

    fn expect(&mut self, text: &str) -> Result<(), Error> {
        if self.eat(text) {
            Ok(())
        } else {
            Err(self.error(format!("expected {text}, found {}", self.peek())))
        }
    }

    fn ident(&mut self) -> Result<String, Error> {
        let text = self.peek();
        if text
            .as_bytes()
            .first()
            .is_some_and(|c| c.is_ascii_alphabetic() || *c == b'_')
            && text.bytes().all(|c| c.is_ascii_alphanumeric() || c == b'_')
        {
            self.take()
        } else {
            Err(self.error("expected identifier"))
        }
    }

    fn number(&mut self) -> Result<u32, Error> {
        let n = self
            .peek()
            .parse()
            .map_err(|_| self.error("expected u32"))?;
        self.pos += 1;
        Ok(n)
    }

    fn string(&mut self) -> Result<String, Error> {
        let raw = self.take()?;
        if !raw.starts_with('"') || !raw.ends_with('"') {
            return Err(self.error("expected string"));
        }
        let mut result = String::new();
        let mut chars = raw[1..raw.len() - 1].chars();
        while let Some(c) = chars.next() {
            if c != '\\' {
                result.push(c);
                continue;
            }
            match chars.next() {
                Some('n') => result.push('\n'),
                Some('r') => result.push('\r'),
                Some('t') => result.push('\t'),
                Some('b') => result.push('\u{8}'),
                Some('"') => result.push('"'),
                Some('\\') => result.push('\\'),
                Some(c @ '0'..='9') => {
                    let mut digits = c.to_string();
                    for _ in 0..2 {
                        digits.push(
                            chars
                                .next()
                                .ok_or_else(|| self.error("short decimal escape"))?,
                        );
                    }
                    let byte: u8 = digits
                        .parse()
                        .map_err(|_| self.error("bad decimal escape"))?;
                    result.push(char::from(byte));
                }
                _ => return Err(self.error("invalid string escape")),
            }
        }
        Ok(result)
    }

    fn nested<T>(&mut self, parse: impl FnOnce(&mut Self) -> Result<T, Error>) -> Result<T, Error> {
        if self.depth >= 128 {
            return Err(self.error("nesting limit exceeded"));
        }
        self.depth += 1;
        let result = parse(self);
        self.depth -= 1;
        result
    }

    fn ty(&mut self) -> Result<Type, Error> {
        self.nested(Self::ty_inner)
    }

    fn ty_inner(&mut self) -> Result<Type, Error> {
        let name = self.take()?;
        Ok(match name.as_str() {
            "%i" => Type::Integer(None),
            "%bv" => Type::Bits(None),
            "%unit" => Type::Unit,
            "%bool" => Type::Bool,
            "%bit" => Type::Bit,
            "%string" => Type::String,
            "%real" => Type::Real,
            "%rounding_mode" => Type::RoundingMode,
            "%enum" => Type::Enum(self.ident()?),
            "%struct" => Type::Struct(self.ident()?),
            "%union" => Type::Union(self.ident()?),
            "%vec" | "%list" | "&" => {
                self.expect("(")?;
                let t = Box::new(self.ty()?);
                self.expect(")")?;
                match name.as_str() {
                    "%vec" => Type::Vector(t),
                    "%list" => Type::List(t),
                    _ => Type::Ref(t),
                }
            }
            "%fvec" => {
                self.expect("(")?;
                let n = self.number()?;
                self.expect(",")?;
                let t = Box::new(self.ty()?);
                self.expect(")")?;
                Type::FixedVector(n, t)
            }
            s if s.starts_with("%bv") => Type::Bits(Some(
                s[3..]
                    .parse()
                    .map_err(|_| self.error("invalid bit width"))?,
            )),
            s if s.starts_with("%i") => Type::Integer(Some(
                s[2..]
                    .parse()
                    .map_err(|_| self.error("invalid integer width"))?,
            )),
            s if s.starts_with("%f") => Type::Float(
                s[2..]
                    .parse()
                    .map_err(|_| self.error("invalid float width"))?,
            ),
            _ => return Err(self.error(format!("unknown type {name}"))),
        })
    }

    fn list<T>(
        &mut self,
        end: &str,
        mut parse: impl FnMut(&mut Self) -> Result<T, Error>,
    ) -> Result<Vec<T>, Error> {
        let mut items = Vec::new();
        if self.eat(end) {
            return Ok(items);
        }
        loop {
            items.push(parse(self)?);
            if self.eat(end) {
                return Ok(items);
            }
            self.expect(",")?;
            if self.eat(end) {
                return Ok(items);
            }
        }
    }

    fn expr(&mut self) -> Result<Expr, Error> {
        self.nested(Self::expr_inner)
    }

    fn expr_inner(&mut self) -> Result<Expr, Error> {
        let mut expr = match self.peek() {
            "(" => {
                self.take()?;
                self.expect(")")?;
                Expr::Unit
            }
            "true" | "false" => Expr::Bool(self.take()? == "true"),
            "bitzero" => {
                self.take()?;
                Expr::Bits("0b0".into())
            }
            "bitone" => {
                self.take()?;
                Expr::Bits("0b1".into())
            }
            "emptybitvec" => {
                self.take()?;
                Expr::Bits("0b".into())
            }
            "undefined" => {
                self.take()?;
                self.expect(":")?;
                Expr::Undefined(self.ty()?)
            }
            "&" => {
                self.take()?;
                Expr::Ref(self.ident()?)
            }
            "struct" => {
                self.take()?;
                let name = self.ident()?;
                self.expect("{")?;
                let fields = self.list("}", |p| {
                    let n = p.ident()?;
                    p.expect("=")?;
                    Ok((n, p.expr()?))
                })?;
                Expr::Struct(name, fields)
            }
            s if s.starts_with('"') => Expr::String(self.string()?),
            s if s.starts_with("0x")
                || s.starts_with("0b")
                || s.starts_with("#x")
                || s.starts_with("#b") =>
            {
                let bits = self.take()?;
                let radix = if bits.as_bytes()[1] == b'x' { 16 } else { 2 };
                if !bits[2..].chars().all(|c| c.is_digit(radix)) {
                    return Err(self.error("bad bitvector literal"));
                }
                Expr::Bits(bits)
            }
            s if s == "-" || s.as_bytes().first().is_some_and(u8::is_ascii_digit) => {
                let negative = self.eat("-");
                let mut digits = self.take()?;
                if !digits.bytes().all(|c| c.is_ascii_digit()) || digits.is_empty() {
                    return Err(self.error("invalid integer"));
                }
                if negative {
                    digits.insert(0, '-');
                }
                let width = if self.eat(":") {
                    match self.ty()? {
                        Type::Integer(w) => w,
                        _ => return Err(self.error("expected integer annotation")),
                    }
                } else {
                    None
                };
                Expr::Integer(digits, width)
            }
            s if s.starts_with('@') => {
                let op = self.take()?;
                if !matches!(
                    op.as_str(),
                    "@not"
                        | "@or"
                        | "@and"
                        | "@eq"
                        | "@neq"
                        | "@slice"
                        | "@set_slice"
                        | "@signed"
                        | "@unsigned"
                        | "@zero_extend"
                        | "@bvnot"
                        | "@bvor"
                        | "@bvxor"
                        | "@bvand"
                        | "@bvadd"
                        | "@bvsub"
                        | "@bvaccess"
                        | "@concat"
                        | "@lteq"
                        | "@lt"
                        | "@gteq"
                        | "@gt"
                        | "@hd"
                        | "@tl"
                        | "@is_empty"
                        | "@iadd"
                        | "@isub"
                ) {
                    return Err(self.error(format!("unknown operation {op}")));
                }
                let width = if self.eat("::<") {
                    let n = self.number()?;
                    self.expect(">")?;
                    Some(n)
                } else {
                    None
                };
                self.expect("(")?;
                Expr::Op(op, width, self.list(")", Self::expr)?)
            }
            "UINT64_C" => {
                self.take()?;
                self.expect("(")?;
                let n = self.take()?;
                self.expect(")")?;
                if n != "0" {
                    return Err(self.error("nonzero empty bitvector"));
                }
                Expr::Bits("0b".into())
            }
            _ => Expr::Id(self.ident()?),
        };
        let mut suffixes = 0;
        loop {
            suffixes += 1;
            if suffixes > 128 {
                return Err(self.error("expression suffix nesting limit exceeded"));
            }
            if self.eat(".") {
                expr = Expr::Field(Box::new(expr), self.ident()?);
            } else if self.eat("is") {
                expr = Expr::Is(Box::new(expr), self.ident()?);
            } else if self.eat("as") {
                expr = Expr::As(Box::new(expr), self.ident()?);
            } else {
                break;
            }
        }
        Ok(expr)
    }

    fn location(&mut self) -> Result<Location, Error> {
        let mut loc = Location::Id(self.ident()?);
        let mut suffixes = 0;
        loop {
            suffixes += 1;
            if suffixes > 128 {
                return Err(self.error("location nesting limit exceeded"));
            }
            if self.eat(".") {
                loc = Location::Field(Box::new(loc), self.ident()?);
            } else if self.eat("*") {
                loc = Location::Deref(Box::new(loc));
            } else {
                return Ok(loc);
            }
        }
    }

    fn instruction(&mut self) -> Result<Instruction, Error> {
        let statement = match self.peek() {
            "jump" => {
                self.take()?;
                let e = self.expr()?;
                self.expect("goto")?;
                Statement::Jump(e, self.number()? as usize)
            }
            "goto" => {
                self.take()?;
                Statement::Goto(self.number()? as usize)
            }
            "mono" => {
                self.take()?;
                let n = self.ident()?;
                self.expect(":")?;
                Statement::Monomorphize(n, self.ty()?)
            }
            "exit" => {
                self.take()?;
                Statement::Exit(self.ident()?)
            }
            "arbitrary" => {
                self.take()?;
                Statement::Arbitrary
            }
            "end" => {
                self.take()?;
                Statement::End
            }
            _ => {
                let loc = self.location()?;
                if self.eat(":") {
                    let Location::Id(name) = loc else {
                        return Err(self.error("declaration needs identifier"));
                    };
                    let ty = self.ty()?;
                    let init = if self.eat("=") {
                        Some(self.expr()?)
                    } else {
                        None
                    };
                    Statement::Declare(name, ty, init)
                } else {
                    self.expect("=")?;
                    let external = self.eat("$");
                    let call = self.tokens.get(self.pos + 1).is_some_and(|t| t.text == "(")
                        && !self.peek().starts_with('@')
                        && self.peek() != "UINT64_C";
                    if external || call {
                        let name = self.ident()?;
                        self.expect("(")?;
                        Statement::Call(loc, external, name, self.list(")", Self::expr)?)
                    } else {
                        Statement::Copy(loc, self.expr()?)
                    }
                }
            }
        };
        let mut source = Vec::new();
        if self.eat("`") {
            while self.peek() != ";" {
                let t = self.peek();
                if !t.bytes().all(|c| c.is_ascii_digit()) && !matches!(t, ":" | "-") {
                    return Err(self.error("invalid source location"));
                }
                source.push(self.take()?);
            }
        }
        self.expect(";")?;
        Ok(Instruction { statement, source })
    }

    fn body(&mut self) -> Result<Vec<Instruction>, Error> {
        self.expect("{")?;
        let mut body = Vec::new();
        while !self.eat("}") {
            body.push(self.instruction()?);
        }
        for instruction in &body {
            if let Statement::Goto(n) | Statement::Jump(_, n) = &instruction.statement
                && *n >= body.len()
            {
                return Err(self.error(format!(
                    "branch target {n} outside {} instructions",
                    body.len()
                )));
            }
        }
        Ok(body)
    }

    fn arg(&mut self) -> Result<(String, Type), Error> {
        let n = self.ident()?;
        self.expect(":")?;
        Ok((n, self.ty()?))
    }

    fn definition(&mut self) -> Result<Definition, Error> {
        let keyword = self.take()?;
        Ok(match keyword.as_str() {
            s if s.starts_with('#') => Definition::Pragma(keyword),
            "enum" => {
                let n = self.ident()?;
                self.expect("{")?;
                Definition::Enum(n, self.list("}", Self::ident)?)
            }
            "struct" | "union" => {
                let n = self.ident()?;
                self.expect("{")?;
                let fields = self.list("}", Self::arg)?;
                if keyword == "struct" {
                    Definition::Struct(n, fields)
                } else {
                    Definition::Union(n, fields)
                }
            }
            "register" => {
                let (n, t) = self.arg()?;
                let b = if self.peek() == "{" {
                    self.body()?
                } else {
                    Vec::new()
                };
                Definition::Register(n, t, b)
            }
            "let" => {
                self.expect("(")?;
                let args = self.list(")", Self::arg)?;
                Definition::Let(args, self.body()?)
            }
            "fn" => {
                let n = self.ident()?;
                self.expect("(")?;
                let args = self.list(")", Self::ident)?;
                Definition::Function(n, args, self.body()?)
            }
            "val" | "abstract" => {
                let name = self.ident()?;
                let external = if self.eat("=") {
                    Some(self.string()?)
                } else {
                    None
                };
                self.expect(":")?;
                self.expect("(")?;
                let args = self.list(")", Self::ty)?;
                self.expect("->")?;
                Definition::Signature {
                    name,
                    external,
                    abstract_: keyword == "abstract",
                    args,
                    result: self.ty()?,
                }
            }
            "files" => {
                let mut files = Vec::new();
                while self.peek().starts_with('"') {
                    files.push(self.string()?);
                }
                Definition::Files(files)
            }
            _ => return Err(self.error(format!("unknown definition {keyword}"))),
        })
    }
}

pub fn parse(source: &str) -> Result<Document<'_>, Error> {
    let mut parser = Parser {
        tokens: lex(source)?,
        pos: 0,
        end: source.len(),
        depth: 0,
    };
    let mut definitions = Vec::new();
    while parser.pos < parser.tokens.len() {
        definitions.push(parser.definition()?);
    }
    Ok(Document {
        definitions,
        source,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn syntax_and_lossless_round_trip() {
        let source = "#mangled original zname\nunion zU { zA: %bv128 }\nval zf : (%bv128) -> %bv128\nfn zf(zx) {\n zz : %bv128; zz = @slice::<128>(zx, 0); jump zz is zA goto 4; zz = zx as zA; return = zz; end;\n}\nfiles \"a\\n.sail\"";
        let doc = parse(source).unwrap();
        assert_eq!(doc.to_string(), source);
        assert_eq!(
            parse(&doc.to_string()).unwrap().definitions,
            doc.definitions
        );
    }

    #[test]
    fn rejects_malformed_input() {
        for s in [
            "fn f() { goto 100; end; }",
            "val f : (%bogus) -> %unit",
            "fn f() { x = 0b29; end; }",
            "\"unterminated",
            "fn f() { x = @add(1,); }",
        ] {
            assert!(parse(s).is_err(), "accepted {s}");
        }
    }

    #[test]
    fn bounded_nesting() {
        let s = format!(
            "val f : ({}%unit{}) -> %unit",
            "%vec(".repeat(1000),
            ")".repeat(1000)
        );
        assert!(parse(&s).is_err());
    }
}
