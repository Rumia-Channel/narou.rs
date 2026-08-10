mod composition;

use serde::{Deserialize, Serialize};
use worker::*;
pub const WORKER_JOB_ENVELOPE_VERSION: u32 = 1;

#[derive(Debug, Clone, Deserialize, Serialize)]
pub struct WorkerJobEnvelope {
    pub version: u32,
    pub job: QueueJob,
}

#[derive(Debug, Clone, Deserialize, Serialize)]
pub struct QueueJob {
    pub kind: String,
    pub target: Option<String>,
}

#[event(fetch)]
pub async fn main(req: Request, _env: Env, _ctx: Context) -> Result<Response> {
    let _services = composition::build_services();
    let path = req.path();

    match path.as_str() {
        "/" | "/health" => Response::ok("narou.rs worker is ready"),
        _ => Response::error("Not Found", 404),
    }
}

#[event(scheduled)]
pub async fn scheduled(_event: ScheduledEvent, _env: Env, _ctx: ScheduleContext) {}

#[event(queue)]
pub async fn queue(
    message_batch: MessageBatch<WorkerJobEnvelope>,
    _env: Env,
    _ctx: Context,
) -> Result<()> {
    for message in message_batch.messages()? {
        let envelope = message.body();
        if envelope.version != WORKER_JOB_ENVELOPE_VERSION {
            console_log!(
                "unsupported narou job envelope version: {}",
                envelope.version
            );
            message.retry();
            continue;
        }
        console_log!("received narou job: {:?}", envelope.job);
        message.ack();
    }
    Ok(())
}
