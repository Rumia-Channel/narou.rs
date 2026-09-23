//! The global settings map is not library state.
//!
//! `~/.narousetting/global_setting.yaml` must survive every storage mode:
//! narou.rb only reads the file, and a library in YAML mode must see the same
//! values as one in SQLite mode. These tests drive the real commands with a
//! private home directory so the process-wide `USERPROFILE` override stays
//! inside this test binary.

use std::path::{Path, PathBuf};
use std::process::Command;

static HOME_LOCK: std::sync::Mutex<()> = std::sync::Mutex::new(());

fn narou_binary() -> PathBuf {
    PathBuf::from(env!("CARGO_BIN_EXE_narou_rs"))
}

struct FakeHome {
    _guard: std::sync::MutexGuard<'static, ()>,
    dir: PathBuf,
}

impl FakeHome {
    fn new(name: &str) -> Self {
        let guard = HOME_LOCK.lock().unwrap_or_else(|e| e.into_inner());
        let dir = std::env::temp_dir().join(format!("narou-global-settings-{name}"));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(dir.join(".narousetting")).unwrap();
        Self { _guard: guard, dir }
    }

    fn global_file(&self) -> PathBuf {
        self.dir.join(".narousetting").join("global_setting.yaml")
    }

    fn write_global(&self, content: &str) {
        std::fs::write(self.global_file(), content).unwrap();
    }

    fn run(&self, cwd: &Path, args: &[&str]) -> String {
        let output = Command::new(narou_binary())
            .args(args)
            .current_dir(cwd)
            .env("USERPROFILE", &self.dir)
            .env("HOME", &self.dir)
            .output()
            .expect("run narou_rs");
        assert!(
            output.status.success(),
            "narou_rs {args:?} failed: {}{}",
            String::from_utf8_lossy(&output.stdout),
            String::from_utf8_lossy(&output.stderr)
        );
        String::from_utf8_lossy(&output.stdout).into_owned()
    }
}

fn init_library(home: &FakeHome, name: &str) -> PathBuf {
    let root = std::env::temp_dir().join(format!("narou-global-settings-lib-{name}"));
    let _ = std::fs::remove_dir_all(&root);
    std::fs::create_dir_all(&root).unwrap();
    home.run(&root, &["init"]);
    root
}

#[test]
fn global_settings_stay_in_the_home_file_across_storage_modes() {
    let home = FakeHome::new("modes");
    home.write_global("aozoraepub3dir: C:\\AozoraEpub3\nline-height: 1.8\n");
    let root = init_library(&home, "modes");

    // Writing through the CLI lands in the file.
    home.run(&root, &["setting", "over18=true"]);
    let written = std::fs::read_to_string(home.global_file()).unwrap();
    assert!(written.contains("over18: true"), "got: {written}");

    // Switching the library to SQLite must not move or rename the file.
    std::fs::write(root.join(".narou").join("storage-backend"), "sqlite\n").unwrap();
    let listed = home.run(&root, &["setting", "-l"]);
    assert!(listed.contains("aozoraepub3dir"), "got: {listed}");
    assert!(listed.contains("over18=true"), "got: {listed}");
    assert!(
        home.global_file().exists(),
        "global_setting.yaml must survive SQLite mode"
    );

    // …and the same values are visible after switching back.
    std::fs::remove_file(root.join(".narou").join("storage-backend")).unwrap();
    let listed = home.run(&root, &["setting", "-l"]);
    assert!(listed.contains("aozoraepub3dir"), "got: {listed}");
    assert!(listed.contains("over18=true"), "got: {listed}");

    // A second library in another directory sees them too (they are global).
    let other = init_library(&home, "modes-other");
    let listed = home.run(&other, &["setting", "-l"]);
    assert!(listed.contains("over18=true"), "got: {listed}");
}

#[test]
fn a_legacy_database_row_restores_the_global_file() {
    let home = FakeHome::new("legacy");
    let root = init_library(&home, "legacy");
    std::fs::write(root.join(".narou").join("storage-backend"), "sqlite\n").unwrap();
    // Touch the SQLite backend once so the database exists.
    home.run(&root, &["setting", "-l"]);

    // Simulate what an older build left behind: the settings only in
    // `app_state`, the file renamed away.
    let db = root.join(".narou").join("db.sqlite");
    assert!(db.exists(), "sqlite backend should have created db.sqlite");
    {
        let conn = rusqlite::Connection::open(&db).unwrap();
        conn.execute(
            "INSERT OR REPLACE INTO app_state (scope, key, value_json, value_yaml) \
             VALUES (?1, ?2, '{}', ?3)",
            rusqlite::params!["global", "global_setting", "over18: true\n"],
        )
        .unwrap();
    }
    let _ = std::fs::remove_file(home.global_file());

    let listed = home.run(&root, &["setting", "-l"]);
    assert!(
        listed.contains("over18=true"),
        "the legacy row should be readable: {listed}"
    );
    let restored = std::fs::read_to_string(home.global_file()).unwrap();
    assert!(restored.contains("over18: true"), "got: {restored}");

    // The row is consumed, so it cannot resurrect a deleted file later.
    let _ = std::fs::remove_file(home.global_file());
    let listed = home.run(&root, &["setting", "-l"]);
    assert!(!listed.contains("over18=true"), "got: {listed}");
}
