use regex::RegexBuilder;
use serde_json::Value;
use std::collections::HashMap;

use super::ast::*;

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

pub(super) fn run_stmts(stmts: &[Stmt], source: &mut String) {
    let mut ctx = Ctx::new();
    eval_stmts(&mut ctx, stmts, source);
}
