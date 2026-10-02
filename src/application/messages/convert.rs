//! `narou convert` が出す文言。実行順の案内・完了・エラー行と、
//! `--enc` 系や dc:subject 埋め込みのエラー文言をここに集約する。

use std::fmt::Display;

/// 選択端末に向けた変換予告 (呼び出し側で magenta にする)。
pub fn converting_for(device: impl Display) -> String {
    format!(">> {device}用に変換します")
}

pub fn convert_started(total: usize) -> String {
    format!("変換処理開始: {total}件の小説を処理します")
}

pub fn processing(index: usize, total: usize, target: impl Display) -> String {
    format!("[{index}/{total}] 処理中: {target}")
}

pub fn completed(index: usize, total: usize, target: impl Display) -> String {
    format!("[{index}/{total}] 完了: {target}")
}

pub fn item_error(
    index: usize,
    total: usize,
    target: impl Display,
    error: impl Display,
) -> String {
    format!("[{index}/{total}] エラー: {target} - {error}")
}

pub fn convert_finished(completed: usize, total: usize) -> String {
    format!("変換処理完了: {completed}/{total}件が正常に変換されました")
}

/// DB 上の ID に紐付かない対象 (先頭の "  Error:" 込みで出していたので
/// 字下げ込みで保持)。
pub fn id_missing(id: impl Display) -> String {
    format!("  Error: ID: {id} は存在しません")
}

pub fn output_written(filename: impl Display) -> String {
    format!("{filename} を出力しました")
}

/// 色付きで出す報告行 (呼び出し側で green)。
pub fn epub_written() -> &'static str {
    "EPUBファイルを出力しました"
}

pub fn mobi_written() -> &'static str {
    "MOBIファイルを出力しました"
}

/// convert.multi-device の不正な端末名。
pub fn invalid_device_name(name: impl Display) -> String {
    format!("[convert.multi-device] {name} は有効な端末名ではありません")
}

pub fn no_valid_devices() -> &'static str {
    "有効な端末名がひとつもありませんでした"
}

// --- dc:subject 埋め込み ---

pub fn dc_subject_added(subjects: impl Display) -> String {
    format!("dc:subjectを追加しました: {subjects}")
}

pub fn dc_subject_error(error: impl Display) -> String {
    format!("dc:subject追加中にエラーが発生しました: {error}")
}

pub fn dc_subject_continue() -> &'static str {
    "dc:subject埋め込み処理に失敗しましたが、変換を続行します"
}

// --- ZIP コピー ---

pub fn zip_copied_to(path: impl Display) -> String {
    format!("{path} へZIPをコピーしました")
}

/// ZIP のコピー先が無い/消えた (末尾が「ZIPをコピー出来ませんでした」)。
pub fn zip_copy_dest_not_dir(dir: impl Display) -> String {
    format!("{dir} はフォルダではないかすでに削除されています。ZIPをコピー出来ませんでした")
}

// --- EPUB (dc:subject) 操作のエラー文言 ---

pub fn opf_missing() -> &'static str {
    "standard.opfファイルが見つかりませんでした"
}

pub fn metadata_close_missing() -> &'static str {
    "</metadata> が見つかりませんでした"
}

pub fn mimetype_missing() -> &'static str {
    "mimetypeファイルが見つかりません"
}

pub fn invalid_converted_filename() -> &'static str {
    "Invalid converted filename"
}

pub fn invalid_zip_filename() -> &'static str {
    "Invalid ZIP filename"
}

// --- テキスト入力 (--enc) 系のエラー文言 ---

/// `--enc` の未知の文字コード。
pub fn enc_invalid() -> &'static str {
    "--enc で指定された文字コードは存在しません。sjis, eucjp, utf-8 等を指定して下さい"
}

/// --enc 未指定で UTF-8 以外のテキスト。
pub fn text_not_utf8() -> &'static str {
    "テキストファイルの文字コードがUTF-8ではありません。--enc オプションでテキストの文字コードを指定して下さい"
}

/// 指定した --enc と実際の文字コードが違う (2 行目が指摘行)。
pub fn encoding_mismatch(path: impl Display, label: impl Display) -> String {
    format!(
        "{path}:\nテキストファイルの文字コードは{label}ではありませんでした。\n正しい文字コードを指定して下さい"
    )
}

#[cfg(test)]
mod tests {
    #[test]
    fn fixed_literals_match_native() {
        assert_eq!(super::no_valid_devices(), "有効な端末名がひとつもありませんでした");
        assert_eq!(super::epub_written(), "EPUBファイルを出力しました");
        assert_eq!(super::mobi_written(), "MOBIファイルを出力しました");
        assert_eq!(
            super::dc_subject_continue(),
            "dc:subject埋め込み処理に失敗しましたが、変換を続行します"
        );
        assert_eq!(super::opf_missing(), "standard.opfファイルが見つかりませんでした");
        assert_eq!(super::metadata_close_missing(), "</metadata> が見つかりませんでした");
        assert_eq!(super::mimetype_missing(), "mimetypeファイルが見つかりません");
        assert_eq!(super::invalid_converted_filename(), "Invalid converted filename");
        assert_eq!(super::invalid_zip_filename(), "Invalid ZIP filename");
        assert_eq!(
            super::enc_invalid(),
            "--enc で指定された文字コードは存在しません。sjis, eucjp, utf-8 等を指定して下さい"
        );
        assert_eq!(
            super::text_not_utf8(),
            "テキストファイルの文字コードがUTF-8ではありません。--enc オプションでテキストの文字コードを指定して下さい"
        );
    }

    #[test]
    fn progress_lines_match_native() {
        assert_eq!(super::converting_for("kindle"), ">> kindle用に変換します");
        assert_eq!(super::convert_started(3), "変換処理開始: 3件の小説を処理します");
        assert_eq!(super::processing(2, 3, "n1"), "[2/3] 処理中: n1");
        assert_eq!(super::completed(2, 3, "n1"), "[2/3] 完了: n1");
        assert_eq!(super::item_error(2, 3, "n1", "e"), "[2/3] エラー: n1 - e");
        assert_eq!(
            super::convert_finished(2, 3),
            "変換処理完了: 2/3件が正常に変換されました"
        );
        assert_eq!(super::id_missing(42), "  Error: ID: 42 は存在しません");
        assert_eq!(super::output_written("a.epub"), "a.epub を出力しました");
        assert_eq!(
            super::invalid_device_name("bogus"),
            "[convert.multi-device] bogus は有効な端末名ではありません"
        );
    }

    #[test]
    fn embed_and_zip_lines_match_native() {
        assert_eq!(
            super::dc_subject_added("a, b"),
            "dc:subjectを追加しました: a, b"
        );
        assert_eq!(
            super::dc_subject_error("io"),
            "dc:subject追加中にエラーが発生しました: io"
        );
        assert_eq!(super::zip_copied_to("/tmp/a.zip"), "/tmp/a.zip へZIPをコピーしました");
        assert_eq!(
            super::zip_copy_dest_not_dir("/out"),
            "/out はフォルダではないかすでに削除されています。ZIPをコピー出来ませんでした"
        );
        assert_eq!(
            super::encoding_mismatch("/tmp/x.txt", "utf-8"),
            "/tmp/x.txt:\nテキストファイルの文字コードはutf-8ではありませんでした。\n正しい文字コードを指定して下さい"
        );
    }
}
