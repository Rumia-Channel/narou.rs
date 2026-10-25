//! 新機能ツアーの定義と表示条件 (native `src/web/feature_tour.rs` と Worker
//! `webui/ui_prefs.rs` が共有する)。
//!
//! ツアーの本文テーブル・未表示判定・バージョン比較はここが唯一の定義。
//! seen / disabled の保存先 (native: local_setting.yaml、Worker: D1) と
//! ストレージ移行プロンプトは各ランタイムのアダプタに残す。

use serde::Serialize;

use crate::application::version_compare::{version_compare, version_core};

/// 表示済みツアーのバージョンを保存する設定キー (`webui.feature-tour.seen-version`)。
pub const SEEN_VERSION_KEY: &str = "webui.feature-tour.seen-version";
/// ツアー表示自体を止めたことを保存する設定キー (`webui.feature-tour.disabled`)。
pub const DISABLED_KEY: &str = "webui.feature-tour.disabled";

/// Never lower a previously acknowledged tour version.
pub async fn mark_seen(
    settings: &crate::application::settings::SettingsService,
    requested: &str,
) -> Result<(), crate::application::ApplicationError> {
    settings.update_raw(crate::setting_core::SettingScope::Local, SEEN_VERSION_KEY, |current| {
        let seen = current.and_then(serde_yaml::Value::as_str)
            .filter(|seen| version_greater(seen, requested))
            .unwrap_or(requested);
        serde_yaml::Value::String(seen.to_owned())
    }).await
}

#[derive(Clone, Copy, Debug, Serialize)]
pub struct FeatureTourEntry {
    pub version: &'static str,
    pub title: &'static str,
    pub body: &'static str,
    pub items: &'static [&'static str],
}

pub const FEATURE_TOURS: &[FeatureTourEntry] = &[
    FeatureTourEntry {
        version: "0.2.0",
        title: "narou.rb から広がった Web UI",
        body: "Narou.rs の Web UI では、narou.rb 互換の管理データを使いながら、まとめて扱う操作が増えています。",
        items: &[
            "複数作品を選択した update / convert / tag / freeze / remove",
            "キュー管理と進捗表示",
            "タグ指定 update と最新話掲載日の確認",
        ],
    },
    FeatureTourEntry {
        version: "0.2.0",
        title: "シリーズ URL の一括登録",
        body: "作品 URL だけでなく、シリーズやコレクションの URL から個別作品を展開して登録できます。",
        items: &[
            "小説家になろうのシリーズ URL",
            "ノクターンなど R18 系のシリーズ URL",
            "カクヨムのコレクション URL",
        ],
    },
    FeatureTourEntry {
        version: "0.2.2",
        title: "更新後の自動変換とコピー設定",
        body: "更新後の convert と copy-to 周りの挙動を見直し、Web UI と CLI の update からの変換結果が揃うようにしています。",
        items: &[
            "convert.multi-device を update 後の自動変換でも反映",
            "EPUB 変換時の copy-to 出力を優先",
            "text-only 変換時に txt を EPUB 保存先へコピーしない",
        ],
    },
    FeatureTourEntry {
        version: "0.2.3",
        title: "新機能ツアー",
        body: "バージョンごとの追加機能を、必要な分だけ起動時に表示するようになりました。",
        items: &[
            "表示済みのツアー version を local_setting.yaml に保存",
            "次の更新では、未表示のツアーだけを表示",
            "ツアー項目が追加されないバージョンでは何も表示しない",
        ],
    },
    FeatureTourEntry {
        version: "0.3.0",
        title: "ハーメルン R18 作品に対応",
        body: "h.syosetu.org に分離されたハーメルンの R18 作品を、h あり・なしどちらの URL からでも登録・ダウンロードできます。",
        items: &[
            "h.syosetu.org と syosetu.org を同一サイトとして扱い、重複登録を防止",
            "R18 作品の取得時は h ドメインへの転送を自動で追跡",
            "R18 作品は over18(18歳以上確認)設定が有効な場合のみ取得",
        ],
    },
    FeatureTourEntry {
        version: "0.3.0",
        title: "新規タグの既定色設定",
        body: "新しく作られるタグに割り当てる色を webui.new-tag-color 設定で固定できます。",
        items: &[
            "設定画面の WebUI タブから色を選択",
            "default のままなら従来どおりタグの追加順に色を巡回",
            "Web UI と CLI のどちらでタグを追加しても同じ色を適用",
        ],
    },
    FeatureTourEntry {
        version: "0.3.0",
        title: "失敗したジョブの自動リトライ",
        body: "キューのジョブが一時的なエラーで失敗した場合、間隔を広げながら自動で再試行します。",
        items: &[
            "再試行回数は queue.max-retries 設定で調整(既定 3 回)",
            "再試行間隔は queue.retry-backoff 設定で調整(既定 1m,5m,15m)",
            "再試行の予定は queue_retry 通知として Web UI に配信",
        ],
    },
    FeatureTourEntry {
        version: "0.4.0",
        title: "管理データの SQLite 移行とライブラリバックアップ",
        body: "0.4.0 では .narou 配下の管理データを SQLite へ移行できます。破壊的な変更に備え、アップデート後の初回起動時にライブラリ全体のバックアップを提案します。",
        items: &[
            "初回起動時にライブラリ全体 (小説データ + .narou + webnovel 等) のバックアップを提案",
            "narou_rs_backup サブ実行ファイルでいつでもライブラリ全体を zip 化可能",
            "管理方式は Web UI ツアーまたは .narou/storage-backend で SQLite / YAML を選択",
            "narou db verify / export-yaml / vacuum で SQLite 管理データを保守",
            "narou diff --history / --restore で小説本文のバージョン履歴を参照・復元",
        ],
    },
    FeatureTourEntry {
        version: "0.4.3",
        title: "Pixiv の小説・イラスト・漫画に対応",
        body: "Pixiv の URL から作品を登録し、本文やページ画像を取得できるようになりました。",
        items: &[
            "小説・小説シリーズ・イラスト/漫画・漫画シリーズの URL に対応",
            "挿絵や漫画のページ画像を保存し、うごイラはネイティブ版で APNG に変換",
            "R18・ログイン限定作品を取得するには、先にログイン Cookie を登録",
        ],
    },
    FeatureTourEntry {
        version: "0.4.3",
        title: "ログインが必要な作品を取得",
        body: "ログインを求められた作品は、保存済みの Cookie を使って再取得できます。",
        items: &[
            "同梱の narou_rs_login でブラウザの Cookie を取得し、書き出しファイルで別端末へ持ち込めます",
            "環境設定の「ログイン」タブや narou login で管理し、複数のアカウントを試す順序も変更できます",
            "Cookie は暗号化して保存し、ログイン不要な作品には通常送信しません",
        ],
    },
    FeatureTourEntry {
        version: "0.4.3",
        title: "ライブラリの検索が便利に",
        body: "Web UI 右上のフィルター欄で、作品の URL や ID を貼り付けて探せます。",
        items: &[
            "作品 URL、なろうの N コード、数値の作品 ID・登録 ID で検索",
            "従来のタイトル・タグ・作者などの検索も引き続き利用可能",
        ],
    },
    FeatureTourEntry {
        version: "0.4.3",
        title: "更新時の GPL 版・通常版を選択",
        body: "環境設定の Global タブで、今後のセルフアップデートで取得する版を選べます。",
        items: &[
            "GPL 版は AozoraEpub3_Lite を組み込み、通常版は外部 AozoraEpub3 を利用",
            "self-update.variant で GPL 版 / 通常版 / 未設定を選択。CLI の narou setting でも変更可能",
            "未設定なら、現在利用中のビルドと同じ種類の版を取得",
        ],
    },
    FeatureTourEntry {
        version: "0.4.13",
        title: "アルファポリスの小説・作者一覧に対応",
        body: "アルファポリスの無料公開の話と、アカウントでレンタル中の話を、章分けを保って取得できます。",
        items: &[
            "小説トップ・各話・作者ページの URL から作品を登録できます",
            "レンタル中の話は narou_rs_login alphapolis.co.jp で取得したログイン Cookie を登録して取得します",
            "閲覧制限で保存済みの話が見えなくなった場合は、更新を見送って既存の目次・本文を保持します",
        ],
    },
    FeatureTourEntry {
        version: "0.4.13",
        title: "カクヨムネクストに対応",
        body: "カクヨムネクストの作品を取得できます。ログインなしでも無料分を取得し、閲覧権のある Cookie を登録すると有料部分も取得できます。",
        items: &[
            "NEXT 契約など、対象の話を読めるアカウントの閲覧権が必要です",
            "Cookie は narou_rs_login kakuyomu.jp で取得し、環境設定の「ログイン」から取り込めます",
            "無料分を取得した作品も、閲覧権のある Cookie の登録後に更新すれば残りの話を追加できます",
        ],
    },
    FeatureTourEntry {
        version: "0.5.1",
        title: "作品をまとめる「バーチャル小説」",
        body: "「XXX その1」「XXX その2」のような分かれた作品を、1つの作品として一覧に登録できます。実体は各作品のまま、EPUB だけを登録順に結合します。",
        items: &[
            "narou virtual add / list / order / remove（Web UI のオプションメニューからも登録・並べ替え・削除）",
            "ncode は vs_<ID>。仮想小説には vs_<ID> タグが付き、既存の非表示設定で隠せます",
            "narou convert vs_<ID> でメンバーを登録順に結合した EPUB を出力",
        ],
    },
    FeatureTourEntry {
        version: "0.5.1",
        title: "作品・タグ・作者・サイトの非表示設定",
        body: "一覧に出したくない作品を、作品単位だけでなくタグ・作者・サイト単位でも隠せるようになりました。",
        items: &[
            "一覧の「非表示も表示」トグルと、右クリックの「非表示 / 表示に戻す」",
            "設定の非表示マネージャー、CLI の narou hidden add/remove/list/clear",
            "タグを非表示にすると、そのタグを持つ作品も一覧から隠れます",
        ],
    },
    FeatureTourEntry {
        version: "0.5.1",
        title: "小説データを ZIP で持ち出し・取り込み",
        body: "選択した作品の toc.yaml・本文・setting.ini・replace.txt・挿絵などを ZIP にまとめ、別のライブラリへ取り込めます。",
        items: &[
            "narou export <作品...> -o novels.zip / narou import novels.zip",
            "Web UI のオプションメニューから選択作品をダウンロード、ZIP をアップロード",
            "toc_url が同じ作品はスキップまたは上書き。古いアーカイブで既存の本文を削りません",
        ],
    },
    FeatureTourEntry {
        version: "0.5.1",
        title: "異なるサイトの並列ダウンロード",
        body: "なろう・カクヨム・Pixiv などをサイト系列ごとに直列化し、異なる系列は並列に更新できるようになりました。",
        items: &[
            "なろう / なろうR18 / 作者ページ / API は同じ系列として直列",
            "サイト定義の site_family で系列を指定でき、未指定時はドメインから自動判定",
            "Web UI の個別ダウンロードも異なる系列なら同時に実行",
        ],
    },
];

/// 現在のバージョンまでに存在するツアー (表示対象になり得る全件)。
pub fn current_entries(current_version: &str) -> Vec<FeatureTourEntry> {
    FEATURE_TOURS
        .iter()
        .copied()
        .filter(|entry| !version_greater(entry.version, current_version))
        .collect()
}

/// 未表示のツアー: 現在のバージョン以下で、表示済み (`seen_version`) より
/// 新しいもの。`seen_version` が無いときは現在のバージョンまでの全件。
pub fn pending_entries(seen_version: Option<&str>, current_version: &str) -> Vec<FeatureTourEntry> {
    FEATURE_TOURS
        .iter()
        .copied()
        .filter(|entry| {
            !version_greater(entry.version, current_version)
                && seen_version
                    .map(|seen| version_greater(entry.version, seen))
                    .unwrap_or(true)
        })
        .collect()
}

/// テーブル中の最新バージョン (無ければ空文字列)。
pub fn latest_tour_version() -> &'static str {
    FEATURE_TOURS
        .iter()
        .map(|entry| entry.version)
        .max_by(|a, b| compare_versions(a, b))
        .unwrap_or("")
}

/// テーブルに存在するバージョンか (`POST /api/feature_tour/seen` の受理判定)。
pub fn is_known_tour_version(version: &str) -> bool {
    FEATURE_TOURS.iter().any(|entry| entry.version == version)
}

/// `left` が `right` より新しいか。
pub fn version_greater(left: &str, right: &str) -> bool {
    compare_versions(left, right).is_gt()
}

/// バージョン比較。`application::version_compare` の数値比較をそのまま使う
/// (旧実装の 3 要素切り詰めと違い `1.0.0.1` のような 4 要素も比較できる)。
pub fn compare_versions(left: &str, right: &str) -> std::cmp::Ordering {
    version_compare(&comparable_core(left), &comparable_core(right))
        .unwrap_or(std::cmp::Ordering::Equal)
}

/// 解釈できないバージョンは `0.0.0` とみなす (旧実装が数値化できない要素を
/// 0 詰めしていたのと同じ向き — 手で書かれた未知の seen 値でもツアーは
/// 表示側に倒れる)。
fn comparable_core(version: &str) -> String {
    let core = version_core(version);
    if version_compare(&core, "0").is_some() {
        core
    } else {
        "0.0.0".to_string()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn acknowledging_older_tour_cannot_lower_seen_version() {
        let store = std::sync::Arc::new(crate::application::settings::MemorySettingsStore::new());
        let settings = crate::application::settings::SettingsService::new(store);
        futures::executor::block_on(async {
            mark_seen(&settings, "0.4.3").await.unwrap();
            mark_seen(&settings, "0.2.0").await.unwrap();
            let seen = settings.get_raw(crate::setting_core::SettingScope::Local, SEEN_VERSION_KEY)
                .await.unwrap().unwrap();
            assert!(pending_entries(seen.as_str(), "0.4.3").is_empty());
            mark_seen(&settings, "0.4.10").await.unwrap();
            mark_seen(&settings, "0.4.3").await.unwrap();
            assert_eq!(settings.get_raw(crate::setting_core::SettingScope::Local, SEEN_VERSION_KEY)
                .await.unwrap().unwrap().as_str(), Some("0.4.10"));
        });
    }


    #[test]
    fn version_greater_compares_numeric_parts() {
        assert!(version_greater("0.4.10", "0.4.3"));
        assert!(!version_greater("0.4.3", "0.4.3"));
        assert!(version_greater("v0.4.3", "0.4.2"));
        assert!(version_greater("0.4.3", "0.4"));
        // 4 要素バージョンも数値比較できる (旧実装は 3 要素で切り詰めていた)。
        assert!(version_greater("0.4.3.1", "0.4.3"));
    }

    #[test]
    fn unparsable_versions_are_treated_as_oldest() {
        assert!(version_greater("0.2.0", "手で書いた値"));
        assert!(!version_greater("手で書いた値", "0.2.0"));
        assert!(!version_greater("手で書いた値", ""));
    }

    #[test]
    fn pending_entries_transition_as_seen_version_advances() {
        let all = current_entries("0.4.3");
        assert!(!all.is_empty());
        assert_eq!(pending_entries(None, "0.4.3").len(), all.len());

        let seen = "0.4.0";
        let pending = pending_entries(Some(seen), "0.4.3");
        assert_eq!(
            pending.iter().map(|entry| entry.version).collect::<Vec<_>>(),
            vec!["0.4.3", "0.4.3", "0.4.3", "0.4.3"]
        );
        assert!(pending_entries(Some("0.4.3"), "0.4.3").is_empty());
        // 現在のバージョンより新しいツアーは seen が何であっても出ない。
        assert!(pending_entries(None, "0.1.0").is_empty());
    }

}

