//! Runtime-only invocation ownership. Never serialize an execution token into
//! transfer metadata, a job checkpoint, or a browser response.

#[doc(hidden)]
#[derive(Clone, PartialEq, Eq)]
pub struct JobExecutionOwner {
    job_id: String,
    execution_token: String,
}

impl JobExecutionOwner {
    pub fn new(job_id: impl Into<String>, execution_token: impl Into<String>) -> Self {
        Self { job_id: job_id.into(), execution_token: execution_token.into() }
    }

    pub fn job_id(&self) -> &str {
        &self.job_id
    }

    pub fn execution_token(&self) -> &str {
        &self.execution_token
    }
}

impl std::fmt::Debug for JobExecutionOwner {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter.debug_struct("JobExecutionOwner")
            .field("job_id", &self.job_id)
            .field("execution_token", &"<redacted>")
            .finish()
    }
}
