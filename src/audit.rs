use std::collections::HashSet;
use std::fs::{self, File, Metadata};
use std::io::{self, BufReader};
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex, MutexGuard};
use std::time::{SystemTime, UNIX_EPOCH};

use serde::{Deserialize, Serialize, de::DeserializeOwned};
use uuid::Uuid;

use crate::{
    catalog::{Catalog, Receipt},
    durable,
};

const START: &str = "start.json";
const RESULT: &str = "result.json";
const REGISTRATION: &str = "registration.json";

type Active = Mutex<HashSet<Uuid>>;

pub(crate) struct Audit {
    directory: PathBuf,
    active: Arc<Active>,
}

#[derive(Debug)]
pub(crate) struct Operation {
    pub(crate) id: Uuid,
    active: Arc<Active>,
}

impl Drop for Operation {
    fn drop(&mut self) {
        lock_active(&self.active).remove(&self.id);
    }
}

#[derive(Clone, Copy, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(rename_all = "snake_case")]
pub(crate) enum Action {
    Create,
    Import,
    Delete,
    Search,
}

#[derive(Clone, Copy, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(rename_all = "snake_case")]
pub(crate) enum Outcome {
    InProgress,
    Succeeded,
    Failed,
    Interrupted,
    Unknown,
}

impl Outcome {
    fn terminal(self) -> bool {
        matches!(self, Self::Succeeded | Self::Failed | Self::Interrupted)
    }
}

#[derive(Debug, Serialize)]
pub(crate) struct OperationStatus {
    pub(crate) id: Uuid,
    pub(crate) employee: String,
    pub(crate) employee_self_reported: bool,
    pub(crate) action: Action,
    pub(crate) library: Option<String>,
    pub(crate) started_at_ms: u64,
    pub(crate) outcome: Outcome,
}

#[derive(Serialize)]
struct Registration<'a> {
    id: Uuid,
    employee: &'a str,
    employee_self_reported: bool,
    action: &'static str,
    outcome: Outcome,
    timestamp_ms: u64,
}

#[derive(Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
struct Start<S> {
    id: Uuid,
    employee: S,
    employee_self_reported: bool,
    action: Action,
    library: Option<S>,
    started_at_ms: u64,
}

#[derive(Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
struct ResultRecord {
    id: Uuid,
    outcome: Outcome,
    finished_at_ms: u64,
}

#[derive(Eq, PartialEq)]
enum CommitEvidence {
    Committed,
    NoCommit,
    Unknown,
}

impl Audit {
    /// Open only under the approved, exclusively locked service root.
    pub(crate) fn open(root: &Path, catalog: &Catalog) -> io::Result<Self> {
        let directory = root.join("audit");
        match fs::create_dir(&directory) {
            Ok(()) => {}
            Err(error) if error.kind() == io::ErrorKind::AlreadyExists => {}
            Err(error) => return Err(error),
        }
        check_directory(&directory)?;
        #[cfg(target_os = "linux")]
        {
            // Persist the audit directory entry and any visible record left by a prior
            // process whose rename succeeded but directory sync did not.
            File::open(root)?.sync_all()?;
            File::open(&directory)?.sync_all()?;
        }
        let audit = Self {
            directory,
            active: Arc::default(),
        };
        for entry in fs::read_dir(&audit.directory)? {
            let entry = entry?;
            let name = entry.file_name();
            let Some(id) = name.to_str().and_then(start_id) else {
                continue;
            };
            if audit.recover(id, catalog).is_err() {
                warn_storage("could not reconcile an operation");
            }
        }
        Ok(audit)
    }

    pub(crate) fn register(&self, employee: &str) -> io::Result<()> {
        check_employee(employee)?;
        let _guard = lock_active(&self.active);
        let id = Uuid::new_v4();
        self.write_new(
            id,
            REGISTRATION,
            &Registration {
                id,
                employee,
                employee_self_reported: true,
                action: "register",
                outcome: Outcome::Succeeded,
                timestamp_ms: now_ms(),
            },
        )
    }

    pub(crate) fn begin(
        &self,
        employee: &str,
        action: Action,
        library: Option<&str>,
    ) -> io::Result<Operation> {
        check_employee(employee)?;
        let mut active = lock_active(&self.active);
        let id = Uuid::new_v4();
        self.write_new(
            id,
            START,
            &Start {
                id,
                employee,
                employee_self_reported: true,
                action,
                library,
                started_at_ms: now_ms(),
            },
        )?;
        active.insert(id);
        Ok(Operation {
            id,
            active: Arc::clone(&self.active),
        })
    }

    /// Only an Ok result permits the caller to clear a catalog receipt.
    pub(crate) fn finish(&self, operation: &Operation, outcome: Outcome) -> io::Result<()> {
        if !Arc::ptr_eq(&self.active, &operation.active) {
            return Err(io::Error::new(
                io::ErrorKind::InvalidInput,
                "invalid audit result",
            ));
        }
        let mut active = lock_active(&self.active);
        if !active.remove(&operation.id) {
            return Err(io::Error::new(
                io::ErrorKind::AlreadyExists,
                "audit operation is no longer active",
            ));
        }
        if !outcome.terminal() {
            return Err(io::Error::new(
                io::ErrorKind::InvalidInput,
                "invalid audit result",
            ));
        }
        self.write_result(operation.id, outcome)
    }

    /// Stop reporting an uncertain commit as active without overwriting durable evidence.
    pub(crate) fn unresolved(&self, operation: &Operation) {
        if Arc::ptr_eq(&self.active, &operation.active) {
            lock_active(&self.active).remove(&operation.id);
        }
    }

    pub(crate) fn status(
        &self,
        id: Uuid,
        catalog: &Catalog,
    ) -> io::Result<Option<OperationStatus>> {
        // Serialize the evidence read with finish: callers may clear the receipt immediately
        // after finish returns, so a poll must not miss both the receipt and result.
        let active = lock_active(&self.active);
        let Some(start) = self.read_start(id)? else {
            return Ok(None);
        };
        let commit = commit_evidence(catalog, id, start.action);
        let outcome = if commit == CommitEvidence::Committed {
            Outcome::Succeeded
        } else if active.contains(&id) {
            Outcome::InProgress
        } else {
            match self.read_result(id) {
                Ok(Some(result)) => result.outcome,
                Ok(None) if commit == CommitEvidence::NoCommit => Outcome::Interrupted,
                Ok(None) => Outcome::Unknown,
                Err(_) => {
                    warn_storage("could not read an operation result");
                    Outcome::Unknown
                }
            }
        };
        Ok(Some(OperationStatus {
            id,
            employee: start.employee,
            employee_self_reported: start.employee_self_reported,
            action: start.action,
            library: start.library,
            started_at_ms: start.started_at_ms,
            outcome,
        }))
    }

    fn recover(&self, id: Uuid, catalog: &Catalog) -> io::Result<()> {
        let Some(start) = self.read_start(id)? else {
            return Ok(());
        };
        let commit = commit_evidence(catalog, id, start.action);
        let outcome = match self.read_result(id)? {
            Some(result) => result.outcome,
            None => {
                let outcome = match commit {
                    CommitEvidence::Committed => Outcome::Succeeded,
                    CommitEvidence::NoCommit => Outcome::Interrupted,
                    // Never persist an uncertain inference in a terminal record.
                    CommitEvidence::Unknown => return Ok(()),
                };
                self.write_result(id, outcome)?;
                outcome
            }
        };
        if commit == CommitEvidence::Committed {
            if outcome != Outcome::Succeeded {
                return Err(invalid_record());
            }
            if catalog.clear_receipt(id).is_err() {
                warn_storage("could not clear a reconciled catalog receipt");
            }
        }
        Ok(())
    }

    fn read_start(&self, id: Uuid) -> io::Result<Option<Start<String>>> {
        let record: Option<Start<String>> = self.read(id, START)?;
        if record.as_ref().is_some_and(|start| {
            start.id != id || !start.employee_self_reported || !valid_employee(&start.employee)
        }) {
            return Err(invalid_record());
        }
        Ok(record)
    }

    fn read_result(&self, id: Uuid) -> io::Result<Option<ResultRecord>> {
        let record: Option<ResultRecord> = self.read(id, RESULT)?;
        if record
            .as_ref()
            .is_some_and(|result| result.id != id || !result.outcome.terminal())
        {
            return Err(invalid_record());
        }
        Ok(record)
    }

    fn write_result(&self, id: Uuid, outcome: Outcome) -> io::Result<()> {
        self.write_new(
            id,
            RESULT,
            &ResultRecord {
                id,
                outcome,
                finished_at_ms: now_ms(),
            },
        )
    }

    fn path(&self, id: Uuid, suffix: &str) -> PathBuf {
        self.directory.join(format!("{id}.{suffix}"))
    }

    fn read<T: DeserializeOwned>(&self, id: Uuid, suffix: &str) -> io::Result<Option<T>> {
        check_directory(&self.directory)?;
        let path = self.path(id, suffix);
        match fs::symlink_metadata(&path) {
            Ok(metadata) if metadata.is_file() && !is_link(&metadata) => {}
            Err(error) if error.kind() == io::ErrorKind::NotFound => return Ok(None),
            Err(error) => return Err(error),
            Ok(_) => return Err(invalid_record()),
        }
        let file = File::open(path)?;
        serde_json::from_reader(BufReader::new(file))
            .map(Some)
            .map_err(|error| {
                error.io_error_kind().map_or_else(invalid_record, |kind| {
                    io::Error::new(kind, "could not read audit record")
                })
            })
    }

    // The exclusive service root and audit mutex own this namespace; never replace an
    // existing record, including a visible result after an uncertain write.
    fn write_new(&self, id: Uuid, suffix: &str, record: &impl Serialize) -> io::Result<()> {
        check_directory(&self.directory)?;
        let path = self.path(id, suffix);
        match fs::symlink_metadata(&path) {
            Err(error) if error.kind() == io::ErrorKind::NotFound => {}
            Err(error) => return Err(error),
            Ok(_) => {
                return Err(io::Error::new(
                    io::ErrorKind::AlreadyExists,
                    "audit record path already exists",
                ));
            }
        }
        let bytes = serde_json::to_vec(record).map_err(|_| invalid_record())?;
        durable::replace(&path, &bytes).map_err(|error| {
            io::Error::new(
                io::Error::from(error).kind(),
                "could not persist audit record",
            )
        })
    }
}

pub(crate) fn valid_employee(employee: &str) -> bool {
    employee.len() == 8 && employee.bytes().all(|byte| byte.is_ascii_digit())
}

fn check_employee(employee: &str) -> io::Result<()> {
    if valid_employee(employee) {
        Ok(())
    } else {
        Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            "employee must contain exactly eight ASCII digits",
        ))
    }
}

fn start_id(name: &str) -> Option<Uuid> {
    let uuid = name.strip_suffix(".start.json")?;
    let id = Uuid::parse_str(uuid).ok()?;
    (id.hyphenated().encode_lower(&mut Uuid::encode_buffer()) == uuid).then_some(id)
}

fn commit_evidence(catalog: &Catalog, id: Uuid, action: Action) -> CommitEvidence {
    if action == Action::Search {
        // Browser search only releases evidence after its result record is durable.
        return CommitEvidence::NoCommit;
    }
    match (action, catalog.receipt(id)) {
        (Action::Create, Ok(Some(Receipt::Created { .. })))
        | (Action::Delete, Ok(Some(Receipt::Deleted { .. })))
        | (Action::Import, Ok(Some(Receipt::Published { .. }))) => CommitEvidence::Committed,
        (_, Ok(None)) => CommitEvidence::NoCommit,
        _ => CommitEvidence::Unknown,
    }
}

fn check_directory(path: &Path) -> io::Result<()> {
    let metadata = fs::symlink_metadata(path)?;
    if !metadata.is_dir() || is_link(&metadata) {
        return Err(io::Error::new(
            io::ErrorKind::InvalidData,
            "expected a service-managed audit directory",
        ));
    }
    Ok(())
}

fn is_link(metadata: &Metadata) -> bool {
    #[cfg(windows)]
    {
        use std::os::windows::fs::MetadataExt;
        use windows_sys::Win32::Storage::FileSystem::FILE_ATTRIBUTE_REPARSE_POINT;

        metadata.file_attributes() & FILE_ATTRIBUTE_REPARSE_POINT != 0
    }
    #[cfg(not(windows))]
    metadata.file_type().is_symlink()
}

fn lock_active(active: &Active) -> MutexGuard<'_, HashSet<Uuid>> {
    active
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner)
}

fn invalid_record() -> io::Error {
    io::Error::new(io::ErrorKind::InvalidData, "invalid audit record")
}

fn warn_storage(reason: &str) {
    eprintln!("audit storage warning: {reason}");
}

fn now_ms() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_millis() as u64
}

#[cfg(test)]
mod tests;
