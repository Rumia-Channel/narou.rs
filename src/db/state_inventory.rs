//! Managed-state metadata for the SQLite backend.
//!
//! One table owns the small states that historically lived as
//! `.narou/*.yaml` (and `~/.narousetting/global_setting.yaml`): which of them
//! `Inventory` manages (routed through `app_state`), which are imported from
//! the legacy file on first open and rendered back by `narou db export-yaml`,
//! and which one never leaves its file. `sqlite/state.rs` (import, compat
//! files), `sqlite/export_yaml.rs` (rollback bundle) and `Inventory` iterate
//! this table instead of keeping private lists, so a state cannot be added to
//! one list and forgotten in the next.
//!
//! `global_setting` is deliberately [`StatePurpose::FileOnly`]: the global
//! settings map stays in `~/.narousetting/global_setting.yaml` in every mode
//! so narou.rb — which only reads the file — keeps seeing the user's settings.
//! `queue` / `notepad` are imported and exported but not
//! [`StatePurpose::Inventory`]: their readers use `StateDb` directly.

use std::path::{Path, PathBuf};

/// Where a state's file lives.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum StateScope {
    /// `<library>/.narou/`; stored as `app_state(scope = 'inv')`.
    Local,
    /// `~/.narousetting/`.
    Global,
}

impl StateScope {
    /// The `app_state.scope` value of this half.
    fn app_state_scope(self) -> &'static str {
        match self {
            Self::Local => "inv",
            Self::Global => "global",
        }
    }

    fn matches(self, scope: &str) -> bool {
        self.app_state_scope() == scope
    }
}

/// How a state takes part in the SQLite backend.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum StatePurpose {
    /// `Inventory` owns it: reads and writes go through `app_state`, and the
    /// YAML file is the import source / rollback target.
    Inventory,
    /// Imported to `app_state` and exported back to YAML, but not
    /// `Inventory`-managed: `queue` and `notepad` read and write their own
    /// store.
    ImportExport,
    /// The file is the only storage; SQLite never mirrors it.
    FileOnly,
}

impl StatePurpose {
    pub(crate) fn inventory_managed(self) -> bool {
        matches!(self, Self::Inventory)
    }

    pub(crate) fn import_export(self) -> bool {
        matches!(self, Self::Inventory | Self::ImportExport)
    }
}

/// One managed state: its `app_state` key, its legacy file and how it is used.
#[derive(Debug, Clone, Copy)]
pub(crate) struct ManagedState {
    pub(crate) key: &'static str,
    pub(crate) file: &'static str,
    pub(crate) scope: StateScope,
    pub(crate) purpose: StatePurpose,
}

const fn state(
    key: &'static str,
    file: &'static str,
    scope: StateScope,
    purpose: StatePurpose,
) -> ManagedState {
    ManagedState {
        key,
        file,
        scope,
        purpose,
    }
}

/// The single owner of the managed-state list (content order is the export
/// order of the rollback bundle).
pub(crate) const MANAGED_STATES: &[ManagedState] = &[
    state("freeze", "freeze.yaml", StateScope::Local, StatePurpose::Inventory),
    state("alias", "alias.yaml", StateScope::Local, StatePurpose::Inventory),
    state("tag_colors", "tag_colors.yaml", StateScope::Local, StatePurpose::Inventory),
    state("login_cookie", "login_cookie.yaml", StateScope::Local, StatePurpose::Inventory),
    state("latest_convert", "latest_convert.yaml", StateScope::Local, StatePurpose::Inventory),
    state("author", "author.yaml", StateScope::Local, StatePurpose::Inventory),
    state("hidden", "hidden.yaml", StateScope::Local, StatePurpose::Inventory),
    state(
        "favorite_tags",
        "favorite_tags.yaml",
        StateScope::Local,
        StatePurpose::Inventory,
    ),
    state("virtual_novel_members", "virtual_novel_members.yaml", StateScope::Local, StatePurpose::Inventory),
    state("local_setting", "local_setting.yaml", StateScope::Local, StatePurpose::Inventory),
    state("queue", "queue.yaml", StateScope::Local, StatePurpose::ImportExport),
    state("notepad", "notepad.txt", StateScope::Local, StatePurpose::ImportExport),
    state("global_setting", "global_setting.yaml", StateScope::Global, StatePurpose::FileOnly),
];

/// Is `key` a local state `Inventory` routes through `app_state`?
pub(crate) fn inventory_managed(key: &str) -> bool {
    MANAGED_STATES.iter().any(|state| {
        state.key == key
            && state.scope == StateScope::Local
            && state.purpose.inventory_managed()
    })
}

/// `(key, legacy file name)` pairs of the local states the SQLite backend
/// imports on first open and `narou db export-yaml` writes back.
pub(crate) fn import_export_states() -> impl Iterator<Item = (&'static str, &'static str)> {
    MANAGED_STATES
        .iter()
        .filter(|state| state.scope == StateScope::Local && state.purpose.import_export())
        .map(|state| (state.key, state.file))
}

/// Legacy file backing `(scope, key)`: local states in `narou_dir`, the global
/// settings map in `~/.narousetting/`. `None` for keys without a file — the
/// bookkeeping rows (`meta`, the site-definition import marker) stay in
/// `app_state` in every mode.
pub(crate) fn legacy_path(scope: &str, key: &str, narou_dir: &Path) -> Option<PathBuf> {
    let state = MANAGED_STATES
        .iter()
        .find(|state| state.key == key && state.scope.matches(scope))?;
    Some(match state.scope {
        StateScope::Local => narou_dir.join(state.file),
        StateScope::Global => {
            crate::db::inventory::global_setting_dir(narou_dir.parent()?).join(state.file)
        }
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn purposes_split_inventory_from_import_export() {
        let inventory: Vec<&str> = MANAGED_STATES
            .iter()
            .filter(|state| state.purpose.inventory_managed())
            .map(|state| state.key)
            .collect();
        assert_eq!(
            inventory,
            vec![
                "freeze",
                "alias",
                "tag_colors",
                "login_cookie",
                "latest_convert",
                "author",
                "hidden",
                "favorite_tags",
                "virtual_novel_members",
                "local_setting",
            ]
        );
        assert!(inventory_managed("freeze"));
        // queue/notepad keep their own store; global_setting never leaves its
        // file.
        assert!(!inventory_managed("queue"));
        assert!(!inventory_managed("notepad"));
        assert!(!inventory_managed("global_setting"));

        let exported: Vec<&str> = import_export_states().map(|(key, _)| key).collect();
        assert_eq!(
            exported,
            vec![
                "freeze",
                "alias",
                "tag_colors",
                "login_cookie",
                "latest_convert",
                "author",
                "hidden",
                "favorite_tags",
                "virtual_novel_members",
                "local_setting",
                "queue",
                "notepad",
            ]
        );
    }

    #[test]
    fn legacy_paths_use_the_state_files() {
        let narou_dir = Path::new("/library/.narou");
        assert_eq!(
            legacy_path("inv", "notepad", narou_dir),
            Some(narou_dir.join("notepad.txt"))
        );
        assert_eq!(
            legacy_path("inv", "local_setting", narou_dir),
            Some(narou_dir.join("local_setting.yaml"))
        );
        // The scope half must match: a global key has no `.narou/` file.
        assert_eq!(legacy_path("inv", "global_setting", narou_dir), None);
        assert_eq!(legacy_path("meta", "database_yaml_sha", narou_dir), None);
        assert_eq!(legacy_path("inv", "webnovel_imported", narou_dir), None);
    }
}
