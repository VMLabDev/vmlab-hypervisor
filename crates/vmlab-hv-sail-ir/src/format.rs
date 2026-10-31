use crate::{Definition, Expr, Instruction, Location, Statement, Type};
use std::fmt::{self, Write};

/// Emit canonical Jib from the AST, independently of the original input bytes.
pub fn format(definitions: &[Definition]) -> String {
    let mut text = String::new();
    for definition in definitions {
        writeln!(text, "{definition}\n").expect("String write");
    }
    text
}

struct Quoted<'a>(&'a str);
impl fmt::Display for Quoted<'_> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str("\"")?;
        for c in self.0.chars() {
            match c {
                '"' => f.write_str("\\\"")?,
                '\\' => f.write_str("\\\\")?,
                '\n' => f.write_str("\\n")?,
                '\r' => f.write_str("\\r")?,
                '\t' => f.write_str("\\t")?,
                c if c.is_ascii_control() => write!(f, "\\{:03}", c as u32)?,
                c => write!(f, "{c}")?,
            }
        }
        f.write_str("\"")
    }
}

fn list<T: fmt::Display>(f: &mut fmt::Formatter<'_>, values: &[T]) -> fmt::Result {
    for (i, value) in values.iter().enumerate() {
        if i != 0 {
            f.write_str(", ")?;
        }
        write!(f, "{value}")?;
    }
    Ok(())
}

fn fields<T: fmt::Display>(
    f: &mut fmt::Formatter<'_>,
    values: &[(String, T)],
    separator: &str,
) -> fmt::Result {
    for (i, (name, value)) in values.iter().enumerate() {
        if i != 0 {
            f.write_str(", ")?;
        }
        write!(f, "{name}{separator}{value}")?;
    }
    Ok(())
}

impl fmt::Display for Type {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Integer(width) | Self::Bits(width) => {
                f.write_str(if matches!(self, Self::Integer(_)) {
                    "%i"
                } else {
                    "%bv"
                })?;
                if let Some(width) = width {
                    write!(f, "{width}")?;
                }
                Ok(())
            }
            Self::Float(width) => write!(f, "%f{width}"),
            Self::Unit => f.write_str("%unit"),
            Self::Bool => f.write_str("%bool"),
            Self::Bit => f.write_str("%bit"),
            Self::String => f.write_str("%string"),
            Self::Real => f.write_str("%real"),
            Self::RoundingMode => f.write_str("%rounding_mode"),
            Self::Enum(name) => write!(f, "%enum {name}"),
            Self::Struct(name) => write!(f, "%struct {name}"),
            Self::Union(name) => write!(f, "%union {name}"),
            Self::Vector(ty) => write!(f, "%vec({ty})"),
            Self::List(ty) => write!(f, "%list({ty})"),
            Self::Ref(ty) => write!(f, "&({ty})"),
            Self::FixedVector(n, ty) => write!(f, "%fvec({n}, {ty})"),
        }
    }
}

impl fmt::Display for Expr {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Id(name) => f.write_str(name),
            Self::Integer(value, width) => {
                f.write_str(value)?;
                if let Some(width) = width {
                    write!(f, " : %i{width}")?;
                }
                Ok(())
            }
            Self::Bits(value) => f.write_str(value),
            Self::Bool(value) => write!(f, "{value}"),
            Self::String(value) => write!(f, "{}", Quoted(value)),
            Self::Unit => f.write_str("()"),
            Self::Undefined(ty) => write!(f, "undefined : {ty}"),
            Self::Ref(name) => write!(f, "&{name}"),
            Self::Struct(name, values) => {
                write!(f, "struct {name} {{")?;
                fields(f, values, " = ")?;
                f.write_str("}")
            }
            Self::Field(expr, field) => write!(f, "{expr}.{field}"),
            Self::Is(expr, tag) => write!(f, "{expr} is {tag}"),
            Self::As(expr, tag) => write!(f, "{expr} as {tag}"),
            Self::Op(name, width, args) => {
                f.write_str(name)?;
                if let Some(width) = width {
                    write!(f, "::<{width}>")?;
                }
                f.write_str("(")?;
                list(f, args)?;
                f.write_str(")")
            }
        }
    }
}

impl fmt::Display for Location {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Id(name) => f.write_str(name),
            Self::Field(base, field) => write!(f, "{base}.{field}"),
            Self::Deref(base) => write!(f, "{base}*"),
        }
    }
}

impl fmt::Display for Instruction {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match &self.statement {
            Statement::Declare(name, ty, value) => {
                write!(f, "{name} : {ty}")?;
                if let Some(value) = value {
                    write!(f, " = {value}")?;
                }
            }
            Statement::Copy(location, value) => write!(f, "{location} = {value}")?,
            Statement::Call(location, external, name, args) => {
                write!(
                    f,
                    "{location} = {}{name}(",
                    if *external { "$" } else { "" }
                )?;
                list(f, args)?;
                f.write_str(")")?;
            }
            Statement::Jump(condition, target) => write!(f, "jump {condition} goto {target}")?,
            Statement::Goto(target) => write!(f, "goto {target}")?,
            Statement::Monomorphize(name, ty) => write!(f, "mono {name} : {ty}")?,
            Statement::Exit(cause) => write!(f, "exit {cause}")?,
            Statement::Arbitrary => f.write_str("arbitrary")?,
            Statement::End => f.write_str("end")?,
        }
        if !self.source.is_empty() {
            write!(f, " `{}", self.source.join(" "))?;
        }
        f.write_str(";")
    }
}

fn body(f: &mut fmt::Formatter<'_>, instructions: &[Instruction]) -> fmt::Result {
    f.write_str(" {\n")?;
    for instruction in instructions {
        writeln!(f, "  {instruction}")?;
    }
    f.write_str("}")
}

impl fmt::Display for Definition {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Enum(name, members) => {
                write!(f, "enum {name} {{")?;
                list(f, members)?;
                f.write_str("}")
            }
            Self::Struct(name, values) | Self::Union(name, values) => {
                write!(
                    f,
                    "{} {name} {{",
                    if matches!(self, Self::Struct(_, _)) {
                        "struct"
                    } else {
                        "union"
                    }
                )?;
                fields(f, values, ": ")?;
                f.write_str("}")
            }
            Self::Register(name, ty, instructions) => {
                write!(f, "register {name} : {ty}")?;
                if !instructions.is_empty() {
                    body(f, instructions)?;
                }
                Ok(())
            }
            Self::Let(values, instructions) => {
                f.write_str("let (")?;
                fields(f, values, ": ")?;
                f.write_str(")")?;
                body(f, instructions)
            }
            Self::Signature {
                name,
                external,
                abstract_,
                args,
                result,
            } => {
                write!(f, "{} {name}", if *abstract_ { "abstract" } else { "val" })?;
                if let Some(external) = external {
                    write!(f, " = {}", Quoted(external))?;
                }
                f.write_str(" : (")?;
                list(f, args)?;
                write!(f, ") -> {result}")
            }
            Self::Function(name, args, instructions) => {
                write!(f, "fn {name}(")?;
                list(f, args)?;
                f.write_str(")")?;
                body(f, instructions)
            }
            Self::Pragma(text) => f.write_str(text),
            Self::Files(paths) => {
                f.write_str("files")?;
                for path in paths {
                    write!(f, " {}", Quoted(path))?;
                }
                Ok(())
            }
        }
    }
}
