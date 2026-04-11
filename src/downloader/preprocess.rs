use pest::Parser;
use pest_derive::Parser;
use regex::RegexBuilder;
use serde_json::Value;
use std::collections::HashMap;

#[derive(Parser)]
#[grammar = "downloader/preprocess.pest"]
struct PreprocessParser;

// ── AST ────────────────────────────────────────────────

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

// ── Parse ───────────────────────────────────────────────

pub fn parse_preprocess(source: &str) -> Result<Vec<Stmt>, String> {
    let pairs = PreprocessParser::parse(Rule::program, source)
        .map_err(|e| format!("preprocess parse error: {e}"))?;
    let mut stmts = Vec::new();
    for pair in pairs {
        for inner in pair.into_inner() {
            if inner.as_rule() == Rule::stmt {
                let stmt_inner = inner.into_inner().next().unwrap();
                stmts.push(build_stmt(stmt_inner));
            }
        }
    }
    Ok(stmts)
}

fn build_stmt(pair: pest::iterators::Pair<Rule>) -> Stmt {
    match pair.as_rule() {
        Rule::guard_stmt => {
            let s = pair.into_inner().next().unwrap();
            Stmt::Guard(build_string(s))
        }
        Rule::let_stmt => {
            let mut inner = pair.into_inner();
            let var = inner.next().unwrap().as_str().to_string();
            let expr = build_expr(inner.next().unwrap());
            Stmt::Let { var, expr }
        }
        Rule::set_stmt => {
            let mut inner = pair.into_inner();
            let lv = build_lvalue(inner.next().unwrap());
            let expr = build_expr(inner.next().unwrap());
            Stmt::Set { target: lv, expr }
        }
        Rule::emit_stmt => {
            let expr = build_expr(pair.into_inner().next().unwrap());
            Stmt::Emit(expr)
        }
        Rule::insert_stmt => Stmt::InsertAtMatch,
        Rule::if_stmt => {
            let mut inner = pair.into_inner();
            let cond = build_expr(inner.next().unwrap());
            let body = inner.next().map(build_stmt_list).unwrap_or_default();
            let else_body = inner.next().map(build_stmt_list);
            Stmt::If {
                cond,
                body,
                else_body,
            }
        }
        Rule::for_stmt => {
            let mut inner = pair.into_inner();
            let var = inner.next().unwrap().as_str().to_string();
            let iter = build_expr(inner.next().unwrap());
            let body = inner.next().map(build_stmt_list).unwrap_or_default();
            Stmt::For { var, iter, body }
        }
        _ => unreachable!("unexpected stmt rule: {:?}", pair.as_rule()),
    }
}

fn build_stmt_list(pair: pest::iterators::Pair<Rule>) -> Vec<Stmt> {
    pair.into_inner()
        .filter(|p| p.as_rule() == Rule::stmt)
        .map(|p| build_stmt(p.into_inner().next().unwrap()))
        .collect()
}

fn build_expr(pair: pest::iterators::Pair<Rule>) -> Expr {
    match pair.as_rule() {
        Rule::or_expr => {
            let mut inner = pair.into_inner();
            let left = build_expr(inner.next().unwrap());
            let rest: Vec<_> = inner.collect();
            if rest.is_empty() {
                return left;
            }
            rest.into_iter().fold(left, |acc, p| {
                Expr::Or(Box::new(acc), Box::new(build_expr(p)))
            })
        }
        Rule::and_expr => {
            let mut inner = pair.into_inner();
            let left = build_expr(inner.next().unwrap());
            let rest: Vec<_> = inner.collect();
            if rest.is_empty() {
                return left;
            }
            rest.into_iter().fold(left, |acc, p| {
                Expr::And(Box::new(acc), Box::new(build_expr(p)))
            })
        }
        Rule::unary_expr => {
            let mut inner = pair.into_inner();
            let first = inner.next().unwrap();
            if first.as_rule() == Rule::unary_expr {
                Expr::Not(Box::new(build_expr(first)))
            } else {
                build_expr(first)
            }
        }
        Rule::cmp_expr => {
            let mut inner = pair.into_inner();
            let left = build_expr(inner.next().unwrap());
            let rest: Vec<_> = inner.collect();
            if rest.len() == 2 {
                let op = rest[0].as_str();
                let right = build_expr(rest[1].clone());
                match op {
                    "==" => Expr::Eq(Box::new(left), Box::new(right)),
                    "!=" => Expr::Ne(Box::new(left), Box::new(right)),
                    _ => left,
                }
            } else {
                left
            }
        }
        Rule::primary => build_expr(pair.into_inner().next().unwrap()),
        Rule::string | Rule::i_string | Rule::r_string => Expr::String(build_string_parts(pair)),
        Rule::extract_json_expr => {
            let mut inner = pair.into_inner();
            let regex_pair = inner.next().unwrap();
            let (pat, flags) = build_regex(regex_pair);
            Expr::ExtractJson(pat, flags)
        }
        Rule::chain_expr => {
            let mut inner = pair.into_inner();
            let base_name = inner.next().unwrap().as_str().to_string();
            let mut path: Vec<AccessPart> = Vec::new();
            let mut methods: Vec<Method> = Vec::new();
            for part in inner {
                match part.as_rule() {
                    Rule::dot_access => {
                        let field = part.into_inner().next().unwrap().as_str().to_string();
                        path.push(AccessPart::Dot(field));
                    }
                    Rule::bracket_access => {
                        let key_pair = part.into_inner().next().unwrap();
                        path.push(AccessPart::Bracket(build_bracket_key(key_pair)));
                    }
                    _ => {
                        methods.push(build_method(part));
                    }
                }
            }
            let accessor = Accessor {
                base: base_name,
                path,
            };
            if methods.is_empty() {
                Expr::Access(accessor)
            } else {
                Expr::Chain {
                    base: accessor,
                    methods,
                }
            }
        }
        Rule::array_chain_expr => {
            let mut inner = pair.into_inner();
            let base = build_expr(inner.next().unwrap());
            let methods = inner.map(build_method).collect();
            Expr::ValueChain {
                base: Box::new(base),
                methods,
            }
        }
        Rule::array_literal => {
            let exprs = pair.into_inner().map(build_expr).collect();
            Expr::Array(exprs)
        }
        Rule::null_lit => Expr::Null,
        _ => unreachable!("unexpected expr rule: {:?}", pair.as_rule()),
    }
}

fn build_string(pair: pest::iterators::Pair<Rule>) -> String {
    let parts = build_string_parts(pair);
    resolve_string_parts(&parts)
}

fn build_string_parts(pair: pest::iterators::Pair<Rule>) -> Vec<StrPart> {
    let mut parts = Vec::new();
    for p in pair.into_inner() {
        match p.as_rule() {
            Rule::interp => {
                let accessor = build_accessor(p.into_inner().next().unwrap());
                parts.push(StrPart::Interp(accessor));
            }
            Rule::i_text | Rule::r_text => {
                let raw = p.as_str();
                parts.push(StrPart::Lit(unescape_str(raw)));
            }
            _ => {}
        }
    }
    parts
}

fn unescape_str(s: &str) -> String {
    let mut result = String::with_capacity(s.len());
    let mut chars = s.chars();
    while let Some(c) = chars.next() {
        if c == '\\' {
            match chars.next() {
                Some('n') => result.push('\n'),
                Some('t') => result.push('\t'),
                Some('\\') => result.push('\\'),
                Some('"') => result.push('"'),
                Some('\'') => result.push('\''),
                Some('/') => result.push('/'),
                Some(other) => {
                    result.push('\\');
                    result.push(other);
                }
                None => result.push('\\'),
            }
        } else {
            result.push(c);
        }
    }
    result
}

fn resolve_string_parts(parts: &[StrPart]) -> String {
    let mut s = String::new();
    for part in parts {
        match part {
            StrPart::Lit(lit) => s.push_str(lit),
            StrPart::Interp(_) => s.push_str("\x00INTERP\x00"),
        }
    }
    s
}

fn build_regex(pair: pest::iterators::Pair<Rule>) -> (String, String) {
    let mut inner = pair.into_inner();
    let body = inner.next().unwrap().as_str().to_string();
    let flags = inner
        .next()
        .map(|p| p.as_str().to_string())
        .unwrap_or_default();
    (body, flags)
}

fn build_accessor(pair: pest::iterators::Pair<Rule>) -> Accessor {
    let mut inner = pair.into_inner();
    let base = inner.next().unwrap().as_str().to_string();
    let path = inner.map(build_access_part).collect();
    Accessor { base, path }
}

fn build_access_part(pair: pest::iterators::Pair<Rule>) -> AccessPart {
    match pair.as_rule() {
        Rule::dot_access => {
            let field = pair.into_inner().next().unwrap().as_str().to_string();
            AccessPart::Dot(field)
        }
        Rule::bracket_access => {
            let inner = pair.into_inner().next().unwrap();
            let key = build_bracket_key(inner);
            AccessPart::Bracket(key)
        }
        Rule::bracket_inner => {
            let inner = pair.into_inner().next().unwrap();
            let key = build_bracket_key(inner);
            AccessPart::Bracket(key)
        }
        _ => unreachable!("unexpected access_part: {:?}", pair.as_rule()),
    }
}

fn build_bracket_key(pair: pest::iterators::Pair<Rule>) -> BracketKey {
    match pair.as_rule() {
        Rule::string | Rule::i_string | Rule::r_string => BracketKey::Str(build_string_parts(pair)),
        Rule::chain_expr => {
            let accessor = {
                let mut inner = pair.into_inner();
                let base = inner.next().unwrap().as_str().to_string();
                let path = inner
                    .filter_map(|p| match p.as_rule() {
                        Rule::dot_access => {
                            let field = p.into_inner().next().unwrap().as_str().to_string();
                            Some(AccessPart::Dot(field))
                        }
                        Rule::bracket_access => {
                            let key_pair = p.into_inner().next().unwrap();
                            Some(AccessPart::Bracket(build_bracket_key(key_pair)))
                        }
                        _ => None,
                    })
                    .collect();
                Accessor { base, path }
            };
            BracketKey::Accessor(accessor)
        }
        _ => unreachable!("unexpected bracket_key: {:?}", pair.as_rule()),
    }
}

fn build_string_from_inner(pair: pest::iterators::Pair<Rule>) -> Vec<StrPart> {
    match pair.as_rule() {
        Rule::i_string | Rule::r_string => build_string_parts(pair),
        other => unreachable!("unexpected string inner: {:?}", other),
    }
}

fn build_lvalue(pair: pest::iterators::Pair<Rule>) -> LValue {
    let mut inner = pair.into_inner();
    let base = inner.next().unwrap().as_str().to_string();
    let mut dot_keys: Vec<String> = Vec::new();
    let mut bracket_keys: Vec<Vec<StrPart>> = Vec::new();
    for part in inner {
        match part.as_rule() {
            Rule::dot_key => {
                dot_keys.push(part.into_inner().next().unwrap().as_str().to_string());
            }
            Rule::bracket_key => {
                let key_inner = part.into_inner().next().unwrap();
                bracket_keys.push(build_string_from_inner(key_inner));
            }
            other => unreachable!("unexpected lvalue part: {:?}", other),
        }
    }
    if dot_keys.len() == 1 && bracket_keys.is_empty() {
        LValue::Hash {
            base,
            keys: vec![dot_keys.into_iter().map(|k| StrPart::Lit(k)).collect()],
        }
    } else if !bracket_keys.is_empty() {
        LValue::Hash {
            base,
            keys: bracket_keys,
        }
    } else if dot_keys.is_empty() {
        LValue::Var(base)
    } else {
        LValue::Hash {
            base,
            keys: dot_keys
                .into_iter()
                .map(|k| vec![StrPart::Lit(k)])
                .collect(),
        }
    }
}

fn build_block(pair: pest::iterators::Pair<Rule>) -> (String, Expr) {
    let mut inner = pair.into_inner();
    let var = inner.next().unwrap().as_str().to_string();
    let body = Box::new(build_expr(inner.next().unwrap()));
    (var, *body)
}

fn build_method(pair: pest::iterators::Pair<Rule>) -> Method {
    let s = pair.as_str();
    match s {
        s if s.starts_with(".map ") || s.starts_with(".map{") => {
            let block = pair.into_inner().next().unwrap();
            let (var, body) = build_block(block);
            Method::Map {
                var,
                body: Box::new(body),
            }
        }
        s if s.starts_with(".flat_map") => {
            let block = pair.into_inner().next().unwrap();
            let (var, body) = build_block(block);
            Method::FlatMap {
                var,
                body: Box::new(body),
            }
        }
        ".flatten" => Method::Flatten,
        ".compact" => Method::Compact,
        s if s.starts_with(".join") => {
            let mut inner = pair.into_inner();
            let sep = build_string_parts(inner.next().unwrap());
            Method::Join(sep)
        }
        s if s.starts_with(".gsub") => {
            let mut inner = pair.into_inner();
            let from = build_string_parts(inner.next().unwrap());
            let to = build_string_parts(inner.next().unwrap());
            Method::Gsub(from, to)
        }
        s if s.starts_with(".replace") => {
            let mut inner = pair.into_inner();
            let from = build_string_parts(inner.next().unwrap());
            let to = build_string_parts(inner.next().unwrap());
            Method::Replace(from, to)
        }
        ".is_array" => Method::IsArray,
        ".empty" => Method::Empty,
        _ => unreachable!("unexpected method: {s}"),
    }
}

// ── Interpreter ─────────────────────────────────────────

struct Ctx {
    vars: HashMap<String, Value>,
    output: Vec<String>,
    match_start: Option<usize>,
}

impl Ctx {
    fn new() -> Self {
        Self {
            vars: HashMap::new(),
            output: Vec::new(),
            match_start: None,
        }
    }

    fn get(&self, name: &str) -> Option<&Value> {
        self.vars.get(name)
    }

    fn set(&mut self, name: &str, val: Value) {
        self.vars.insert(name.to_string(), val);
    }

    fn resolve_str_parts(&self, parts: &[StrPart]) -> String {
        let mut s = String::new();
        for part in parts {
            match part {
                StrPart::Lit(lit) => s.push_str(lit),
                StrPart::Interp(accessor) => {
                    let val = self.resolve_accessor(accessor);
                    s.push_str(&val_to_string(&val));
                }
            }
        }
        s
    }

    fn resolve_accessor(&self, acc: &Accessor) -> Value {
        let base = match self.vars.get(&acc.base) {
            Some(v) => v.clone(),
            None => return Value::Null,
        };
        self.walk_accessor(&base, &acc.path)
    }

    fn walk_accessor(&self, val: &Value, path: &[AccessPart]) -> Value {
        let mut current = val.clone();
        for part in path {
            current = match part {
                AccessPart::Dot(field) => current.get(field).cloned().unwrap_or(Value::Null),
                AccessPart::Bracket(key) => {
                    let key_val = match key {
                        BracketKey::Str(parts) => Value::String(self.resolve_str_parts(parts)),
                        BracketKey::Accessor(acc) => self.resolve_accessor(acc),
                    };
                    match key_val {
                        Value::String(k) => current.get(&k).cloned().unwrap_or(Value::Null),
                        Value::Number(n) => {
                            let idx = n.as_u64().unwrap_or(0) as usize;
                            current.get(idx).cloned().unwrap_or(Value::Null)
                        }
                        _ => Value::Null,
                    }
                }
            };
        }
        current
    }
}

fn val_to_string(val: &Value) -> String {
    match val {
        Value::String(s) => s.clone(),
        Value::Number(n) => n.to_string(),
        Value::Bool(b) => b.to_string(),
        Value::Null => String::new(),
        Value::Array(arr) => arr.iter().map(val_to_string).collect::<Vec<_>>().join("\n"),
        Value::Object(_) => String::new(),
    }
}

fn is_truthy(val: &Value) -> bool {
    match val {
        Value::Null => false,
        Value::Bool(b) => *b,
        Value::Number(n) => n.as_f64().unwrap_or(0.0) != 0.0,
        Value::String(s) => !s.is_empty(),
        Value::Array(a) => !a.is_empty(),
        Value::Object(m) => !m.is_empty(),
    }
}

fn eval_expr(ctx: &mut Ctx, expr: &Expr) -> Value {
    match expr {
        Expr::String(parts) => Value::String(ctx.resolve_str_parts(parts)),
        Expr::Null => Value::Null,
        Expr::Not(inner) => Value::Bool(!is_truthy(&eval_expr(ctx, inner))),
        Expr::Or(left, right) => {
            let lv = eval_expr(ctx, left);
            if is_truthy(&lv) {
                lv
            } else {
                eval_expr(ctx, right)
            }
        }
        Expr::And(left, right) => {
            let lv = eval_expr(ctx, left);
            if !is_truthy(&lv) {
                lv
            } else {
                eval_expr(ctx, right)
            }
        }
        Expr::Eq(left, right) => {
            Value::Bool(val_equals(&eval_expr(ctx, left), &eval_expr(ctx, right)))
        }
        Expr::Ne(left, right) => {
            Value::Bool(!val_equals(&eval_expr(ctx, left), &eval_expr(ctx, right)))
        }
        Expr::Access(acc) => ctx.resolve_accessor(acc),
        Expr::Chain { base, methods } => {
            let mut val = ctx.resolve_accessor(base);
            for method in methods {
                val = eval_method(ctx, val, method);
            }
            val
        }
        Expr::ValueChain { base, methods } => {
            let mut val = eval_expr(ctx, base);
            for method in methods {
                val = eval_method(ctx, val, method);
            }
            val
        }
        Expr::Array(items) => Value::Array(items.iter().map(|e| eval_expr(ctx, e)).collect()),
        Expr::ExtractJson(_, _) | Expr::Regex(_, _) => Value::Null,
    }
}

fn eval_extract_json(ctx: &mut Ctx, pattern: &str, flags: &str, source: &str) -> Option<Value> {
    let mut builder = RegexBuilder::new(pattern);
    builder.dot_matches_new_line(flags.contains('s'));
    builder.multi_line(flags.contains('m'));
    let re = builder.build().ok()?;
    let caps = re.captures(source)?;
    let json_str = caps.get(1)?.as_str();
    ctx.match_start = Some(caps.get(0)?.start());
    serde_json::from_str(json_str).ok()
}

fn val_equals(a: &Value, b: &Value) -> bool {
    match (a, b) {
        (Value::Null, Value::Null) => true,
        (Value::Bool(a), Value::Bool(b)) => a == b,
        (Value::String(a), Value::String(b)) => a == b,
        (Value::Number(a), Value::Number(b)) => a == b,
        _ => false,
    }
}

fn eval_method(ctx: &mut Ctx, val: Value, method: &Method) -> Value {
    match method {
        Method::Map { var, body } => {
            let arr = match val.as_array() {
                Some(a) => a,
                None => return val,
            };
            let mapped: Vec<Value> = arr
                .iter()
                .map(|item| {
                    ctx.set(var, item.clone());
                    eval_expr(ctx, body)
                })
                .collect();
            Value::Array(mapped)
        }
        Method::FlatMap { var, body } => {
            let arr = match val.as_array() {
                Some(a) => a,
                None => return val,
            };
            let mut result = Vec::new();
            for item in arr {
                ctx.set(var, item.clone());
                let mapped = eval_expr(ctx, body);
                if let Some(sub) = mapped.as_array() {
                    for elem in sub {
                        if let Some(nested) = elem.as_array() {
                            result.extend(nested.iter().cloned());
                        } else {
                            result.push(elem.clone());
                        }
                    }
                } else {
                    result.push(mapped);
                }
            }
            Value::Array(result)
        }
        Method::Flatten => {
            let arr = match val.as_array() {
                Some(a) => a,
                None => return val,
            };
            let mut result = Vec::new();
            for item in arr {
                if let Some(sub) = item.as_array() {
                    result.extend(sub.iter().cloned());
                } else {
                    result.push(item.clone());
                }
            }
            Value::Array(result)
        }
        Method::Compact => {
            let arr = match val.as_array() {
                Some(a) => a,
                None => return val,
            };
            let filtered: Vec<Value> = arr.iter().filter(|v| !v.is_null()).cloned().collect();
            Value::Array(filtered)
        }
        Method::Join(sep_parts) => {
            let sep = ctx.resolve_str_parts(sep_parts);
            let arr = match val.as_array() {
                Some(a) => a,
                None => return val,
            };
            let joined = arr.iter().map(val_to_string).collect::<Vec<_>>().join(&sep);
            Value::String(joined)
        }
        Method::Gsub(from_parts, to_parts) => {
            let from = ctx.resolve_str_parts(from_parts);
            let to = ctx.resolve_str_parts(to_parts);
            match val.as_str() {
                Some(s) => Value::String(s.replace(&from, &to)),
                None => val,
            }
        }
        Method::Replace(from_parts, to_parts) => {
            let from = ctx.resolve_str_parts(from_parts);
            let to = ctx.resolve_str_parts(to_parts);
            match val.as_str() {
                Some(s) => Value::String(s.replace(&from, &to)),
                None => val,
            }
        }
        Method::IsArray => Value::Bool(matches!(val, Value::Array(_))),
        Method::Empty => match &val {
            Value::String(s) => Value::Bool(s.is_empty()),
            Value::Array(a) => Value::Bool(a.is_empty()),
            Value::Null => Value::Bool(true),
            _ => Value::Bool(false),
        },
    }
}

fn eval_stmts(ctx: &mut Ctx, stmts: &[Stmt], source: &mut String) {
    for stmt in stmts {
        eval_stmt(ctx, stmt, source);
    }
}

fn eval_stmt(ctx: &mut Ctx, stmt: &Stmt, source: &mut String) {
    match stmt {
        Stmt::Guard(guard_text) => {
            if source.contains(guard_text.as_str()) {
                ctx.output.clear();
                ctx.set("__abort__", Value::Bool(true));
            }
        }
        Stmt::Let { var, expr } => {
            if let Some(Value::Bool(true)) = ctx.get("__abort__") {
                return;
            }
            let val = match expr {
                Expr::ExtractJson(pattern, flags) => {
                    eval_extract_json(ctx, pattern, flags, source).unwrap_or(Value::Null)
                }
                _ => eval_expr(ctx, expr),
            };
            ctx.set(var, val);
        }
        Stmt::Set { target, expr } => {
            if let Some(Value::Bool(true)) = ctx.get("__abort__") {
                return;
            }
            let val = eval_expr(ctx, expr);
            match target {
                LValue::Var(name) => ctx.set(name, val),
                LValue::Hash { base, keys } => {
                    let mut obj = ctx.get(base).cloned().unwrap_or(Value::Null);
                    if let Some(key_parts) = keys.first() {
                        let key = ctx.resolve_str_parts(key_parts);
                        if let Value::Object(ref mut map) = obj {
                            map.insert(key, val);
                        } else if obj.is_null() {
                            let mut map = serde_json::Map::new();
                            map.insert(key, val);
                            obj = Value::Object(map);
                        }
                    }
                    ctx.set(base, obj);
                }
            }
        }
        Stmt::Emit(expr) => {
            if let Some(Value::Bool(true)) = ctx.get("__abort__") {
                return;
            }
            let val = eval_expr(ctx, expr);
            ctx.output.push(val_to_string(&val));
        }
        Stmt::InsertAtMatch => {
            if ctx.output.is_empty() {
                return;
            }
            let block = ctx.output.join("\n");
            let pos = ctx.match_start.unwrap_or(0);
            source.insert_str(pos, &block);
            ctx.output.clear();
        }
        Stmt::If {
            cond,
            body,
            else_body,
        } => {
            if let Some(Value::Bool(true)) = ctx.get("__abort__") {
                return;
            }
            let val = eval_expr(ctx, cond);
            if is_truthy(&val) {
                eval_stmts(ctx, body, source);
            } else if let Some(else_body) = else_body {
                eval_stmts(ctx, else_body, source);
            }
        }
        Stmt::For { var, iter, body } => {
            if let Some(Value::Bool(true)) = ctx.get("__abort__") {
                return;
            }
            let iter_val = eval_expr(ctx, iter);
            let arr = match iter_val.as_array() {
                Some(a) => a.clone(),
                None => return,
            };
            for item in arr {
                ctx.set(var, item);
                eval_stmts(ctx, body, source);
                if let Some(Value::Bool(true)) = ctx.get("__abort__") {
                    return;
                }
            }
        }
    }
}

// ── Public API ──────────────────────────────────────────

#[derive(Debug, Clone)]
pub struct PreprocessPipeline {
    stmts: Vec<Stmt>,
}

impl PreprocessPipeline {
    pub fn compile(source: &str) -> Result<Self, String> {
        let stmts = parse_preprocess(source)?;
        Ok(Self { stmts })
    }

    pub fn execute(&self, source: &mut String) {
        let mut ctx = Ctx::new();
        eval_stmts(&mut ctx, &self.stmts, source);
    }
}

pub fn run_preprocess(pipeline: &PreprocessPipeline, source: &mut String) {
    pipeline.execute(source);
}
