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

pub fn default_local_setting_value(name: &str) -> Option<serde_yaml::Value> {
    match name {
        "convert.dc-subject-exclude-tags" => Some(serde_yaml::Value::String("404,end".to_string())),
        "download.interval" => serde_yaml::to_value(0.7f64).ok(),
        "download.wait-steps" => Some(serde_yaml::Value::Number(serde_yaml::Number::from(0))),
        "folder-length-limit" | "filename-length-limit" => {
            Some(serde_yaml::Value::Number(serde_yaml::Number::from(50)))
        }
        "time-zone" => Some(serde_yaml::Value::String("Asia/Tokyo".to_string())),
        "user-agent" => Some(serde_yaml::Value::String("auto".to_string())),
        _ => None,
    }
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
        | "ebook-filename-length-limit" | "time-zone" | "user-agent" => Some("detail"),

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

/// WEB UI theme names
pub const WEBUI_THEME_NAMES: &[&str] = &[
    "Cerulean",
    "Darkly",
    "Readable",
    "Slate",
    "Superhero",
    "United",
];

/// Command names that support `default_args.*`.
pub const DEFAULT_ARG_COMMAND_NAMES: &[&str] = &[
    "alias", "backup", "browser", "clean", "console", "convert", "csv", "diff", "download",
    "folder", "freeze", "help", "init", "inspect", "list", "log", "mail", "remove", "send",
    "setting", "tag", "trace", "update", "version", "web",
];

pub fn default_arg_command_names() -> &'static [&'static str] {
    DEFAULT_ARG_COMMAND_NAMES
}

pub fn is_known_default_arg_name(name: &str) -> bool {
    name.strip_prefix("default_args.")
        .is_some_and(|cmd| DEFAULT_ARG_COMMAND_NAMES.contains(&cmd))
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
            "enable_inspect",
            info(
                VarType::Boolean,
                "小説に対する各種調査を実行する。結果を表示するには narou inspect コマンドを使用",
            ),
        ),
        (
            "enable_convert_num_to_kanji",
            info(VarType::Boolean, "数字の漢数字変換を有効にする"),
        ),
        (
            "enable_kanji_num_with_units",
            info(VarType::Boolean, "漢数字変換した場合、千・万などに変換する"),
        ),
        (
            "kanji_num_with_units_lower_digit_zero",
            info(
                VarType::Integer,
                "〇(ゼロ)が最低この数字以上付いてないと千・万などをつける対象にしない",
            ),
        ),
        (
            "enable_alphabet_force_zenkaku",
            info(
                VarType::Boolean,
                "アルファベットを強制的に全角にする。false の場合は英文は半角、8文字未満の英単語は全角になる",
            ),
        ),
        (
            "disable_alphabet_word_to_zenkaku",
            info(
                VarType::Boolean,
                "enable_alphabet_force_zenkaku が false の場合に、8文字未満の英単語を全角にする機能を抑制する。英文中にルビがふってあり、英文ではなく英単語と認識されて全角化されてしまう場合などに使用",
            ),
        ),
        (
            "enable_half_indent_bracket",
            info(VarType::Boolean, "行頭かぎ括弧に二分アキを挿入する"),
        ),
        (
            "enable_auto_indent",
            info(
                VarType::Boolean,
                "自動行頭字下げ機能。行頭字下げが行われているかを判断し、適切に行頭字下げをするか",
            ),
        ),
        (
            "enable_force_indent",
            info(
                VarType::Boolean,
                "行頭字下げを必ず行うか。enable_auto_indent の設定は無視される",
            ),
        ),
        (
            "enable_auto_join_in_brackets",
            info(
                VarType::Boolean,
                "かぎ括弧内自動連結を有効にする\n例)\n「～～～！\n　＊＊＊？」  → 「～～～！　＊＊＊？」",
            ),
        ),
        (
            "enable_auto_join_line",
            info(
                VarType::Boolean,
                "行末が読点で終わっている部分を出来るだけ連結する",
            ),
        ),
        (
            "enable_enchant_midashi",
            info(
                VarType::Boolean,
                "［＃改ページ］直後の行に中見出しを付与する（テキストファイルを直接変換する場合のみの設定）",
            ),
        ),
        (
            "enable_author_comments",
            info(
                VarType::Boolean,
                "作者コメントを検出する（テキストファイルを直接変換する場合のみの設定）",
            ),
        ),
        (
            "enable_erase_introduction",
            info(VarType::Boolean, "前書きを削除する"),
        ),
        (
            "enable_erase_postscript",
            info(VarType::Boolean, "後書きを削除する"),
        ),
        (
            "enable_ruby",
            info(VarType::Boolean, "ルビ処理を有効にする"),
        ),
        (
            "enable_illust",
            info(VarType::Boolean, "挿絵タグを有効にする（false なら削除）"),
        ),
        (
            "enable_transform_fraction",
            info(
                VarType::Boolean,
                "○／×表記を×分の○表記に変換する。日付表記(10/23)と誤爆しやすいので注意",
            ),
        ),
        (
            "enable_transform_date",
            info(
                VarType::Boolean,
                "日付表記(20yy/mm/dd)を任意の形式(date_formatで指定)に変換する",
            ),
        ),
        (
            "date_format",
            info(VarType::String, "書式は http://bit.ly/date_format を参考"),
        ),
        (
            "enable_convert_horizontal_ellipsis",
            info(
                VarType::Boolean,
                "中黒(・)を並べて三点リーダーもどきにしているのを三点リーダーに変換する",
            ),
        ),
        (
            "enable_convert_page_break",
            info(
                VarType::Boolean,
                "`to_page_break_threshold` で設定した個数以上連続する空行を改ページに変換する",
            ),
        ),
        (
            "to_page_break_threshold",
            info(
                VarType::Integer,
                "ここで設定した値が `enable_convert_page_break` に反映される",
            ),
        ),
        (
            "enable_dakuten_font",
            info(
                VarType::Boolean,
                "濁点表現をNarou.rbで処理する(濁点フォントを使用する)。false の場合はAozoraEpub3に任せる",
            ),
        ),
        (
            "enable_display_end_of_book",
            info(VarType::Boolean, "小説の最後に本を読み終わった表示をする"),
        ),
        (
            "enable_add_date_to_title",
            info(
                VarType::Boolean,
                "変換後の小説のタイトルに最新話掲載日や更新日等の日付を付加する",
            ),
        ),
        (
            "title_date_format",
            info(
                VarType::String,
                "enable_add_date_to_title で付与する日付のフォーマット。書式は http://bit.ly/date_format を参照。\nNarou.rb専用の書式として下記のものも使用可能。\n$t 小説のタイトル($tを使った場合はtitle_date_alignは無視される)\n$s 2045年までの残り時間(10分単位の4桁の36進数)\n$ns 小説が掲載されているサイト名\n$nt 小説種別（短編 or 連載）\n$ntag 小説のタグをカンマ区切りにしたもの",
            ),
        ),
        (
            "title_date_align",
            select(
                "enable_add_date_to_title が有効な場合に付与される日付の位置。left(タイトルの前) か right(タイトルの後)。title_date_format で $t を使用した場合この設定は無視される",
                vec!["left", "right"],
            ),
        ),
        (
            "title_date_target",
            select(
                "enable_add_date_to_title で付与する日付の種類。\ngeneral_lastup(最新話掲載日),last_update(更新日),new_arrivals_date(新着を確認した日),convert(変換した日)",
                vec![
                    "general_lastup",
                    "last_update",
                    "new_arrivals_date",
                    "convert",
                ],
            ),
        ),
        (
            "enable_ruby_youon_to_big",
            info(
                VarType::Boolean,
                "ルビの拗音(ぁ、ぃ等)を商業書籍のように大きくする",
            ),
        ),
        (
            "enable_pack_blank_line",
            info(VarType::Boolean, "縦書きで読みやすいように空行を減らす"),
        ),
        (
            "enable_kana_ni_to_kanji_ni",
            info(
                VarType::Boolean,
                "漢字の二と間違えてカタカナのニを使っていそうなのを、漢字に直す",
            ),
        ),
        (
            "enable_insert_word_separator",
            info(
                VarType::Boolean,
                "単語選択がしやすいように単語単位の区切りデータを挿入する（Kindle専用）※Kindle ファームウェア 5.9.6.1 から MOBI ファイルでも単語選択が可能になったので、この機能を使う必要がなくなりました",
            ),
        ),
        (
            "enable_insert_char_separator",
            info(
                VarType::Boolean,
                "文字選択がしやすいように１文字ずつ区切りデータを挿入する（Kindle専用。enable_insert_word_separator が有効な場合無この設定は無視される）※Kindle ファームウェア 5.9.6.1 から MOBI ファイルでも単語選択が可能になったので、この機能を使う必要がなくなりました",
            ),
        ),
        (
            "enable_strip_decoration_tag",
            info(
                VarType::Boolean,
                "HTMLの装飾系タグを削除する（主にArcadiaの作品に影響）",
            ),
        ),
        (
            "enable_add_end_to_title",
            info(VarType::Boolean, "完結済み小説のタイトルに(完結)と表示する"),
        ),
        (
            "enable_prolonged_sound_mark_to_dash",
            info(
                VarType::Boolean,
                "長音記号を２つ以上つなげている場合に全角ダッシュに置換する",
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

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn default_arg_command_names_cover_web_and_alias() {
        assert!(default_arg_command_names().contains(&"web"));
        assert!(default_arg_command_names().contains(&"alias"));
        assert!(is_known_default_arg_name("default_args.convert"));
        assert!(!is_known_default_arg_name("default_args.not_exists"));
    }

    #[test]
    fn external_bind_auth_override_has_no_webui_tab() {
        assert_eq!(
            tab_for_setting("server-basic-auth.require-for-external-bind"),
            None
        );
        assert!(setting_variables()
            .get("server-basic-auth.require-for-external-bind")
            .is_some());
    }
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
                "ログファイル名のフォーマット。日付でファイルを分けたくなければ固定ファイル名にする。書式は http://bit.ly/date_format 参照",
            ),
        ),
        (
            "logging.format-timestamp",
            vis(
                VarType::String,
                "ログ内のタイムスタンプのフォーマット。タイムスタンプを記録したくなければ $none とだけ入力",
            ),
        ),
        (
            "update.interval",
            vis(
                VarType::Float,
                "更新時に各作品間で指定した秒数待機する(処理時間を含む)",
            ),
        ),
        (
            "update.strong",
            vis(
                VarType::Boolean,
                "改稿日当日の連続更新でも更新漏れが起きないように、中身もチェックして更新を検知する(やや処理が重くなる)",
            ),
        ),
        (
            "update.convert-only-new-arrival",
            vis(VarType::Boolean, "更新時に新着がある場合のみ変換を実行する"),
        ),
        (
            "update.sort-by",
            sel(
                "アップデートを指定した項目順で行う",
                vec![
                    "id",
                    "last_update",
                    "title",
                    "author",
                    "new_arrivals_date",
                    "general_lastup",
                ],
            ),
        ),
        (
            "update.auto-schedule.enable",
            vis(VarType::Boolean, "自動アップデート機能を有効にする"),
        ),
        (
            "update.auto-schedule",
            vis(
                VarType::String,
                "自動アップデートする時間を指定する。カンマ区切りで複数指定可能。\n      書式：HHMM (例: 0800,1200,1800 = 8時、12時、18時)",
            ),
        ),
        (
            "convert.copy-to",
            vis(
                VarType::Directory,
                "変換したらこのフォルダにコピーする\n      ※注意：存在しないフォルダだとエラーになる",
            ),
        ),
        (
            "convert.copy-zip-to",
            vis(
                VarType::Directory,
                "生成したZIPファイルをこのフォルダにコピーする\n      ※注意：存在しないフォルダだとエラーになる",
            ),
        ),
        (
            "convert.copy-to-grouping",
            multi(
                "copy-toで指定したフォルダの中で更に指定の各種フォルダにまとめる",
                vec!["device", "site"],
            ),
        ),
        (
            "convert.copy_to",
            invis(VarType::Directory, "copy-toの昔の書き方(非推奨)"),
        ),
        (
            "convert.no-epub",
            invis(VarType::Boolean, "EPUB変換を無効にする"),
        ),
        (
            "convert.no-mobi",
            invis(VarType::Boolean, "MOBI変換を無効にする"),
        ),
        (
            "convert.no-strip",
            invis(VarType::Boolean, "MOBIのstripを無効にする"),
        ),
        (
            "convert.no-zip",
            invis(VarType::Boolean, "i文庫用のzipファイル作成を無効にする"),
        ),
        (
            "convert.make-zip",
            vis(
                VarType::Boolean,
                "ZIPファイルの作成を有効にする（対応端末: i文庫）",
            ),
        ),
        (
            "convert.no-open",
            vis(VarType::Boolean, "変換時に保存フォルダを開かないようにする"),
        ),
        (
            "convert.inspect",
            vis(VarType::Boolean, "常に変換時に調査結果を表示する"),
        ),
        (
            "convert.multi-device",
            multi(
                "複数の端末用に同時に変換する。deviceよりも優先される。端末名をカンマ区切りで入力。ただのEPUBを出力したい場合はepubを指定",
                vec!["kindle", "kobo", "epub", "ibunko", "reader", "ibooks"],
            ),
        ),
        (
            "convert.filename-to-ncode",
            vis(
                VarType::Boolean,
                "書籍ファイル名をNコードで出力する(ドメイン_Nコードの形式)",
            ),
        ),
        (
            "convert.add-dc-subject-to-epub",
            vis(
                VarType::Boolean,
                "EPUB変換時にstandard.opfファイルにdc:subject要素を追加する。小説のタグ情報がdc:subjectとして埋め込まれます",
            ),
        ),
        (
            "convert.dc-subject-exclude-tags",
            vis(
                VarType::String,
                "dc:subjectから除外するタグをカンマ区切りで指定する。初期値は「404,end」（初回実行時に自動設定される）。すべてのタグを埋め込みたい場合は空文字列を設定",
            ),
        ),
        (
            "download.interval",
            vis(VarType::Float, "各話DL時に指定秒数待機する"),
        ),
        (
            "download.wait-steps",
            vis(
                VarType::Integer,
                "指定した話数ごとに長めのウェイトが入る\n      ※注意：11以上を設定してもなろうの場合は10話ごとにウェイトが入ります",
            ),
        ),
        (
            "download.use-subdirectory",
            vis(
                VarType::Boolean,
                "小説を一定数ごとにサブフォルダへ分けて保存する",
            ),
        ),
        (
            "download.choices-of-digest-options",
            vis(
                VarType::String,
                "ダイジェスト化選択肢が出た場合に自動で項目を選択する",
            ),
        ),
        (
            "send.without-freeze",
            vis(VarType::Boolean, "送信時に凍結された小説は対象外にする"),
        ),
        (
            "send.backup-bookmark",
            vis(
                VarType::Boolean,
                "一括送信時に栞データを自動でバックアップする(KindlePW系用)",
            ),
        ),
        (
            "multiple-delimiter",
            vis(VarType::String, "--multiple指定時の区切り文字"),
        ),
        (
            "economy",
            multi(
                "容量節約に関する設定。カンマ区切りで設定\n(cleanup_temp:変換後に作業ファイルを削除 send_delete:送信後に書籍ファイルを削除 nosave_diff:差分ファイルを保存しない nosave_raw:rawデータを保存しない)",
                vec!["cleanup_temp", "send_delete", "nosave_diff", "nosave_raw"],
            ),
        ),
        (
            "guard-spoiler",
            vis(
                VarType::Boolean,
                "ネタバレ防止機能。ダウンロード時の各話タイトルを伏せ字で表示する",
            ),
        ),
        (
            "auto-add-tags",
            vis(
                VarType::Boolean,
                "サイトから取得したタグを自動的に小説データに追加する",
            ),
        ),
        (
            "normalize-filename",
            vis(
                VarType::Boolean,
                "ファイル名の文字列をNFCで正規化する。※既存データとの互換性が無くなる可能性があるので、バックアップを取った上で機能を理解の上有効にして下さい",
            ),
        ),
        (
            "folder-length-limit",
            vis(
                VarType::Integer,
                "小説を格納するフォルダ名の長さを制限する。デフォルトは50文字",
            ),
        ),
        (
            "filename-length-limit",
            vis(
                VarType::Integer,
                "各話保存時のファイル名の長さを制限する。出力される電子書籍ファイル名の長さを制限する場合は ebook-filename-length-limit を設定すること。※この設定は既存小説にも影響が出るのでファイル名の長さでエラーが出ない限り基本的にはいじらないこと。デフォルトは50文字",
            ),
        ),
        (
            "ebook-filename-length-limit",
            vis(
                VarType::Integer,
                "出力される電子書籍ファイル名の長さを制限する。保存時に長さでエラーが出る場合などに設定する。※デフォルトは無制限",
            ),
        ),
        (
            "user-agent",
            vis(
                VarType::String,
                "User-Agent 設定\n未指定時 auto",
            ),
        ),
        (
            "time-zone",
            vis(
                VarType::String,
                "サイト側日時にタイムゾーン表記がない場合の既定タイムゾーン。例: Asia/Tokyo",
            ),
        ),
        (
            "webui.theme",
            invis_sel("WEB UI 用テーマ選択", WEBUI_THEME_NAMES.to_vec()),
        ),
        (
            "webui.table.reload-timing",
            invis_sel(
                "小説リストの更新タイミングを選択。未設定時は１作品ごとに更新",
                vec!["every", "queue"],
            ),
        ),
        (
            "webui.performance-mode",
            sel(
                "パフォーマンスモードを設定。autoの場合は小説数2000件以上で自動的に有効になります",
                vec!["auto", "on", "off"],
            ),
        ),
    ];

    let global_vars = vec![
        (
            "aozoraepub3dir",
            invis(VarType::Directory, "AozoraEpub3のあるフォルダを指定"),
        ),
        (
            "line-height",
            invis(
                VarType::Float,
                "行間サイズ(narou init から指定しないと反映されません)",
            ),
        ),
        (
            "difftool",
            vis(VarType::String, "diffで使うツールのパスを指定する"),
        ),
        (
            "difftool.arg",
            vis(VarType::String, "difftoolで使う引数を設定(オプション)"),
        ),
        ("no-color", vis(VarType::Boolean, "カラー表示を無効にする")),
        (
            "color-parser",
            sel(
                "コンソール上でのANSIカラーを表示する方法の選択(Windowsのみ)。system: システムに任せる(デフォルト) / self: Narou.rbで処理",
                vec!["system", "self"],
            ),
        ),
        (
            "server-port",
            vis(
                VarType::Integer,
                "WEBサーバ起動時のポート。server-port + 1 のポートも WebSocket で使用",
            ),
        ),
        (
            "server-bind",
            invis(VarType::String, "WEBサーバのホスト制限(未設定時:起動PCのIP)。頻繁にローカルIPが変わってしまう場合は127.0.0.1の指定を推奨"),
        ),
        (
            "server-basic-auth.enable",
            invis(VarType::Boolean, "WEBサーバでBasic認証を使用するかどうか"),
        ),
        (
            "server-basic-auth.user",
            invis(VarType::String, "WEBサーバでBasic認証をするユーザ名"),
        ),
        (
            "server-basic-auth.password",
            invis(VarType::String, "WEBサーバのBasic認証のパスワード"),
        ),
        (
            "server-basic-auth.require-for-external-bind",
            invis(
                VarType::Boolean,
                "外部公開bind時にBasic認証未設定での起動を拒否するかどうか",
            ),
        ),
        (
            "server-ws-add-accepted-domains",
            invis(
                VarType::String,
                "PushServer の accepted_domains に追加するホストのリスト（カンマ区切り）",
            ),
        ),
        ("over18", invis(VarType::Boolean, "18歳以上かどうか")),
    ];

    SettingVariables {
        local: local_vars,
        global: global_vars,
    }
}

/// WEB UI specific help-text overrides.
/// Matches narou.rb's `SETTING_VARIABLES_WEBUI_MESSAGES`.
/// `%%ORIG%%` is replaced with the base help text at lookup time.
pub fn webui_help_override(name: &str, base_help: &str) -> Option<String> {
    let raw = match name {
        "convert.multi-device" => "複数の端末用に同時に変換する。deviceよりも優先される。\nただのEPUBを出力したい場合はepubを指定",
        "device" => "変換、送信対象の端末",
        "difftool" => "%%ORIG%%。※WEB UIでは使われません",
        "update.sort-by" => "アップデートを指定した項目順で行う",
        "default.title_date_align" => "enable_add_date_to_title で付与する日付の位置",
        "force.title_date_align" => "enable_add_date_to_title で付与する日付の位置",
        "difftool.arg" => "difftoolで使う引数(指定しなければ単純に新旧ファイルを引数に呼び出す)\n特殊な変数\n<b>%NEW</b> : 最新データの差分用ファイルパス\n<b>%OLD</b> : 古い方の差分用ファイルパス",
        "no-color" => "コンソールのカラー表示を無効にする\n※要サーバ再起動",
        "economy" => "容量節約に関する設定",
        "send.without-freeze" => "一括送信時に凍結された小説は対象外にする。（個別送信時は凍結済みでも送信可能）",
        "server-basic-auth.enable" => "%%ORIG%%\n※basic-auth関連の設定を変更した場合サーバの再起動が必要",
        "concurrency" => "%%ORIG%% ※要サーバ再起動",
        "logging" => "%%ORIG%%\n※要サーバ再起動",
        "logging.format-filename" => "%%ORIG%%\n※要サーバ再起動",
        "logging.format-timestamp" => "%%ORIG%%\n※要サーバ再起動",
        "auto-add-tags" => "小説サイトから取得したタグを自動的に小説データに追加する",
        "convert.add-dc-subject-to-epub" => "EPUB変換時にstandard.opfファイルにdc:subject要素を追加する。\n小説のタグ情報がdc:subjectとして埋め込まれ、\n電子書籍リーダーでの検索やカテゴリ分類に活用できます。\n除外するタグは下の設定で指定できます",
        "convert.dc-subject-exclude-tags" => "dc:subjectに埋め込まないタグをカンマ区切りで指定します。\n<b>初期値:</b> 404,end（初回実行時に自動設定）\n<b>404:</b> 削除された小説に付くタグ\n<b>end:</b> 完結を示すタグ\n※すべてのタグを埋め込みたい場合は空欄にしてください",
        "convert.copy-zip-to" => "i文庫用などで生成したZIPを、変換完了時にコピーするフォルダを指定",
        "convert.make-zip" => "ZIPファイルを出力するかどうか（対応端末: i文庫）",
        _ => return None,
    };
    Some(raw.replace("%%ORIG%%", base_help))
}
