//! Persistence: the files each role keeps and the records inside them.
//!
//! Every file is JSON, written whole and atomically. The new content goes to a temporary file in
//! the same directory, is flushed to disk, and replaces the old file by a rename, so a crash
//! leaves the old file or the new one and never a mix. Files that hold secrets are *private*:
//! mode `0600`, in directories created with mode `0700`. Files meant for other roles or for the
//! dashboard, like delegation packages and status snapshots, are *public* and get the permissions
//! any new file gets.
//!
//! The role stores ([`OwnerStore`], [`AgentStore`], [`MerchantStore`]) are only ever written
//! privately. Their JSON is built with the [`serde_hex`], [`serde_amount`], [`serde_address`] and
//! [`serde_opening`] helpers, which give keys, ids and transactions as hex, amounts as decimal
//! strings and addresses as `tf1...` strings.
//!
//! Every type that holds a secret has a `Debug` that names its type and shows nothing secret.

pub mod serde_address;
pub mod serde_amount;
pub mod serde_hex;
pub mod serde_opening;

mod agent;
#[cfg(test)]
mod fixtures;
mod merchant;
mod owner;
mod package;
mod state;
mod status;

use std::fs::{self, DirBuilder, File, OpenOptions};
use std::io::{self, Write};
use std::os::unix::fs::{DirBuilderExt, OpenOptionsExt};
use std::path::{Path, PathBuf};
use std::time::{SystemTime, UNIX_EPOCH};

use serde::Serialize;
use serde::de::DeserializeOwned;

pub use agent::{AgentAllowance, AgentStore, AgentVoucher, Import};
pub use merchant::{MerchantStore, Receipt, Spend};
pub use owner::{OwnerAllowance, OwnerStore, OwnerVoucher, Recovery, WalletOutput};
pub use package::{DelegationPackage, PackageError, PackageVoucher, allowance_id};
pub use state::{Observed, Progress, Role, VoucherState, reconcile};
pub use status::{
    AllowanceStatus, MerchantStatus, OutputStatus, OwnerStatus, RecoveryStatus, VoucherStatus,
    WalletStatus,
};

/// The format version of every store and delegation package this build reads and writes.
pub const VERSION: u32 = 1;

/// Writes `value` as pretty JSON to a private file, replacing any file already there.
///
/// Missing directories are created with mode `0700` and the file has mode `0600`. Directories
/// that already exist keep their permissions.
pub fn write_private<T>(path: &Path, value: &T) -> Result<(), StoreError>
where
    T: Serialize + ?Sized,
{
    write_json(path, value, Access::Private)
}

/// Writes `value` as pretty JSON to a public file, replacing any file already there.
///
/// Directories and the file get the permissions any new one gets.
pub fn write_public<T>(path: &Path, value: &T) -> Result<(), StoreError>
where
    T: Serialize + ?Sized,
{
    write_json(path, value, Access::Public)
}

/// Writes `line` and a newline to a public file, replacing any file already there.
///
/// This is the form of the one-line public files, such as the merchant's address.
pub fn write_public_line(path: &Path, line: &str) -> Result<(), StoreError> {
    write_atomic(path, format!("{line}\n").as_bytes(), Access::Public)
}

/// The JSON file at `path`, decoded as a `T`.
pub fn read<T>(path: &Path) -> Result<T, StoreError>
where
    T: DeserializeOwned,
{
    let bytes = fs::read(path).map_err(|source| StoreError::io(path, source))?;
    serde_json::from_slice(&bytes).map_err(|source| StoreError::json(path, source))
}

/// The text of the file at `path` without its surrounding whitespace.
pub fn read_line(path: &Path) -> Result<String, StoreError> {
    let text = fs::read_to_string(path).map_err(|source| StoreError::io(path, source))?;
    Ok(text.trim().to_owned())
}

/// The current time in whole seconds since the Unix epoch, the time format of every file.
pub fn unix_now() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map_or(0, |elapsed| elapsed.as_secs())
}

/// Why a file could not be read or written.
#[derive(Debug, thiserror::Error)]
pub enum StoreError {
    /// The file system refused.
    #[error("{}: {source}", .path.display())]
    Io {
        /// The file or directory involved.
        path: PathBuf,
        /// What happened.
        source: io::Error,
    },

    /// The file is not the JSON it should be, or the value cannot be written as JSON.
    #[error("{}: {source}", .path.display())]
    Json {
        /// The file involved.
        path: PathBuf,
        /// What is wrong with the JSON.
        source: serde_json::Error,
    },

    /// The file is of a version this build does not read.
    #[error(
        "{}: format version {found}, but this build reads version {expected}",
        .path.display()
    )]
    Version {
        /// The file involved.
        path: PathBuf,
        /// The version the file declares.
        found: u32,
        /// The version this build reads.
        expected: u32,
    },
}

impl StoreError {
    /// Whether the file does not exist.
    pub fn is_not_found(&self) -> bool {
        matches!(self, StoreError::Io { source, .. } if source.kind() == io::ErrorKind::NotFound)
    }

    pub(crate) fn io(path: &Path, source: io::Error) -> StoreError {
        StoreError::Io {
            path: path.to_path_buf(),
            source,
        }
    }

    pub(crate) fn json(path: &Path, source: serde_json::Error) -> StoreError {
        StoreError::Json {
            path: path.to_path_buf(),
            source,
        }
    }
}

/// Appends `line`, which must be one complete JSON object, to the file at `path`, creating the
/// file and its directories when needed.
///
/// The line is written in one call to a file opened for appending, so entries from several
/// processes do not interleave.
pub(crate) fn append_line(path: &Path, line: &[u8]) -> Result<(), StoreError> {
    let directory = directory_of(path);
    fs::create_dir_all(directory).map_err(|source| StoreError::io(directory, source))?;
    let mut file = OpenOptions::new()
        .create(true)
        .append(true)
        .open(path)
        .map_err(|source| StoreError::io(path, source))?;
    let mut entry = line.to_vec();
    entry.push(b'\n');
    file.write_all(&entry)
        .and_then(|()| file.sync_data())
        .map_err(|source| StoreError::io(path, source))
}

/// Checks that a file of `found` format version is one this build reads.
pub(crate) fn check_version(path: &Path, found: u32) -> Result<(), StoreError> {
    if found != VERSION {
        return Err(StoreError::Version {
            path: path.to_path_buf(),
            found,
            expected: VERSION,
        });
    }
    Ok(())
}

/// Who may read a file once it is written.
#[derive(Clone, Copy, PartialEq, Eq)]
enum Access {
    /// Its owner only.
    Private,
    /// Whoever the umask allows.
    Public,
}

fn write_json<T>(path: &Path, value: &T, access: Access) -> Result<(), StoreError>
where
    T: Serialize + ?Sized,
{
    let mut bytes =
        serde_json::to_vec_pretty(value).map_err(|source| StoreError::json(path, source))?;
    bytes.push(b'\n');
    write_atomic(path, &bytes, access)
}

/// Replaces the file at `path` with `bytes`, so that a reader sees the old file or the new one.
fn write_atomic(path: &Path, bytes: &[u8], access: Access) -> Result<(), StoreError> {
    let directory = directory_of(path);
    let temp = temp_path(path).map_err(|source| StoreError::io(path, source))?;

    create_directories(directory, access).map_err(|source| StoreError::io(directory, source))?;
    let replaced = write_synced(&temp, bytes, access).and_then(|()| fs::rename(&temp, path));
    match replaced {
        // The rename is only durable once the directory that holds it is flushed.
        Ok(()) => File::open(directory)
            .and_then(|directory| directory.sync_all())
            .map_err(|source| StoreError::io(directory, source)),
        Err(source) => {
            // The temporary file is useless now; failing to remove it changes nothing.
            let _ = fs::remove_file(&temp);
            Err(StoreError::io(path, source))
        }
    }
}

/// The directory a file at `path` lives in.
fn directory_of(path: &Path) -> &Path {
    match path.parent() {
        Some(parent) if !parent.as_os_str().is_empty() => parent,
        _ => Path::new("."),
    }
}

/// A fresh name next to `path` for the file that is written before it replaces `path`.
fn temp_path(path: &Path) -> io::Result<PathBuf> {
    let Some(name) = path.file_name() else {
        return Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            "the path has no file name",
        ));
    };
    let mut temp = name.to_os_string();
    temp.push(format!(".{:016x}.tmp", rand::random::<u64>()));
    Ok(directory_of(path).join(format!(".{}", temp.to_string_lossy())))
}

/// Creates `directory` and its missing parents.
fn create_directories(directory: &Path, access: Access) -> io::Result<()> {
    let mut builder = DirBuilder::new();
    builder.recursive(true);
    if access == Access::Private {
        builder.mode(0o700);
    }
    builder.create(directory)
}

/// Creates a new file at `path` holding `bytes`, and flushes it to disk.
fn write_synced(path: &Path, bytes: &[u8], access: Access) -> io::Result<()> {
    let mut options = OpenOptions::new();
    options.write(true).create_new(true);
    if access == Access::Private {
        options.mode(0o600);
    }
    let mut file = options.open(path)?;
    file.write_all(bytes)?;
    file.sync_all()
}

#[cfg(test)]
mod tests {
    use std::os::unix::fs::PermissionsExt;

    use serde::Deserialize;
    use tempfile::TempDir;

    use super::*;

    #[derive(Debug, PartialEq, Serialize, Deserialize)]
    struct Sample {
        name: String,
        count: u32,
    }

    fn sample() -> Sample {
        Sample {
            name: "voucher".to_owned(),
            count: 4,
        }
    }

    fn mode(path: &Path) -> u32 {
        fs::metadata(path).expect("metadata").permissions().mode() & 0o777
    }

    /// The mode a new file or directory gets under this process's umask.
    fn default_modes(dir: &Path) -> (u32, u32) {
        let file = dir.join("default-file");
        File::create(&file).expect("create a file");
        let directory = dir.join("default-dir");
        fs::create_dir(&directory).expect("create a directory");
        (mode(&file), mode(&directory))
    }

    /// Everything in `dir`, to show that nothing is left behind.
    fn names(dir: &Path) -> Vec<String> {
        let mut names: Vec<String> = fs::read_dir(dir)
            .expect("read the directory")
            .map(|entry| {
                entry
                    .expect("an entry")
                    .file_name()
                    .to_string_lossy()
                    .into_owned()
            })
            .collect();
        names.sort();
        names
    }

    #[test]
    fn a_private_file_is_readable_by_its_owner_alone_in_a_private_directory() {
        let dir = TempDir::new().expect("a temporary directory");
        let path = dir.path().join("data/owner/owner.json");
        write_private(&path, &sample()).expect("write");

        assert_eq!(mode(&path), 0o600);
        assert_eq!(mode(path.parent().expect("a parent")), 0o700);
        assert_eq!(mode(&dir.path().join("data")), 0o700);
        let back: Sample = read(&path).expect("read");
        assert_eq!(back, sample());
    }

    #[test]
    fn a_directory_that_already_exists_keeps_its_permissions() {
        let dir = TempDir::new().expect("a temporary directory");
        let shared = dir.path().join("shared");
        fs::create_dir(&shared).expect("create");
        fs::set_permissions(&shared, fs::Permissions::from_mode(0o755)).expect("chmod");
        write_private(&shared.join("secret.json"), &sample()).expect("write");
        assert_eq!(mode(&shared), 0o755);
        assert_eq!(mode(&shared.join("secret.json")), 0o600);
    }

    #[test]
    fn a_public_file_gets_the_permissions_any_new_file_gets() {
        let dir = TempDir::new().expect("a temporary directory");
        let (file_mode, directory_mode) = default_modes(dir.path());
        let path = dir.path().join("public/owner-status.json");
        write_public(&path, &sample()).expect("write");
        assert_eq!(mode(&path), file_mode);
        assert_eq!(mode(path.parent().expect("a parent")), directory_mode);

        let line = dir.path().join("public/merchant-address");
        write_public_line(&line, "tf1example").expect("write");
        assert_eq!(mode(&line), file_mode);
        assert_eq!(fs::read_to_string(&line).expect("read"), "tf1example\n");
        assert_eq!(read_line(&line).expect("read"), "tf1example");
    }

    #[test]
    fn writing_replaces_the_file_whole_and_leaves_nothing_behind() {
        let dir = TempDir::new().expect("a temporary directory");
        let path = dir.path().join("store.json");
        write_private(&path, &sample()).expect("write");
        let second = Sample {
            name: "a much longer name than the first one had".to_owned(),
            count: 99,
        };
        write_private(&path, &second).expect("rewrite");
        assert_eq!(read::<Sample>(&path).expect("read"), second);
        assert_eq!(mode(&path), 0o600);
        assert_eq!(names(dir.path()), ["store.json"]);

        // Shorter content replaces longer content without a remainder.
        write_private(&path, &sample()).expect("rewrite again");
        assert_eq!(read::<Sample>(&path).expect("read"), sample());
    }

    #[test]
    fn a_file_is_pretty_json_with_a_final_newline() {
        let dir = TempDir::new().expect("a temporary directory");
        let path = dir.path().join("store.json");
        write_private(&path, &sample()).expect("write");
        let text = fs::read_to_string(&path).expect("read");
        assert_eq!(text, "{\n  \"name\": \"voucher\",\n  \"count\": 4\n}\n");
    }

    #[test]
    fn a_value_that_cannot_be_written_leaves_the_old_file_alone() {
        /// Fails to serialize, as a map with a non-string key does.
        struct Unwritable;
        impl Serialize for Unwritable {
            fn serialize<S>(&self, _: S) -> Result<S::Ok, S::Error>
            where
                S: serde::Serializer,
            {
                Err(serde::ser::Error::custom("refused"))
            }
        }

        let dir = TempDir::new().expect("a temporary directory");
        let path = dir.path().join("store.json");
        write_private(&path, &sample()).expect("write");
        let error = write_private(&path, &Unwritable).expect_err("not writable");
        assert!(matches!(error, StoreError::Json { .. }), "{error}");
        assert_eq!(read::<Sample>(&path).expect("read"), sample());
        assert_eq!(names(dir.path()), ["store.json"]);
    }

    #[test]
    fn a_missing_file_is_reported_as_not_found() {
        let dir = TempDir::new().expect("a temporary directory");
        let error = read::<Sample>(&dir.path().join("absent.json")).expect_err("absent");
        assert!(error.is_not_found(), "{error}");
        assert!(error.to_string().contains("absent.json"), "{error}");
    }

    #[test]
    fn a_file_that_is_not_the_expected_json_names_the_file() {
        let dir = TempDir::new().expect("a temporary directory");
        let path = dir.path().join("broken.json");
        fs::write(&path, "{\"name\": 5}").expect("write");
        let error = read::<Sample>(&path).expect_err("wrong shape");
        assert!(matches!(error, StoreError::Json { .. }), "{error}");
        assert!(!error.is_not_found());
        assert!(error.to_string().contains("broken.json"), "{error}");
    }

    #[test]
    fn a_relative_path_with_no_directory_writes_beside_the_working_directory() {
        // Resolving the directory of a bare file name must not try to create or chmod "".
        assert_eq!(directory_of(Path::new("owner.json")), Path::new("."));
        assert_eq!(directory_of(Path::new("a/b.json")), Path::new("a"));
        assert!(temp_path(Path::new("..")).is_err());
        let temp = temp_path(Path::new("a/owner.json")).expect("a temporary name");
        assert_eq!(temp.parent(), Some(Path::new("a")));
        let name = temp.file_name().expect("a name").to_string_lossy();
        assert!(
            name.starts_with(".owner.json.") && name.ends_with(".tmp"),
            "{name}"
        );
    }

    #[test]
    fn append_adds_whole_lines_and_creates_the_directories() {
        let dir = TempDir::new().expect("a temporary directory");
        let path = dir.path().join("public/log.jsonl");
        append_line(&path, b"{\"a\":1}").expect("append");
        append_line(&path, b"{\"a\":2}").expect("append");
        assert_eq!(
            fs::read_to_string(&path).expect("read"),
            "{\"a\":1}\n{\"a\":2}\n"
        );
    }

    #[test]
    fn a_file_of_another_version_is_refused() {
        let path = Path::new("owner.json");
        assert!(check_version(path, VERSION).is_ok());
        let error = check_version(path, VERSION + 1).expect_err("another version");
        assert!(matches!(error, StoreError::Version { found, .. } if found == VERSION + 1));
        assert!(error.to_string().contains("owner.json"), "{error}");
    }

    #[test]
    fn unix_now_is_after_the_year_2020() {
        assert!(unix_now() > 1_577_836_800);
    }
}
