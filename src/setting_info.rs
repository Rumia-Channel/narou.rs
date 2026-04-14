//! Setting variable metadata shared between CLI commands and web API.
//!
//! Types and data that describe the setting variables available in narou.rs,
//! matching narou.rb's SETTING_VARIABLES / SETTING_TAB_NAMES.

/// Variable type for a setting
#[derive(Debug, Clone, Copy, serde::Serialize)]
#[serde(rename_all = "lowercase")]
pub enum VarType {
    Boolean,
    Integer,
    Float,
    String,
    Select,
    Multiple,
    Directory,
}

/// Metadata for a setting variable
#[derive(Debug, Clone, serde::Serialize)]
pub struct VarInfo {
    pub var_type: VarType,
    pub help: &'static str,
    #[serde(skip_serializing_if = "std::ops::Not::not")]
    pub invisible: bool,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub select_keys: Option<Vec<String>>,
}

/// Collection of local and global setting variables
pub struct SettingVariables {
    pub local: Vec<(&'static str, VarInfo)>,
    pub global: Vec<(&'static str, VarInfo)>,
}

impl SettingVariables {
    pub fn get(&self, name: &str) -> Option<&VarInfo> {
        for (n, info) in &self.local {
            if *n == name {
                return Some(info);
            }
        }
        for (n, info) in &self.global {
            if *n == name {
                return Some(info);
            }
        }
        None
    }
}

/// Returns the tab assignment for a setting variable name.
/// Matches narou.rb's SETTING_VARIABLES tab assignments.
pub fn tab_for_setting(name: &str) -> Option<&'static str> {
    if name.starts_with("default.") {
        return Some("default");
    }
    if name.starts_with("force.") {
        return Some("force");
    }
    if name.starts_with("default_args.") {
        return Some("command");
    }
    match name {
        // local → general
        "device" | "hotentry" | "concurrency" | "logging"
        | "update.interval" | "update.strong" | "update.convert-only-new-arrival"
        | "update.sort-by" | "update.auto-schedule.enable" | "update.auto-schedule"
        | "convert.copy-to" | "convert.copy-zip-to" | "convert.copy-to-grouping"
        | "convert.make-zip" | "convert.no-open" | "convert.multi-device"
        | "convert.filename-to-ncode" | "convert.add-dc-subject-to-epub"
        | "convert.dc-subject-exclude-tags"
        | "send.without-freeze" | "auto-add-tags" => Some("general"),

        // local → detail
        "hotentry.auto-mail" | "logging.format-filename" | "logging.format-timestamp"
        | "download.interval" | "download.wait-steps" | "download.use-subdirectory"
        | "download.choices-of-digest-options" | "send.backup-bookmark"
        | "multiple-delimiter" | "economy" | "guard-spoiler" | "normalize-filename"
        | "convert.inspect" | "folder-length-limit" | "filename-length-limit"
        | "ebook-filename-length-limit" | "user-agent" => Some("detail"),

        // local → webui
        "webui.theme" | "webui.table.reload-timing" | "webui.performance-mode" => Some("webui"),

        // global → global
        "difftool" | "difftool.arg" | "no-color" | "color-parser"
        | "server-port" | "server-bind"
        | "server-basic-auth.enable" | "server-basic-auth.user" | "server-basic-auth.password"
        | "server-ws-add-accepted-domains" | "over18" => Some("global"),

        _ => None,
    }
}

/// Per-novel setting variable metadata (used for default.*/force.* tabs)
pub fn original_setting_var_infos() -> Vec<(&'static str, VarInfo)> {
    let info = |vt: VarType, help: &'static str| VarInfo {
        var_type: vt,
        help,
        invisible: true,
        select_keys: None,
    };
    let select = |help: &'static str, keys: Vec<&'static str>| VarInfo {
        var_type: VarType::Select,
        help,
        invisible: true,
        select_keys: Some(keys.iter().map(|s| s.to_string()).collect()),
    };

    vec![
        ("enable_yokogaki", info(VarType::Boolean, "横書きにする")),
        (
            "enable_convert_num_to_kanji",
            info(VarType::Boolean, "漢数字に変換する"),
        ),
        (
            "enable_kanji_num_with_units",
            info(
                VarType::Boolean,
                "漢数字化する際、単位がついている場合についた場合だけ漢数字化する",
            ),
        ),
        (
            "enable_half_indent_bracket",
            info(
                VarType::Boolean,
                "行頭かぎ括弧に二分アキを挿入する(kindle様式)",
            ),
        ),
        (
            "enable_auto_indent",
            info(VarType::Boolean, "行頭字下げ処理を有効にする"),
        ),
        (
            "enable_auto_join_in_brackets",
            info(
                VarType::Boolean,
                "カギ括弧内の自動連結を有効にする",
            ),
        ),
        (
            "enable_auto_join_line",
            info(
                VarType::Boolean,
                "自動行連結処理を有効にする(読点で終わる行を結合する)",
            ),
        ),
        (
            "enable_enchant_midashi",
            info(
                VarType::Boolean,
                "見出し自動装飾（見出しの前に空行と改ページを入れる）を有効にする",
            ),
        ),
        (
            "enable_author_comments",
            info(VarType::Boolean, "前書き・後書きを出力する"),
        ),
        (
            "enable_erase_introduction",
            info(
                VarType::Boolean,
                "前書きを削除する（前書きの検出はサイト様のHTML仕様に依存）",
            ),
        ),
        (
            "enable_erase_postscript",
            info(
                VarType::Boolean,
                "後書きを削除する（後書きの検出はサイト様のHTML仕様に依存）",
            ),
        ),
        (
            "enable_ruby",
            info(
                VarType::Boolean,
                "ルビを有効にする",
            ),
        ),
        (
            "enable_illust",
            info(
                VarType::Boolean,
                "挿絵のダウンロード・変換を有効にする",
            ),
        ),
        (
            "enable_transform_fraction",
            info(
                VarType::Boolean,
                "分数表記(※/※)を有効にする",
            ),
        ),
        (
            "enable_transform_date",
            info(
                VarType::Boolean,
                "「Y/M/D」のような日付表記のスラッシュを全角に変換する",
            ),
        ),
        (
            "enable_convert_horizontal_ellipsis",
            info(
                VarType::Boolean,
                "中黒(・)が3つ以上連続した場合三点リーダー(…)に変換する",
            ),
        ),
        (
            "enable_convert_page_break",
            info(
                VarType::Boolean,
                "空行が閾値以上続いた場合に改ページを挿入する",
            ),
        ),
        (
            "enable_convert_double_dash",
            info(
                VarType::Boolean,
                "ダッシュ(―)が2つ以上連続した場合ダブルダッシュ(――)に変換する",
            ),
        ),
        (
            "enable_display_end_of_book",
            info(
                VarType::Boolean,
                "本の末尾に「(本を読み終わりました)」と表示する",
            ),
        ),
        (
            "enable_pack_blank_line",
            info(VarType::Boolean, "2行以上の空行を1行にまとめる"),
        ),
        (
            "enable_inspect",
            info(
                VarType::Boolean,
                "小説の変換時に調査データを出力するかどうか（convert.inspect）",
            ),
        ),
        (
            "title_date_format",
            info(
                VarType::String,
                "タイトルに日付を付ける場合のフォーマット",
            ),
        ),
        (
            "title_date_align",
            select(
                "タイトルの日付の付与位置",
                vec!["right", "left", "none"],
            ),
        ),
        (
            "page_break_empty_line_size",
            info(
                VarType::Integer,
                "空行が何行以上続いたら改ページを挿入するか",
            ),
        ),
        (
            "minimum_blank_line",
            info(
                VarType::Integer,
                "連続する空行の最小数",
            ),
        ),
        (
            "cut_old_subtitles",
            info(
                VarType::Integer,
                "１話目から指定した話数分、変換の対象外にする。全話数分以上の数値を指定した場合、最新話だけ変換する",
            ),
        ),
        (
            "slice_size",
            info(
                VarType::Integer,
                "小説が指定した話数より多い場合、指定した話数ごとに分割する。cut_old_subtitlesで処理した後の話数を対象に処理する",
            ),
        ),
        (
            "author_comment_style",
            select(
                "作者コメント(前書き・後書き)の装飾方法を指定する。KoboやAdobe Digital Editionでは「CSSで装飾」にするとデザインが崩れるのでそれ以外を推奨。css:CSSで装飾、simple:シンプルに段落、plain:装飾しない",
                vec!["css", "simple", "plain"],
            ),
        ),
        (
            "novel_author",
            info(
                VarType::String,
                "小説の著者名を変更する。作品内著者名及び出力ファイル名に影響する",
            ),
        ),
        (
            "novel_title",
            info(
                VarType::String,
                "小説のタイトルを変更する。作品内タイトル及び出力ファイル名に影響する",
            ),
        ),
        (
            "output_filename",
            info(
                VarType::String,
                "出力ファイル名を任意の文字列に変更する。convert.filename-to-ncode の設定よりも優先される。※拡張子を含めないで下さい",
            ),
        ),
    ]
}

/// Local setting variable metadata
pub fn setting_variables() -> SettingVariables {
    let vis = |vt: VarType, help: &'static str| VarInfo {
        var_type: vt,
        help,
        invisible: false,
        select_keys: None,
    };
    let invis = |vt: VarType, help: &'static str| VarInfo {
        var_type: vt,
        help,
        invisible: true,
        select_keys: None,
    };
    let invis_sel = |help: &'static str, keys: Vec<&'static str>| VarInfo {
        var_type: VarType::Select,
        help,
        invisible: true,
        select_keys: Some(keys.iter().map(|s| s.to_string()).collect()),
    };
    let sel = |help: &'static str, keys: Vec<&'static str>| VarInfo {
        var_type: VarType::Select,
        help,
        invisible: false,
        select_keys: Some(keys.iter().map(|s| s.to_string()).collect()),
    };
    let multi = |help: &'static str, keys: Vec<&'static str>| VarInfo {
        var_type: VarType::Multiple,
        help,
        invisible: false,
        select_keys: Some(keys.iter().map(|s| s.to_string()).collect()),
    };

    let local_vars = vec![
        (
            "device",
            sel(
                "変換、送信対象の端末(sendの--help参照)",
                vec!["kindle", "kobo", "epub", "ibunko", "reader", "ibooks"],
            ),
        ),
        (
            "hotentry",
            vis(VarType::Boolean, "新着投稿だけをまとめたデータを作る"),
        ),
        (
            "hotentry.auto-mail",
            vis(
                VarType::Boolean,
                "hotentryをメールで送る(mail設定済みの場合)",
            ),
        ),
        (
            "concurrency",
            vis(
                VarType::Boolean,
                "ダウンロードと変換の同時実行を有効にする。有効にするとログの出力方式が変更される",
            ),
        ),
        (
            "concurrency.format-queue-text",
            invis(
                VarType::String,
                "同時実行時の変換キュー表示テキストのフォーマット",
            ),
        ),
        (
            "concurrency.format-queue-style",
            vis(
                VarType::String,
                "同時実行時の変換キュー表示スタイルのフォーマット",
            ),
        ),
        (
            "logging",
            vis(
                VarType::Boolean,
                "ログの保存を有効にする。保存場所はlogフォルダ。concurrencyが有効な場合、変換ログだけ別ファイルに出力される",
            ),
        ),
        (
            "logging.format-filename",
            vis(
                VarType::String,
                "ログの保存ファイル名のフォーマット",
            ),
        ),
        (
            "logging.format-timestamp",
            vis(
                VarType::String,
                "ログのタイムスタンプのフォーマット",
            ),
        ),
        (
            "update.interval",
            vis(
                VarType::Float,
                "更新時にサイトからのダウンロード間隔(秒)を指定する",
            ),
        ),
        (
            "update.strong",
            vis(
                VarType::Boolean,
                "更新時に全話差分チェックを行う(強制更新モード)",
            ),
        ),
        (
            "update.convert-only-new-arrival",
            vis(
                VarType::Boolean,
                "更新時に新着だけを変換する",
            ),
        ),
        (
            "update.sort-by",
            sel(
                "更新時のソート順",
                vec!["id", "last_update", "title", "author", "site", "new_firing_firing_firing"],
            ),
        ),
        (
            "update.auto-schedule.enable",
            vis(
                VarType::Boolean,
                "自動更新スケジュールを有効にする",
            ),
        ),
        (
            "update.auto-schedule",
            vis(
                VarType::String,
                "自動更新を行う時刻(HHMM)をカンマ区切りで指定する",
            ),
        ),
        (
            "convert.copy-to",
            vis(
                VarType::Directory,
                "変換したファイルのコピー先ディレクトリ",
            ),
        ),
        (
            "convert.copy-zip-to",
            vis(
                VarType::Directory,
                "ZIPファイルのコピー先ディレクトリ",
            ),
        ),
        (
            "convert.copy-to-grouping",
            vis(
                VarType::Boolean,
                "コピー先を「デバイス名/サイト名/」のサブフォルダにグルーピングする",
            ),
        ),
        (
            "convert.copy_to",
            invis(
                VarType::Directory,
                "(旧互換)変換したファイルのコピー先ディレクトリ",
            ),
        ),
        (
            "convert.no-epub",
            invis(VarType::Boolean, "EPUB変換を行わない"),
        ),
        (
            "convert.no-mobi",
            invis(VarType::Boolean, "MOBI変換を行わない"),
        ),
        (
            "convert.no-strip",
            invis(VarType::Boolean, "MOBIのSRCSセクション除去をしない"),
        ),
        (
            "convert.no-zip",
            invis(VarType::Boolean, "ZIP作成を行わない"),
        ),
        (
            "convert.make-zip",
            vis(
                VarType::Boolean,
                "変換時にZIPファイルを作成する(i文庫HD用)",
            ),
        ),
        (
            "convert.no-open",
            vis(
                VarType::Boolean,
                "変換後に出力フォルダを開かない",
            ),
        ),
        (
            "convert.inspect",
            vis(
                VarType::Boolean,
                "変換時に調査データを出力する",
            ),
        ),
        (
            "convert.multi-device",
            vis(
                VarType::String,
                "複数のデバイス形式で出力する(カンマ区切り)",
            ),
        ),
        (
            "convert.filename-to-ncode",
            vis(
                VarType::Boolean,
                "ファイル名をNcodeにする",
            ),
        ),
        (
            "convert.add-dc-subject-to-epub",
            vis(
                VarType::Boolean,
                "EPUBにdc:subjectタグを追加する",
            ),
        ),
        (
            "convert.dc-subject-exclude-tags",
            vis(
                VarType::String,
                "dc:subjectから除外するタグ(カンマ区切り)",
            ),
        ),
        (
            "download.interval",
            vis(
                VarType::Float,
                "ダウンロード時のウェイト間隔(秒)",
            ),
        ),
        (
            "download.wait-steps",
            vis(
                VarType::Integer,
                "ダウンロード時のウェイトステップ(何話毎にウェイトを入れるか)",
            ),
        ),
        (
            "download.use-subdirectory",
            vis(
                VarType::Boolean,
                "サブディレクトリにダウンロードする",
            ),
        ),
        (
            "download.choices-of-digest-options",
            vis(
                VarType::String,
                "ダイジェスト化された場合の自動選択肢(カンマ区切り)",
            ),
        ),
        (
            "send.without-freeze",
            vis(
                VarType::Boolean,
                "送信時に小説を凍結しない",
            ),
        ),
        (
            "send.backup-bookmark",
            vis(
                VarType::Boolean,
                "送信時にブックマーク情報のバックアップを作成する",
            ),
        ),
        (
            "multiple-delimiter",
            vis(
                VarType::String,
                "複数タグの区切り文字",
            ),
        ),
        (
            "economy",
            vis(
                VarType::Boolean,
                "経済モード（更新時のサイトアクセスを最小限にする）",
            ),
        ),
        (
            "guard-spoiler",
            vis(
                VarType::Boolean,
                "ネタバレ防止モード（更新結果一覧でサブタイトルを伏せる）",
            ),
        ),
        (
            "auto-add-tags",
            vis(
                VarType::String,
                "ダウンロード時に自動付与するタグ（カンマ区切り）",
            ),
        ),
        (
            "normalize-filename",
            vis(
                VarType::Boolean,
                "ファイル名にNFKC正規化を適用する",
            ),
        ),
        (
            "folder-length-limit",
            vis(
                VarType::Integer,
                "フォルダ名の最大文字数",
            ),
        ),
        (
            "filename-length-limit",
            vis(
                VarType::Integer,
                "ファイル名の最大文字数",
            ),
        ),
        (
            "ebook-filename-length-limit",
            vis(
                VarType::Integer,
                "電子書籍ファイル名の最大文字数",
            ),
        ),
        (
            "user-agent",
            vis(
                VarType::String,
                "ダウンロード時のUser-Agent(randomで毎回ランダム)",
            ),
        ),
        (
            "webui.theme",
            sel(
                "WEB UIのテーマ",
                vec![
                    "Cerulean", "Cosmo", "Cyborg", "Darkly", "Flatly", "Journal", "Lumen",
                    "Paper", "Readable", "Sandstone", "Simplex", "Slate", "Spacelab",
                    "Superhero", "United", "Yeti",
                ],
            ),
        ),
        (
            "webui.table.reload-timing",
            invis_sel(
                "テーブルの自動リロードタイミング",
                vec!["every", "distribution", "once", "none"],
            ),
        ),
        (
            "webui.performance-mode",
            invis_sel(
                "パフォーマンスモード",
                vec!["auto", "on", "off"],
            ),
        ),
    ];

    let global_vars = vec![
        (
            "aozoraepub3dir",
            invis(
                VarType::Directory,
                "AozoraEpub3の場所",
            ),
        ),
        (
            "line-height",
            invis(
                VarType::Float,
                "行の高さ",
            ),
        ),
        (
            "difftool",
            vis(
                VarType::String,
                "diffで使用する外部ツールのパス",
            ),
        ),
        (
            "difftool.arg",
            vis(
                VarType::String,
                "diffツールに渡す引数のフォーマット($OLDと$NEWが使用可能)",
            ),
        ),
        (
            "no-color",
            vis(
                VarType::Boolean,
                "カラー出力を無効にする",
            ),
        ),
        (
            "color-parser",
            vis(
                VarType::Boolean,
                "カラーパーサーを有効にする",
            ),
        ),
        (
            "server-port",
            vis(
                VarType::Integer,
                "WEB UIサーバーのポート番号",
            ),
        ),
        (
            "server-bind",
            vis(
                VarType::String,
                "WEB UIサーバーのバインドアドレス",
            ),
        ),
        (
            "server-basic-auth.enable",
            vis(
                VarType::Boolean,
                "Basic認証を有効にする",
            ),
        ),
        (
            "server-basic-auth.user",
            vis(
                VarType::String,
                "Basic認証のユーザー名",
            ),
        ),
        (
            "server-basic-auth.password",
            vis(
                VarType::String,
                "Basic認証のパスワード",
            ),
        ),
        (
            "server-ws-add-accepted-domains",
            vis(
                VarType::String,
                "WebSocket接続を許可する追加ドメイン",
            ),
        ),
        (
            "over18",
            vis(
                VarType::Boolean,
                "18歳以上であることを確認済みにする(R18サイトの確認ダイアログをスキップ)",
            ),
        ),
    ];

    SettingVariables {
        local: local_vars,
        global: global_vars,
    }
}
