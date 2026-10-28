//! `narou setting` で `server-*` の値を変えたとき、稼働中の Web UI サーバへ
//! 再適用が伝わることの回帰テスト。
//!
//! 以前は CLI からの変更がサーバ側の 30 秒ポーリングまで反映されなかった。
//! 許可ホストに IP アドレスを含むと、反映されるまで IP 経由では接続できず、
//! Web UI から保存し直すと初めて効くという非対称だった。
use std::io::{Read, Write};
use std::net::TcpStream;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::time::Duration;

fn narou_binary() -> PathBuf {
    PathBuf::from(env!("CARGO_BIN_EXE_narou_rs"))
}

/// `Host` ヘッダに指定値を乗せて `GET /` を送り、応答コードを返す。
fn status_for_host(port: u16, host_header: &str) -> u16 {
    let mut stream = TcpStream::connect(("127.0.0.1", port)).expect("サーバへ接続できる");
    stream
        .set_read_timeout(Some(Duration::from_secs(10)))
        .expect("読み込みタイムアウトを設定できる");
    let request = format!("GET / HTTP/1.1\r\nHost: {host_header}\r\nConnection: close\r\n\r\n");
    stream
        .write_all(request.as_bytes())
        .expect("リクエストを送信できる");
    let mut response = String::new();
    let _ = stream.read_to_string(&mut response);
    response
        .lines()
        .next()
        .and_then(|line| line.split_whitespace().nth(1))
        .and_then(|code| code.parse::<u16>().ok())
        .unwrap_or(0)
}

fn run_setting(cwd: &Path, home: &Path, args: &[&str]) -> String {
    let output = Command::new(narou_binary())
        .args(args)
        .current_dir(cwd)
        .env("USERPROFILE", home)
        .env("HOME", home)
        .stdin(Stdio::null())
        .output()
        .expect("narou_rs を起動できる");
    assert!(
        output.status.code().is_some(),
        "narou_rs {args:?} が正常に終了しなかった"
    );
    String::from_utf8_lossy(&output.stdout).into_owned()
}

#[test]
fn cli_setting_change_reaches_the_running_server_immediately() {
    let temp = tempfile::tempdir().unwrap();
    let library = temp.path().join("library");
    let home = temp.path().join("home");
    std::fs::create_dir_all(library.join(".narou")).unwrap();
    std::fs::create_dir_all(home.join(".narousetting")).unwrap();

    // テスト用のポートは OS に割り当てさせる。
    let port = {
        let listener = std::net::TcpListener::bind("127.0.0.1:0").expect("ポートを取れる");
        listener.local_addr().unwrap().port()
    };

    let mut server = Command::new(narou_binary())
        .args(["web", "--port", &port.to_string(), "--no-browser"])
        .current_dir(&library)
        .env("USERPROFILE", &home)
        .env("HOME", &home)
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .spawn()
        .expect("サーバを起動できる");

    // 起動待ち (ポートが開くまで)。
    let mut ready = false;
    for _ in 0..80 {
        std::thread::sleep(Duration::from_millis(250));
        if TcpStream::connect(("127.0.0.1", port)).is_ok() {
            ready = true;
            break;
        }
    }
    assert!(ready, "サーバが起動しなかった");

    let result = std::panic::catch_unwind(|| {
        // 許可ホストにワイルドカードだけを入れる。
        let stdout = run_setting(
            &library,
            &home,
            &["setting", "server-add-accepted-hosts=*.allowed.test"],
        );
        assert!(
            stdout.contains("server-add-accepted-hosts"),
            "設定が保存されなかった: {stdout}"
        );
        // 許可されたホストは 200 を返す。
        assert_eq!(
            status_for_host(port, &format!("sub.allowed.test:{port}")),
            200,
            "許可ホストが拒否された"
        );
        // 未許可のホストは 400 を返す。
        assert_eq!(
            status_for_host(port, &format!("denied.example:{port}")),
            400,
            "未許可ホストが許可された"
        );

        // CLI から許可ホストに IP アドレスを追加する。
        let stdout = run_setting(
            &library,
            &home,
            &["setting", "server-add-accepted-hosts=*.allowed.test,192.168.2.63"],
        );
        assert!(
            stdout.contains("server-add-accepted-hosts"),
            "設定が保存されなかった: {stdout}"
        );

        // 再起動なしで IP アドレスのホストが許されるようになる
        // (以前は 30 秒ポーリング待ちで、反映されなかった)。
        assert_eq!(
            status_for_host(port, &format!("192.168.2.63:{port}")),
            200,
            "CLI で追加した IP アドレスが反映されなかった"
        );
        // 追加前のホストは引き続き許可される。
        assert_eq!(
            status_for_host(port, &format!("sub.allowed.test:{port}")),
            200,
            "既存の許可ホストが失われた"
        );
        // 未許可のホストは引き続き拒否される。
        assert_eq!(
            status_for_host(port, &format!("denied.example:{port}")),
            400,
            "未許可ホストが許可された"
        );
    });

    let _ = server.kill();
    let _ = server.wait();
    if let Err(payload) = result {
        std::panic::resume_unwind(payload);
    }
}
