use std::io::{self, Write};

struct CmdInfo {
    name: &'static str,
    oneline: &'static str,
}

const COMMANDS: &[CmdInfo] = &[
    CmdInfo {
        name: "download",
        oneline: "指定した小説をダウンロードします",
    },
    CmdInfo {
        name: "update",
        oneline: "小説を更新します",
    },
    CmdInfo {
        name: "list",
        oneline: "現在管理している小説の一覧を表示します",
    },
    CmdInfo {
        name: "convert",
        oneline: "小説を変換します。管理小説以外にテキストファイルも変換可能",
    },
    CmdInfo {
        name: "diff",
        oneline: "更新された小説の差分を表示します",
    },
    CmdInfo {
        name: "setting",
        oneline: "各コマンドの設定を変更します",
    },
    CmdInfo {
        name: "alias",
        oneline: "小説のIDに紐付けた別名を作成します",
    },
    CmdInfo {
        name: "inspect",
        oneline: "小説状態の調査状況ログを表示します",
    },
    CmdInfo {
        name: "send",
        oneline: "変換したEPUB/MOBIを電子書籍端末に送信します",
    },
    CmdInfo {
        name: "folder",
        oneline: "小説の保存フォルダを開きます",
    },
    CmdInfo {
        name: "browser",
        oneline: "小説の掲載ページをブラウザで開きます",
    },
    CmdInfo {
        name: "remove",
        oneline: "小説を削除します",
    },
    CmdInfo {
        name: "freeze",
        oneline: "小説の凍結設定を行います",
    },
    CmdInfo {
        name: "tag",
        oneline: "各小説にタグを設定及び閲覧が出来ます",
    },
    CmdInfo {
        name: "web",
        oneline: "WEBアプリケーション用サーバを起動します",
    },
    CmdInfo {
        name: "mail",
        oneline: "変換したEPUB/MOBIをメールで送信します",
    },
    CmdInfo {
        name: "backup",
        oneline: "小説のバックアップを作成します",
    },
    CmdInfo {
        name: "csv",
        oneline: "小説リストをCSV形式で出力したりインポートしたりします",
    },
    CmdInfo {
        name: "clean",
        oneline: "ゴミファイルを削除します",
    },
    CmdInfo {
        name: "log",
        oneline: "保存したログを表示します",
    },
    CmdInfo {
        name: "trace",
        oneline: "直前のバックトレースを表示します",
    },
    CmdInfo {
        name: "help",
        oneline: "このヘルプを表示します",
    },
    CmdInfo {
        name: "version",
        oneline: "バージョンを表示します",
    },
    CmdInfo {
        name: "init",
        oneline: "現在のフォルダを小説用に初期化します",
    },
];

const HEADER: &str = "Narou.rb ― 小説家になろうダウンローダ＆縦書き用整形スクリプト";

fn use_color() -> bool {
    std::env::var("NO_COLOR").is_err()
}

struct Style;

impl Style {
    fn bold(s: &str) -> String {
        if use_color() {
            format!("\x1b[1m{}\x1b[0m", s)
        } else {
            s.to_string()
        }
    }
    fn underline(s: &str) -> String {
        if use_color() {
            format!("\x1b[4m{}\x1b[0m", s)
        } else {
            s.to_string()
        }
    }
    fn bold_green(s: &str) -> String {
        if use_color() {
            format!("\x1b[1;32m{}\x1b[0m", s)
        } else {
            s.to_string()
        }
    }
    fn bold_yellow(s: &str) -> String {
        if use_color() {
            format!("\x1b[1;33m{}\x1b[0m", s)
        } else {
            s.to_string()
        }
    }
}

fn is_initialized() -> bool {
    let cwd = match std::env::current_dir() {
        Ok(c) => c,
        Err(_) => return false,
    };
    cwd.join(".narou").is_dir()
}

pub fn cmd_help() {
    let stdout = io::stdout();
    let mut out = stdout.lock();

    if is_initialized() {
        display_help(&mut out);
    } else {
        display_help_first_time(&mut out);
    }
}

fn display_help(out: &mut dyn Write) {
    let _ = writeln!(out, "{}", HEADER);
    let _ = writeln!(out);
    let _ = writeln!(
        out,
        " {}",
        Style::bold_green("Usage: narou <command> [arguments...] [options...]")
    );
    let _ = writeln!(
        out,
        "              [--no-color] [--multiple] [--time] [--backtrace]"
    );
    let _ = writeln!(out);
    let _ = writeln!(out, " コマンドの簡単な説明:");

    for cmd in COMMANDS {
        let padded = format!("{:<12}", cmd.name);
        let _ = writeln!(out, "   {} {}", Style::bold_green(&padded), cmd.oneline);
    }

    let _ = writeln!(out);
    let _ = writeln!(
        out,
        "  各コマンドの詳細は narou <command> -h を参照してください。"
    );
    let _ = writeln!(out, "  各コマンドは先頭の一文字か二文字でも指定できます。");
    let _ = writeln!(
        out,
        "  (e.g. `narou {} n4259s', `narou {} musyoku')",
        Style::bold_yellow("d"),
        Style::bold_yellow("fr")
    );
    let _ = writeln!(out);
    let _ = writeln!(
        out,
        "  {}",
        Style::underline(&Style::bold("Global Options:"))
    );
    let _ = writeln!(out, "    --no-color   カラー表示を無効にする");
    let _ = writeln!(
        out,
        "    --multiple   引数の区切りにスペースの他に\",\"も使えるようにする"
    );
    let _ = writeln!(out, "    --time       実行時間表示");
    let _ = writeln!(out, "    --backtrace  エラー発生時詳細情報を表示");
}

fn display_help_first_time(out: &mut dyn Write) {
    let _ = writeln!(out, "{}", HEADER);
    let _ = writeln!(out);
    let _ = writeln!(out, " {}", Style::bold_green("Usage: narou init"));
    let _ = writeln!(out);
    let _ = writeln!(out, "   まだこのフォルダは初期化されていません。");
    let _ = writeln!(
        out,
        "   {} コマンドを実行して初期化を行いましょう。",
        Style::bold_yellow("narou init")
    );
}

struct CmdHelp {
    banner: &'static str,
    description: &'static str,
    options: &'static [CmdOption],
}

struct CmdOption {
    short: Option<&'static str>,
    long: &'static str,
    arg: Option<&'static str>,
    help: &'static str,
}

const fn opt(
    short: Option<&'static str>,
    long: &'static str,
    arg: Option<&'static str>,
    help: &'static str,
) -> CmdOption {
    CmdOption {
        short,
        long,
        arg,
        help,
    }
}

const DOWNLOAD_HELP: CmdHelp = CmdHelp {
    banner: "[<target> <target2> ...] [options]",
    description: "\
  ・ダウンロードしたい小説のNコードもしくはURLを指定して下さい。
  ・対応サイトは小説家になろう(小説を読もう)、ノクターンノベルズ、ムーンライトノベルズ、Arcadia、ハーメルン、暁、カクヨムです。
  ・ArcadiaのURLを入力するときは\" \"で囲って下さい。
  ・ダウンロード終了後に変換処理を行います。ダウンロードのみする場合は-nオプションを指定して下さい。
  ・すでにダウンロード済みの小説の場合は何もしません。
  ・--remove オプションをつけてダウンロードすると、ダウンロード（とその後の変換、送信）が終わったあと削除します。データベースのインデックスを外すだけなので、変換した書籍データは残ったままになります。ファイルを全て消す場合は手動で削除する必要があります。
  ・--mail オプションをつけてダウンロードすると、ダウンロード後にメールで送信します。
  ・NコードもURLも指定しなかった場合、対話モード移行します。

  Examples:
    narou download n9669bk
    narou download http://ncode.syosetu.com/n9669bk/
    narou download n9669bk http://ncode.syosetu.com/n4259s/
    narou download 0 1 -f
    narou download n9669bk -n
    narou download n6864bt --remove",
    options: &[
        opt(Some("-f"), "--force",         None, "全話を強制再ダウンロードする"),
        opt(Some("-n"), "--no-convert",    None, "変換をせずダウンロードのみ実行する"),
        opt(Some("-z"), "--freeze",        None, "ダウンロードが終了したあと凍結する"),
        opt(Some("-r"), "--remove",        None, "ダウンロードが終了したあと削除する"),
        opt(Some("-m"), "--mail",          None, "ダウンロードが終了したあとメールで送信する"),
    ],
};

const UPDATE_HELP: CmdHelp = CmdHelp {
    banner: "[<target> ...] [options]",
    description: "\
  ・管理対象の小説を更新します。
    更新したい小説のNコード、URL、タイトル、IDもしくは別名を指定して下さい。
    IDは narou list を参照して下さい。
  ・対象を指定しなかった場合、すべての小説の更新をチェックします。
  ・一度に複数の小説を指定する場合は空白で区切って下さい。
  ・標準入力から渡されたID/URL/Nコード/タイトル/別名も対象として解釈します。
  ・全て更新する場合、convert.no-openが設定されていなくても保存フォルダは開きません。

  Examples:
    narou update               # 全て更新
    narou u                    # 短縮コマンド
    narou update 0 1 2 4
    narou update n9669bk 異世界迷宮で奴隷ハーレムを
    narou update http://ncode.syosetu.com/n9669bk/

    # foo タグが付いた小説と bar タグが付いた小説を更新(タグのOR指定)
    narou u foo bar

    # foo タグ及び bar タグが両方付いた小説のみ更新(タグのAND指定)
    narou tag foo bar | narou u
    narou l -t \"foo bar\" | narou u",
    options: &[
        opt(
            Some("-n"),
            "--no-convert",
            None,
            "変換をせずアップデートのみ実行する",
        ),
        opt(
            Some("-a"),
            "--convert-only-new-arrival",
            None,
            "新着がある場合のみ変換を実行する",
        ),
        opt(
            None,
            "--gl",
            Some("[OPT]"),
            "データベースに最新話掲載日を反映させる",
        ),
        opt(Some("-f"), "--force", None, "凍結済みも更新する"),
        opt(
            Some("-s"),
            "--sort-by",
            Some("KEY"),
            "更新順を指定したキーでソートする",
        ),
        opt(
            Some("-i"),
            "--ignore-all",
            None,
            "引数なし時の全更新を無効化する",
        ),
    ],
};

const CONVERT_HELP: CmdHelp = CmdHelp {
    banner: "<target> [<target2> ...] [options]",
    description: "\
  ・指定した小説を縦書き用に整形及びEPUB、MOBIに変換します。
  ・変換したい小説のNコード、URL、タイトルもしくはIDを指定して下さい。
    IDは narou list を参照して下さい。
  ・一度に複数の小説を指定する場合は空白で区切って下さい。
  ※-oオプションがない場合、[作者名] 小説名.txtが小説の保存フォルダに出力されます
  ・管理小説以外にもテキストファイルを変換出来ます。
    テキストファイルのファイルパスを指定します。
  ※複数指定した場合に-oオプションがあった場合、ファイル名に連番がつきます。
  ・MOBI化する場合は narou setting device=kindle をして下さい。
  ・device=kobo の場合、.kepub.epub を出力します。

  Examples:
    narou convert n9669bk
    narou convert http://ncode.syosetu.com/n9669bk/
    narou convert 異世界迷宮で奴隷ハーレムを
    narou convert 1 -o \"ハーレム -変換済み-.txt\"
    narou convert mynovel.txt --enc sjis",
    options: &[
        opt(
            Some("-o"),
            "--output",
            Some("FILE"),
            "出力ファイル名を指定する。フォルダパス部分は無視される",
        ),
        opt(None, "--make-zip", None, "i文庫用のzipファイルを作る"),
        opt(
            Some("-e"),
            "--enc",
            Some("ENCODING"),
            "テキストファイル指定時の文字コードを指定する。デフォルトはUTF-8",
        ),
        opt(None, "--no-epub", None, "EPUB書き出しを行わない"),
        opt(None, "--no-mobi", None, "MOBI変換を行わない"),
        opt(None, "--no-strip", None, "MOBIのstripを行わない"),
        opt(None, "--no-zip", None, "i文庫用zipファイルを作らない"),
        opt(
            None,
            "--no-open",
            None,
            "変換終了後に保存フォルダを開かない",
        ),
        opt(Some("-i"), "--inspect", None, "小説状態調査ログを表示する"),
        opt(
            Some("-v"),
            "--verbose",
            None,
            "AozoraEpub3・kindlegenの出力を表示する",
        ),
        opt(None, "--ignore-default", None, "default.* 設定を無視する"),
        opt(None, "--ignore-force", None, "force.* 設定を無視する"),
    ],
};

const LIST_HELP: CmdHelp = CmdHelp {
    banner: "[<limit>] [options]",
    description: "\
  ・現在管理している小説の一覧を表示します
  ・表示されるIDは各コマンドで指定することで小説名等を入力する手間を省けます
  ・個数を与えることで、最大表示数を制限できます(デフォルトは全て表示)
  ・narou listのデフォルト動作を narou s default_arg.list= で設定すると便利です
  ・パイプで他のnarouコマンドに繋ぐとID入力の代わりにできます

  Examples:
    narou list             # IDの小さい順に全て表示
    narou list 10 -r       # IDの大きい順に10件表示
    narou list 5 -l        # 最近更新のあった5件表示
    narou list 10 -rl      # 古い順に10件表示
    narou list -f ss       # 短編小説だけ表示

    # 小説家になろうの小説のみを表示
    narou list --site --grep 小説家になろう
    narou l -sg 小説家になろう    # 上記と同じ意味",
    options: &[
        opt(
            Some("-l"),
            "--latest",
            None,
            "最近更新のあった順に小説を表示する",
        ),
        opt(None, "--gl", None, "更新日ではなく最新話掲載日を使用する"),
        opt(Some("-r"), "--reverse", None, "逆順に表示する"),
        opt(Some("-u"), "--url", None, "小説の掲載ページも表示する"),
        opt(
            Some("-k"),
            "--kind",
            None,
            "小説の種別（短編／連載）も表示する",
        ),
        opt(Some("-s"), "--site", None, "掲載サイトも表示する"),
        opt(Some("-a"), "--author", None, "作者名も表示する"),
        opt(
            Some("-f"),
            "--filter",
            Some("VAL"),
            "フィルターを設定する(series/ss/frozen/nonfrozen)",
        ),
        opt(
            Some("-g"),
            "--grep",
            Some("VAL"),
            "検索文字列を指定する(- prefixでNOT)",
        ),
        opt(Some("-t"), "--tag", Some("[TAGS]"), "タグ表示/フィルタ"),
        opt(Some("-e"), "--echo", None, "パイプ時も人間可読出力"),
        opt(None, "--frozen", None, "凍結済みのみ表示"),
    ],
};

const SETTING_HELP: CmdHelp = CmdHelp {
    banner: "[<name>=<value> ...] [options]\n       --burn <target> [<target2> ...]",
    description: "\
  ・各コマンドの設定の変更が出来ます。
  ・Global な設定はユーザープロファイルに保存され、すべての narou コマンドで使われます
  ・下の一覧は一部です。すべてを確認するには -a オプションを付けて確認して下さい
  ・default. で始まる設定は、setting.ini で未設定時の項目の挙動を指定することが出来ます
  ・force. で始まる設定は、setting.ini や default.* 等の指定を全て無視して項目の挙動を強制出来ます

  Examples:
    narou setting --list                 # 現在の設置値一覧を表示
    narou setting convert.no-open=true   # 値を設定する
    narou setting convert.no-epub=       # 右辺に何も書かないとその設定を削除出来る
    narou setting device                 # 変数名だけ書くと現在の値を確認出来る",
    options: &[
        opt(Some("-l"), "--list", None, "現在の設定値を確認する"),
        opt(
            Some("-a"),
            "--all",
            None,
            "設定できる全ての変数名を表示する",
        ),
        opt(
            None,
            "--burn",
            None,
            "指定した小説の未設定項目に共通設定を焼き付ける",
        ),
    ],
};

const REMOVE_HELP: CmdHelp = CmdHelp {
    banner: "<target> [<target2> ...] [options]",
    description: "\
  ・削除したい小説のNコード、URL、タイトルもしくはIDを指定して下さい。
    IDは narou list を参照して下さい。
  ・一度に複数の小説を指定する場合は空白で区切って下さい。
  ・削除確認をスキップするには -y オプションを有効にして下さい。
  ・削除するのはデータベースのインデックスだけで、変換済みテキストファイルやMOBIファイル等はそのまま残ります。ファイルをすべて削除する場合は --with-file オプションを指定して下さい。

  Examples:
    narou remove n9669bk
    narou remove http://ncode.syosetu.com/n9669bk/
    narou remove n9669bk http://ncode.syosetu.com/n4259s/
    narou remove 0 1 -y
    narou remove n9669bk --with-file   # ファイルも完全に削除する
    narou remove --all-ss              # 短編小説をすべて削除する
    narou r 0 -wy    # ID:0を確認メッセージなしにファイルも含めて完全に削除する",
    options: &[
        opt(Some("-y"), "--yes",       None, "削除確認メッセージを表示しない"),
        opt(Some("-w"), "--with-file", None, "小説の保存フォルダ・ファイルも全て削除する"),
        opt(None,       "--all-ss",    None, "短編小説をすべて削除する"),
    ],
};

const FREEZE_HELP: CmdHelp = CmdHelp {
    banner: "<target> [<target2> ...] [options]",
    description: "\
  ・指定した小説を凍結し、変更不可属性を付与します。
  ・凍結することでダウンロード、アップデート及び削除が出来なくなります。
  ・凍結済みの小説を指定した場合、凍結が解除されます。

  Examples:
    narou freeze --list
    narou freeze n9669bk
    narou freeze 0 1 musyoku",
    options: &[
        opt(Some("-l"), "--list", None, "凍結中小説の一覧を表示"),
        opt(None, "--on", None, "現在の状態にかかわらず凍結する"),
        opt(None, "--off", None, "現在の状態にかかわらず解除する"),
    ],
};

const TAG_HELP: CmdHelp = CmdHelp {
    banner: "<option> <tagname> <target> [<target2> ...]\n       <tagname> [<tagname2> ...]",
    description: "\
  ・小説にタグを設定します。設定個数の上限はありません
  ・タグ名にはスペース以外の文字が使えます(大文字小文字区別)
  ・タグには自動で色がつきます。自分で指定する場合は--colorを指定して下さい
  ・一部特殊なタグがあります。設定することでlistコマンドに反映されます
      - end: 小説が完結状態
      - 404: 掲載サイトから削除された状態
  ・設定したタグは他のコマンドで指定することで、小説ID指定の代わりにすることができます

  Examples:
    narou tag --add fav 0 2     # ID:0と2の小説にfavタグを設定(追加)
    narou t -a fav 0 2          # もしくはこの様に書けます
    narou t -a \"fav later\" 0 2  # 一度に複数のタグを指定出来ます
    narou t -a fav -c red 0     # favというタグを赤色で設定する
    narou tag --delete fav 2    # ID:2の小説のfavタグを外す
    narou t -d fav 2

    narou tag end               # endタグ(完結)の付いている小説の一覧を表示
    narou tag fav later         # fav,laterタグ両方付いている小説を表示
    narou tag                   # 何も指定しない場合、存在するタグ一覧を表示",
    options: &[
        opt(Some("-a"), "--add", Some("TAGS"), "タグを追加する"),
        opt(Some("-d"), "--delete", Some("TAGS"), "タグを外す"),
        opt(Some("-c"), "--color", Some("COL"), "タグの色を指定する"),
        opt(None, "--clear", None, "全てのタグを削除する"),
        opt(Some("-l"), "--list", None, "存在するタグ一覧を表示する"),
    ],
};

const WEB_HELP: CmdHelp = CmdHelp {
    banner: "[options...]",
    description: "\
  ・WEBアプリケーション用サーバを起動します
  ・小説の管理及び設定をブラウザで行うことができます
  ・--port を指定しない場合、ポートは初回起動時にランダムで設定します
    (以降同じ設定を引き継ぎます)
  ・サーバ起動後にブラウザを立ち上げます
  ・サーバの停止はコンソールで Ctrl+C を入力します

  Examples:
    narou web   # サーバ起動(ポートはランダム。ポート設定保存)
    narou web -p 4567   # ポート4567で起動(保存はされない)

    # 先に決めておく
    narou s server-port=8000
    narou web   # ポート8000で起動",
    options: &[
        opt(Some("-p"), "--port", Some("PORT"), "起動するポートを指定"),
        opt(
            Some("-n"),
            "--no-browser",
            None,
            "起動時にブラウザは開かない",
        ),
    ],
};

const INIT_HELP: CmdHelp = CmdHelp {
    banner: "[options]",
    description: "\
  ・現在のフォルダを小説格納用フォルダとして初期化します。
  ・初期化されるまでは他のコマンドは使えません。

  Examples:
    narou init
    narou init -p /opt/narou/aozora    # AozoraEpub3 のフォルダを直接指定
    narou init -p :keep                # 設定済みと同じ場所を指定(既に初期化済の場合)

    # 行の高さの調整
    narou init --line-height 1.8       # 行の高さを1.8emに設定(1.8文字分相当)",
    options: &[
        opt(
            Some("-p"),
            "--path",
            Some("FOLDER"),
            "指定したフォルダの AozoraEpub3 を利用する",
        ),
        opt(
            Some("-l"),
            "--line-height",
            Some("SIZE"),
            "行の高さを変更する(単位em)。オススメは1.8",
        ),
    ],
};

const DIFF_HELP: CmdHelp = CmdHelp {
    banner: "[<target>] [options]",
    description: "\
  ・更新された小説の差分を表示します。
  ・対象を指定しなかった場合、最後に更新した小説が対象になります。

  Examples:
    narou diff 0
    narou diff 0 -n 2
    narou diff 0 --list
    narou diff --all-clean",
    options: &[
        opt(Some("-n"), "--number", Some("NUM"), "差分番号(1=最新)"),
        opt(Some("-l"), "--list", None, "差分一覧を表示"),
        opt(Some("-c"), "--clean", None, "指定小説の差分全削除"),
        opt(None, "--all-clean", None, "凍結以外の全差分削除"),
        opt(None, "--no-tool", None, "外部diffツールを使わない"),
    ],
};

const SEND_HELP: CmdHelp = CmdHelp {
    banner: "[<device>] [<target> ...] [options]",
    description: "\
  ・変換したEPUB/MOBIを電子書籍端末に送信します。
  ・USB接続された端末の documents フォルダにコピーします。

  Examples:
    narou send
    narou send kindle
    narou send 0 1 2",
    options: &[
        opt(
            Some("-w"),
            "--without-freeze",
            None,
            "凍結済み小説を送信対象から除外する",
        ),
        opt(
            Some("-f"),
            "--force",
            None,
            "タイムスタンプに関わらず強制送信する",
        ),
        opt(
            Some("-b"),
            "--backup-bookmark",
            None,
            "ブックマークをバックアップする(KindlePW)",
        ),
        opt(
            Some("-r"),
            "--restore-bookmark",
            None,
            "ブックマークを復元する",
        ),
    ],
};

const MAIL_HELP: CmdHelp = CmdHelp {
    banner: "[<target> ...] [options]",
    description: "\
  ・変換したEPUB/MOBIをメールで送信します。
  ・Send-to-Kindle 等のメール送信機能を利用します。

  Examples:
    narou mail
    narou mail -f
    narou mail hotentry",
    options: &[opt(
        Some("-f"),
        "--force",
        None,
        "全非凍結小説を強制送信する",
    )],
};

const ALIAS_HELP: CmdHelp = CmdHelp {
    banner: "[<name>=<target>] [options]",
    description: "\
  ・小説のIDに紐付けた別名を作成します。
  ・別名は各コマンドでID指定の代わりに使用できます。

  Examples:
    narou alias musyoku=0
    narou alias --list
    narou alias musyoku=   # 削除",
    options: &[opt(Some("-l"), "--list", None, "現在の別名一覧を表示")],
};

const INSPECT_HELP: CmdHelp = CmdHelp {
    banner: "[<target>]",
    description: "\
  ・小説状態の調査状況ログを表示します。
  ・対象を指定しなかった場合、最後に変換した小説が対象になります。

  Examples:
    narou inspect 0
    narou inspect n9669bk",
    options: &[],
};

const FOLDER_HELP: CmdHelp = CmdHelp {
    banner: "[<target>] [options]",
    description: "\
  ・小説の保存フォルダを開きます。
  ・対象を指定しなかった場合、最後に変換した小説が対象になります。

  Examples:
    narou folder 0
    narou folder 0 -n    # パスのみ表示",
    options: &[opt(
        Some("-n"),
        "--no-open",
        None,
        "フォルダを開かずパスのみ表示する",
    )],
};

const BROWSER_HELP: CmdHelp = CmdHelp {
    banner: "[<target>] [options]",
    description: "\
  ・小説の掲載ページをブラウザで開きます。
  ・対象を指定しなかった場合、最後に変換した小説が対象になります。

  Examples:
    narou browser 0
    narou browser 0 --vote",
    options: &[opt(
        Some("-v"),
        "--vote",
        None,
        "感想ページを開く(なろうのみ)",
    )],
};

const BACKUP_HELP: CmdHelp = CmdHelp {
    banner: "[<target>]",
    description: "\
  ・小説のバックアップを作成します。
  ・対象を指定しなかった場合、最後に変換した小説が対象になります。
  ・バックアップは <novel_dir>/backup/ にZIP形式で保存されます。

  Examples:
    narou backup 0
    narou backup n9669bk",
    options: &[],
};

const CSV_HELP: CmdHelp = CmdHelp {
    banner: "[options]",
    description: "\
  ・小説リストをCSV形式で出力したりインポートしたりします。
  ・エクスポート: 全小説の情報をCSV形式で出力
  ・インポート: CSVファイルから小説を一括ダウンロード

  Examples:
    narou csv -o list.csv
    narou csv -i list.csv",
    options: &[
        opt(
            Some("-o"),
            "--output",
            Some("FILE"),
            "CSVファイルの出力先を指定",
        ),
        opt(
            Some("-i"),
            "--import",
            Some("FILE"),
            "CSVファイルからインポート",
        ),
    ],
};

const CLEAN_HELP: CmdHelp = CmdHelp {
    banner: "[<target>] [options]",
    description: "\
  ・ゴミファイルを削除します。
  ・TOCに含まれないraw_data/*.txtやsection_save/*.yamlを検出します。
  ・対象を指定しなかった場合、最後に変換した小説が対象になります。

  Examples:
    narou clean 0
    narou clean --all
    narou clean --all -f    # 実際に削除",
    options: &[
        opt(Some("-f"), "--force", None, "実際に削除する"),
        opt(Some("-n"), "--dry-run", None, "表示のみ(削除しない)"),
        opt(Some("-a"), "--all", None, "全小説を対象にする"),
    ],
};

const LOG_HELP: CmdHelp = CmdHelp {
    banner: "[<target>] [options]",
    description: "\
  ・保存したログを表示します。
  ・logging=true の設定が必要です。

  Examples:
    narou log 0
    narou log 0 -n 50
    narou log 0 --tail",
    options: &[
        opt(Some("-n"), "--num", Some("NUM"), "表示行数(デフォルト20)"),
        opt(
            Some("-t"),
            "--tail",
            None,
            "ストリーミング表示(tail -f相当)",
        ),
        opt(Some("-c"), "--source-convert", None, "変換ログを表示"),
    ],
};

const TRACE_HELP: CmdHelp = CmdHelp {
    banner: "",
    description: "  直前のバックトレースを表示します。",
    options: &[],
};

const VERSION_HELP: CmdHelp = CmdHelp {
    banner: "[options]",
    description: "  バージョンを表示します。",
    options: &[opt(
        Some("-m"),
        "--more",
        None,
        "Java/AozoraEpub3 のバージョンも表示する",
    )],
};

struct CommandHelpEntry {
    name: &'static str,
    help: &'static CmdHelp,
}

const ALL_COMMAND_HELP: &[CommandHelpEntry] = &[
    CommandHelpEntry {
        name: "download",
        help: &DOWNLOAD_HELP,
    },
    CommandHelpEntry {
        name: "update",
        help: &UPDATE_HELP,
    },
    CommandHelpEntry {
        name: "list",
        help: &LIST_HELP,
    },
    CommandHelpEntry {
        name: "convert",
        help: &CONVERT_HELP,
    },
    CommandHelpEntry {
        name: "diff",
        help: &DIFF_HELP,
    },
    CommandHelpEntry {
        name: "setting",
        help: &SETTING_HELP,
    },
    CommandHelpEntry {
        name: "alias",
        help: &ALIAS_HELP,
    },
    CommandHelpEntry {
        name: "inspect",
        help: &INSPECT_HELP,
    },
    CommandHelpEntry {
        name: "send",
        help: &SEND_HELP,
    },
    CommandHelpEntry {
        name: "folder",
        help: &FOLDER_HELP,
    },
    CommandHelpEntry {
        name: "browser",
        help: &BROWSER_HELP,
    },
    CommandHelpEntry {
        name: "remove",
        help: &REMOVE_HELP,
    },
    CommandHelpEntry {
        name: "freeze",
        help: &FREEZE_HELP,
    },
    CommandHelpEntry {
        name: "tag",
        help: &TAG_HELP,
    },
    CommandHelpEntry {
        name: "web",
        help: &WEB_HELP,
    },
    CommandHelpEntry {
        name: "mail",
        help: &MAIL_HELP,
    },
    CommandHelpEntry {
        name: "backup",
        help: &BACKUP_HELP,
    },
    CommandHelpEntry {
        name: "csv",
        help: &CSV_HELP,
    },
    CommandHelpEntry {
        name: "clean",
        help: &CLEAN_HELP,
    },
    CommandHelpEntry {
        name: "log",
        help: &LOG_HELP,
    },
    CommandHelpEntry {
        name: "trace",
        help: &TRACE_HELP,
    },
    CommandHelpEntry {
        name: "help",
        help: &CmdHelp {
            banner: "",
            description: "  このヘルプを表示します。",
            options: &[],
        },
    },
    CommandHelpEntry {
        name: "version",
        help: &VERSION_HELP,
    },
    CommandHelpEntry {
        name: "init",
        help: &INIT_HELP,
    },
];

fn find_command_help(name: &str) -> Option<&'static CmdHelp> {
    ALL_COMMAND_HELP
        .iter()
        .find(|e| e.name == name)
        .map(|e| e.help)
}

pub fn display_command_help(cmd_name: &str) -> bool {
    let Some(help) = find_command_help(cmd_name) else {
        return false;
    };
    let stdout = io::stdout();
    let mut out = stdout.lock();
    render_command_help(&mut out, cmd_name, help);
    true
}

fn render_command_help(out: &mut dyn Write, cmd_name: &str, help: &CmdHelp) {
    for line in help.banner.lines() {
        if line.is_empty() {
            let _ = writeln!(
                out,
                " {}",
                Style::bold_green(&format!("Usage: narou {}", cmd_name))
            );
        } else {
            let _ = writeln!(
                out,
                " {}",
                Style::bold_green(&format!("Usage: narou {} {}", cmd_name, line))
            );
        }
    }
    if help.banner.is_empty() {
        let _ = writeln!(
            out,
            " {}",
            Style::bold_green(&format!("Usage: narou {}", cmd_name))
        );
    }

    let _ = writeln!(out);
    for line in help.description.lines() {
        if line.is_empty() {
            let _ = writeln!(out);
        } else if line.starts_with(' ') || line.starts_with('\t') {
            let _ = writeln!(out, "{}", line);
        } else {
            let _ = writeln!(out, "  {}", line);
        }
    }

    if !help.options.is_empty() {
        let _ = writeln!(out);
        let _ = writeln!(out, "  {}", Style::underline(&Style::bold("Options:")));
        for opt_item in help.options {
            let switches = match (opt_item.short, opt_item.arg) {
                (Some(s), Some(a)) => format!("{}, {}={}", s, opt_item.long, a),
                (Some(s), None) => format!("{}, {}", s, opt_item.long),
                (None, Some(a)) => format!("    {}={}", opt_item.long, a),
                (None, None) => format!("    {}", opt_item.long),
            };
            let _ = writeln!(out, "    {:30} {}", switches, opt_item.help);
        }
    }
}
