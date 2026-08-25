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

## 未検証リスク

- 濁点注記 (`［＃濁点］` 等): テストデータに該当なし。DakutenFontGuard との相互作用未検証
- mobi/kindle (kindlegen 連鎖)、ibunko zip は対象外
- GUI・ネットワーク取得機能は Lite の設計上対象外 (narou.rs 側で担う範囲のみ)
