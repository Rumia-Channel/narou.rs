//! Scheduler service: pure scheduling policy over an injected [`Clock`].
//!
//! Owns the decision logic behind the auto-update scheduler: parsing the
//! `update.auto-schedule` setting (comma-separated `HHMM` times), computing
//! the next run time, and deciding whether a run is due. It contains no
//! sleeping, spawning, or queueing — the web layer takes the decisions and
//! performs the actual work.

use std::sync::Arc;
use std::time::Duration;

use chrono::{DateTime, NaiveDate, NaiveTime, Utc};

use crate::platform::clock::Clock;

/// A parsed schedule: one or more daily run times (hour, minute).
///
/// Times are sorted ascending and deduplicated, so the next-run computation
/// is deterministic.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Schedule {
    /// Daily run times, sorted ascending, deduplicated.
    pub times: Vec<(u32, u32)>,
}

impl Schedule {
    /// Parse a schedule string: comma-separated `HHMM` times (e.g.
    /// `"0800,1200,1800"`). Invalid entries are dropped; an empty result
    /// means the schedule is disabled.
    pub fn parse(schedule: &str) -> Self {
        let mut times: Vec<(u32, u32)> = schedule
            .split(',')
            .filter_map(|value| {
                let trimmed = value.trim();
                if trimmed.len() != 4 || !trimmed.chars().all(|ch| ch.is_ascii_digit()) {
                    return None;
                }
                let hour = trimmed[0..2].parse::<u32>().ok()?;
                let minute = trimmed[2..4].parse::<u32>().ok()?;
                (hour < 24 && minute < 60).then_some((hour, minute))
            })
            .collect();
        times.sort_unstable();
        times.dedup();
        Self { times }
    }

    /// True when the schedule has at least one run time.
    pub fn is_enabled(&self) -> bool {
        !self.times.is_empty()
    }

    /// The next run time strictly after `after`, or `None` when the schedule
    /// is empty.
    pub fn next_run_after(&self, after: DateTime<Utc>) -> Option<DateTime<Utc>> {
        let mut times = self.times.iter();
        for &(hour, minute) in times.by_ref() {
            if let Some(candidate) = daily_time(after, hour, minute)
                && candidate > after
            {
                return Some(candidate);
            }
        }
        let (hour, minute) = *self.times.first()?;
        let tomorrow = after.date_naive().succ_opt()?;
        daily_time_on(tomorrow, hour, minute)
    }

    /// The next run time at or after `now` (used for "due now" checks).
    pub fn next_run(&self, now: DateTime<Utc>) -> Option<DateTime<Utc>> {
        self.next_run_after(now)
    }
}

/// The auto-update policy: whether it is enabled, which targets it covers,
/// and how often it runs.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct AutoUpdatePolicy {
    pub enabled: bool,
    /// Target selection: `All` updates every non-frozen novel; `Tag` updates
    /// novels carrying any of the given tags.
    pub targets: AutoUpdateTargets,
    /// How often to run: at fixed daily times, or every `interval`.
    pub schedule: AutoUpdateSchedule,
}

/// Which novels an auto-update run covers.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum AutoUpdateTargets {
    /// Every non-frozen novel.
    All,
    /// Novels carrying at least one of these tags.
    Tag(Vec<String>),
}

/// How often the auto-update runs.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum AutoUpdateSchedule {
    /// At fixed daily times (parsed from `update.auto-schedule`).
    Daily(Schedule),
    /// Every `interval` (parsed from `update.interval`).
    Interval(Duration),
}

/// The decision for one check: whether to run now, and when the next run is.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ScheduleDecision {
    /// True when a run is due at (or was missed before) `now`.
    pub run_now: bool,
    /// The next scheduled run time after `now`, when known.
    pub next_run: Option<DateTime<Utc>>,
    /// The last run time the decision was based on, when provided.
    pub last_run: Option<DateTime<Utc>>,
}

/// Concrete scheduler service: pure policy over an injected [`Clock`].
pub struct SchedulerService {
    clock: Arc<dyn Clock>,
}

impl SchedulerService {
    /// Create the service with an injected clock.
    pub fn new(clock: Arc<dyn Clock>) -> Self {
        Self { clock }
    }

    /// Parse a schedule string into a [`Schedule`].
    pub fn parse_schedule(&self, schedule: &str) -> Schedule {
        Schedule::parse(schedule)
    }

    /// Build an [`AutoUpdatePolicy`] from the raw setting values.
    ///
    /// `schedule_string` is the `update.auto-schedule` value (comma-separated
    /// `HHMM` times); `interval_secs` is the `update.interval` value in
    /// seconds, used when no fixed times are configured. `tags` selects the
    /// `Tag` target mode; an empty tag list falls back to `All`.
    pub fn policy(
        &self,
        enabled: bool,
        schedule_string: &str,
        interval_secs: Option<f64>,
        tags: Vec<String>,
    ) -> AutoUpdatePolicy {
        let schedule = Schedule::parse(schedule_string);
        let schedule = if schedule.is_enabled() {
            AutoUpdateSchedule::Daily(schedule)
        } else {
            let secs = interval_secs
                .filter(|value| value.is_finite())
                .unwrap_or(0.0)
                .max(0.0);
            AutoUpdateSchedule::Interval(Duration::from_secs_f64(secs))
        };
        let targets = if tags.is_empty() {
            AutoUpdateTargets::All
        } else {
            AutoUpdateTargets::Tag(tags)
        };
        AutoUpdatePolicy {
            enabled,
            targets,
            schedule,
        }
    }

    /// Decide whether an auto-update run is due now.
    ///
    /// For daily schedules, a run is due when the next run time after
    /// `last_run` is at or before `now` (catch-up semantics). For interval
    /// schedules, a run is due when `now - last_run >= interval`. When
    /// `last_run` is `None`, a daily schedule runs at the next fixed time
    /// and an interval schedule runs immediately.
    pub fn decide(
        &self,
        policy: &AutoUpdatePolicy,
        last_run: Option<DateTime<Utc>>,
    ) -> ScheduleDecision {
        let now = self.clock.now_utc();
        if !policy.enabled {
            return ScheduleDecision {
                run_now: false,
                next_run: None,
                last_run,
            };
        }

        let (run_now, next_run) = match &policy.schedule {
            AutoUpdateSchedule::Daily(schedule) => {
                let next = schedule.next_run_after(now);
                let missed = last_run
                    .and_then(|last| schedule.next_run_after(last))
                    .is_some_and(|candidate| candidate <= now);
                (missed, next)
            }
            AutoUpdateSchedule::Interval(interval) => {
                let interval_delta = chrono::TimeDelta::from_std(*interval)
                    .unwrap_or_else(|_| chrono::TimeDelta::try_seconds(i64::MAX).unwrap());
                let due = last_run.is_none_or(|last| now - last >= interval_delta);
                let next = last_run.map(|last| last + *interval);
                (due, next)
            }        };

        ScheduleDecision {
            run_now,
            next_run,
            last_run,
        }
    }
}

/// Build a `DateTime<Utc>` for `(hour, minute)` on the same day as `after`.
fn daily_time(after: DateTime<Utc>, hour: u32, minute: u32) -> Option<DateTime<Utc>> {
    daily_time_on(after.date_naive(), hour, minute)
}

/// Build a `DateTime<Utc>` for `(hour, minute)` on `date`.
fn daily_time_on(date: NaiveDate, hour: u32, minute: u32) -> Option<DateTime<Utc>> {
    let time = NaiveTime::from_hms_opt(hour, minute, 0)?;
    date.and_time(time)
        .and_local_timezone(Utc)
        .single()
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::atomic::{AtomicI64, Ordering};

    #[derive(Debug, Default)]
    struct FakeClock(AtomicI64);

    impl Clock for FakeClock {
        fn now_utc(&self) -> DateTime<Utc> {
            DateTime::from_timestamp(self.0.load(Ordering::SeqCst), 0).unwrap()
        }
        fn advance(&self, duration: Duration) {
            self.0
                .fetch_add(duration.as_secs() as i64, Ordering::SeqCst);
        }
    }

    fn service_at(timestamp: i64) -> SchedulerService {
        SchedulerService::new(Arc::new(FakeClock(AtomicI64::new(timestamp))))
    }

    fn utc(y: i32, m: u32, d: u32, h: u32, min: u32) -> DateTime<Utc> {
        NaiveDate::from_ymd_opt(y, m, d)
            .unwrap()
            .and_hms_opt(h, min, 0)
            .unwrap()
            .and_local_timezone(Utc)
            .single()
            .unwrap()
    }

    #[test]
    fn schedule_parse_accepts_four_digit_times() {
        let schedule = Schedule::parse("0930, 2215");
        assert_eq!(schedule.times, vec![(9, 30), (22, 15)]);
        assert!(schedule.is_enabled());
    }

    #[test]
    fn schedule_parse_drops_invalid_and_deduplicates() {
        let schedule = Schedule::parse("9999,0800,0800,abc");
        assert_eq!(schedule.times, vec![(8, 0)]);
    }

    #[test]
    fn schedule_parse_empty_is_disabled() {
        let schedule = Schedule::parse("");
        assert!(!schedule.is_enabled());
        assert_eq!(schedule.next_run_after(utc(2026, 1, 1, 0, 0)), None);
    }

    #[test]
    fn next_run_after_returns_today_or_tomorrow() {
        let schedule = Schedule::parse("0930,2215");
        // Before both times today → the earlier one today.
        assert_eq!(
            schedule.next_run_after(utc(2026, 1, 1, 8, 0)),
            Some(utc(2026, 1, 1, 9, 30))
        );
        // Between the two → the later one today.
        assert_eq!(
            schedule.next_run_after(utc(2026, 1, 1, 10, 0)),
            Some(utc(2026, 1, 1, 22, 15))
        );
        // After both → the earliest one tomorrow.
        assert_eq!(
            schedule.next_run_after(utc(2026, 1, 1, 23, 0)),
            Some(utc(2026, 1, 2, 9, 30))
        );
    }

    #[test]
    fn daily_schedule_missed_run_is_due() {
        let policy = AutoUpdatePolicy {
            enabled: true,
            targets: AutoUpdateTargets::All,
            schedule: AutoUpdateSchedule::Daily(Schedule::parse("0800")),
        };
        // Last run was yesterday 08:00; now is after today's 08:00.
        let last_run = utc(2026, 1, 1, 8, 0);
        let now = utc(2026, 1, 2, 9, 0);
        let service = service_at(now.timestamp());
        let decision = service.decide(&policy, Some(last_run));
        assert!(decision.run_now);
        assert_eq!(decision.next_run, Some(utc(2026, 1, 3, 8, 0)));
    }

    #[test]
    fn daily_schedule_not_due_before_next_run() {
        let policy = AutoUpdatePolicy {
            enabled: true,
            targets: AutoUpdateTargets::All,
            schedule: AutoUpdateSchedule::Daily(Schedule::parse("0800")),
        };
        let last_run = utc(2026, 1, 1, 8, 0);
        let now = utc(2026, 1, 2, 7, 0);
        let service = service_at(now.timestamp());
        let decision = service.decide(&policy, Some(last_run));
        assert!(!decision.run_now);
        assert_eq!(decision.next_run, Some(utc(2026, 1, 2, 8, 0)));
    }

    #[test]
    fn interval_schedule_runs_when_interval_elapsed() {
        let policy = AutoUpdatePolicy {
            enabled: true,
            targets: AutoUpdateTargets::All,
            schedule: AutoUpdateSchedule::Interval(Duration::from_secs(3600)),
        };
        let last_run = utc(2026, 1, 1, 8, 0);
        let now = utc(2026, 1, 1, 9, 0);
        let service = service_at(now.timestamp());
        let decision = service.decide(&policy, Some(last_run));
        assert!(decision.run_now);
        assert_eq!(decision.next_run, Some(utc(2026, 1, 1, 9, 0)));
    }

    #[test]
    fn interval_schedule_not_due_before_interval() {
        let policy = AutoUpdatePolicy {
            enabled: true,
            targets: AutoUpdateTargets::All,
            schedule: AutoUpdateSchedule::Interval(Duration::from_secs(3600)),
        };
        let last_run = utc(2026, 1, 1, 8, 0);
        let now = utc(2026, 1, 1, 8, 30);
        let service = service_at(now.timestamp());
        let decision = service.decide(&policy, Some(last_run));
        assert!(!decision.run_now);
        assert_eq!(decision.next_run, Some(utc(2026, 1, 1, 9, 0)));
    }

    #[test]
    fn disabled_policy_never_runs() {
        let policy = AutoUpdatePolicy {
            enabled: false,
            targets: AutoUpdateTargets::All,
            schedule: AutoUpdateSchedule::Daily(Schedule::parse("0800")),
        };
        let service = service_at(utc(2026, 1, 1, 9, 0).timestamp());
        let decision = service.decide(&policy, None);
        assert!(!decision.run_now);
        assert_eq!(decision.next_run, None);
    }

    #[test]
    fn policy_builds_from_raw_settings() {
        let service = service_at(0);
        let policy = service.policy(true, "0800,1200", Some(0.7), vec!["modified".into()]);
        assert!(policy.enabled);
        assert_eq!(policy.targets, AutoUpdateTargets::Tag(vec!["modified".into()]));
        assert_eq!(
            policy.schedule,
            AutoUpdateSchedule::Daily(Schedule::parse("0800,1200"))
        );

        let policy = service.policy(true, "", Some(0.7), vec![]);
        assert_eq!(
            policy.schedule,
            AutoUpdateSchedule::Interval(Duration::from_secs_f64(0.7))
        );
        assert_eq!(policy.targets, AutoUpdateTargets::All);
    }
}
