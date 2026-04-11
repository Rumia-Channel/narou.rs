use serde_json::Value;

#[derive(Debug, Clone)]
pub enum Stmt {
    Guard(String),
    Let {
        var: String,
        expr: Expr,
    },
    Set {
        target: LValue,
        expr: Expr,
    },
    If {
        cond: Expr,
        body: Vec<Stmt>,
        else_body: Option<Vec<Stmt>>,
    },
    For {
        var: String,
        iter: Expr,
        body: Vec<Stmt>,
    },
    Emit(Expr),
    InsertAtMatch,
}

#[derive(Debug, Clone)]
pub enum Expr {
    String(Vec<StrPart>),
    Regex(String, String),
    ExtractJson(String, String),
    Access(Accessor),
    Chain {
        base: Accessor,
        methods: Vec<Method>,
    },
    ValueChain {
        base: Box<Expr>,
        methods: Vec<Method>,
    },
    Array(Vec<Expr>),
    Null,
    Not(Box<Expr>),
    Or(Box<Expr>, Box<Expr>),
    And(Box<Expr>, Box<Expr>),
    Eq(Box<Expr>, Box<Expr>),
    Ne(Box<Expr>, Box<Expr>),
}

#[derive(Debug, Clone)]
pub enum StrPart {
    Lit(String),
    Interp(Accessor),
}

#[derive(Debug, Clone)]
pub struct Accessor {
    pub base: String,
    pub path: Vec<AccessPart>,
}

#[derive(Debug, Clone)]
pub enum AccessPart {
    Dot(String),
    Bracket(BracketKey),
}

#[derive(Debug, Clone)]
pub enum BracketKey {
    Str(Vec<StrPart>),
    Accessor(Accessor),
}

#[derive(Debug, Clone)]
pub enum LValue {
    Var(String),
    Hash {
        base: String,
        keys: Vec<Vec<StrPart>>,
    },
}

#[derive(Debug, Clone)]
pub enum Method {
    Map { var: String, body: Box<Expr> },
    FlatMap { var: String, body: Box<Expr> },
    Flatten,
    Compact,
    Join(Vec<StrPart>),
    Gsub(Vec<StrPart>, Vec<StrPart>),
    Replace(Vec<StrPart>, Vec<StrPart>),
    IsArray,
    Empty,
}

pub fn val_to_string(val: &Value) -> String {
    match val {
        Value::String(s) => s.clone(),
        Value::Number(n) => n.to_string(),
        Value::Bool(b) => b.to_string(),
        Value::Null => String::new(),
        Value::Array(arr) => arr.iter().map(val_to_string).collect::<Vec<_>>().join("\n"),
        Value::Object(_) => String::new(),
    }
}

pub fn is_truthy(val: &Value) -> bool {
    match val {
        Value::Null => false,
        Value::Bool(b) => *b,
        Value::Number(n) => n.as_f64().unwrap_or(0.0) != 0.0,
        Value::String(s) => !s.is_empty(),
        Value::Array(a) => !a.is_empty(),
        Value::Object(m) => !m.is_empty(),
    }
}

pub fn val_equals(a: &Value, b: &Value) -> bool {
    match (a, b) {
        (Value::Null, Value::Null) => true,
        (Value::Bool(a), Value::Bool(b)) => a == b,
        (Value::String(a), Value::String(b)) => a == b,
        (Value::Number(a), Value::Number(b)) => a == b,
        _ => false,
    }
}
