mod ast;
mod interpreter;
mod parser;

pub use ast::*;

use super::preprocess::interpreter::run_stmts;
use super::preprocess::parser::parse_preprocess;

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
        run_stmts(&self.stmts, source);
    }
}

pub fn run_preprocess(pipeline: &PreprocessPipeline, source: &mut String) {
    pipeline.execute(source);
}
