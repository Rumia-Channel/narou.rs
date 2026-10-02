//! リトライ方針 (`queue.max-retries` / `queue.retry-backoff`) の解釈。
//!
//! native のキュー (`src/queue.rs` / `src/web/worker.rs`) と Cloudflare
//! Worker (`worker_entry/src/consumer.rs`) が同じ設定値から同じ
//! `queue_retry` ペイロード (`retry_count` / `max_retries` / `backoff_secs`)
//! を出すための共有実装。設定値の読み出し (local_setting.yaml / D1) は
//! 呼び出し側が行い、このモジュールは値の解釈だけを担う。
//!
//! Worker の台帳は失敗ごとに `attempts` を後置インクリメントするため、
//! `attempts` 回目を記録した時点で native の `retry_count` に相当するのは
//! `attempts - 1` である (`native` は再キュー時に `retry_count` を増やす)。

use serde_yaml::Value;

/// `queue.max-retries` の設定キー。
pub const MAX_RETRIES_KEY: &str = "queue.max-retries";
/// `queue.retry-backoff` の設定キー。
pub const RETRY_BACKOFF_KEY: &str = "queue.retry-backoff";

/// `queue.max-retries` が未設定・不正値のときの既定値。
pub const DEFAULT_MAX_RETRIES: u32 = 3;
/// `queue.retry-backoff` が未設定・有効要素なしのときの既定スケジュール
/// (既定値 "1m,5m,15m" を秒に展開したもの)。
pub const DEFAULT_RETRY_BACKOFF_SECS: &[i64] = &[60, 300, 900];

/// 解釈済みのリトライ方針。
///
/// `max_retries` は「失敗後に再キューする回数の上限」(native
/// `QueueJob::max_retries` と同じ意味: `retry_count < max_retries` の間だけ
/// 再キューされる)。`backoff_schedule` は `retry_count` 番目の再キューに
/// 使う待機秒数の列で、末尾を超えた分は最後の値を再利用する。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RetryPolicy {
    pub max_retries: u32,
    pub backoff_schedule: Vec<i64>,
}

impl RetryPolicy {
    /// `queue.max-retries` / `queue.retry-backoff` の設定値から解釈する。
    /// 未設定・型が合わない・有効要素が 0 の設定値は既定値に倒す
    /// (native と同じ規則)。
    pub fn resolve(max_retries: Option<&Value>, retry_backoff: Option<&Value>) -> Self {
        Self {
            max_retries: self::max_retries(max_retries),
            backoff_schedule: backoff_schedule(retry_backoff),
        }
    }

    /// `retry_count` 回の再キュー済みジョブがまだ再キューできるか
    /// (native `job.retry_count < job.max_retries` と同じ条件)。
    pub fn can_retry(&self, retry_count: u32) -> bool {
        retry_count < self.max_retries
    }

    /// `retry_count` 回の再キュー済みジョブを再キューするときの待機秒数。
    pub fn backoff_secs(&self, retry_count: u32) -> i64 {
        backoff_secs(retry_count, &self.backoff_schedule)
    }
}

/// `queue.max-retries` 設定値の解釈。`None` (未設定)・不正値は既定値。
///
/// native `src/queue.rs::configured_max_retries` と同じ規則。
pub fn max_retries(value: Option<&Value>) -> u32 {
    value
        .and_then(parse_max_retries)
        .unwrap_or(DEFAULT_MAX_RETRIES)
}

/// `queue.max-retries` の値を解釈する。整数 (Number / 数値文字列) を
/// `0..=u32::MAX` にクランプして受け付け、それ以外の型・非数文字列は `None`。
///
/// native `src/queue.rs::parse_max_retries_value` と同じ規則。
pub fn parse_max_retries(value: &Value) -> Option<u32> {
    let parsed = match value {
        Value::Number(number) => number
            .as_i64()
            .or_else(|| number.as_u64().and_then(|value| i64::try_from(value).ok())),
        Value::String(raw) => raw.trim().parse::<i64>().ok(),
        _ => None,
    }?;
    Some(parsed.clamp(0, u32::MAX as i64) as u32)
}

/// `queue.retry-backoff` 設定値の解釈。値がない・スカラでない・有効な要素が
/// 1 つもない場合は既定スケジュールを返す。
///
/// native `src/web/worker.rs::load_retry_backoff_schedule` と同じ規則。
pub fn backoff_schedule(value: Option<&Value>) -> Vec<i64> {
    backoff_schedule_from_spec(value.and_then(setting_value_string).as_deref())
}

/// 文字列化済みの `queue.retry-backoff` 指定からスケジュールを解釈する。
/// `None`・空・全要素不正のとき既定スケジュールを返す。
///
/// native が `load_local_setting_string` で文字列化した値をそのまま渡す
/// ための入口 (スカラ → 文字列の変換は呼び出し側の責務)。
pub fn backoff_schedule_from_spec(spec: Option<&str>) -> Vec<i64> {
    let parsed = spec.map(parse_backoff_spec).unwrap_or_default();
    if parsed.is_empty() {
        DEFAULT_RETRY_BACKOFF_SECS.to_vec()
    } else {
        parsed
    }
}

/// `retry_count` 番目 (0-based) の再キューに使う待機秒数。スケジュールの
/// 末尾を超えたら最後の値を再利用し、空スケジュールは既定の先頭 (60s)。
///
/// native `src/web/worker.rs::compute_retry_backoff_secs` と同じ規則。
pub fn backoff_secs(retry_count: u32, schedule: &[i64]) -> i64 {
    if schedule.is_empty() {
        return DEFAULT_RETRY_BACKOFF_SECS[0];
    }
    let idx = (retry_count as usize).min(schedule.len() - 1);
    schedule[idx]
}

/// カンマ区切りの指定 (`"1m,5m,15m"`) を秒の列に展開する。空白は許容し、
/// 不正な要素は捨てる。
///
/// native `src/web/worker.rs::parse_backoff_spec` と同じ規則。
pub fn parse_backoff_spec(spec: &str) -> Vec<i64> {
    spec.split(',')
        .map(str::trim)
        .filter_map(parse_backoff_value)
        .collect()
}

/// 1 要素の解釈: `s` / `m` / `h` (大小可) の接尾辞つき非負整数を秒に変換
/// する。接尾辞なしは秒、負数・非数は `None`。
///
/// native `src/web/worker.rs::parse_backoff_value` と同じ規則。
pub fn parse_backoff_value(raw: &str) -> Option<i64> {
    let raw = raw.trim();
    if raw.is_empty() {
        return None;
    }
    let (num_str, multiplier): (&str, i64) = if let Some(rest) = raw.strip_suffix(['s', 'S']) {
        (rest, 1)
    } else if let Some(rest) = raw.strip_suffix(['m', 'M']) {
        (rest, 60)
    } else if let Some(rest) = raw.strip_suffix(['h', 'H']) {
        (rest, 3600)
    } else {
        (raw, 1)
    };
    let num_str = num_str.trim();
    let parsed = num_str.parse::<i64>().ok()?;
    if parsed < 0 {
        return None;
    }
    Some(parsed * multiplier)
}

/// native `src/compat.rs::yaml_value_to_string` と同じく、スカラ値だけを
/// 文字列化する (Number/Bool は表示形式になる)。配列・マップ等は `None`。
fn setting_value_string(value: &Value) -> Option<String> {
    match value {
        Value::String(s) => Some(s.clone()),
        Value::Number(n) => Some(n.to_string()),
        Value::Bool(b) => Some(b.to_string()),
        _ => None,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn yaml(raw: &str) -> Value {
        serde_yaml::from_str(raw).unwrap()
    }

    #[test]
    fn resolve_uses_defaults_when_settings_are_unset() {
        let policy = RetryPolicy::resolve(None, None);
        assert_eq!(policy.max_retries, 3);
        assert_eq!(policy.backoff_schedule, [60, 300, 900]);
        // 設定読み取り失敗も既定値と同じ扱い (native の `load` 失敗同様)。
        assert_eq!(max_retries(None), 3);
        assert_eq!(backoff_schedule(None), [60, 300, 900]);
    }

    #[test]
    fn max_retries_parses_numbers_and_numeric_strings() {
        assert_eq!(parse_max_retries(&yaml("5")), Some(5));
        assert_eq!(parse_max_retries(&yaml("\"7\"")), Some(7));
        assert_eq!(parse_max_retries(&Value::String("  9 ".into())), Some(9));
        // u32 の範囲を超える整数は上限にクランプ。
        assert_eq!(parse_max_retries(&yaml("4000000000")), Some(4000000000));
        assert_eq!(parse_max_retries(&yaml("5000000000")), Some(u32::MAX));
    }

    #[test]
    fn max_retries_zero_and_negatives_disable_retries() {
        // `0` はリトライ無効 (既定値ではなく 0 として解釈する)。
        assert_eq!(parse_max_retries(&yaml("0")), Some(0));
        assert_eq!(max_retries(Some(&yaml("0"))), 0);
        // 負数・負の文字列は 0 にクランプ。
        assert_eq!(parse_max_retries(&yaml("-4")), Some(0));
        assert_eq!(parse_max_retries(&yaml("\"-2\"")), Some(0));
        let policy = RetryPolicy::resolve(Some(&yaml("0")), None);
        assert!(!policy.can_retry(0));
    }

    #[test]
    fn max_retries_rejects_uninterpretable_values() {
        // 浮動小数点数・真偽値・配列・i64 に収まらない整数は `None` → 既定値。
        assert_eq!(parse_max_retries(&yaml("3.0")), None);
        assert_eq!(parse_max_retries(&yaml("true")), None);
        assert_eq!(parse_max_retries(&yaml("[3]")), None);
        assert_eq!(parse_max_retries(&yaml("18446744073709551615")), None);
        assert_eq!(parse_max_retries(&yaml("\"abc\"")), None);
        assert_eq!(max_retries(Some(&yaml("\"abc\""))), 3);
    }

    #[test]
    fn backoff_value_parses_suffixes_and_bare_seconds() {
        assert_eq!(parse_backoff_value("30s"), Some(30));
        assert_eq!(parse_backoff_value("2M"), Some(120));
        assert_eq!(parse_backoff_value("1h"), Some(3600));
        assert_eq!(parse_backoff_value("90"), Some(90));
        assert_eq!(parse_backoff_value(" 45 "), Some(45));
    }

    #[test]
    fn backoff_value_rejects_invalid_forms() {
        assert_eq!(parse_backoff_value(""), None);
        assert_eq!(parse_backoff_value("-5m"), None);
        assert_eq!(parse_backoff_value("1.5m"), None);
        assert_eq!(parse_backoff_value("1d"), None);
        assert_eq!(parse_backoff_value("m"), None);
        assert_eq!(parse_backoff_value("junk"), None);
    }

    #[test]
    fn backoff_spec_skips_invalid_elements() {
        assert_eq!(parse_backoff_spec("1m,5m,15m"), [60, 300, 900]);
        assert_eq!(parse_backoff_spec("1m,oops,5m"), [60, 300]);
        assert_eq!(parse_backoff_spec("-5m,2m"), [120]);
        assert_eq!(parse_backoff_spec(" 30s , , 2h "), [30, 7200]);
        assert!(parse_backoff_spec("").is_empty());
        assert!(parse_backoff_spec(", ,,").is_empty());
        assert!(parse_backoff_spec("junk,-1").is_empty());
    }

    #[test]
    fn backoff_schedule_falls_back_to_default_on_invalid_settings() {
        // 空文字・全要素不正・スカラでない値はすべて既定スケジュール。
        assert_eq!(backoff_schedule(Some(&yaml("\"\""))), [60, 300, 900]);
        assert_eq!(backoff_schedule(Some(&yaml("\"junk\""))), [60, 300, 900]);
        assert_eq!(backoff_schedule(Some(&yaml("[1, 2]"))), [60, 300, 900]);
        // スカラは文字列化してから解釈する (yaml_value_to_string 同様)。
        assert_eq!(backoff_schedule(Some(&yaml("45"))), [45]);
        assert_eq!(backoff_schedule(Some(&yaml("true"))), [60, 300, 900]);
        assert_eq!(backoff_schedule(Some(&yaml("\"30s,2m\""))), [30, 120]);
    }

    #[test]
    fn backoff_secs_indexes_schedule_and_clamps_tail() {
        let schedule = [60, 300, 900];
        assert_eq!(backoff_secs(0, &schedule), 60);
        assert_eq!(backoff_secs(1, &schedule), 300);
        assert_eq!(backoff_secs(2, &schedule), 900);
        // スケジュール末尾を超えた試行は最後の値を再利用。
        assert_eq!(backoff_secs(5, &schedule), 900);
        // 空スケジュールは既定の先頭 (60s)。
        let empty: [i64; 0] = [];
        assert_eq!(backoff_secs(0, &empty), 60);
    }

    /// native と Worker が同じ設定値・同じ進行度から同じ待ち時間を出すことを
    /// 固定する。
    ///
    /// native (`src/web/worker.rs`): `job.retry_count < job.max_retries` の間
    /// `schedule[retry_count]` だけ待って再キューし、イベントには
    /// `retry_count + 1` を載せる。
    /// Worker (`worker_entry/src/consumer.rs`): 失敗ごとに `attempts` を
    /// インクリメントしたあと `can_retry(attempts - 1)` で判断し、
    /// `backoff_secs(attempts - 1)` だけ遅延させて `retry_count = attempts`
    /// を載せる。
    #[test]
    fn native_and_worker_resolve_identical_retry_schedules() {
        for (max, backoff) in [(None, None), (Some("0"), None), (Some("3"), Some("1m,5m"))] {
            let max_value = max.map(|raw| yaml(&format!("\"{raw}\"")));
            let backoff_value = backoff.map(|raw| Value::String(raw.to_string()));
            let policy = RetryPolicy::resolve(max_value.as_ref(), backoff_value.as_ref());

            // native の観点: retry_count = 0,1,… の各再キューで出す値。
            let native: Vec<(u32, i64)> = (0..=10)
                .take_while(|&retry_count| policy.can_retry(retry_count))
                .map(|retry_count| (retry_count + 1, policy.backoff_secs(retry_count)))
                .collect();

            // Worker の観点: attempts = 1,2,… の記録ごとに出す値。
            let worker: Vec<(u32, i64)> = (1..=11)
                .map(|attempts| (attempts, attempts - 1))
                .take_while(|&(_, retry_count)| policy.can_retry(retry_count))
                .map(|(attempts, retry_count)| (attempts, policy.backoff_secs(retry_count)))
                .collect();

            assert_eq!(native, worker);
        }

        // 既定設定での両者の `queue_retry` 列 (retry_count, backoff_secs)。
        let policy = RetryPolicy::resolve(None, None);
        let events: Vec<(u32, i64)> = (1..=policy.max_retries)
            .map(|attempts| (attempts, policy.backoff_secs(attempts - 1)))
            .collect();
        assert_eq!(events, [(1, 60), (2, 300), (3, 900)]);
    }
}
