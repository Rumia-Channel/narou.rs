//! キュー表示 (ジョブ 1 行のラベルと応答 payload) の共有実装。
//!
//! native は `PersistentQueue` + 実行 spec (`queue.yaml`)、Worker は D1 の
//! job ledger を読むため記録の持ち方が違うが、表示文言・省略条件は同一で
//! なければならない。ここには文字列の組み立てだけを置き、「どの記録を読むか」
//! は各ランタイムのアダプタ (`src/web/jobs.rs` / `webui/queue.rs`) に残す。

use serde_json::Value;

/// 対象文字列のフォールバック表示: タブ区切りの成分を `" / "` で連結する
/// (native `queue_target_fallback_text`)。
pub fn queue_target_fallback_text(target: &str) -> String {
    target
        .split('\t')
        .filter(|part| !part.is_empty())
        .collect::<Vec<_>>()
        .join(" / ")
}

/// 更新対象の説明: 0 件は「全ての小説」、`tag:` 付きは「タグ「…」の小説」、
/// 数字だけは「ID n の小説」、複数は「n件の小説」。
pub fn describe_update_targets(targets: &[String]) -> String {
    match targets {
        [] => "全ての小説".to_string(),
        [target] => {
            if let Some(tag) = target.strip_prefix("tag:") {
                format!("タグ「{}」の小説", tag)
            } else if target.chars().all(|ch| ch.is_ascii_digit()) {
                format!("ID {} の小説", target)
            } else {
                target.to_string()
            }
        }
        _ => format!("{}件の小説", targets.len()),
    }
}

/// `--gl` (最新話掲載日) の表示。`args` は `--gl` の後続引数。
pub fn general_lastup_queue_target(args: &[String]) -> String {
    match args.first().map(String::as_str) {
        Some("narou") => "なろうAPIで最新話掲載日を確認".to_string(),
        Some("other") => "その他サイトの最新話掲載日を確認".to_string(),
        _ => "最新話掲載日を確認".to_string(),
    }
}

/// 対象説明に「を更新」「を凍結済みも含めて更新」を付ける。
pub fn update_queue_label(target_text: &str, force: bool) -> String {
    if force {
        format!("{}を凍結済みも含めて更新", target_text)
    } else {
        format!("{}を更新", target_text)
    }
}

/// update 系ジョブ 1 件の表示ターゲット。
///
/// - `args`: native は実行 spec の引数 (対象 id も含む)、Worker は ledger の
///   `options` (フラグのみ)。
/// - `start_message`: native の meta 由来の開始メッセージ。あるときはそれを
///   そのまま表示する (Worker は常に `None`)。
/// - `target`: 対象が独立カラムにあるランタイム (Worker) の正規化ターゲット。
///   `None` のときは native と同じく `args` から対象を読む。
pub fn format_update_queue_target(
    args: &[String],
    start_message: Option<&str>,
    target: Option<&str>,
) -> String {
    if let Some(message) = start_message {
        return message.to_string();
    }

    if args.first().map(String::as_str) == Some("--gl") {
        return general_lastup_queue_target(&args[1..]);
    }

    let mut force = false;
    let mut targets = Vec::new();
    if let Some(target) = target {
        force = args.iter().any(|arg| arg == "--force");
        if target != "*" {
            targets = target
                .split('\t')
                .filter(|part| !part.is_empty())
                .map(str::to_string)
                .collect();
        }
    } else {
        let mut skip_next = false;
        for arg in args {
            if skip_next {
                skip_next = false;
                continue;
            }
            if arg == "--force" {
                force = true;
                continue;
            }
            if arg == "--sort-by" {
                skip_next = true;
                continue;
            }
            targets.push(arg.clone());
        }
    }

    update_queue_label(&describe_update_targets(&targets), force)
}

/// 表示用のジョブ種別名: 実行記録に残ったコマンド名 (`recorded_cmd`) が
/// あればそれを、無ければ種別の正準名 (`canonical_kind`) を返す。
pub fn queue_type_label(recorded_cmd: Option<&str>, canonical_kind: &str) -> String {
    recorded_cmd.unwrap_or(canonical_kind).to_string()
}

/// 実行中ジョブのラベル: 1 件なら表示ターゲット、複数なら「n 件実行中」、
/// 0 件は `null`。`count` は queue / 台帳が持つ実件数 (ページ内の行数では
/// ない) で、`single_display_target` は 1 件のときだけ使う。
pub fn queue_running_label(count: usize, single_display_target: Option<String>) -> Value {
    match count {
        0 => Value::Null,
        1 => Value::String(single_display_target.unwrap_or_default()),
        n => Value::String(format!("{} 件実行中", n)),
    }
}

/// `/api/get_pending_tasks` の 1 行。`created_at` は Unix 秒 (`None` は
/// `null` — Worker は台帳の RFC3339 を解釈できないとき null にする)。
pub fn queue_job_payload(
    id: &str,
    job_type: &str,
    target: &str,
    display_target: &str,
    created_at: Option<i64>,
) -> Value {
    serde_json::json!({
        "id": id,
        "type": job_type,
        "target": target,
        "display_target": display_target,
        "created_at": created_at,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn target_fallback_joins_tab_separated_parts() {
        assert_eq!(queue_target_fallback_text("3\t5"), "3 / 5");
        assert_eq!(queue_target_fallback_text(""), "");
        assert_eq!(queue_target_fallback_text("a\t\tb"), "a / b");
    }

    #[test]
    fn update_targets_describe_single_and_multiple_targets() {
        assert_eq!(describe_update_targets(&[]), "全ての小説");
        assert_eq!(
            describe_update_targets(&["tag:modified".to_string()]),
            "タグ「modified」の小説"
        );
        assert_eq!(describe_update_targets(&["42".to_string()]), "ID 42 の小説");
        assert_eq!(
            describe_update_targets(&["https://example.com/".to_string()]),
            "https://example.com/"
        );
        assert_eq!(
            describe_update_targets(&["1".to_string(), "2".to_string(), "3".to_string()]),
            "3件の小説"
        );
    }

    #[test]
    fn general_lastup_labels_match_native_wording() {
        assert_eq!(
            general_lastup_queue_target(&["narou".to_string()]),
            "なろうAPIで最新話掲載日を確認"
        );
        assert_eq!(
            general_lastup_queue_target(&["other".to_string()]),
            "その他サイトの最新話掲載日を確認"
        );
        assert_eq!(general_lastup_queue_target(&[]), "最新話掲載日を確認");
    }

    #[test]
    fn update_queue_target_prefers_start_message() {
        assert_eq!(
            format_update_queue_target(
                &["tag:modified".to_string()],
                Some("開始メッセージ"),
                None
            ),
            "開始メッセージ"
        );
    }

    #[test]
    fn update_queue_target_parses_args_and_flags() {
        // `--force` は「凍結済みも含めて更新」になり、対象には残らない。
        assert_eq!(
            format_update_queue_target(
                &["--force".to_string(), "42".to_string()],
                None,
                None
            ),
            "ID 42 の小説を凍結済みも含めて更新"
        );
        // `--sort-by` とその値は対象から外れる (値はソートキー)。
        assert_eq!(
            format_update_queue_target(
                &["--sort-by".to_string(), "title".to_string(), "42".to_string()],
                None,
                None
            ),
            "ID 42 の小説を更新"
        );
        // `--gl` は後続引数で表示が決まる。
        assert_eq!(
            format_update_queue_target(&["--gl".to_string(), "narou".to_string()], None, None),
            "なろうAPIで最新話掲載日を確認"
        );
    }

    #[test]
    fn update_queue_target_reads_normalized_target_column() {
        // Worker は対象が別カラム。`*` は全件 (対象なし) で、フラグは options に残る。
        assert_eq!(
            format_update_queue_target(&[], None, Some("*")),
            "全ての小説を更新"
        );
        assert_eq!(
            format_update_queue_target(
                &["--force".to_string()],
                None,
                Some("3\t5")
            ),
            "2件の小説を凍結済みも含めて更新"
        );
    }

    #[test]
    fn type_label_prefers_recorded_command() {
        assert_eq!(
            queue_type_label(Some("download_force"), "download"),
            "download_force"
        );
        assert_eq!(queue_type_label(None, "auto_update"), "auto_update");
    }

    #[test]
    fn running_label_uses_display_target_only_for_single_job() {
        assert_eq!(queue_running_label(0, None), Value::Null);
        assert_eq!(
            queue_running_label(1, Some("ID 3 の小説を更新".to_string())),
            serde_json::json!("ID 3 の小説を更新")
        );
        assert_eq!(
            queue_running_label(3, None),
            serde_json::json!("3 件実行中")
        );
    }

    #[test]
    fn job_payload_keeps_keys_and_null_created_at() {
        assert_eq!(
            queue_job_payload("job-1", "update", "3", "ID 3 の小説を更新", Some(100)),
            serde_json::json!({
                "id": "job-1",
                "type": "update",
                "target": "3",
                "display_target": "ID 3 の小説を更新",
                "created_at": 100,
            })
        );
        assert_eq!(
            queue_job_payload("job-2", "download", "x", "x", None),
            serde_json::json!({
                "id": "job-2",
                "type": "download",
                "target": "x",
                "display_target": "x",
                "created_at": null,
            })
        );
    }
}
