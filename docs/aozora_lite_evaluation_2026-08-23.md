# AozoraEpub3_Lite 代替性評価 (2026-08-23)

Java 版 AozoraEpub3 の代替として [AozoraEpub3_Lite](https://github.com/Rumia-Channel/AozoraEpub3_Lite.git) を使えるかを検証した調査記録。

## 結論

**代替として使用可能。** narou.rs の CLI 引数 (`-enc UTF-8 -of [-c 0] -dst <dir> [-ext .kepub.epub] [-hor] <txt>`) をそのまま受理し、エンドツーエンドの `convert` もコード無変更で動作する。ただし以下の注意がある。

1. Java 版と出力は完全一致ではない。差分は5系統 (下記)。実質的な問題は全角マイナス `－`(U+FF0D) を `―`(U+2015) へ勝手に変換する点のみ。
2. ~~`aozoraepub3dir` 設定経由では選択できない~~ → **実装済み (2026-08-25)**: `canonicalize_aozoraepub3_tool_path` (`src/compat.rs`) が jar を優先しつつ `AozoraEpub3_Lite.exe` / `AozoraEpub3.exe` / 拡張子なしバイナリを受理する。設定で Lite ディレクトリを直接指定可能。
3. 実装前の暫定経路: 設定の `aozoraepub3dir` を無効化 → Lite 実行ファイルを **`AozoraEpub3.exe` にリネーム**して PATH 配置 → `OutputManager::find_external_tool` の `where` フォールバックで発見・実行。
4. ~~正式対応するなら compat.rs / device.rs の小改修が妥当~~ → 実装完了。E2E (`convert n8021mo`, 設定→deploy ディレクトリ) で直接 CLI 実行と byte 等価を再確認。

## テスト環境

- Java 版: `C:/Users/rumia/Documents/AozoraEpub3` (`java -cp AozoraEpub3.jar AozoraEpub3 ...`)
- Lite: 上記リポジトリを clone → `cargo build --release`
- 配置: exe と `assets/aozora/` (chuki_*.txt, template, presets, gaiji) を同じディレクトリに展開 (本家と同レイアウト)
- 検証データ: `C:/Users/rumia/Documents/WebNocel/小説データ/` 配下の実データ
- 成果物: `C:/Users/rumia/Documents/WebNocel/aozora_lite_test/` (ローカルのみ、リポジトリ外)

## テスト結果マトリクス

| テスト | Java | Lite | 結果 |
|---|---|---|---|
| smoke / dash_test (自作テキスト) | ✅ | ✅ | EPUB 生成成功 |
| n8021mo 光の記憶 (15KB·4節·挿絵なし) | ✅ | ✅ | ファイル構成同一 |
| n0449mj 牌アガる！(113KB·26節·挿絵14枚+`-c 0`) | ✅ | ✅ | 画像14枚すべて byte 一致 |
| n0316gv 闘争饗宴集 (3.2MB·大規模) | ✅ | ✅ | xhtml/css 97ファイル中56一致 |
| `-hor` / `-ext .kepub.epub` / `-of` / `-dst` / `-enc UTF-8` | ✅ | ✅ | 受理 |
| narou.rs `convert` エンドツーエンド (PATH フォールバック) | ✅ | ✅ | 直接実行と完全一致 |

## 出力差分の詳細 (5系統)

EPUB 解凍後、CR 差を除いて byte 比較。闘争饗宴集では xhtml 89 + css 8 = 97ファイル中 56一致 / 41不一致。不一致は4パターンに集約される。

### B. ダッシュ文字変換 [実質問題はこれのみ]

Lite 内部ロジック (replace.txt ではない) が `－`(U+FF0D 全角マイナス) を `―`(U+2015 水平バー) に変換する。Java は保持。

```diff
- <p>マイ「－－－ンゥノコッタァァァッ…」</p>
+ <p>マイ「―――ンゥノコッタァァァッ…」</p>
- <p>美紀－ｓｉｄｅ－</p>
+ <p>美紀―ｓｉｄｅ―</p>
```

見た目は類似だが文字コードが変わるため reader 内検索・コピーで `－` が引っかからなくなる。該当3ファイル (0003, 0004, 0045)。

### A. 節末尾の空行追加 (36ファイル)

```diff
  （本文の最終行）
+ <p><br/></p>
  </div>
```

各話末尾に空行1行分が増えるだけ。それ以外は完全一致。

### C. 巻末ブロックの p タグラップ (最終話)

```diff
- <div class="btm"><span class="kogaki">（本を読み終わりました）</span></div>
+ <div class="btm">
+ <p><span class="kogaki">（本を読み終わりました）</span></p>
+ </div>
```

### D. @page margin CSS 差

```diff
  @page {
-   margin: 0 0 0 0;
+   margin: 0.5em 0.5em 0.5em 0.5em;   /* Lite epub.rs ハードコード */
  }
```

`@page` 余白を見る reader で版面外側余白が変わる。

### E. 表紙処理 (逆に Lite が改善)

n0449mj (`-c 0`) で比較。画像本体は14枚すべて byte 一致だが、Lite は EPUB3 標準の `properties="cover-image"` と SVG cover ページ (`xhtml/cover.xhtml` + spine 登録) を追加する。Java 版はどちらも出さない。

### toc.ncx について

大作 (闘争饗宴集) では uuid/date 以外完全一致。ただし n8021mo (光の記憶) で先頭セクションの navLabel が Java=最初の見出し (`第１話　序章…`) / Lite=書籍タイトルになるケースを確認。条件依存のため要観察。

## narou.rs 側の呼び出し契約 (現状コード)

- 引数構築: `build_aozora_epub3_args` (`src/converter/device.rs:303`) — `-enc UTF-8`, `-of`, `[`-device kindle`]`, `[`-c 0`]`, `-dst <abs>`, `[`-ext .kepub.epub`]`, `[`-hor`]`, 入力 txt
- 実行: `.jar` なら `java -cp jar AozoraEpub3`、それ以外は直接 spawn。cwd はツールの親ディレクトリ (アセットはそこから読まれる)
- 発見順序 (`find_external_tool`, device.rs:186): settings (`aozoraepub3dir` → **jar 必須**) → kindlegen 専用経路 → `where`/`which` PATH → `C:\Tools\<name>\` 固定候補
- Windows リスクパス (〜/‼ 等) 時は temp ワークスペースへ入出力をコピー (`prepare_aozora_invocation`)
- 濁点注記時は `DakutenFontGuard` が aozora ディレクトリへ DMincho.ttf + CSS を書き込む (Java 向け機構。Lite は独自の `gaiji/dakuten` 機構を持つ。併存の影響は未検証)

## 切り替え手順 (コード無変更・現状確認済み)

```powershell
# 1. Lite ビルド & 配置 (本家と同レイアウト)
git clone https://github.com/Rumia-Channel/AozoraEpub3_Lite.git
cd AozoraEpub3_Lite; cargo build --release
mkdir C:/path/to/tools; cp target/release/AozoraEpub3_Lite.exe C:/path/to/tools/AozoraEpub3.exe   # 名前変更必須
cp -r assets/aozora C:/path/to/tools/

# 2. global_setting.yaml の aozoraepub3dir をコメントアウト (C:/Users/<user>/.narousetting/)
#    ※ 残すと jar チェックで java 経路が優先される
# 3. tools ディレクトリを PATH へ追加 (Windows 形式セミコロン区切りで)
```

注意: MSYS/git-bash の `PATH="/c/...:$PATH"` (コロン区切り) だと Rust 側 `where` が解決しない。Windows 形式 (`C:/...;%PATH%`) で渡すこと。

## 正式対応 (実装済み 2026-08-25)

- `canonicalize_aozoraepub3_jar_dir` を `canonicalize_aozoraepub3_tool_path` に改称し、「jar があれば jar、なければ `AozoraEpub3_Lite.exe` / `AozoraEpub3.exe` / 拡張子なしバイナリ」の解決を実装。単体テスト3件追加 (`src/compat.rs` tests)
- `build_aozora_command` は変更不要だった (非 jar を直接 spawn 済み)。device.rs は呼び出し名の更新のみ
- kindlegen 探索 (`find_kindlegen_next_to_aozora`) はツールパスの親ディレクトリを見るため jar/exe 両対応。PATH 経由時のみ Kindle Previewer フォールバックに依存
- E2E: `aozoraepub3dir` → Lite deploy ディレクトリを指定して `convert n8021mo` が成功、出力は Lite 直接実行と byte 等価

## ライブラリ組み込み実装 (2026-08-25, feature `lite`)

- cargo feature `lite`: `aozora_epub3_lite` を git 依存 (rev `8e0e3f6` に pin) で追加。**`worker-runtime` は `lite` を自動的に内包**し、Worker ビルドは常にライブラリ変換になる。GPL-3.0-only のため CI 成果物は `-GPL` 付きで頒布する
- `src/epub_lite.rs`: chuki テーブル7種 + replace.txt を `include_str!` 埋め込みした `embedded_config()`、テキスト→`EpubBook` 組立 (`build_book`)、seek 不要の ZIP data descriptor 書き出し (`stream_epub`)。画像は provider 経由で書き出し時に都度解決 (省メモリ)。単体テスト6件 (+`NAROU_EPUB_SMOKE_TXT` で実データ smoke を opt-in)
- Worker: `GET /api/novels/:id/download.epub` — ObjectStore 上の `novel.txt` (= 新設 `NovelObjectKeys::converted_text()`) を DL 時に EPUB 化して返す。挿絵は 512 枚 / 64 MiB 上限の prefetch 後にメモリ解決。未生成時は 409
- Native Web: 既存 `/novels/{id}/download` で EPUB が見つからない場合、`lite` ビルドなら変換済み txt からその場で EPUB 生成して返す
- Native 変換後、固定名ミラー `novel.txt` を小説ディレクトリへ併せて書き出し (feature `lite` 時)。ObjectStore レイアウト経由で Worker と同じキーで参照できる
- CI: platform.yml に `native-gpl` job (`--features lite` の check/test/build)。release.yml は全8プラットフォームに GPL 版を追加 (`narou_rs_{plat}_{arch}-GPL.zip`)、package-release.ps1 に `-Variant` 引数を追加。タグ push に加え `workflow_dispatch` で任意ブランチから両 variant の zip を生成可能 (dispatch 時は GitHub Release を作成しない)

### 更新 (2026-09-25): v0.1.4 で濁点フォントを組み込みエンジンでも使う

外部ツール (`DakutenFontGuard`) は `aozoraepub3dir/template/OPS/fonts/DMincho.ttf`
と `css_custom/vertical_font.css` を流し込んで効かせるが、組み込みエンジンはその
ディレクトリを見ていなかった。crate 側 (v0.1.4) が呼び出し側の `style/*.css`
アセットを本文からリンクするようになったので、narou は同じ内容を
`EpubBuildOptions::extra_assets` で渡す。

- pin: `aozora_epub3_lite` = `cd67ddb` (v0.1.4)。`Cargo.toml` の `rev` を書き換えて
  `cargo update -p aozora_epub3_lite`
- 設定 `convert.epub-font`: `auto` (既定。濁点注記のある小説だけ DMincho) /
  `always` (本文全体を `DakutenAokinMincho` で組む。`U+3000` を描けない Reader 向け)
- 検証: 該当小説 (pixiv n29131692) を組み込みエンジン + `always` で変換し、
  `item/style/vertical_font.css` (`@font-face` + `body, p` ルール)・
  `item/fonts/DMincho.ttf`・本文の `<link>`・OPF の manifest を確認

### 更新 (2026-09-15): v0.1.3 で Java 出力とほぼ完全一致

Lite 側に資産注入の口が揃ったため、`src/epub_lite.rs` を組み込みエンジンの公開
パイプラインへ載せ替えた。独自実装（挿絵の連番化、外字フォント収集、UUID 生成）は
すべて削除し、ライブラリの API を直接使う。

- pin: `aozora_epub3_lite` = `1c3fca6` (v0.1.3)。`Cargo.toml` の `rev` を書き換えて
  `cargo update -p aozora_epub3_lite`
- `config_for(aozoraepub3dir)`: `AozoraConfig::load_from_dirs([dir], <dir>/AozoraEpub3.ini)`。
  Java 版と同じ注記表 (`chuki_*.txt`)・外字フォント (`gaiji/*.ttf`)・INI を読む。
  INI が無ければ `preset/AozoraEpub3.ini` 相当のフラグを適用
- `preset/custom_chuki_tag.txt` (21 行) を `include_str!` で常時重ねる。`narou init` が
  インストール先 `chuki_tag.txt` へ注入する内容と同一なので、同梱資産だけで動く
  wasm / 未設定時でも `ここから柱` / 前書き / 後書き / 一字下げ 等が効く
- `build_book(input_txt, options)`: Lite CLI と同じ順で組み立てる —
  `collect_assets` → `decorate_image_tags` → `rewrite_image_source` →
  `remove_missing_image_sources` → `remove_image_sources` (自動表紙) →
  `reflow_image_sections` → `build_metadata` (`urn:uuid:` は Java と同じ
  `java_name_uuid`) → `build_title_page_markup` → `append_gaiji_assets`
- 挿絵は `EpubBuild::resolve` が書き出し時に 1 枚ずつ読み、Java と同じ前処理
  (`image::process`: 余白除去・リサイズ・回転) をかける。寸法だけ事前に読む
- 表紙は Java 経路と同じ条件 (`cover.jpg/png/jpeg` の有無) で `-c 0` 相当を渡す
- 実データ検証 (2026-09-15): `WebNovel` の n0421du (401 セクション、濁点外字・
  custom chuki 使用) で、Java 版 (`java -cp AozoraEpub3.jar AozoraEpub3 -enc UTF-8
  -of -dst out novel.txt`) と **422/423 ファイルがバイト完全一致**。挿絵を 1 枚
  入れた入力でも **425/426 がバイト完全一致**（単ページ画像化・連番・表紙処理まで含む）。
  唯一の差は `dcterms:modified`（Java はローカル時刻に `Z` を付ける、Lite は UTC）

### 制限
- Worker の Convert ジョブ自体は引き続き blocked (セクション→テキスト組立の portable 化は後続作業)。DL 時 EPUB は `novel.txt` オブジェクトが存在する場合のみ動作
- HTTP レイヤはレスポンス全体をバッファしてから返す (Lite 自体はチャンク書き出し対応済み)。真の chunked 転送は後続作業
- デバッグプロファイルでの 3MB 超テキスト変換は大幅に遅い (Lite CLI release 比較では 3.8s)。実用は release ビルド前提

## 未検証リスク

- 濁点注記 (`［＃濁点］` 等): テストデータに該当なし。DakutenFontGuard との相互作用未検証
- mobi/kindle (kindlegen 連鎖)、ibunko zip は対象外
- GUI・ネットワーク取得機能は Lite の設計上対象外 (narou.rs 側で担う範囲のみ)
