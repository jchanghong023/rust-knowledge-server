use serde_json::Value;

use super::*;
use crate::{catalog::CatalogError, data_root::Root};

const EMPLOYEE: &str = "00001234";
const LIBRARY: &str = "engineering/docs";

fn open(path: &Path) -> (Catalog, Audit) {
    let catalog = Catalog::open(Root::open(path).unwrap()).unwrap();
    let audit = Audit::open(catalog.root_path(), &catalog).unwrap();
    (catalog, audit)
}

fn status(audit: &Audit, catalog: &Catalog, id: Uuid) -> OperationStatus {
    audit.status(id, catalog).unwrap().unwrap()
}

fn file_with_suffix(audit: &Audit, suffix: &str) -> PathBuf {
    let files: Vec<_> = fs::read_dir(&audit.directory)
        .unwrap()
        .map(|entry| entry.unwrap().path())
        .filter(|path| {
            path.file_name()
                .unwrap()
                .to_str()
                .unwrap()
                .ends_with(suffix)
        })
        .collect();
    let [path]: [PathBuf; 1] = files.try_into().expect("exactly one matching audit file");
    path
}

#[test]
fn catalog_receipt_survives_result_write_failure_and_reconciles_once() {
    let disk = tempfile::tempdir().unwrap();
    let (catalog, audit) = open(disk.path());
    let operation = audit
        .begin(EMPLOYEE, Action::Create, Some(LIBRARY))
        .unwrap();
    let id = operation.id;
    assert_eq!(status(&audit, &catalog, id).outcome, Outcome::InProgress);
    let receipt = catalog.create(LIBRARY, Some(id)).unwrap();

    // A directory cannot be overwritten with a result file on Windows or Linux.
    let blocked_result = audit.path(id, RESULT);
    fs::create_dir(&blocked_result).unwrap();
    assert!(audit.finish(&operation, Outcome::Succeeded).is_err());
    assert_eq!(status(&audit, &catalog, id).outcome, Outcome::Succeeded);
    assert_eq!(catalog.receipt(id).unwrap(), Some(receipt));
    drop(operation);
    drop(audit);
    drop(catalog);

    // Restart with the fault still present. The service must retain its commit evidence.
    let (catalog, audit) = open(disk.path());
    assert_eq!(status(&audit, &catalog, id).outcome, Outcome::Succeeded);
    assert!(blocked_result.is_dir());
    assert_eq!(catalog.receipt(id).unwrap(), Some(receipt));
    fs::remove_dir(&blocked_result).unwrap();
    drop(audit);
    drop(catalog);

    let (catalog, audit) = open(disk.path());
    assert_eq!(catalog.list().unwrap()[0].name, LIBRARY);
    assert_eq!(catalog.receipt(id).unwrap(), None);
    let recovered = status(&audit, &catalog, id);
    assert_eq!(recovered.id, id);
    assert_eq!(recovered.employee, EMPLOYEE);
    assert!(recovered.employee_self_reported);
    assert_eq!(recovered.action, Action::Create);
    assert_eq!(recovered.library.as_deref(), Some(LIBRARY));
    assert_eq!(recovered.outcome, Outcome::Succeeded);
    let result_path = file_with_suffix(&audit, ".result.json");
    let result = fs::read(&result_path).unwrap();
    drop(audit);
    drop(catalog);

    let (catalog, audit) = open(disk.path());
    assert_eq!(status(&audit, &catalog, id).outcome, Outcome::Succeeded);
    assert_eq!(file_with_suffix(&audit, ".result.json"), result_path);
    assert_eq!(fs::read(result_path).unwrap(), result);
}

#[test]
fn recovery_clears_receipt_without_replacing_an_existing_result() {
    let disk = tempfile::tempdir().unwrap();
    let (catalog, audit) = open(disk.path());
    catalog.create(LIBRARY, None).unwrap();
    let operation = audit
        .begin(EMPLOYEE, Action::Delete, Some(LIBRARY))
        .unwrap();
    let id = operation.id;
    let receipt = catalog.delete(LIBRARY, Some(id)).unwrap();
    audit.finish(&operation, Outcome::Succeeded).unwrap();
    assert_eq!(catalog.receipt(id).unwrap(), Some(receipt));
    let path = file_with_suffix(&audit, ".result.json");
    let original = fs::read(&path).unwrap();
    drop(operation);
    drop(audit);
    drop(catalog);

    let (catalog, audit) = open(disk.path());
    assert!(catalog.list().unwrap().is_empty());
    assert_eq!(catalog.receipt(id).unwrap(), None);
    assert_eq!(status(&audit, &catalog, id).outcome, Outcome::Succeeded);
    assert_eq!(file_with_suffix(&audit, ".result.json"), path);
    assert_eq!(fs::read(path).unwrap(), original);
}

#[test]
fn dropped_import_without_receipt_is_interrupted_before_and_after_restart() {
    let disk = tempfile::tempdir().unwrap();
    let (catalog, audit) = open(disk.path());
    catalog.create(LIBRARY, None).unwrap();
    let operation = audit
        .begin(EMPLOYEE, Action::Import, Some(LIBRARY))
        .unwrap();
    let id = operation.id;
    let build = catalog.start_build(LIBRARY).unwrap();
    assert_eq!(status(&audit, &catalog, id).outcome, Outcome::InProgress);
    drop(build);
    drop(operation);
    assert_eq!(status(&audit, &catalog, id).outcome, Outcome::Interrupted);
    assert!(!audit.path(id, RESULT).exists());
    drop(audit);
    drop(catalog);

    let (catalog, audit) = open(disk.path());
    assert_eq!(catalog.receipt(id).unwrap(), None);
    assert!(catalog.snapshot(Some(LIBRARY)).unwrap().is_empty());
    let recovered = status(&audit, &catalog, id);
    assert_eq!(recovered.employee, EMPLOYEE);
    assert_eq!(recovered.action, Action::Import);
    assert_eq!(recovered.outcome, Outcome::Interrupted);
    assert_eq!(
        file_with_suffix(&audit, ".result.json"),
        audit.path(id, RESULT)
    );
}

#[test]
fn uncertain_commit_is_unknown_until_catalog_reopens() {
    let disk = tempfile::tempdir().unwrap();
    let (catalog, audit) = open(disk.path());
    catalog.create("existing", None).unwrap();
    let operation = audit
        .begin(EMPLOYEE, Action::Create, Some(LIBRARY))
        .unwrap();
    let id = operation.id;
    let table = catalog.root_path().join("catalog.json");
    let saved = catalog.root_path().join("saved-catalog");
    fs::rename(&table, &saved).unwrap();
    fs::create_dir(&table).unwrap();
    assert!(matches!(
        catalog.create(LIBRARY, Some(id)),
        Err(CatalogError::CommitUncertain(_))
    ));
    audit.unresolved(&operation);
    assert_eq!(status(&audit, &catalog, id).outcome, Outcome::Unknown);
    assert!(!audit.path(id, RESULT).exists());
    fs::remove_dir(&table).unwrap();
    fs::rename(&saved, &table).unwrap();
    drop(operation);
    drop(audit);
    drop(catalog);

    let (catalog, audit) = open(disk.path());
    assert_eq!(catalog.list().unwrap()[0].name, "existing");
    assert_eq!(status(&audit, &catalog, id).outcome, Outcome::Interrupted);
}

#[test]
fn registration_and_start_fail_closed_and_validate_employee_format() {
    let disk = tempfile::tempdir().unwrap();
    let (catalog, audit) = open(disk.path());
    for invalid in [
        "1234567",
        "123456789",
        "0000123 ",
        " 0001234",
        "１２３４５６７８",
        "00001e34",
        "",
    ] {
        assert_eq!(
            audit.register(invalid).unwrap_err().kind(),
            io::ErrorKind::InvalidInput
        );
        assert_eq!(
            audit
                .begin(invalid, Action::Search, None)
                .unwrap_err()
                .kind(),
            io::ErrorKind::InvalidInput
        );
    }
    assert!(fs::read_dir(&audit.directory).unwrap().next().is_none());
    audit.register(EMPLOYEE).unwrap();
    let path = file_with_suffix(&audit, ".registration.json");
    let record: Value = serde_json::from_slice(&fs::read(&path).unwrap()).unwrap();
    assert_eq!(record["employee"], EMPLOYEE);
    assert_eq!(record["employee_self_reported"], true);
    assert_eq!(record["action"], "register");
    assert_eq!(record["outcome"], "succeeded");
    assert!(record["timestamp_ms"].as_u64().is_some());

    // Directory ACLs differ across platforms; an ordinary file at the managed directory
    // is an unambiguous storage obstruction without elevated rights or global state.
    let preserved = catalog.root_path().join("preserved-audit");
    fs::rename(&audit.directory, &preserved).unwrap();
    fs::write(&audit.directory, b"not a directory").unwrap();
    assert!(audit.register(EMPLOYEE).is_err());
    assert!(
        audit
            .begin(EMPLOYEE, Action::Create, Some(LIBRARY))
            .is_err()
    );
    assert!(Audit::open(catalog.root_path(), &catalog).is_err());
    fs::remove_file(&audit.directory).unwrap();
    fs::rename(&preserved, &audit.directory).unwrap();
    assert!(path.is_file());
    assert_eq!(fs::read_dir(&audit.directory).unwrap().count(), 1);
    assert!(catalog.list().unwrap().is_empty());
}

#[test]
fn terminal_result_cannot_be_replaced() {
    let disk = tempfile::tempdir().unwrap();
    let (catalog, audit) = open(disk.path());
    let operation = audit.begin(EMPLOYEE, Action::Search, None).unwrap();
    let id = operation.id;
    assert!(audit.status(Uuid::new_v4(), &catalog).unwrap().is_none());
    audit.finish(&operation, Outcome::Failed).unwrap();
    assert_eq!(
        audit
            .finish(&operation, Outcome::Succeeded)
            .unwrap_err()
            .kind(),
        io::ErrorKind::AlreadyExists
    );
    assert_eq!(status(&audit, &catalog, id).outcome, Outcome::Failed);
    drop(operation);
    drop(audit);
    drop(catalog);

    let (catalog, audit) = open(disk.path());
    assert_eq!(status(&audit, &catalog, id).outcome, Outcome::Failed);
    assert_eq!(
        file_with_suffix(&audit, ".result.json"),
        audit.path(id, RESULT)
    );
}
