use super::*;
use crate::application::transfer::{PlanAction,PublicationReceipt};

struct Fixture { root:tempfile::TempDir, path:PathBuf, marker:TransferCommitMarker, original:String, replacement:String }
impl Fixture {
    fn new()->Self {
        let root=tempfile::tempdir().unwrap();let path=root.path().join(".narou/database.yaml");fs::create_dir_all(path.parent().unwrap()).unwrap();
        let receipt=PublicationReceipt{novel_ordinal:3,action:PlanAction::Overwrite,outcome:"overwritten".into(),assigned_id:Some(13),files_installed:1,error:None};let marker=TransferCommitMarker{session_id:"session".into(),publication_id:3,novel_id:NovelId(13),receipt:serde_json::to_vec(&receipt).unwrap(),yaml_commit_path:root.path().join(".narou/transfer/publications/session/3/committed")};
        let original=format!("13:\n  title: {}\n","a".repeat(2*crate::transfer::protocol::IO_CHUNK_BYTES+19));let replacement=format!("13:\n  title: {}\n","b".repeat(2*crate::transfer::protocol::IO_CHUNK_BYTES+31));fs::write(&path,&original).unwrap();Self{root,path,marker,original,replacement}
    }
    fn publication(&self)->&Path {self.marker.yaml_commit_path.parent().unwrap()}
    fn stage(&self){stage_metadata(&self.marker,&self.path,Some(&self.original),&self.replacement).unwrap();}
}

#[test]
fn premark_recovery_restores_exact_fingerprint_idempotently(){
    let fixture=Fixture::new();fixture.stage();install_metadata(&fixture.marker,&fixture.path).unwrap();assert_eq!(fs::read_to_string(&fixture.path).unwrap(),fixture.replacement);assert!(!metadata_committed(fixture.root.path(),fixture.publication()).unwrap());
    for _ in 0..2{recover_metadata_locked(fixture.root.path(),fixture.publication(),false).unwrap();assert_eq!(fs::read_to_string(&fixture.path).unwrap(),fixture.original);}
}

#[test]
fn startup_observes_actual_sql_receipt_before_state_database_is_open(){
    let fixture=Fixture::new();fixture.stage();let connection=rusqlite::Connection::open(fixture.root.path().join(".narou/db.sqlite")).unwrap();connection.execute_batch("CREATE TABLE transfer_backend_marks(key TEXT PRIMARY KEY,value BLOB NOT NULL)").unwrap();connection.execute("INSERT INTO transfer_backend_marks(key,value) VALUES('commit:session:3',?1)",[&fixture.marker.receipt]).unwrap();drop(connection);
    recover_pending_metadata_before_open(fixture.root.path()).unwrap();assert_eq!(fs::read_to_string(&fixture.path).unwrap(),fixture.replacement);assert!(metadata_committed(fixture.root.path(),fixture.publication()).unwrap());assert!(!fixture.root.path().join(".narou/transfer/metadata-pending").exists());
}

#[test]
fn corrupt_receipt_and_third_fingerprint_never_choose_a_blind_undo_or_redo(){
    let fixture=Fixture::new();fixture.stage();fs::write(&fixture.marker.yaml_commit_path,b"not a receipt").unwrap();assert!(recover_pending_metadata_before_open(fixture.root.path()).is_err());assert_eq!(fs::read_to_string(&fixture.path).unwrap(),fixture.original);
    fs::write(&fixture.marker.yaml_commit_path,&fixture.marker.receipt).unwrap();fs::write(&fixture.path,"third-party metadata\n").unwrap();for committed in [false,true]{assert!(recover_metadata_locked(fixture.root.path(),fixture.publication(),committed).is_err());assert_eq!(fs::read_to_string(&fixture.path).unwrap(),"third-party metadata\n");}
}

#[test]
fn metadata_images_are_reclaimed_only_after_recovery_and_receipts_survive(){
    let fixture=Fixture::new();fixture.stage();assert!(reclaim_metadata_images(fixture.root.path(),"session",3,1).is_err());fs::write(&fixture.marker.yaml_commit_path,&fixture.marker.receipt).unwrap();recover_pending_metadata_before_open(fixture.root.path()).unwrap();let mut done=false;for _ in 0..10{if reclaim_metadata_images(fixture.root.path(),"session",3,1).unwrap(){done=true;break;}}assert!(done);assert!(!fixture.publication().join("metadata").exists());assert!(metadata_committed(fixture.root.path(),fixture.publication()).unwrap());
}

#[test]
fn resolved_redo_preserves_later_global_metadata_edits_even_if_pointer_reappears(){
    let fixture=Fixture::new();fixture.stage();fs::write(&fixture.marker.yaml_commit_path,&fixture.marker.receipt).unwrap();recover_pending_metadata_before_open(fixture.root.path()).unwrap();fs::write(&fixture.path,"later legitimate global metadata\n").unwrap();let owner=fixture.publication().strip_prefix(fixture.root.path()).unwrap().to_string_lossy().replace('\\',"/");fs::write(fixture.root.path().join(".narou/transfer/metadata-pending"),owner).unwrap();recover_pending_metadata_before_open(fixture.root.path()).unwrap();assert_eq!(fs::read_to_string(&fixture.path).unwrap(),"later legitimate global metadata\n");assert!(recover_metadata_locked(fixture.root.path(),fixture.publication(),false).is_err());
}
