use pest::Parser;
use pest_derive::Parser;

use super::ast::*;

#[derive(Parser)]
#[grammar = "downloader/preprocess.pest"]
struct PreprocessParser;

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
            keys: vec![dot_keys.into_iter().map(StrPart::Lit).collect()],
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
