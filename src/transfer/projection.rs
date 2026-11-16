//! Runtime vs. complete file selection (`FileRole` projection).
//!
//! Both directions share these rules:
//!
//! * the export cataloguing side classifies library files, the WASM import
//!   side classifies archive entries with the same function, and the server
//!   re-classifies the claimed paths independently (client roles are never
//!   trusted);
//! * `runtime` (the Web default) moves the logical data needed to keep
//!   reading/updating/converting — current body, TOC, settings, illustrations
//!   and the illustration index — and keeps unknown/user files. Raw fetched
//!   HTML, optional history and proven regenerable output are excluded;
//! * `complete` preserves every eligible novel file;
//! * implementation internals (database bodies, WAL/SHM, locks, temp files,
//!   credentials, `.narou/` bookkeeping) never transfer;
//! * unknown files are never dropped just because their role is unclear, and
//!   `*.yaml` is never dropped just because the destination stores data in a
//!   database.
//!
//! See `docs/client-transfer-design.md` §4.1–4.3.

use serde::{Deserialize, Serialize};

use super::{Result, TransferError};

/// The role a file plays for the owning novel.
///
/// Serde names are part of the `/api/transfer/v2` wire contract and must not
/// change: `body`, `toc`, `metadata`, `settings`, `illustration`,
/// `illustration_index`, `raw`, `history`, `generated`, `unknown`, `internal`.
#[derive(Serialize, Deserialize, Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
#[serde(rename_all = "snake_case")]
pub enum FileRole {
    /// Current body chapters (`本文/*.yaml`).
    Body,
    /// Table of contents (`toc.yaml`).
    Toc,
    /// Novel metadata records travelling as manifest metadata rather than files.
    Metadata,
    /// User conversion settings and explicit covers
    /// (`setting.ini`, `replace.txt`, `converter.yaml`, `cover.*`).
    Settings,
    /// Illustration/APNG payloads (`挿絵/*`, resolved pool objects).
    Illustration,
    /// `.illustration_cache.yaml` — the index needed to resolve illustrations;
    /// "cache" in the name does not make it disposable.
    IllustrationIndex,
    /// Fetched raw HTML owned by the downloader (`raw/*`).
    Raw,
    /// Old body versions / diff backups (`本文/cache/*`, `cache/*`).
    History,
    /// Output proven to be regenerable by this application (export-side only;
    /// unproven files are classified [`FileRole::Unknown`] instead).
    Generated,
    /// Unknown or user-added files. Kept by default and shown separately —
    /// "unrecognised" is never a reason to drop user data.
    Unknown,
    /// Implementation internals and runtime state: DB bodies/WAL/SHM, row or
    /// object IDs, `backup/`, locks, temp files, credentials. Never sent.
    Internal,
}

impl FileRole {
    /// All roles in wire order.
    pub const ALL: [FileRole; 11] = [
        FileRole::Body,
        FileRole::Toc,
        FileRole::Metadata,
        FileRole::Settings,
        FileRole::Illustration,
        FileRole::IllustrationIndex,
        FileRole::Raw,
        FileRole::History,
        FileRole::Generated,
        FileRole::Unknown,
        FileRole::Internal,
    ];

    /// Wire name (`snake_case`).
    pub fn as_str(&self) -> &'static str {
        match self {
            FileRole::Body => "body",
            FileRole::Toc => "toc",
            FileRole::Metadata => "metadata",
            FileRole::Settings => "settings",
            FileRole::Illustration => "illustration",
            FileRole::IllustrationIndex => "illustration_index",
            FileRole::Raw => "raw",
            FileRole::History => "history",
            FileRole::Generated => "generated",
            FileRole::Unknown => "unknown",
            FileRole::Internal => "internal",
        }
    }

    /// Parses a wire role name.
    pub fn parse(value: &str) -> Option<FileRole> {
        FileRole::ALL.iter().copied().find(|role| role.as_str() == value)
    }

    /// Roles a destination cannot persist are never transferred there.
    /// Destination policy is applied by `project`, never by reclassifying
    /// forbidden files as an unrelated role.
    pub fn is_internal(&self) -> bool {
        matches!(self, FileRole::Internal)
    }
}

/// Selection profile: ordinary migration (`runtime`) vs. full preservation
/// (`complete`). Serde names are part of the wire contract.
#[derive(Serialize, Deserialize, Debug, Clone, Copy, PartialEq, Eq, Hash, Default)]
#[serde(rename_all = "snake_case")]
pub enum TransferProfile {
    /// Default: move logical data needed to keep using the library; exclude
    /// raw fetches, history and regenerable output unless explicitly added.
    #[default]
    Runtime,
    /// Preserve every eligible novel file, including raw/history/generated.
    Complete,
}


impl TransferProfile {
    /// Wire name.
    pub fn as_str(&self) -> &'static str {
        match self {
            TransferProfile::Runtime => "runtime",
            TransferProfile::Complete => "complete",
        }
    }


    /// Roles the profile sends by default (before `extra_roles` toggles and
    /// destination policy). `internal` never defaults on.
    fn default_selected(&self, role: FileRole) -> bool {
        match self {
            TransferProfile::Runtime => !matches!(
                role,
                FileRole::Raw | FileRole::History | FileRole::Generated | FileRole::Internal
            ),
            TransferProfile::Complete => role != FileRole::Internal,
        }
    }

    /// Whether the role may be toggled by `extra_roles` under this profile.
    ///
    /// `runtime` keeps `generated`/`internal` deselected unconditionally:
    /// regenerable output has no business travelling in an ordinary
    /// migration. `complete` allows opting *out* of nothing — `extra_roles`
    /// only ever *adds* roles — but exposes the same optional set so callers
    /// can describe them in one place.
    fn is_optional(&self, role: FileRole) -> bool {
        match self {
            TransferProfile::Runtime => matches!(role, FileRole::Raw | FileRole::History),
            TransferProfile::Complete => {
                matches!(role, FileRole::Raw | FileRole::History | FileRole::Generated)
            }
        }
    }

    /// Whether the file would be sent under this profile and optional set.
    ///
    /// `extra_roles` names [`FileRole::as_str`] values (from
    /// `POST /imports`/`exports`); unknown names are ignored here and rejected
    /// at the selection layer.
    pub fn is_selected(&self, role: FileRole, extra_roles: &[&str]) -> bool {
        if self.default_selected(role) {
            return true;
        }
        self.is_optional(role) && extra_roles.iter().any(|extra| *extra == role.as_str())
    }

    /// Whether the role is mandatory under this profile: the user may not
    /// exclude it (essential body/toc/settings/illustrations/index/unknown).
    pub fn is_required(&self, role: FileRole) -> bool {
        self.default_selected(role) && !self.is_optional(role)
    }

    /// Whether an explicit toggle exists for this role under this profile.
    pub fn is_selectable(&self, role: FileRole) -> bool {
        self.is_optional(role)
    }
}

/// Evidence the classifier may use beyond the path itself.
///
/// Import (client- and server-side) uses [`ClassifyContext::default`] —
/// archives carry no ownership proof, so regenerable-looking files degrade to
/// [`FileRole::Unknown`] and are kept. Export cataloguing sets
/// `owns_generated`/`raw_supported`/`history_supported` from source ownership
/// and destination capabilities.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ClassifyContext {
    /// True when the caller can prove the file is app-owned derived output
    /// (EPUB exports, `novel.txt`, conversion caches). Import: always false.
    pub owns_generated: bool,
    /// Destination may persist fetched raw HTML (false for Workers).
    pub raw_supported: bool,
    /// Destination may persist body history/cache versions.
    pub history_supported: bool,
}

impl Default for ClassifyContext {
    fn default() -> Self {
        Self { owns_generated: false, raw_supported: true, history_supported: true }
    }
}

/// Classify one file by its path relative to the novel directory.
///
/// `virtual_parent` marks files owned by a virtual parent novel; parents only
/// ever contain conversion settings and an explicit cover, so everything else
/// under them is treated as unknown metadata to preserve (never dropped).
///
/// The classification is purely logical: it never depends on the site name,
/// the storage backend, or whether the destination persists files in a DB.
/// When the caller cannot prove ownership, generated-looking files stay
/// [`FileRole::Unknown`] and are preserved.
pub fn classify(path: &str, virtual_parent: bool, ctx: &ClassifyContext) -> FileRole {
    if virtual_parent {
        return classify_virtual_parent(path);
    }
    if is_internal_path(path) {
        return FileRole::Internal;
    }
    if path == "toc.yaml" {
        return FileRole::Toc;
    }
    if path == ".illustration_cache.yaml" {
        return FileRole::IllustrationIndex;
    }
    if let Some(rest) = path.strip_prefix("本文/") {
        if rest.strip_prefix("cache/").is_some() {
            return FileRole::History;
        }
        return FileRole::Body;
    }
    if path.strip_prefix("raw/").is_some() {
        return FileRole::Raw;
    }
    if path.strip_prefix("cache/").is_some() || path.strip_prefix("backup/").is_some() {
        // `cache/` holds diff backups (history); `backup/` is app-owned
        // rollback state and is never part of a novel's portable data.
        return if path.starts_with("cache/") {
            FileRole::History
        } else {
            FileRole::Internal
        };
    }
    if path.strip_prefix("挿絵/").is_some() {
        return FileRole::Illustration;
    }
    if is_settings_file(path) {
        return FileRole::Settings;
    }
    if ctx.owns_generated && is_generated_path(path) {
        return FileRole::Generated;
    }
    FileRole::Unknown
}

/// Paths that are never portable novel data.
fn is_internal_path(path: &str) -> bool {
    // Runtime state and implementation internals. `.narou*` covers the
    // library bookkeeping directories (`.narou/`, `.narou-staging/`,
    // `.narou-transfer/`) when they appear inside a novel folder.
    if path.starts_with("backup/") || path.starts_with(".narou") {
        return true;
    }
    let name = path.rsplit('/').next().unwrap_or(path);
    if name.ends_with(".lock") || name.ends_with(".tmp") || name.ends_with(".swp") {
        return true;
    }
    // Database bodies and journals are never novel files.
    if name.ends_with(".db") || name.ends_with(".db-wal") || name.ends_with(".db-shm")
        || name.ends_with(".sqlite") || name.ends_with(".sqlite-wal") || name.ends_with(".sqlite-shm")
        || name.ends_with(".wal") || name.ends_with(".shm")
    {
        return true;
    }
    // Credential material must never leave the source.
    if name.eq_ignore_ascii_case("credentials.yaml")
        || name.eq_ignore_ascii_case("session.yaml")
        || name.ends_with(".key")
    {
        return true;
    }
    false
}

fn is_settings_file(path: &str) -> bool {
    matches!(path, "setting.ini" | "replace.txt" | "converter.yaml")
        || matches!(path, "cover.jpg" | "cover.png" | "cover.jpeg")
}

/// App-owned regenerable outputs. Only reached when the caller has proven
/// ownership (`ClassifyContext::owns_generated`); without that proof the same
/// paths classify as [`FileRole::Unknown`] and are preserved.
fn is_generated_path(path: &str) -> bool {
    let name = path.rsplit('/').next().unwrap_or(path);
    name == "novel.txt"
        || name.ends_with(".epub")
        || path.starts_with("converted/")
        || path.starts_with("epub/")
}

fn classify_virtual_parent(path: &str) -> FileRole {
    if is_internal_path(path) {
        return FileRole::Internal;
    }
    if is_settings_file(path) {
        return FileRole::Settings;
    }
    // Parents have no body/toc/illustrations of their own; anything else the
    // archive carries under them is user data and is preserved as unknown.
    FileRole::Unknown
}

/// Effective send decision for one file under a profile.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Send {
    /// Always sent; the user cannot deselect it.
    Required,
    /// Sent iff the user selected it (`extra_roles`); `default` reports the
    /// profile's default stance for UI display.
    Optional { default: bool },
    /// Never sent under this profile; `reason` is a stable machine-readable
    /// code for UI/error display.
    Excluded { reason: &'static str },
}

/// Compute the send decision for `role` under `profile` and `extra_roles`.
pub fn send_decision(profile: TransferProfile, role: FileRole, extra_roles: &[&str]) -> Send {
    if role == FileRole::Internal {
        return Send::Excluded { reason: "internal" };
    }
    if profile.is_required(role) {
        return Send::Required;
    }
    if profile.is_selectable(role) {
        let default = profile.default_selected(role);
        if default || extra_roles.iter().any(|extra| *extra == role.as_str()) {
            return Send::Optional { default: true };
        }
        return Send::Optional { default };
    }
    let reason = match role {
        FileRole::Generated => "generated",
        _ => "excluded",
    };
    Send::Excluded { reason }
}

/// A validated `extra_roles` list. Rejects unknown role names and roles that
/// have no toggle under `profile` (required/excluded roles are not options).
pub fn validate_extra_roles(
    profile: TransferProfile,
    extra_roles: &[String],
) -> std::result::Result<Vec<FileRole>, TransferError> {
    let mut roles = Vec::with_capacity(extra_roles.len());
    for name in extra_roles {
        let role = FileRole::parse(name).ok_or_else(|| {
            TransferError::InvalidSelection(format!("unknown extra role {name:?}"))
        })?;
        if !profile.is_selectable(role) {
            return Err(TransferError::InvalidSelection(format!(
                "role {name:?} is not selectable under {}",
                profile.as_str()
            )));
        }
        roles.push(role);
    }
    Ok(roles)
}

/// One file after projection: its role, whether it is sent, and why not.
#[derive(Debug, Clone)]
pub struct ProjectedFile {
    /// Position in the novel's `files` list.
    pub file_index: usize,
    /// Path relative to the novel directory (validated form).
    pub path: String,
    /// Classified role.
    pub role: FileRole,
    /// Send decision under the chosen profile.
    pub send: Send,
    /// Declared size and digest from the validated source, when known.
    pub size: Option<u64>,
    pub sha256: Option<String>,
}

/// One novel after projection.
#[derive(Debug, Clone)]
pub struct ProjectedNovel {
    /// Position in the manifest.
    pub index: usize,
    /// Whether this is a virtual parent.
    pub virtual_parent: bool,
    /// Projected files in manifest order; `file_id` on the wire is the ordinal
    /// of sent files across the whole manifest.
    pub files: Vec<ProjectedFile>,
}

/// The full selection result over a validated manifest.
#[derive(Debug, Clone)]
pub struct TransferProjection {
    pub profile: TransferProfile,
    pub extra_roles: Vec<FileRole>,
    pub novels: Vec<ProjectedNovel>,
    /// Ordinal → (novel index, file index) for every *sent* file, in wire
    /// `file_id` order.
    pub wire_files: Vec<(usize, usize)>,
}

impl TransferProjection {
    /// Number of files that will be transferred.
    pub fn file_count(&self) -> usize { self.wire_files.len() }

    /// Stable identity of the confirmed projection; novel metadata is already
    /// covered by `fingerprint`, paths/sizes/SHA and selections are bound here.
    pub fn digest(&self, policy_version: u32, fingerprint: &str) -> Result<String> {
        let extra_roles: Vec<_> = self.extra_roles.iter().map(|role| role.as_str().to_owned()).collect();
        let mut digest = ProjectionDigest::new(policy_version, self.profile, &extra_roles, fingerprint)?;
        for (index, novel) in self.novels.iter().enumerate() {
            digest.novel(index as u32);
            for file in &novel.files {
                if !file.send.is_send() { continue; }
                digest.file(&file.path,
                    file.size.ok_or_else(|| TransferError::InvalidArchive("projection missing size".into()))?,
                    file.sha256.as_deref().ok_or_else(|| TransferError::InvalidArchive("projection missing SHA".into()))?)?;
            }
        }
        Ok(digest.finish())
    }
}

/// Incremental selection digest for independent server planning. Feed each
/// dense projected novel, then its selected files in wire order. Persist the
/// algorithm-tagged SHA checkpoint with the parser cursor between advances.
pub struct ProjectionDigest(sha2::Sha256);
impl ProjectionDigest {
    pub fn new(policy_version: u32, profile: TransferProfile, extra_roles: &[String], fingerprint: &str) -> Result<Self> {
        use sha2::Digest;
        if policy_version == 0 { return Err(TransferError::InvalidSelection("zero policy version".into())); }
        super::protocol::check_sha256_hex(fingerprint)?;
        let mut roles = validate_extra_roles(profile, extra_roles)?;
        roles.sort_unstable(); roles.dedup();
        let mut hash = sha2::Sha256::new();
        hash.update(b"narou-transfer-projection-v2\0");
        hash.update(policy_version.to_be_bytes());
        let mut raw = [0u8; 32];
        hex::decode_to_slice(fingerprint, &mut raw).map_err(|_| TransferError::Protocol("invalid fingerprint".into()))?;
        hash.update(raw);
        hash.update(profile.as_str().as_bytes()); hash.update([0]);
        hash.update((roles.len() as u32).to_be_bytes());
        for role in roles { hash.update(role.as_str().as_bytes()); hash.update([0]); }
        Ok(Self(hash))
    }
    pub fn novel(&mut self, index: u32) {
        use sha2::Digest;
        self.0.update(b"N"); self.0.update(index.to_be_bytes());
    }
    pub fn file(&mut self, path: &str, size: u64, sha256: &str) -> Result<()> {
        use sha2::Digest;
        super::protocol::check_sha256_hex(sha256)?;
        let mut raw = [0u8; 32];
        hex::decode_to_slice(sha256, &mut raw).map_err(|_| TransferError::Protocol("invalid sha256".into()))?;
        self.0.update(b"F"); self.0.update((path.len() as u64).to_be_bytes());
        self.0.update(path.as_bytes()); self.0.update(size.to_be_bytes()); self.0.update(raw);
        Ok(())
    }
    pub fn checkpoint(&self) -> Vec<u8> {
        use sha2::digest::common::hazmat::SerializableState;
        let state = self.0.serialize();
        let mut bytes = Vec::with_capacity(state.len() + 1);
        bytes.push(2);
        bytes.extend_from_slice(state.as_ref());
        bytes
    }
    pub fn from_checkpoint(bytes: &[u8]) -> Result<Self> {
        use sha2::digest::common::hazmat::{SerializableState, SerializedState};
        let Some((&2, bytes)) = bytes.split_first() else {
            return Err(TransferError::Protocol("unknown projection hash checkpoint".into()));
        };
        let state = SerializedState::<sha2::Sha256>::try_from(bytes)
            .map_err(|_| TransferError::Protocol("invalid projection hash checkpoint length".into()))?;
        sha2::Sha256::deserialize(&state).map(Self)
            .map_err(|_| TransferError::Protocol("corrupt projection hash checkpoint".into()))
    }
    pub fn finish(self) -> String {
        use sha2::Digest;
        hex::encode(self.0.finalize())
    }
}

/// Apply `profile` + `extra_roles` to a validated manifest.
///
/// `selected_novels` limits the projection to manifest positions (`None` =
/// all). Required files are always included; `excluded_files` may remove only
/// files whose send decision is `Optional { .. }`. Unknown extra-role names or
/// attempts to exclude required files are rejected.
pub fn project(
    manifest: &super::ArchiveManifest,
    profile: TransferProfile,
    extra_roles: &[String],
    selected_novels: Option<&[usize]>,
    excluded_files: &[usize],
    ctx: &ClassifyContext,
) -> Result<TransferProjection> {
    let mut roles = validate_extra_roles(profile, extra_roles)?;
    roles.sort_unstable();
    roles.dedup();
    let names: Vec<&str> = roles.iter().map(FileRole::as_str).collect();
    let mut included = vec![selected_novels.is_none(); manifest.novels.len()];
    if let Some(indexes) = selected_novels {
        for &index in indexes {
            let selected = included.get_mut(index).ok_or_else(|| {
                TransferError::InvalidSelection(format!("novel index {index} out of range"))
            })?;
            *selected = true;
        }
    }
    // Selecting a virtual parent selects its ordinary members, in their
    // original order. The archive's validated graph guarantees uniqueness.
    let by_url: std::collections::HashMap<&str, usize> = manifest.novels.iter()
        .enumerate().map(|(index, novel)| (novel.toc_url.as_str(), index)).collect();
    for (index, novel) in manifest.novels.iter().enumerate() {
        if included[index] && let Some(members) = &novel.virtual_members {
            for member in members {
                let member_index = *by_url.get(member.as_str()).ok_or_else(|| {
                    TransferError::InvalidSelection(format!("missing member {member:?}"))
                })?;
                if manifest.novels[member_index].virtual_members.is_some() {
                    return Err(TransferError::InvalidSelection("nested virtual parent".into()));
                }
                included[member_index] = true;
            }
        }
    }
    let mut projection = TransferProjection {
        profile, extra_roles: roles.clone(), novels: Vec::new(), wire_files: Vec::new(),
    };
    for (index, novel) in manifest.novels.iter().enumerate() {
        if !included[index] { continue; }
        let virtual_parent = novel.virtual_members.is_some();
        let mut files = Vec::with_capacity(novel.files.len());
        for (file_index, file) in novel.files.iter().enumerate() {
            let role = classify(&file.path, virtual_parent, ctx);
            let mut send = send_decision(profile, role, &names);
            if role == FileRole::Raw && !ctx.raw_supported {
                send = Send::Excluded { reason: "raw_forbidden" };
            } else if role == FileRole::History && !ctx.history_supported {
                send = Send::Excluded { reason: "history_unsupported" };
            }
            if send.is_send() { projection.wire_files.push((index, file_index)); }
            files.push(ProjectedFile {
                file_index, path: file.path.clone(), role, send,
                size: file.size, sha256: file.sha256.clone(),
            });
        }
        projection.novels.push(ProjectedNovel { index, virtual_parent, files });
    }
    let mut excluded = excluded_files.to_vec();
    excluded.sort_unstable();
    excluded.dedup();
    for &ordinal in &excluded {
        let &(novel, file) = projection.wire_files.get(ordinal).ok_or_else(|| {
            TransferError::InvalidSelection(format!("excluded file ordinal {ordinal} out of range"))
        })?;
        let projected = projection.novels.iter_mut().find(|item| item.index == novel)
            .expect("projected novel");
        if !matches!(projected.files[file].send, Send::Optional { .. }) {
            return Err(TransferError::InvalidSelection(format!(
                "file ordinal {ordinal} is required and cannot be excluded"
            )));
        }
        projected.files[file].send = Send::Excluded { reason: "deselected" };
    }
    if !excluded.is_empty() {
        let mut ordinal = 0usize;
        projection.wire_files.retain(|_| {
            let keep = excluded.binary_search(&ordinal).is_err();
            ordinal += 1;
            keep
        });
    }
    Ok(projection)
}

impl Send {
    pub fn is_send(&self) -> bool {
        matches!(self, Self::Required | Self::Optional { default: true })
    }
}

/// Canonical client-side selection. The destination independently validates
/// its policy; `allow_raw` is never authorization to bypass a server policy.
#[derive(Serialize, Deserialize, Debug, Clone)]
#[serde(deny_unknown_fields)]
pub struct TransferSelection {
    #[serde(default)]
    pub profile: TransferProfile,
    pub policy_version: u32,
    #[serde(default)]
    pub extra_roles: Vec<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub novels: Option<Vec<usize>>,
    #[serde(default)]
    pub excluded_files: Vec<usize>,
    #[serde(default = "default_true")]
    pub allow_raw: bool,
}

fn default_true() -> bool { true }

impl Default for TransferSelection {
    fn default() -> Self {
        Self { profile: TransferProfile::Runtime, policy_version: 1,
            extra_roles: Vec::new(), novels: None, excluded_files: Vec::new(), allow_raw: true }
    }
}

impl TransferSelection {
    pub fn from_json(json: &str) -> Result<Self> {
        if json.len() > super::protocol::CONTROL_BODY_MAX_BYTES as usize {
            return Err(TransferError::TooLarge("selection document exceeds control limit".into()));
        }
        let selection: Self = serde_json::from_str(json)?;
        if selection.policy_version == 0 {
            return Err(TransferError::InvalidSelection("policy_version must be positive".into()));
        }
        validate_extra_roles(selection.profile, &selection.extra_roles)?;
        Ok(selection)
    }

    pub fn context(&self) -> ClassifyContext {
        ClassifyContext { owns_generated: false, raw_supported: self.allow_raw, history_supported: true }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn runtime_never_sends_unselected_optional_files_and_policy_keeps_role() {
        let manifest = super::super::ArchiveManifest { novels: vec![super::super::ArchiveNovel {
            files: ["toc.yaml", "本文/1.yaml", "raw/1.html", "本文/cache/1.yaml",
                "memo.yaml", "novel.txt", ".illustration_cache.yaml"].into_iter()
                .map(|path| super::super::ArchiveFile { path: path.into(), size: Some(1), sha256: Some("a".repeat(64)), ..Default::default() }).collect(),
            ..Default::default()
        }], ..Default::default() };
        let selected = project(&manifest, TransferProfile::Runtime, &[], None, &[], &ClassifyContext::default()).unwrap();
        assert_eq!(selected.wire_files.len(), 5);
        assert!(!selected.novels[0].files[2].send.is_send());
        assert!(!selected.novels[0].files[3].send.is_send());
        let policy = ClassifyContext { raw_supported: false, ..Default::default() };
        let complete = project(&manifest, TransferProfile::Complete, &[], None, &[], &policy).unwrap();
        assert_eq!(complete.novels[0].files[2].role, FileRole::Raw);
        assert_eq!(complete.novels[0].files[2].send, Send::Excluded { reason: "raw_forbidden" });
        assert_eq!(complete.wire_files.len(), 6);
        assert!(project(&manifest, TransferProfile::Runtime, &[], None, &[0], &policy).is_err());
    }

    #[test]
    fn selecting_parent_includes_ordered_members_and_optional_exclusions_are_stable() {
        use super::super::{ArchiveFile, ArchiveManifest, ArchiveNovel};
        let mut novels: Vec<_> = ["one", "two", "unused"].into_iter().map(|name| ArchiveNovel {
            toc_url: name.into(), files: ["toc.yaml", "raw/1.html", "raw/2.html"].into_iter()
                .map(|path| ArchiveFile { path: path.into(), ..Default::default() }).collect(),
            ..Default::default()
        }).collect();
        novels.push(ArchiveNovel { virtual_members: Some(vec!["two".into(), "one".into()]), ..Default::default() });
        let manifest = ArchiveManifest { novels, ..Default::default() };
        let selected = project(&manifest, TransferProfile::Complete, &[], Some(&[3]), &[1, 1], &Default::default()).unwrap();
        assert_eq!(selected.novels.iter().map(|novel| novel.index).collect::<Vec<_>>(), vec![0, 1, 3]);
        assert_eq!(selected.wire_files, vec![(0, 0), (0, 2), (1, 0), (1, 1), (1, 2)]);
        assert!(project(&manifest, TransferProfile::Runtime, &[], Some(&[4]), &[], &Default::default()).is_err());
    }

    #[test]
    fn canonical_selection_rejects_obsolete_map_and_digest_resumes() {
        let selection = TransferSelection::from_json(r#"{"profile":"runtime","policy_version":1,"extra_roles":["raw"],"allow_raw":false}"#).unwrap();
        assert!(!selection.context().raw_supported);
        assert!(TransferSelection::from_json(r#"{"policy_version":1,"roles":{"raw":true}}"#).is_err());
        let mut digest = ProjectionDigest::new(1, selection.profile, &selection.extra_roles, &"a".repeat(64)).unwrap();
        digest.novel(0);
        let mut resumed = ProjectionDigest::from_checkpoint(&digest.checkpoint()).unwrap();
        for value in [&mut digest, &mut resumed] { value.file("toc.yaml", 12, &"b".repeat(64)).unwrap(); }
        assert_eq!(digest.finish(), resumed.finish());
        assert!(ProjectionDigest::from_checkpoint(&[3]).is_err());
    }

    fn ctx() -> ClassifyContext {
        ClassifyContext::default()
    }

    #[test]
    fn essential_files_are_classified() {
        let ctx = ctx();
        assert_eq!(classify("toc.yaml", false, &ctx), FileRole::Toc);
        assert_eq!(classify("本文/1 第一話.yaml", false, &ctx), FileRole::Body);
        assert_eq!(classify("setting.ini", false, &ctx), FileRole::Settings);
        assert_eq!(classify("converter.yaml", false, &ctx), FileRole::Settings);
        assert_eq!(classify("cover.png", false, &ctx), FileRole::Settings);
        assert_eq!(classify("挿絵/i0001.jpg", false, &ctx), FileRole::Illustration);
        assert_eq!(classify(".illustration_cache.yaml", false, &ctx), FileRole::IllustrationIndex);
    }

    #[test]
    fn excluded_classes_are_classified() {
        let ctx = ctx();
        assert_eq!(classify("raw/1.html", false, &ctx), FileRole::Raw);
        assert_eq!(classify("cache/1.yaml", false, &ctx), FileRole::History);
        assert_eq!(classify("本文/cache/1.yaml", false, &ctx), FileRole::History);
        assert_eq!(classify("backup/x", false, &ctx), FileRole::Internal);
        assert_eq!(classify("n.db", false, &ctx), FileRole::Internal);
        assert_eq!(classify("x.lock", false, &ctx), FileRole::Internal);
        assert_eq!(classify("novel.db-wal", false, &ctx), FileRole::Internal);
    }

    #[test]
    fn unknown_files_are_preserved() {
        let ctx = ctx();
        assert_eq!(classify("memo.md", false, &ctx), FileRole::Unknown);
        assert_eq!(classify("novel.txt", false, &ctx), FileRole::Unknown);
        // Without ownership proof even epub-looking files stay unknown.
        assert_eq!(classify("x.epub", false, &ctx), FileRole::Unknown);
        let owned = ClassifyContext { owns_generated: true, ..Default::default() };
        assert_eq!(classify("novel.txt", false, &owned), FileRole::Generated);
        assert_eq!(classify("x.epub", false, &owned), FileRole::Generated);
    }

    #[test]
    fn virtual_parent_files_are_settings_or_unknown() {
        let ctx = ctx();
        assert_eq!(classify("setting.ini", true, &ctx), FileRole::Settings);
        assert_eq!(classify("cover.jpg", true, &ctx), FileRole::Settings);
        assert_eq!(classify("本文/1.yaml", true, &ctx), FileRole::Unknown);
    }

    #[test]
    fn runtime_defaults_match_design() {
        let p = TransferProfile::Runtime;
        for role in [FileRole::Body, FileRole::Toc, FileRole::Settings,
            FileRole::Illustration, FileRole::IllustrationIndex, FileRole::Unknown] {
            assert!(p.is_selected(role, &[]), "{role:?}");
            assert!(p.is_required(role), "{role:?}");
        }
        for role in [FileRole::Raw, FileRole::History] {
            assert!(!p.is_selected(role, &[]), "{role:?}");
            assert!(p.is_selected(role, &[role.as_str()]), "{role:?}");
            assert!(!p.is_required(role), "{role:?}");
        }
        assert!(!p.is_selected(FileRole::Generated, &["generated"]));
        assert!(!p.is_selected(FileRole::Internal, &["internal"]));
    }

    #[test]
    fn complete_selects_everything_but_internal() {
        let p = TransferProfile::Complete;
        for role in FileRole::ALL {
            assert_eq!(p.is_selected(role, &[]), role != FileRole::Internal, "{role:?}");
        }
        assert!(p.is_selected(FileRole::Raw, &[]));
        assert!(p.is_selected(FileRole::History, &[]));
        assert!(p.is_selected(FileRole::Generated, &[]));
    }

    #[test]
    fn destination_policy_turns_roles_off() {
        let ctx = ClassifyContext { owns_generated: false, raw_supported: false, history_supported: false };
        assert_eq!(classify("raw/1.html", false, &ctx), if "raw/1.html".starts_with("raw/") { FileRole::Raw } else { FileRole::History });
        assert_eq!(classify("cache/1.yaml", false, &ctx), if "cache/1.yaml".starts_with("raw/") { FileRole::Raw } else { FileRole::History });
        assert_eq!(classify("本文/cache/1.yaml", false, &ctx), if "本文/cache/1.yaml".starts_with("raw/") { FileRole::Raw } else { FileRole::History });
        assert_eq!(classify("挿絵/a.png", false, &ctx), FileRole::Illustration);
    }

    #[test]
    fn extra_roles_are_validated() {
        assert!(validate_extra_roles(TransferProfile::Runtime, &["raw".into()]).is_ok());
        assert!(validate_extra_roles(TransferProfile::Runtime, &["body".into()]).is_err());
        assert!(validate_extra_roles(TransferProfile::Runtime, &["generated".into()]).is_err());
        assert!(validate_extra_roles(TransferProfile::Runtime, &["bogus".into()]).is_err());
        assert!(validate_extra_roles(TransferProfile::Complete, &["generated".into()]).is_ok());
    }

    #[test]
    fn wire_names_are_stable() {
        assert_eq!(FileRole::IllustrationIndex.as_str(), "illustration_index");
        assert_eq!(serde_json::to_string(&FileRole::Body).unwrap(), "\"body\"");
        assert_eq!(serde_json::to_string(&TransferProfile::Complete).unwrap(), "\"complete\"");
        assert_eq!(FileRole::parse("illustration_index"), Some(FileRole::IllustrationIndex));
    }
}
