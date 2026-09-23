use regex::RegexBuilder;
use serde_json::Value;
use std::collections::HashMap;
use std::sync::{Mutex, OnceLock};

use super::ast::*;
use super::jobs::{MAX_PREPROCESS_JOBS, PreprocessJobs};

const PREPROCESS_STEP_BUDGET: usize = 100_000;
const PREPROCESS_MAX_STRING_BYTES: usize = 16 * 1024 * 1024;
const PREPROCESS_MAX_ARRAY_ITEMS: usize = 100_000;

type PreprocessResult<T> = Result<T, String>;

struct Ctx<'a> {
    vars: HashMap<String, Value>,
    output: Vec<String>,
    match_start: Option<usize>,
    step_budget: usize,
    /// Results of previously requested URLs, exposed to the DSL as `fetched`.
    jobs: &'a PreprocessJobs,
    /// URLs this run asked for, in request order.
    requested: Vec<String>,
}

impl<'a> Ctx<'a> {
    fn new(jobs: &'a PreprocessJobs) -> Self {
        let mut vars = HashMap::new();
        vars.insert(
            "fetched".to_string(),
            Value::Object(jobs.results().iter().map(|(k, v)| (k.clone(), v.clone())).collect()),
        );
        Self {
            vars,
            output: Vec::new(),
            match_start: None,
            step_budget: PREPROCESS_STEP_BUDGET,
            jobs,
            requested: Vec::new(),
        }
    }

    /// Record a fetch request. Invalid or already-known URLs are not queued;
    /// the DSL still gets the URL back so it can look the result up.
    fn request(&mut self, url: String) -> PreprocessResult<Value> {
        if !(url.starts_with("http://") || url.starts_with("https://"))
            || !crate::downloader::security::is_safe_public_url(&url)
        {
            return Ok(Value::Null);
        }
        if !self.jobs.contains(&url)
            && !self.requested.contains(&url)
            && self.requested.len() < MAX_PREPROCESS_JOBS
        {
            self.requested.push(url.clone());
        }
        Ok(Value::String(url))
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

    fn resolve_str_parts(&mut self, parts: &[StrPart]) -> PreprocessResult<String> {
        let mut s = String::new();
        for part in parts {
            match part {
                StrPart::Lit(lit) => s.push_str(lit),
                StrPart::Interp(expr) => {
                    let val = eval_expr(self, expr)?;
                    s.push_str(&val_to_string(&val));
                }
            }
        }
        validate_string_size(&s)?;
        Ok(s)
    }

    fn resolve_accessor(&mut self, acc: &Accessor) -> PreprocessResult<Value> {
        let base = match self.vars.get(&acc.base) {
            Some(v) => v.clone(),
            None => return Ok(Value::Null),
        };
        self.walk_accessor(&base, &acc.path)
    }

    fn walk_accessor(&mut self, val: &Value, path: &[AccessPart]) -> PreprocessResult<Value> {
        let mut current = val.clone();
        for part in path {
            current = match part {
                AccessPart::Dot(field) => current.get(field).cloned().unwrap_or(Value::Null),
                AccessPart::Bracket(key) => {
                    let key_val = match key {
                        BracketKey::Str(parts) => Value::String(self.resolve_str_parts(parts)?),
                        BracketKey::Expr(expr) => eval_expr(self, expr)?,
                        BracketKey::Index(index) => Value::Number((*index).into()),
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
            validate_array_len(items.len())?;
            let mut values = Vec::with_capacity(items.len());
            for item in items {
                values.push(eval_expr(ctx, item)?);
            }
            Value::Array(values)
        }
        Expr::Int(value) => Value::Number((*value).into()),
        Expr::Request(inner) => {
            let url = val_to_string(&eval_expr(ctx, inner)?);
            ctx.request(url)?
        }
        Expr::FetchJson(inner) => {
            let url = val_to_string(&eval_expr(ctx, inner)?);
            let url = val_to_string(&ctx.request(url)?);
            if url.is_empty() {
                Value::Null
            } else {
                ctx.jobs.results().get(&url).cloned().unwrap_or(Value::Null)
            }
        }
        Expr::Arith(left, op, right) => {
            let left = eval_expr(ctx, left)?;
            let right = eval_expr(ctx, right)?;
            match (as_integer(&left), as_integer(&right)) {
                (Some(a), Some(b)) => Value::Number(match op {
                    ArithOp::Add => a.saturating_add(b),
                    ArithOp::Sub => a.saturating_sub(b),
                }
                .into()),
                _ => Value::Null,
            }
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
    let re = cached_preprocess_regex(pattern, flags)?;
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

/// Integers for arithmetic. Numeric strings are coerced (captures are always
/// strings); anything else yields `None`, which makes the whole expression null
/// instead of silently concatenating.
fn as_integer(value: &Value) -> Option<i64> {
    match value {
        Value::Number(number) => number.as_i64(),
        Value::String(text) => text.trim().parse::<i64>().ok(),
        _ => None,
    }
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
        Method::GsubRegex {
            pattern,
            flags,
            to,
        } => {
            let replacement = ctx.resolve_str_parts(to)?;
            match val.as_str() {
                Some(s) => {
                    let re = cached_preprocess_regex(pattern, flags)?;
                    let replaced = re.replace_all(s, replacement.as_str()).into_owned();
                    validate_string_size(&replaced)?;
                    Value::String(replaced)
                }
                None => val,
            }
        }
        Method::GsubBlock {
            pattern,
            flags,
            var,
            body,
        } => match val.as_str() {
            Some(text) => {
                let re = cached_preprocess_regex(pattern, flags)?;
                let mut replaced = String::with_capacity(text.len());
                let mut last = 0;
                for caps in re.captures_iter(text) {
                    let whole = caps.get(0).expect("group 0 always participates");
                    replaced.push_str(&text[last..whole.start()]);
                    let mut groups = Vec::with_capacity(caps.len());
                    for index in 0..caps.len() {
                        groups.push(
                            caps.get(index)
                                .map(|m| Value::String(m.as_str().to_string()))
                                .unwrap_or(Value::Null),
                        );
                    }
                    ctx.set(var, Value::Array(groups))?;
                    replaced.push_str(&val_to_string(&eval_expr(ctx, body)?));
                    last = whole.end();
                }
                replaced.push_str(&text[last..]);
                validate_string_size(&replaced)?;
                Value::String(replaced)
            }
            None => val,
        },
        Method::Field(field) => val.get(field.as_str()).cloned().unwrap_or(Value::Null),
        Method::Bracket(key) => {
            let key_val = match key {
                BracketKey::Str(parts) => Value::String(ctx.resolve_str_parts(parts)?),
                BracketKey::Expr(expr) => eval_expr(ctx, expr)?,
                BracketKey::Index(index) => Value::Number((*index).into()),
            };
            match key_val {
                Value::String(key) => val.get(&key).cloned().unwrap_or(Value::Null),
                Value::Number(number) => {
                    let index = number.as_u64().unwrap_or(0) as usize;
                    val.get(index).cloned().unwrap_or(Value::Null)
                }
                _ => Value::Null,
            }
        }
        Method::IsArray => Value::Bool(matches!(val, Value::Array(_))),
        Method::Empty => match &val {
            Value::String(s) => Value::Bool(s.is_empty()),
            Value::Array(a) => Value::Bool(a.is_empty()),
            Value::Null => Value::Bool(true),
            _ => Value::Bool(false),
        },
        Method::Size => match &val {
            Value::String(s) => Value::Number(s.chars().count().into()),
            Value::Array(a) => Value::Number(a.len().into()),
            Value::Object(map) => Value::Number(map.len().into()),
            Value::Null => Value::Number(0.into()),
            _ => val,
        },
        Method::First => match val.as_array() {
            Some(arr) => arr.first().cloned().unwrap_or(Value::Null),
            None => val,
        },
        Method::Last => match val.as_array() {
            Some(arr) => arr.last().cloned().unwrap_or(Value::Null),
            None => val,
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

pub(super) fn run_stmts(
    stmts: &[Stmt],
    source: &mut String,
    jobs: &PreprocessJobs,
) -> super::PreprocessRun {
    match run_stmts_checked(stmts, source, jobs) {
        Ok(run) => run,
        Err(err) => panic!("{err}"),
    }
}

fn run_stmts_checked(
    stmts: &[Stmt],
    source: &mut String,
    jobs: &PreprocessJobs,
) -> PreprocessResult<super::PreprocessRun> {
    let mut ctx = Ctx::new(jobs);
    eval_stmts(&mut ctx, stmts, source)?;
    Ok(super::PreprocessRun {
        requested: ctx.requested,
    })
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

/// Compile a DSL regex literal, caching by `(pattern, flags)`. `s` enables
/// dot-matches-newline, `m` multi-line anchors, `i` case-insensitive matching.
fn cached_preprocess_regex(pattern: &str, flags: &str) -> PreprocessResult<regex::Regex> {
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
    builder.case_insensitive(flags.contains('i'));
    let regex = builder
        .build()
        .map_err(|err| format!("preprocess: invalid regex: {err}"))?;
    guard.insert(key, regex.clone());
    Ok(regex)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn run(stmts: &[Stmt], source: &mut String) -> Result<(), String> {
        run_stmts_checked(stmts, source, &PreprocessJobs::new()).map(|_| ())
    }

    /// Compile DSL source, run it over `source`, and return the result.
    fn run_program(program: &str, source: &str) -> String {
        let stmts = crate::downloader::preprocess::parser::parse_preprocess(program)
            .unwrap_or_else(|err| panic!("should parse: {err}\n{program}"));
        let mut text = source.to_string();
        run_stmts_checked(&stmts, &mut text, &PreprocessJobs::new())
            .unwrap_or_else(|err| panic!("{program}: {err}"));
        text
    }

    #[test]
    fn list_helpers_size_first_last_and_index() {
        let out = run_program(
            "let a = [10, 20, 30]\n\
             emit \"size=${a.size}\"\n\
             emit \"first=${a.first}\"\n\
             emit \"last=${a.last}\"\n\
             emit \"index0=${a[0]}\"\n\
             emit \"index2=${a[2]}\"\n\
             emit \"oob=${a[9]}\"\n\
             insert_at_match\n",
            "",
        );
        assert_eq!(
            out,
            "size=3\nfirst=10\nlast=30\nindex0=10\nindex2=30\noob="
        );
    }

    #[test]
    fn comparisons_keep_their_operator() {
        // `==` / `!=` が条件から落ちると常に真になるため、両方向で固定する。
        let out = run_program(
            "let a = [1, 2, 3]\n\
             if a.size == 30\n\
             emit \"BIG\"\n\
             else\n\
             emit \"SMALL\"\n\
             end\n\
             if a.size == 3\n\
             emit \"THREE\"\n\
             end\n\
             if a.size != 3\n\
             emit \"NOT-THREE\"\n\
             end\n\
             insert_at_match\n",
            "",
        );
        assert_eq!(out, "SMALL\nTHREE");
    }

    #[test]
    fn chain_steps_keep_their_written_order() {
        // メソッドが後ろに寄せられると `items.first.name` が解決できなくなる。
        let out = run_program(
            "let json = extract_json(/(.+)/s)\n\
             let items = json.items\n\
             emit \"first=${items.first.name}\"\n\
             emit \"last=${items.last.name}\"\n\
             insert_at_match\n",
            r#"{"items":[{"name":"one"},{"name":"two"}]}"#,
        );
        assert!(out.starts_with("first=one\nlast=two"), "got: {out}");
    }

    #[test]
    fn bare_method_names_do_not_swallow_longer_field_names() {
        let out = run_program(
            "let json = extract_json(/(.+)/s)\n\
             emit \"at::${json.lastEpisodePublishedAt}\"\n\
             insert_at_match\n",
            r#"{"lastEpisodePublishedAt":"2021-01-12T16:13:02Z"}"#,
        );
        assert!(out.starts_with("at::2021-01-12T16:13:02Z"), "got: {out}");
    }

    #[test]
    fn regex_gsub_expands_capture_groups() {
        let out = run_program(
            "let text = \"a [[rb:漢字>かんじ]] b\"\n\
             let text = text.gsub(/\\[\\[rb:(.*?)>(.*?)\\]\\]/, \"<ruby>$1<rp>(</rp><rt>$2</rt><rp>)</rp></ruby>\")\n\
             let text = text.gsub(/a/, \"A\")\n\
             emit text\n\
             insert_at_match\n",
            "",
        );
        assert_eq!(
            out,
            "A <ruby>漢字<rp>(</rp><rt>かんじ</rt><rp>)</rp></ruby> b"
        );
    }

    #[test]
    fn gsub_block_receives_the_match_and_replaces_each_occurrence() {
        let out = run_program(
            "let text = \"a [pixivimage:11-2] b [pixivimage:22-1]\"\n\
             let text = text.gsub(/\\[pixivimage:(\\d+)-(\\d+)\\]/) { |m| \"img:${m[1]}:${m[2]}\" }\n\
             emit text\n\
             insert_at_match\n",
            "",
        );
        assert_eq!(out, "a img:11:2 b img:22:1");
    }

    #[test]
    fn arithmetic_coerces_numeric_captures() {
        let out = run_program(
            "let a = [\"zero\", \"one\", \"two\"]\n\
             let text = \"x-2\"\n\
             let text = text.gsub(/x-(\\d+)/) { |m| a[m[1] - 1] }\n\
             emit text\n\
             emit \"sum=${1 + 2}\"\n\
             emit \"bad=${a - 1}\"\n\
             insert_at_match\n",
            "",
        );
        assert_eq!(out, "one\nsum=3\nbad=");
    }

    #[test]
    fn fetch_json_registers_a_request_and_reads_settled_results() {
        let jobs = PreprocessJobs::new();
        let program = "let pages = fetch_json(\"https://example.com/illust/11\")\n\
                       emit \"url=${pages.body[0].urls.original}\"\n\
                       insert_at_match\n";
        let stmts = crate::downloader::preprocess::parser::parse_preprocess(program).unwrap();
        let mut source = String::new();
        let run = run_stmts_checked(&stmts, &mut source, &jobs).unwrap();
        assert_eq!(run.requested, vec!["https://example.com/illust/11"]);
        assert_eq!(source, "url=");

        // The same program with the job settled sees the value.
        let mut settled = PreprocessJobs::new();
        settled.insert(
            "https://example.com/illust/11".to_string(),
            serde_json::json!({"body": [{"urls": {"original": "https://i.example/11.jpg"}}]}),
        );
        let mut source = String::new();
        let run = run_stmts_checked(&stmts, &mut source, &settled).unwrap();
        assert!(run.requested.is_empty(), "settled jobs are not re-requested");
        assert_eq!(source, "url=https://i.example/11.jpg");
    }

    #[test]
    fn request_rejects_urls_that_are_not_fetchable() {
        let stmts = crate::downloader::preprocess::parser::parse_preprocess(
            "let a = request(\"file:///etc/passwd\")\n\
             let b = request(\"http://127.0.0.1/x\")\n\
             emit \"a=${a} b=${b}\"\n\
             insert_at_match\n",
        )
        .unwrap();
        let mut source = String::new();
        let run = run_stmts_checked(&stmts, &mut source, &PreprocessJobs::new()).unwrap();
        assert!(run.requested.is_empty(), "{:?}", run.requested);
    }

    #[test]
    fn interpolated_strings_allow_escaped_quotes() {
        let out = run_program(
            "let name = \"pixiv\"\n\
             emit \"site=\\\"${name}\\\"\"\n\
             insert_at_match\n",
            "",
        );
        assert_eq!(out, "site=\"pixiv\"");
    }

    #[test]
    fn basic_string_interpolation_and_chain_access() {
        let stmts = vec![
            Stmt::Let {
                var: "json".to_string(),
                expr: Expr::ExtractJson(
                    r#"<script[^>]*type="application/json"[^>]*>(.+?)</script>"#.to_string(),
                    "s".to_string(),
                ),
            },
            Stmt::Let {
                var: "title".to_string(),
                expr: Expr::Chain {
                    base: Accessor {
                        base: "json".to_string(),
                        path: vec![AccessPart::Dot("title".to_string())],
                    },
                    methods: vec![],
                },
            },
            Stmt::Emit(Expr::String(vec![
                StrPart::Lit("title::".to_string()),
                StrPart::Interp(Expr::Access(Accessor {
                    base: "title".to_string(),
                    path: vec![],
                })),
            ])),
            Stmt::InsertAtMatch,
        ];
        let mut source =
            r#"before<script id="x" type="application/json">{"title":"test"}</script>after"#
                .to_string();
        run(&stmts, &mut source).unwrap();
        assert!(source.contains("title::test"));
    }

    #[test]
    fn guard_stops_reprocessing_when_magic_word_present() {
        let stmts = vec![
            Stmt::Guard("MagicWord".to_string()),
            Stmt::Emit(Expr::String(vec![StrPart::Lit("should_not_appear".to_string())])),
            Stmt::InsertAtMatch,
        ];
        let mut source = "MagicWord is here".to_string();
        run(&stmts, &mut source).unwrap();
        assert!(!source.contains("should_not_appear"));
    }

    #[test]
    fn guard_allows_processing_when_magic_word_absent() {
        let stmts = vec![
            Stmt::Guard("MagicWord".to_string()),
            Stmt::Emit(Expr::String(vec![StrPart::Lit("processed".to_string())])),
            Stmt::InsertAtMatch,
        ];
        let mut source = "clean source".to_string();
        run(&stmts, &mut source).unwrap();
        assert!(source.contains("processed"));
    }

    #[test]
    fn if_else_branches_evaluate_correctly() {
        let stmts = vec![
            Stmt::Let {
                var: "json".to_string(),
                expr: Expr::ExtractJson(
                    r#"<script[^>]*type="application/json"[^>]*>(.+?)</script>"#.to_string(),
                    "s".to_string(),
                ),
            },
            Stmt::If {
                cond: Expr::And(
                    Box::new(Expr::Chain {
                        base: Accessor {
                            base: "json".to_string(),
                            path: vec![AccessPart::Dot("tocV2".to_string())],
                        },
                        methods: vec![],
                    }),
                    Box::new(Expr::Not(Box::new(Expr::Chain {
                        base: Accessor {
                            base: "json".to_string(),
                            path: vec![AccessPart::Dot("tocV2".to_string())],
                        },
                        methods: vec![Method::Empty],
                    }))),
                ),
                body: vec![Stmt::Emit(Expr::String(vec![StrPart::Lit(
                    "used_v2".to_string(),
                )]))],
                else_body: Some(vec![Stmt::Emit(Expr::String(vec![StrPart::Lit(
                    "used_fallback".to_string(),
                )]))]),
            },
            Stmt::InsertAtMatch,
        ];
        let mut source =
            r#"before<script id="x" type="application/json">{"tocV2":[]}</script>after"#
                .to_string();
        run(&stmts, &mut source).unwrap();
        assert!(source.contains("used_fallback"));
        assert!(!source.contains("used_v2"));
    }

    #[test]
    fn bracket_access_with_interpolated_key() {
        let stmts = vec![
            Stmt::Let {
                var: "json".to_string(),
                expr: Expr::ExtractJson(
                    r#"<script id="x" type="application/json">.*?</script>"#.to_string(),
                    "s".to_string(),
                ),
            },
            Stmt::Let {
                var: "key".to_string(),
                expr: Expr::Access(Accessor {
                    base: "json".to_string(),
                    path: vec![AccessPart::Dot("targetKey".to_string())],
                }),
            },
            Stmt::Let {
                var: "result".to_string(),
                expr: Expr::Chain {
                    base: Accessor {
                        base: "json".to_string(),
                        path: vec![AccessPart::Dot("items".to_string())],
                    },
                    methods: vec![],
                },
            },
            Stmt::Let {
                var: "result".to_string(),
                expr: Expr::Chain {
                    base: Accessor {
                        base: "result".to_string(),
                        path: vec![AccessPart::Bracket(BracketKey::Expr(Expr::Access(Accessor {
                            base: "key".to_string(),
                            path: vec![],
                        })))],
                    },
                    methods: vec![],
                },
            },
            Stmt::Emit(Expr::Chain {
                base: Accessor {
                    base: "result".to_string(),
                    path: vec![],
                },
                methods: vec![],
            }),
            Stmt::InsertAtMatch,
        ];
        let mut source =
            r#"<script id="x" type="application/json">{"targetKey":"K1","items":{"K1":"found"}}</script>"#.to_string();
        run(&stmts, &mut source).unwrap();
        assert!(source.contains("found"));
    }

    #[test]
    fn flat_map_flattens_single_level() {
        let stmts = vec![
            Stmt::Let {
                var: "arr".to_string(),
                expr: Expr::ExtractJson(
                    r#"<script id="x" type="application/json">.*?</script>"#.to_string(),
                    "s".to_string(),
                ),
            },
            Stmt::Let {
                var: "flat".to_string(),
                expr: Expr::ValueChain {
                    base: Box::new(Expr::Chain {
                        base: Accessor {
                            base: "arr".to_string(),
                            path: vec![AccessPart::Dot("items".to_string())],
                        },
                        methods: vec![],
                    }),
                    methods: vec![Method::FlatMap {
                        var: "x".to_string(),
                        body: Box::new(Expr::Array(vec![
                            Expr::Access(Accessor {
                                base: "x".to_string(),
                                path: vec![AccessPart::Dot("a".to_string())],
                            }),
                            Expr::Access(Accessor {
                                base: "x".to_string(),
                                path: vec![AccessPart::Dot("b".to_string())],
                            }),
                        ])),
                    }],
                },
            },
            Stmt::For {
                var: "item".to_string(),
                iter: Expr::Access(Accessor {
                    base: "flat".to_string(),
                    path: vec![],
                }),
                body: vec![Stmt::Emit(Expr::Chain {
                    base: Accessor {
                        base: "item".to_string(),
                        path: vec![],
                    },
                    methods: vec![],
                })],
            },
            Stmt::InsertAtMatch,
        ];
        let mut source =
            r#"<script id="x" type="application/json">{"items":[{"a":1,"b":[2,3]}]}</script>"#.to_string();
        run(&stmts, &mut source).unwrap();
        assert!(source.contains("1"));
        assert!(source.contains("2"));
        assert!(source.contains("3"));
    }

    #[test]
    fn map_then_join_chain_on_array_value() {
        let stmts = vec![
            Stmt::Let {
                var: "tags".to_string(),
                expr: Expr::ExtractJson(
                    r#"<script[^>]*type="application/json"[^>]*>(.+?)</script>"#.to_string(),
                    "s".to_string(),
                ),
            },
            Stmt::Let {
                var: "result".to_string(),
                expr: Expr::ValueChain {
                    base: Box::new(Expr::Chain {
                        base: Accessor {
                            base: "tags".to_string(),
                            path: vec![AccessPart::Dot("labels".to_string())],
                        },
                        methods: vec![],
                    }),
                    methods: vec![
                        Method::Map {
                            var: "tag".to_string(),
                            body: Box::new(Expr::String(vec![
                                StrPart::Lit("tag::".to_string()),
                                StrPart::Interp(Expr::Access(Accessor {
                                    base: "tag".to_string(),
                                    path: vec![],
                                })),
                            ])),
                        },
                        Method::Join(vec![StrPart::Lit("\n".to_string())]),
                    ],
                },
            },
            Stmt::Emit(Expr::Chain {
                base: Accessor {
                    base: "result".to_string(),
                    path: vec![],
                },
                methods: vec![],
            }),
            Stmt::InsertAtMatch,
        ];
        let mut source =
            r#"before<script id="x" type="application/json">{"labels":["a","b"]}</script>after"#
                .to_string();
        run(&stmts, &mut source).unwrap();
        assert!(source.contains("tag::a"));
        assert!(source.contains("tag::b"));
    }

    #[test]
    fn set_mutates_object_field() {
        let stmts = vec![
            Stmt::Let {
                var: "json".to_string(),
                expr: Expr::ExtractJson(
                    r#"<script id="x" type="application/json">.*?</script>"#.to_string(),
                    "s".to_string(),
                ),
            },
            Stmt::Let {
                var: "work".to_string(),
                expr: Expr::Chain {
                    base: Accessor {
                        base: "json".to_string(),
                        path: vec![AccessPart::Dot("work".to_string())],
                    },
                    methods: vec![],
                },
            },
            Stmt::Set {
                target: LValue::Hash {
                    base: "work".to_string(),
                    keys: vec![vec![StrPart::Lit("title".to_string())]],
                },
                expr: Expr::String(vec![StrPart::Lit("new_title".to_string())]),
            },
            Stmt::Emit(Expr::String(vec![
                StrPart::Lit("title::".to_string()),
                StrPart::Interp(Expr::Access(Accessor {
                    base: "work".to_string(),
                    path: vec![AccessPart::Dot("title".to_string())],
                })),
            ])),
            Stmt::InsertAtMatch,
        ];
        let mut source =
            r#"<script id="x" type="application/json">{"work":{"title":"old"}}</script>"#.to_string();
        run(&stmts, &mut source).unwrap();
        assert!(source.contains("title::new_title"));
    }

    #[test]
    fn or_operator_falls_back_to_default() {
        let stmts = vec![
            Stmt::Let {
                var: "json".to_string(),
                expr: Expr::ExtractJson(
                    r#"<script id="x" type="application/json">.*?</script>"#.to_string(),
                    "s".to_string(),
                ),
            },
            Stmt::Let {
                var: "toc".to_string(),
                expr: Expr::Or(
                    Box::new(Expr::Chain {
                        base: Accessor {
                            base: "json".to_string(),
                            path: vec![AccessPart::Dot("missing".to_string())],
                        },
                        methods: vec![],
                    }),
                    Box::new(Expr::Array(vec![])),
                ),
            },
            Stmt::Emit(Expr::String(vec![StrPart::Lit("fallback_ok".to_string())])),
            Stmt::InsertAtMatch,
        ];
        let mut source = r#"<script id="x" type="application/json">{}</script>"#.to_string();
        run(&stmts, &mut source).unwrap();
        assert!(source.contains("fallback_ok"));
    }

    #[test]
    fn extract_json_not_found_returns_null_gracefully() {
        let stmts = vec![
            Stmt::Let {
                var: "json".to_string(),
                expr: Expr::ExtractJson(
                    r#"<script id="x" type="application/json">(.+?)</script>"#.to_string(),
                    "s".to_string(),
                ),
            },
            Stmt::Let {
                var: "title".to_string(),
                expr: Expr::Chain {
                    base: Accessor {
                        base: "json".to_string(),
                        path: vec![AccessPart::Dot("title".to_string())],
                    },
                    methods: vec![],
                },
            },
            Stmt::Emit(Expr::String(vec![StrPart::Lit("title::".to_string())])),
            Stmt::InsertAtMatch,
        ];
        let mut source = "no json here".to_string();
        run(&stmts, &mut source).unwrap();
        assert!(source.contains("title::"));
    }

    #[test]
    fn and_operator_short_circuits_on_falsy() {
        let stmts = vec![
            Stmt::Let {
                var: "json".to_string(),
                expr: Expr::ExtractJson(
                    r#"<script id="x" type="application/json">.*?</script>"#.to_string(),
                    "s".to_string(),
                ),
            },
            Stmt::If {
                cond: Expr::And(
                    Box::new(Expr::Chain {
                        base: Accessor {
                            base: "json".to_string(),
                            path: vec![AccessPart::Dot("empty_arr".to_string())],
                        },
                        methods: vec![Method::Empty],
                    }),
                    Box::new(Expr::Chain {
                        base: Accessor {
                            base: "json".to_string(),
                            path: vec![AccessPart::Dot("has_toc".to_string())],
                        },
                        methods: vec![],
                    }),
                ),
                body: vec![Stmt::Emit(Expr::String(vec![StrPart::Lit(
                    "in_then".to_string(),
                )]))],
                else_body: Some(vec![Stmt::Emit(Expr::String(vec![StrPart::Lit(
                    "in_else".to_string(),
                )]))]),
            },
            Stmt::InsertAtMatch,
        ];
        let mut source =
            r#"<script id="x" type="application/json">{"empty_arr":[],"has_toc":true}</script>"#.to_string();
        run(&stmts, &mut source).unwrap();
        assert!(source.contains("in_else"));
        assert!(!source.contains("in_then"));
    }

    #[test]
    fn for_loop_iterates_and_emits_each_item() {
        let stmts = vec![
            Stmt::Let {
                var: "json".to_string(),
                expr: Expr::ExtractJson(
                    r#"<script id="x" type="application/json">.*?</script>"#.to_string(),
                    "s".to_string(),
                ),
            },
            Stmt::For {
                var: "item".to_string(),
                iter: Expr::Chain {
                    base: Accessor {
                        base: "json".to_string(),
                        path: vec![AccessPart::Dot("items".to_string())],
                    },
                    methods: vec![],
                },
                body: vec![Stmt::Emit(Expr::String(vec![StrPart::Interp(Expr::Access(Accessor {
                    base: "item".to_string(),
                    path: vec![],
                }))]))],
            },
            Stmt::InsertAtMatch,
        ];
        let mut source =
            r#"<script id="x" type="application/json">{"items":["a","b","c"]}</script>"#.to_string();
        run(&stmts, &mut source).unwrap();
        assert!(source.contains("a"));
        assert!(source.contains("b"));
        assert!(source.contains("c"));
    }

    #[test]
    fn preprocess_step_budget_is_enforced() {
        let stmts = vec![
            Stmt::Emit(Expr::String(vec![StrPart::Lit("x".to_string())]));
            (PREPROCESS_STEP_BUDGET / 2) + 1
        ];
        let mut source = String::new();
        let err = run_stmts_checked(&stmts, &mut source, &PreprocessJobs::new()).unwrap_err();
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
        let err = run_stmts_checked(&stmts, &mut source, &PreprocessJobs::new()).unwrap_err();
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
        let err = run_stmts_checked(&stmts, &mut source, &PreprocessJobs::new()).unwrap_err();
        assert_eq!(
            err,
            format!(
                "preprocess: array size limit exceeded ({PREPROCESS_MAX_ARRAY_ITEMS} items)"
            )
        );
    }
}
