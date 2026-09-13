use std::sync::Arc;

use narou_rs::application::{
    AutoUpdateSchedule, JobKind, JobRequest, JobService, JobTarget, Schedule, SchedulerService,
};
use narou_rs::platform::SystemClock;

#[test]
fn public_application_services_plan_targetless_auto_update() {
    let result = JobService.plan(&JobRequest {
        kind: JobKind::AutoUpdate,
        targets: Vec::new(),
        options: Vec::new(),
    });

    assert_eq!(result.invalid, Vec::<String>::new());
    assert_eq!(result.plans.len(), 1);
    assert_eq!(result.plans[0].target, JobTarget::All);
}

#[test]
fn scheduler_policy_is_composed_without_web_or_native_types() {
    let scheduler = SchedulerService::new(Arc::new(SystemClock));
    let policy = scheduler.policy(true, "0800,1800", None, vec!["modified".to_string()]);

    assert!(policy.enabled);
    assert!(matches!(policy.schedule, AutoUpdateSchedule::Daily(Schedule { times }) if times == vec![(8, 0), (18, 0)]));
}
