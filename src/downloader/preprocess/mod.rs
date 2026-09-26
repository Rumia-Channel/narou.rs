mod ast;
mod interpreter;
mod jobs;
mod parser;

pub use ast::*;
pub use jobs::{MAX_PREPROCESS_JOBS, PreprocessJobs};

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

    /// Run the definition over `source`, seeing the results of `jobs`.
    /// Returns the URLs this run asked for; the caller executes them and runs
    /// the definition again so the next pass can see their bodies.
    pub fn execute(&self, source: &mut String, jobs: &PreprocessJobs, url: &str) -> PreprocessRun {
        run_stmts(&self.stmts, source, jobs, url)
    }
}

/// What one `preprocess:` run produced besides the rewritten source.
#[derive(Debug, Default, Clone)]
pub struct PreprocessRun {
    /// URLs the definition asked for, in the order they were requested.
    pub requested: Vec<String>,
}

pub fn run_preprocess(
    pipeline: &PreprocessPipeline,
    source: &mut String,
    jobs: &PreprocessJobs,
    url: &str,
) -> PreprocessRun {
    pipeline.execute(source, jobs, url)
}
