use regex::RegexBuilder;
use serde_json::Value;
use std::collections::HashMap;
use std::sync::{Mutex, OnceLock};

use super::ast::*;

const PREPROCESS_STEP_BUDGET: usize = 100_000;
const PREPROCESS_MAX_STRING_BYTES: usize = 16 * 1024 * 1024;
const PREPROCESS_MAX_ARRAY_ITEMS: usize = 100_000;

type PreprocessResult<T> = Result<T, String>;

struct Ctx {
    vars: HashMap<String, Value>,
    output: Vec<String>,
    match_start: Option<usize>,
    step_budget: usize,
}

impl Ctx {
    fn new() -> Self {
        Self {
            vars: HashMap::new(),
            output: Vec::new(),
            match_start: None,
            step_budget: PREPROCESS_STEP_BUDGET,
        }
    }

    fn get(&self, name: &str) -> Option<&Value> {
        self.vars.get(name)
    }

    fn set(&mut self, name: &str, val: Value) -> PreprocessResult<()> {
        validate_value_limits(&val)?;
        self.vars.insert(name.to_string(), val);
        Ok(())
    }

    fn consume_step(&mut self) -> PreprocessResult<()> {
        if self.step_budget == 0 {
            return Err("preprocess: step budget exceeded".into());
        }
        self.step_budget -= 1;
        Ok(())
    }

    fn push_output(&mut self, value: String) -> PreprocessResult<()> {
        validate_string_size(&value)?;
        self.output.push(value);
        Ok(())
    }

    fn resolve_str_parts(&self, parts: &[StrPart]) -> PreprocessResult<String> {
        let mut s = String::new();
        for part in parts {
            match part {
                StrPart::Lit(lit) => s.push_str(lit),
                StrPart::Interp(accessor) => {
                    let val = self.resolve_accessor(accessor)?;
                    s.push_str(&val_to_string(&val));
                }
            }
        }
        validate_string_size(&s)?;
        Ok(s)
    }

    fn resolve_accessor(&self, acc: &Accessor) -> PreprocessResult<Value> {
        let base = match self.vars.get(&acc.base) {
            Some(v) => v.clone(),
            None => return Ok(Value::Null),
        };
        self.walk_accessor(&base, &acc.path)
    }

    fn walk_accessor(&self, val: &Value, path: &[AccessPart]) -> PreprocessResult<Value> {
        let mut current = val.clone();
        for part in path {
            current = match part {
                AccessPart::Dot(field) => current.get(field).cloned().unwrap_or(Value::Null),
                AccessPart::Bracket(key) => {
                    let key_val = match key {
                        BracketKey::Str(parts) => Value::String(self.resolve_str_parts(parts)?),
                        BracketKey::Accessor(acc) => self.resolve_accessor(acc)?,
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
        validate_value_limits(&current)?;
        Ok(current)
    }
}

fn eval_expr(ctx: &mut Ctx, expr: &Expr) -> PreprocessResult<Value> {
    ctx.consume_step()?;
    let value = match expr {
        Expr::String(parts) => Value::String(ctx.resolve_str_parts(parts)?),
        Expr::Null => Value::Null,
        Expr::Not(inner) => Value::Bool(!is_truthy(&eval_expr(ctx, inner)?)),
        Expr::Or(left, right) => {
            let lv = eval_expr(ctx, left)?;
            if is_truthy(&lv) {
                lv
            } else {
                eval_expr(ctx, right)?
            }
        }
        Expr::And(left, right) => {
            let lv = eval_expr(ctx, left)?;
            if !is_truthy(&lv) {
                lv
            } else {
                eval_expr(ctx, right)?
            }
        }
        Expr::Eq(left, right) => {
            let left_val = eval_expr(ctx, left)?;
            let right_val = eval_expr(ctx, right)?;
            Value::Bool(val_equals(&left_val, &right_val))
        }
        Expr::Ne(left, right) => {
            let left_val = eval_expr(ctx, left)?;
            let right_val = eval_expr(ctx, right)?;
            Value::Bool(!val_equals(&left_val, &right_val))
        }
        Expr::Access(acc) => ctx.resolve_accessor(acc)?,
        Expr::Chain { base, methods } => {
            let mut val = ctx.resolve_accessor(base)?;
            for method in methods {
                val = eval_method(ctx, val, method)?;
            }
            val
        }
        Expr::ValueChain { base, methods } => {
            let mut val = eval_expr(ctx, base)?;
            for method in methods {
                val = eval_method(ctx, val, method)?;
            }
            val
        }
        Expr::Array(items) => {
            let mut values = Vec::with_capacity(items.len());
            for item in items {
                values.push(eval_expr(ctx, item)?);
            }
            validate_array_len(values.len())?;
            Value::Array(values)
        }
        Expr::ExtractJson(_, _) | Expr::Regex(_, _) => Value::Null,
    };
    validate_value_limits(&value)?;
    Ok(value)
}

fn eval_extract_json(
    ctx: &mut Ctx,
    pattern: &str,
    flags: &str,
    source: &str,
) -> PreprocessResult<Option<Value>> {
    let re = cached_extract_json_regex(pattern, flags)?;
    let Some(caps) = re.captures(source) else {
        return Ok(None);
    };
    let Some(json_match) = caps.get(1) else {
        return Ok(None);
    };
    validate_string_size(json_match.as_str())?;
    let Some(full_match) = caps.get(0) else {
        return Ok(None);
    };
    ctx.match_start = Some(full_match.start());
    let value = serde_json::from_str(json_match.as_str()).ok();
    if let Some(ref value) = value {
        validate_value_limits(value)?;
    }
    Ok(value)
}

fn eval_method(ctx: &mut Ctx, val: Value, method: &Method) -> PreprocessResult<Value> {
    let value = match method {
        Method::Map { var, body } => {
            let arr = match val.as_array() {
                Some(a) => a,
                None => return Ok(val),
            };
            validate_array_len(arr.len())?;
            let mut mapped = Vec::with_capacity(arr.len());
            for item in arr {
                ctx.set(var, item.clone())?;
                mapped.push(eval_expr(ctx, body)?);
            }
            Value::Array(mapped)
        }
        Method::FlatMap { var, body } => {
            let arr = match val.as_array() {
                Some(a) => a,
                None => return Ok(val),
            };
            let mut result = Vec::new();
            for item in arr {
                ctx.set(var, item.clone())?;
                let mapped = eval_expr(ctx, body)?;
                if let Some(sub) = mapped.as_array() {
                    for elem in sub {
                        if let Some(nested) = elem.as_array() {
                            result.extend(nested.iter().cloned());
                        } else {
                            result.push(elem.clone());
                        }
                        validate_array_len(result.len())?;
                    }
                } else {
                    result.push(mapped);
                    validate_array_len(result.len())?;
                }
            }
            Value::Array(result)
        }
        Method::Flatten => {
            let arr = match val.as_array() {
                Some(a) => a,
                None => return Ok(val),
            };
            let mut result = Vec::new();
            for item in arr {
                if let Some(sub) = item.as_array() {
                    result.extend(sub.iter().cloned());
                } else {
                    result.push(item.clone());
                }
                validate_array_len(result.len())?;
            }
            Value::Array(result)
        }
        Method::Compact => {
            let arr = match val.as_array() {
                Some(a) => a,
                None => return Ok(val),
            };
            let filtered: Vec<Value> = arr.iter().filter(|v| !v.is_null()).cloned().collect();
            validate_array_len(filtered.len())?;
            Value::Array(filtered)
        }
        Method::Join(sep_parts) => {
            let sep = ctx.resolve_str_parts(sep_parts)?;
            let arr = match val.as_array() {
                Some(a) => a,
                None => return Ok(val),
            };
            let joined = arr.iter().map(val_to_string).collect::<Vec<_>>().join(&sep);
            validate_string_size(&joined)?;
            Value::String(joined)
        }
        Method::Gsub(from_parts, to_parts) => {
            let from = ctx.resolve_str_parts(from_parts)?;
            let to = ctx.resolve_str_parts(to_parts)?;
            match val.as_str() {
                Some(s) => {
                    let replaced = s.replace(&from, &to);
                    validate_string_size(&replaced)?;
                    Value::String(replaced)
                }
                None => val,
            }
        }
        Method::Replace(from_parts, to_parts) => {
            let from = ctx.resolve_str_parts(from_parts)?;
            let to = ctx.resolve_str_parts(to_parts)?;
            match val.as_str() {
                Some(s) => {
                    let replaced = s.replace(&from, &to);
                    validate_string_size(&replaced)?;
                    Value::String(replaced)
                }
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
    };
    validate_value_limits(&value)?;
    Ok(value)
}

fn eval_stmts(ctx: &mut Ctx, stmts: &[Stmt], source: &mut String) -> PreprocessResult<()> {
    for stmt in stmts {
        eval_stmt(ctx, stmt, source)?;
    }
    Ok(())
}

fn eval_stmt(ctx: &mut Ctx, stmt: &Stmt, source: &mut String) -> PreprocessResult<()> {
    ctx.consume_step()?;
    match stmt {
        Stmt::Guard(guard_text) => {
            if source.contains(guard_text.as_str()) {
                ctx.output.clear();
                ctx.set("__abort__", Value::Bool(true))?;
            }
        }
        Stmt::Let { var, expr } => {
            if let Some(Value::Bool(true)) = ctx.get("__abort__") {
                return Ok(());
            }
            let val = match expr {
                Expr::ExtractJson(pattern, flags) => {
                    eval_extract_json(ctx, pattern, flags, source)?.unwrap_or(Value::Null)
                }
                _ => eval_expr(ctx, expr)?,
            };
            ctx.set(var, val)?;
        }
        Stmt::Set { target, expr } => {
            if let Some(Value::Bool(true)) = ctx.get("__abort__") {
                return Ok(());
            }
            let val = eval_expr(ctx, expr)?;
            match target {
                LValue::Var(name) => ctx.set(name, val)?,
                LValue::Hash { base, keys } => {
                    let mut obj = ctx.get(base).cloned().unwrap_or(Value::Null);
                    if let Some(key_parts) = keys.first() {
                        let key = ctx.resolve_str_parts(key_parts)?;
                        if let Value::Object(ref mut map) = obj {
                            map.insert(key, val);
                        } else if obj.is_null() {
                            let mut map = serde_json::Map::new();
                            map.insert(key, val);
                            obj = Value::Object(map);
                        }
                    }
                    ctx.set(base, obj)?;
                }
            }
        }
        Stmt::Emit(expr) => {
            if let Some(Value::Bool(true)) = ctx.get("__abort__") {
                return Ok(());
            }
            let val = eval_expr(ctx, expr)?;
            ctx.push_output(val_to_string(&val))?;
        }
        Stmt::InsertAtMatch => {
            if ctx.output.is_empty() {
                return Ok(());
            }
            let block = ctx.output.join("\n");
            validate_string_size(&block)?;
            let pos = ctx.match_start.unwrap_or(0);
            source.insert_str(pos, &block);
            validate_string_size(source)?;
            ctx.output.clear();
        }
        Stmt::If {
            cond,
            body,
            else_body,
        } => {
            if let Some(Value::Bool(true)) = ctx.get("__abort__") {
                return Ok(());
            }
            let val = eval_expr(ctx, cond)?;
            if is_truthy(&val) {
                eval_stmts(ctx, body, source)?;
            } else if let Some(else_body) = else_body {
                eval_stmts(ctx, else_body, source)?;
            }
        }
        Stmt::For { var, iter, body } => {
            if let Some(Value::Bool(true)) = ctx.get("__abort__") {
                return Ok(());
            }
            let iter_val = eval_expr(ctx, iter)?;
            let arr = match iter_val.as_array() {
                Some(a) => a.clone(),
                None => return Ok(()),
            };
            validate_array_len(arr.len())?;
            for item in arr {
                ctx.set(var, item)?;
                eval_stmts(ctx, body, source)?;
                if let Some(Value::Bool(true)) = ctx.get("__abort__") {
                    return Ok(());
                }
            }
        }
    }
    Ok(())
}

pub(super) fn run_stmts(stmts: &[Stmt], source: &mut String) {
    if let Err(err) = run_stmts_checked(stmts, source) {
        panic!("{err}");
    }
}

fn run_stmts_checked(stmts: &[Stmt], source: &mut String) -> PreprocessResult<()> {
    let mut ctx = Ctx::new();
    eval_stmts(&mut ctx, stmts, source)
}

fn validate_string_size(text: &str) -> PreprocessResult<()> {
    if text.len() > PREPROCESS_MAX_STRING_BYTES {
        return Err(format!(
            "preprocess: string size limit exceeded ({PREPROCESS_MAX_STRING_BYTES} bytes)"
        ));
    }
    Ok(())
}

fn validate_array_len(len: usize) -> PreprocessResult<()> {
    if len > PREPROCESS_MAX_ARRAY_ITEMS {
        return Err(format!(
            "preprocess: array size limit exceeded ({PREPROCESS_MAX_ARRAY_ITEMS} items)"
        ));
    }
    Ok(())
}

fn validate_value_limits(value: &Value) -> PreprocessResult<()> {
    match value {
        Value::String(text) => validate_string_size(text),
        Value::Array(items) => {
            validate_array_len(items.len())?;
            for item in items {
                validate_value_limits(item)?;
            }
            Ok(())
        }
        Value::Object(map) => {
            for value in map.values() {
                validate_value_limits(value)?;
            }
            Ok(())
        }
        _ => Ok(()),
    }
}

fn cached_extract_json_regex(pattern: &str, flags: &str) -> PreprocessResult<regex::Regex> {
    static CACHE: OnceLock<Mutex<HashMap<(String, String), regex::Regex>>> = OnceLock::new();
    let cache = CACHE.get_or_init(|| Mutex::new(HashMap::new()));
    let key = (pattern.to_string(), flags.to_string());
    let mut guard = cache
        .lock()
        .map_err(|_| "preprocess: regex cache lock poisoned".to_string())?;
    if let Some(regex) = guard.get(&key) {
        return Ok(regex.clone());
    }

    let mut builder = RegexBuilder::new(pattern);
    builder.dot_matches_new_line(flags.contains('s'));
    builder.multi_line(flags.contains('m'));
    let regex = builder
        .build()
        .map_err(|err| format!("preprocess: invalid regex: {err}"))?;
    guard.insert(key, regex.clone());
    Ok(regex)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn preprocess_step_budget_is_enforced() {
        let stmts = vec![
            Stmt::Emit(Expr::String(vec![StrPart::Lit("x".to_string())]));
            (PREPROCESS_STEP_BUDGET / 2) + 1
        ];
        let mut source = String::new();
        let err = run_stmts_checked(&stmts, &mut source).unwrap_err();
        assert_eq!(err, "preprocess: step budget exceeded");
    }

    #[test]
    fn preprocess_string_limit_is_enforced() {
        let oversized = "a".repeat((PREPROCESS_MAX_STRING_BYTES / 2) + 1);
        let stmts = vec![Stmt::Emit(Expr::ValueChain {
            base: Box::new(Expr::Array(vec![
                Expr::String(vec![StrPart::Lit(oversized.clone())]),
                Expr::String(vec![StrPart::Lit(oversized)]),
            ])),
            methods: vec![Method::Join(vec![StrPart::Lit(String::new())])],
        })];
        let mut source = String::new();
        let err = run_stmts_checked(&stmts, &mut source).unwrap_err();
        assert_eq!(
            err,
            format!(
                "preprocess: string size limit exceeded ({PREPROCESS_MAX_STRING_BYTES} bytes)"
            )
        );
    }

    #[test]
    fn preprocess_array_limit_is_enforced() {
        let stmts = vec![Stmt::Let {
            var: "items".to_string(),
            expr: Expr::Array(vec![Expr::Null; PREPROCESS_MAX_ARRAY_ITEMS + 1]),
        }];
        let mut source = String::new();
        let err = run_stmts_checked(&stmts, &mut source).unwrap_err();
        assert_eq!(
            err,
            format!(
                "preprocess: array size limit exceeded ({PREPROCESS_MAX_ARRAY_ITEMS} items)"
            )
        );
    }
}
