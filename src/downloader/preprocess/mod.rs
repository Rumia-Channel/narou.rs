mod ast;
mod interpreter;
mod jobs;
mod parser;

pub use ast::*;
pub use jobs::{MAX_PREPROCESS_JOBS, PreprocessError, PreprocessJobs, PreprocessRequest};

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
    /// Returns requests for the caller to execute before rerunning the
    /// definition. A failed run leaves the source unchanged.
    pub fn execute(&self, source: &mut String, jobs: &PreprocessJobs, url: &str) -> Result<PreprocessRun, PreprocessError> {
        run_stmts(&self.stmts, source, jobs, url)
    }
}

/// What one `preprocess:` run produced besides the rewritten source.
#[derive(Debug, Default, Clone)]
pub struct PreprocessRun {
    /// Requests this run asked for, in order.
    pub requested: Vec<PreprocessRequest>,
}

pub fn run_preprocess(
    pipeline: &PreprocessPipeline,
    source: &mut String,
    jobs: &PreprocessJobs,
    url: &str,
) -> Result<PreprocessRun, PreprocessError> {
    pipeline.execute(source, jobs, url)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn form_requests_cannot_cross_origins_or_supply_credentials() {
        let original = "<p>元の本文</p>";
        for target in [
            "https://other.example/body",
            "http://reader.example/body",
            "https://reader.example:8443/body",
            "https://user:password@reader.example/body",
        ] {
            let pipeline = PreprocessPipeline::compile(&format!(
                "let body = post_form(\"{target}\", [[\"episode\", 1]])"
            )).unwrap();
            let mut source = original.to_owned();
            assert!(pipeline.execute(&mut source, &PreprocessJobs::new(), "https://reader.example/episode/1").is_err());
            assert_eq!(source, original);
        }
        for header in ["Cookie", "Authorization", "Host", "Content-Length", "Content-Type"] {
            let pipeline = PreprocessPipeline::compile(&format!(
                "let body = post_form(\"https://reader.example/body\", [[\"episode\", 1]], [[\"{header}\", \"injected\"]])"
            )).unwrap();
            let mut source = original.to_owned();
            assert!(pipeline.execute(&mut source, &PreprocessJobs::new(), "https://reader.example/episode/1").is_err());
            assert_eq!(source, original);
        }
    }

    #[test]
    fn a_definition_failure_does_not_publish_partially_rewritten_source() {
        let pipeline = PreprocessPipeline::compile(
            "emit \"誤って保存される本文\"\nfail \"本文を取得できません\"\ninsert_at_match"
        ).unwrap();
        let original = "<p>元の本文</p>";
        let mut source = original.to_owned();
        assert!(pipeline.execute(&mut source, &PreprocessJobs::new(), "https://reader.example/episode/1").is_err());
        assert_eq!(source, original);
    }
}
