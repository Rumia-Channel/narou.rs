//! Scheduler service: pure scheduling policy over an injected [`Clock`].
//!
//! Owns the decision logic behind the auto-update scheduler: parsing the
//! `update.auto-schedule` setting (comma-separated `HHMM` times), computing
//! the next run time, and deciding whether a run is due. It contains no
//! sleeping, spawning, or queueing — the web layer takes the decisions and
//! performs the actual work.

use std::sync::Arc;
#[cfg(test)]
use std::time::Duration;

use chrono::{DateTime, LocalResult, NaiveDate, NaiveTime, TimeZone, Utc};

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
    /// is empty. UTC wall clock without DST handling: the wall-clock zone and
    /// DST-gap modes are the service's decision inputs.
    pub fn next_run_after(&self, after: DateTime<Utc>) -> Option<DateTime<Utc>> {
        next_run_after(self, after, WallClockZone::Iana(chrono_tz::UTC), DstGap::Skip)
    }

    /// The next run time at or after `now` (used for "due now" checks).
    pub fn next_run(&self, now: DateTime<Utc>) -> Option<DateTime<Utc>> {
        self.next_run_after(now)
    }
}

/// The auto-update policy: whether it is enabled, which targets it covers,
/// and its daily schedule.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct AutoUpdatePolicy {
    pub enabled: bool,
    /// Target selection: `All` updates every non-frozen novel; `Tag` updates
    /// novels carrying any of the given tags.
    pub targets: AutoUpdateTargets,
    /// The daily times when it runs.
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

/// Daily schedule for auto-update runs.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum AutoUpdateSchedule {
    /// At fixed daily times (parsed from `update.auto-schedule`).
    Daily(Schedule),
}

/// The decision for one check: whether to run now, and when the next run is.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ScheduleDecision {
    /// True when a run is due at (or was missed before) `now`.
    pub run_now: bool,
    /// The missed run the decision was based on (the catch-up target), when
    /// any.
    pub missed_run: Option<DateTime<Utc>>,
    /// The next scheduled run time after `now`, when known.
    pub next_run: Option<DateTime<Utc>>,
    /// The last run time the decision was based on, when provided.
    pub last_run: Option<DateTime<Utc>>,
}

/// The wall-clock zone a daily `HHMM` schedule is resolved in.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum WallClockZone {
    /// `update.auto-schedule.timezone` (an IANA name).
    Iana(chrono_tz::Tz),
    /// The host's local zone (`update.auto-schedule.timezone` unset, native).
    Local,
}

/// How a daily time that DST removes from the wall clock is resolved.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum DstGap {
    /// Drop the swallowed time and fall through to the next candidate
    /// (shared default: the Worker keeps this).
    Skip,
    /// Roll the artificial time forward minute by minute until it exists
    /// (native; the step expresses how far each roll moves).
    RollForwardMinutes(u32),
}

/// How the due-ness check treats the recorded last run.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CatchUp {
    /// Run only when a recorded last run exists and the next scheduled time
    /// after it is already in the past (native; no catch-up when unset).
    SingleMissed,
    /// Run as soon as a scheduled time at or before `now` has passed, even
    /// when no last run is recorded (Worker).
    LatestPassed,
}

/// Parse the `update.auto-schedule` settings pair into a schedule.
///
/// `Ok(None)` = disabled or an empty value. `Err(message)` = malformed times
/// (the shared message both runtimes report).
pub fn parse_auto_update_schedule(
    enabled: bool,
    schedule_string: &str,
) -> std::result::Result<Option<Schedule>, String> {
    if !enabled || schedule_string.trim().is_empty() {
        return Ok(None);
    }
    let schedule = Schedule::parse(schedule_string);
    if schedule.is_enabled() {
        Ok(Some(schedule))
    } else {
        Err(crate::application::messages::jobs::auto_update_schedule_invalid(
            schedule_string,
        ))
    }
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

    /// Build an [`AutoUpdatePolicy`] from a parsed daily schedule and targets.
    ///
    /// An empty schedule is never due. An empty tag list falls back to `All`.
    pub fn policy(
        &self,
        enabled: bool,
        schedule: Schedule,
        tags: Vec<String>,
    ) -> AutoUpdatePolicy {
        let targets = if tags.is_empty() {
            AutoUpdateTargets::All
        } else {
            AutoUpdateTargets::Tag(tags)
        };
        AutoUpdatePolicy {
            enabled,
            targets,
            schedule: AutoUpdateSchedule::Daily(schedule),
        }
    }

    /// Decide whether an auto-update run is due using UTC wall-clock time.
    pub fn decide(
        &self,
        policy: &AutoUpdatePolicy,
        last_run: Option<DateTime<Utc>>,
    ) -> ScheduleDecision {
        self.decide_with_modes(
            policy,
            last_run,
            WallClockZone::Iana(chrono_tz::UTC),
            DstGap::Skip,
            CatchUp::LatestPassed,
        )
    }

    /// Decide using the configured IANA timezone for daily HHMM schedules.
    pub fn decide_in_timezone(
        &self,
        policy: &AutoUpdatePolicy,
        last_run: Option<DateTime<Utc>>,
        timezone: chrono_tz::Tz,
    ) -> ScheduleDecision {
        self.decide_with_modes(
            policy,
            last_run,
            WallClockZone::Iana(timezone),
            DstGap::Skip,
            CatchUp::LatestPassed,
        )
    }

    /// Decide with explicit wall-clock-zone, DST-gap and catch-up modes.
    ///
    /// Native passes `WallClockZone::Local`, `DstGap::RollForwardMinutes(1)`
    /// and `CatchUp::SingleMissed` (no catch-up while last-run is unset); the
    /// Worker keeps its IANA default with `Skip` and `LatestPassed`.
    pub fn decide_with_modes(
        &self,
        policy: &AutoUpdatePolicy,
        last_run: Option<DateTime<Utc>>,
        zone: WallClockZone,
        gap: DstGap,
        catch_up: CatchUp,
    ) -> ScheduleDecision {
        let now = self.clock.now_utc();
        if !policy.enabled {
            return ScheduleDecision {
                run_now: false,
                missed_run: None,
                next_run: None,
                last_run,
            };
        }

        let AutoUpdateSchedule::Daily(schedule) = &policy.schedule;
        let next_run = next_run_after(schedule, now, zone, gap);
        let (run_now, missed_run) = match catch_up {
            CatchUp::SingleMissed => {
                let missed = missed_run_time(schedule, last_run, now, zone, gap);
                (missed.is_some(), missed)
            }
            CatchUp::LatestPassed => {
                let missed = match last_run {
                    Some(last) => next_run_after(schedule, last, zone, gap)
                        .filter(|candidate| *candidate <= now),
                    None => latest_run_at_or_before(schedule, now, zone, gap),
                };
                (missed.is_some(), missed)
            }
        };

        ScheduleDecision {
            run_now,
            missed_run,
            next_run,
            last_run,
        }
    }

    /// The next run strictly after the service clock's `now`, when known.
    pub fn next_run(
        &self,
        policy: &AutoUpdatePolicy,
        zone: WallClockZone,
        gap: DstGap,
    ) -> Option<DateTime<Utc>> {
        let AutoUpdateSchedule::Daily(schedule) = &policy.schedule;
        next_run_after(schedule, self.clock.now_utc(), zone, gap)
    }
}
/// The latest scheduled time at or before `now` (0 or 1 local day back).
pub fn latest_run_at_or_before(
    schedule: &Schedule,
    now: DateTime<Utc>,
    zone: WallClockZone,
    gap: DstGap,
) -> Option<DateTime<Utc>> {
    match zone {
        WallClockZone::Iana(timezone) => latest_run_at_or_before_in(schedule, now, &timezone, gap),
        WallClockZone::Local => latest_run_at_or_before_in(schedule, now, &chrono::Local, gap),
    }
}

/// The next scheduled time strictly after `after`.
pub fn next_run_after(
    schedule: &Schedule,
    after: DateTime<Utc>,
    zone: WallClockZone,
    gap: DstGap,
) -> Option<DateTime<Utc>> {
    match zone {
        WallClockZone::Iana(timezone) => next_run_after_in(schedule, after, &timezone, gap),
        WallClockZone::Local => next_run_after_in(schedule, after, &chrono::Local, gap),
    }
}

/// Catch-up decision for one recorded last run: the first scheduled time after
/// it, when that time is already in the past. Unset last runs never catch up.
pub fn missed_run_time(
    schedule: &Schedule,
    last_run: Option<DateTime<Utc>>,
    now: DateTime<Utc>,
    zone: WallClockZone,
    gap: DstGap,
) -> Option<DateTime<Utc>> {
    let last_run = last_run?;
    if last_run >= now {
        return None;
    }
    let missed = next_run_after(schedule, last_run, zone, gap)?;
    (missed < now).then_some(missed)
}

fn latest_run_at_or_before_in<Tz>(
    schedule: &Schedule,
    now: DateTime<Utc>,
    timezone: &Tz,
    gap: DstGap,
) -> Option<DateTime<Utc>>
where
    Tz: TimeZone,
    Tz::Offset: Copy,
{
    let local = now.with_timezone(timezone);
    let mut latest = None;
    for day_offset in 0..=1 {
        let date = local.date_naive() - chrono::TimeDelta::days(day_offset);
        for &(hour, minute) in &schedule.times {
            let Some(candidate) = resolve_daily_time(timezone, date, hour, minute, gap) else {
                continue;
            };
            let candidate = candidate.with_timezone(&Utc);
            if candidate <= now && latest.is_none_or(|value| candidate > value) {
                latest = Some(candidate);
            }
        }
    }
    latest
}

fn next_run_after_in<Tz>(
    schedule: &Schedule,
    after: DateTime<Utc>,
    timezone: &Tz,
    gap: DstGap,
) -> Option<DateTime<Utc>>
where
    Tz: TimeZone,
    Tz::Offset: Copy,
{
    let local = after.with_timezone(timezone);
    for day_offset in 0..=1 {
        let date = local.date_naive() + chrono::TimeDelta::days(day_offset);
        for &(hour, minute) in &schedule.times {
            let Some(candidate) = resolve_daily_time(timezone, date, hour, minute, gap) else {
                continue;
            };
            let candidate = candidate.with_timezone(&Utc);
            if candidate > after {
                return Some(candidate);
            }
        }
    }
    None
}

/// Resolve `(hour, minute)` on `date` to an instant in `timezone`.
///
/// A DST fold takes the first offset. A gap is either skipped or rolled
/// forward `step` minutes at a time until the time exists, as long as the
/// rolled time stays on `date`.
fn resolve_daily_time<Tz>(
    timezone: &Tz,
    date: NaiveDate,
    hour: u32,
    minute: u32,
    gap: DstGap,
) -> Option<DateTime<Tz>>
where
    Tz: TimeZone,
    Tz::Offset: Copy,
{
    let mut candidate = date.and_time(NaiveTime::from_hms_opt(hour, minute, 0)?);
    let max_date = date.succ_opt()?;
    loop {
        match timezone.from_local_datetime(&candidate) {
            LocalResult::Single(value) => return Some(value),
            LocalResult::Ambiguous(first, _) => return Some(first),
            LocalResult::None => {
                let DstGap::RollForwardMinutes(step) = gap else {
                    return None;
                };
                let step = i64::from(step.max(1));
                candidate = candidate.checked_add_signed(chrono::TimeDelta::minutes(step))?;
                if candidate.date() > max_date {
                    return None;
                }
            }
        }
    }
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
    fn daily_schedule_runs_when_never_run_and_time_has_passed() {
        let policy = AutoUpdatePolicy {
            enabled: true,
            targets: AutoUpdateTargets::All,
            schedule: AutoUpdateSchedule::Daily(Schedule::parse("0900")),
        };
        let service = service_at(utc(2026, 1, 2, 1, 0).timestamp());
        let decision = service.decide_in_timezone(
            &policy,
            None,
            "Asia/Tokyo".parse().unwrap(),
        );
        assert!(decision.run_now);
        assert_eq!(decision.next_run, Some(utc(2026, 1, 3, 0, 0)));
    }

    #[test]
    fn daily_schedule_uses_configured_wall_clock_timezone() {
        let policy = AutoUpdatePolicy {
            enabled: true,
            targets: AutoUpdateTargets::All,
            schedule: AutoUpdateSchedule::Daily(Schedule::parse("0900")),
        };
        // 00:30 UTC is 09:30 in Tokyo; a previous Tokyo 09:00 run is due.
        let service = service_at(utc(2026, 1, 2, 0, 30).timestamp());
        let last_run = utc(2026, 1, 1, 0, 0);
        let decision = service.decide_in_timezone(
            &policy,
            Some(last_run),
            "Asia/Tokyo".parse().unwrap(),
        );
        assert!(decision.run_now);
        assert_eq!(decision.next_run, Some(utc(2026, 1, 3, 0, 0)));
    }

    #[test]
    fn policy_builds_from_schedule_and_targets() {
        let service = service_at(0);
        let policy = service.policy(true, Schedule::parse("0800,1200"), vec!["modified".into()]);
        assert!(policy.enabled);
        assert_eq!(policy.targets, AutoUpdateTargets::Tag(vec!["modified".into()]));
        assert_eq!(
            policy.schedule,
            AutoUpdateSchedule::Daily(Schedule::parse("0800,1200"))
        );
    }

    #[test]
    fn empty_or_invalid_schedules_never_run() {
        let service = service_at(utc(2026, 1, 1, 10, 0).timestamp());
        for schedule in ["", "9999"] {
            let policy = service.policy(true, Schedule::parse(schedule), vec![]);
            let decision = service.decide(&policy, None);
            assert!(!decision.run_now, "schedule {schedule:?} ran unexpectedly");
            assert_eq!(decision.next_run, None);
        }
    }

    #[test]
    fn parse_auto_update_schedule_requires_enabled_nonempty_valid_value() {
        assert!(parse_auto_update_schedule(false, "0800").unwrap().is_none());
        assert!(parse_auto_update_schedule(true, "  ").unwrap().is_none());
        assert_eq!(
            parse_auto_update_schedule(true, "not-a-time").unwrap_err(),
            "自動アップデートスケジューラーの時刻指定が不正です: not-a-time"
        );
        assert_eq!(
            parse_auto_update_schedule(true, "0800")
                .unwrap()
                .unwrap()
                .times,
            vec![(8, 0)]
        );
    }

    /// 2024-03-10 02:30 は America/New_York の DST ギャップで存在しない。
    /// native の +1 分丸めと Worker のスキップが同じ resolution で表現できる
    /// ことを固定する。
    #[test]
    fn dst_gap_modes_resolve_a_swallowed_time() {
        let schedule = Schedule::parse("0230");
        let zone = WallClockZone::Iana("America/New_York".parse().unwrap());
        let after = utc(2024, 3, 10, 6, 0);

        // native: 03:00 (EDT) へ繰り上げる。
        assert_eq!(
            next_run_after(&schedule, after, zone, DstGap::RollForwardMinutes(1)),
            Some(utc(2024, 3, 10, 7, 0))
        );
        // 共有既定 (Worker): その日の候補を捨てて翌日へ。
        assert_eq!(
            next_run_after(&schedule, after, zone, DstGap::Skip),
            Some(utc(2024, 3, 11, 6, 30))
        );
    }

    #[test]
    fn missed_run_time_needs_a_recorded_last_run() {
        let schedule = Schedule::parse("0900,1800");
        let zone = WallClockZone::Iana(chrono_tz::Asia::Tokyo);
        let now = utc(2026, 4, 20, 3, 0);

        // last-run 未記録: catch-up しない (native)。
        assert_eq!(
            missed_run_time(&schedule, None, now, zone, DstGap::Skip),
            None
        );
        // 前日 09:00 (JST) の次の予定 (前日 18:00 JST) は既に過ぎている。
        let last_run = utc(2026, 4, 19, 0, 0);
        assert_eq!(
            missed_run_time(&schedule, Some(last_run), now, zone, DstGap::Skip),
            Some(utc(2026, 4, 19, 9, 0))
        );
        // 未来の last-run では catch-up しない。
        assert_eq!(
            missed_run_time(&schedule, Some(utc(2026, 4, 21, 0, 0)), now, zone, DstGap::Skip),
            None
        );
    }

    #[test]
    fn native_catch_up_mode_reports_the_missed_run() {
        let policy = AutoUpdatePolicy {
            enabled: true,
            targets: AutoUpdateTargets::All,
            schedule: AutoUpdateSchedule::Daily(Schedule::parse("0900")),
        };
        // now = JST 12:00。前日 09:00 (JST) を最後に、当日 09:00 (JST) を過ぎた。
        let service = service_at(utc(2026, 4, 20, 3, 0).timestamp());
        let zone = WallClockZone::Iana(chrono_tz::Asia::Tokyo);
        let decision = service.decide_with_modes(
            &policy,
            Some(utc(2026, 4, 19, 0, 0)),
            zone,
            DstGap::Skip,
            CatchUp::SingleMissed,
        );
        assert!(decision.run_now);
        assert_eq!(decision.missed_run, Some(utc(2026, 4, 20, 0, 0)));
        assert_eq!(decision.next_run, Some(utc(2026, 4, 21, 0, 0)));

        // last-run 未記録は native では走らない。Worker モードは走る。
        let decision = service.decide_with_modes(
            &policy,
            None,
            zone,
            DstGap::Skip,
            CatchUp::SingleMissed,
        );
        assert!(!decision.run_now);
        assert_eq!(decision.missed_run, None);
        let decision = service.decide_with_modes(
            &policy,
            None,
            zone,
            DstGap::Skip,
            CatchUp::LatestPassed,
        );
        assert!(decision.run_now);
    }

    #[test]
    fn local_zone_resolves_like_the_host_clock() {
        // `update.auto-schedule.timezone` 未設定の native 経路。
        let schedule = Schedule::parse("0000");
        let now = Utc::now();
        let next = next_run_after(
            &schedule,
            now,
            WallClockZone::Local,
            DstGap::RollForwardMinutes(1),
        );
        assert!(next.is_some_and(|value| value > now));
        assert!(next.is_some_and(|value| (value - now).num_hours() < 48));
    }
}
