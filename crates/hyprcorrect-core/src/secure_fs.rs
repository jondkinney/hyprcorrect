//! Descriptor-safe bounded file I/O for user-owned state.
//!
//! The Omarchy companion reads and changes files that live in mutable user
//! directories. Checking a pathname and reopening it later leaves symlink,
//! FIFO, and replacement races. These helpers instead walk directories with
//! no-follow semantics, pin the final parent, open each source once, and keep
//! staging descriptors open across atomic publication.

use std::fs::File;
use std::io::{self, Read, Write};
use std::path::Path;

/// Identity and metadata captured from one held regular-file descriptor.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct FileIdentity {
    dev: u64,
    ino: u64,
    size: u64,
    modified_seconds: i64,
    modified_nanoseconds: i64,
    changed_seconds: i64,
    changed_nanoseconds: i64,
}

/// One bounded file snapshot and the identity it was read from.
#[derive(Debug)]
pub struct Snapshot {
    pub bytes: Vec<u8>,
    pub identity: FileIdentity,
}

/// What object is expected at a destination when publishing a replacement.
#[derive(Debug, Clone, Copy)]
pub enum ExpectedFile {
    /// Publish against the object observed immediately before staging.
    Any,
    /// Refuse if a destination appeared after the caller observed it missing.
    Missing,
    /// Refuse unless the same inode the caller read is still present.
    Matching(FileIdentity),
}

/// Read at most `maximum` bytes from one owned, safe-mode regular file.
///
/// `Ok(None)` means the file or one of its parent directories does not exist.
pub fn read_limited(path: &Path, maximum: usize) -> io::Result<Option<Snapshot>> {
    imp::read_limited(path, maximum, false)
}

/// Read a bounded regular file owned by either root or the current user.
///
/// This variant is for mixed system/user catalogues such as XDG desktop files.
/// It retains the same no-follow, safe-mode, single-descriptor guarantees.
pub fn read_limited_trusted(path: &Path, maximum: usize) -> io::Result<Option<Snapshot>> {
    imp::read_limited(path, maximum, true)
}

/// Atomically write a user-owned regular file with a safe default mode.
///
/// The requested safe mode is applied exactly. Symbolic links, non-regular
/// targets, unexpected owners, and group/world-writable targets are refused.
pub fn atomic_write(path: &Path, bytes: &[u8], default_mode: u32) -> io::Result<()> {
    imp::atomic_write(path, bytes, default_mode, ExpectedFile::Any)
}

/// Atomically write only if the destination still matches `expected`.
pub fn atomic_write_checked(
    path: &Path,
    bytes: &[u8],
    default_mode: u32,
    expected: ExpectedFile,
) -> io::Result<()> {
    imp::atomic_write(path, bytes, default_mode, expected)
}

/// Remove one directory entry relative to a pinned, validated parent.
///
/// Removing a symlink removes the link itself and never follows its target.
pub fn remove_file(path: &Path) -> io::Result<()> {
    imp::remove_file(path)
}

/// Create if needed, then validate an owner-only directory without following
/// symbolic-link path components.
pub fn ensure_private_directory(path: &Path, create: bool) -> io::Result<()> {
    imp::ensure_private_directory(path, create)
}

/// Exclusively create one regular file through a pinned, no-follow parent.
pub fn create_new_file(path: &Path, mode: u32) -> io::Result<File> {
    imp::create_new_file(path, mode)
}

/// Atomically publish a completed private directory under a new sibling name.
pub fn publish_directory(staging: &Path, destination: &Path) -> io::Result<()> {
    imp::publish_directory(staging, destination)
}

/// Recursively remove one current-user-owned directory tree without following
/// the target or any entry encountered below it.
pub fn remove_directory_tree(path: &Path) -> io::Result<()> {
    imp::remove_directory_tree(path)
}

#[cfg(unix)]
mod imp {
    use super::{ExpectedFile, File, FileIdentity, Path, Read, Snapshot, Write, io};
    use std::ffi::{CStr, CString, OsStr};
    use std::mem::MaybeUninit;
    use std::os::fd::{AsRawFd, FromRawFd};
    use std::os::unix::ffi::OsStrExt;
    use std::os::unix::fs::MetadataExt;
    use std::path::Component;

    const SAFE_DIRECTORY_MODE: libc::mode_t = 0o700;
    const MAX_REMOVAL_ENTRIES: usize = 200_000;
    const MAX_REMOVAL_DEPTH: usize = 64;

    pub(super) fn read_limited(
        path: &Path,
        maximum: usize,
        allow_root_owner: bool,
    ) -> io::Result<Option<Snapshot>> {
        let (directory, name) = match open_parent(path, false, !allow_root_owner) {
            Ok(value) => value,
            Err(error) if error.kind() == io::ErrorKind::NotFound => return Ok(None),
            Err(error) => return Err(error),
        };
        let mut file = match open_regular_at(&directory, &name) {
            Ok(file) => file,
            Err(error) if error.kind() == io::ErrorKind::NotFound => return Ok(None),
            Err(error) => return Err(error),
        };
        let before = validate_regular(&file, allow_root_owner)?;
        if before.size > maximum as u64 {
            return Err(io::Error::new(
                io::ErrorKind::InvalidData,
                format!("{} exceeds the {maximum}-byte limit", path.display()),
            ));
        }

        let mut bytes = Vec::with_capacity((before.size as usize).min(maximum));
        (&mut file)
            .take(maximum as u64 + 1)
            .read_to_end(&mut bytes)?;
        if bytes.len() > maximum {
            return Err(io::Error::new(
                io::ErrorKind::InvalidData,
                format!("{} grew beyond the {maximum}-byte limit", path.display()),
            ));
        }
        let after = validate_regular(&file, allow_root_owner)?;
        if before != after || bytes.len() as u64 != after.size {
            return Err(io::Error::other(format!(
                "{} changed while it was being read",
                path.display()
            )));
        }
        Ok(Some(Snapshot {
            bytes,
            identity: after,
        }))
    }

    pub(super) fn atomic_write(
        path: &Path,
        bytes: &[u8],
        default_mode: u32,
        expected: ExpectedFile,
    ) -> io::Result<()> {
        if default_mode & !0o777 != 0 || default_mode & 0o022 != 0 {
            return Err(io::Error::new(
                io::ErrorKind::InvalidInput,
                "default file mode must not be group/world-writable",
            ));
        }
        let (directory, name) = open_parent(path, true, true)?;
        let existing = match open_regular_at(&directory, &name) {
            Ok(file) => Some((validate_regular(&file, false)?, file)),
            Err(error) if error.kind() == io::ErrorKind::NotFound => None,
            Err(error) => return Err(error),
        };
        match (expected, existing.as_ref().map(|(identity, _)| *identity)) {
            (ExpectedFile::Any, _) | (ExpectedFile::Missing, None) => {}
            (ExpectedFile::Matching(expected), Some(actual)) if expected == actual => {}
            _ => {
                return Err(io::Error::new(
                    io::ErrorKind::AlreadyExists,
                    format!("{} changed before it could be updated", path.display()),
                ));
            }
        }

        let mode = default_mode;

        let (mut temporary, temporary_name) = create_temporary(&directory)?;
        let mut published = false;
        let result = (|| {
            temporary.write_all(bytes)?;
            temporary.sync_all()?;
            let chmod_result = unsafe { libc::fchmod(temporary.as_raw_fd(), mode as libc::mode_t) };
            if chmod_result != 0 {
                return Err(io::Error::last_os_error());
            }
            temporary.sync_all()?;
            let temporary_identity = validate_regular(&temporary, false)?;

            let named_temporary = stat_at(&directory, &temporary_name)?;
            if !same_object(identity_from_stat(&named_temporary), temporary_identity) {
                return Err(io::Error::other(
                    "temporary file was replaced while being written",
                ));
            }
            verify_destination(
                &directory,
                &name,
                existing.as_ref().map(|(value, _)| *value),
            )?;

            let rename_result = unsafe {
                libc::renameat(
                    directory.as_raw_fd(),
                    temporary_name.as_ptr(),
                    directory.as_raw_fd(),
                    name.as_ptr(),
                )
            };
            if rename_result != 0 {
                return Err(io::Error::last_os_error());
            }
            published = true;
            let published_stat = stat_at(&directory, &name)?;
            if !same_object(identity_from_stat(&published_stat), temporary_identity) {
                return Err(io::Error::other(format!(
                    "published {} is not the staged inode",
                    path.display()
                )));
            }
            directory.sync_all()
        })();

        if !published {
            let _ = unlink_at(&directory, &temporary_name);
        }
        result
    }

    pub(super) fn remove_file(path: &Path) -> io::Result<()> {
        let (directory, name) = match open_parent(path, false, true) {
            Ok(value) => value,
            Err(error) if error.kind() == io::ErrorKind::NotFound => return Ok(()),
            Err(error) => return Err(error),
        };
        match unlink_at(&directory, &name) {
            Ok(()) => directory.sync_all(),
            Err(error) if error.kind() == io::ErrorKind::NotFound => Ok(()),
            Err(error) => Err(error),
        }
    }

    pub(super) fn ensure_private_directory(path: &Path, create: bool) -> io::Result<()> {
        let directory = open_directory_path(path, create)?;
        let metadata = directory.metadata()?;
        validate_directory_metadata(&metadata, true)?;
        if metadata.mode() & 0o077 != 0 {
            return Err(io::Error::new(
                io::ErrorKind::PermissionDenied,
                format!("{} must have owner-only permissions", path.display()),
            ));
        }
        Ok(())
    }

    pub(super) fn create_new_file(path: &Path, mode: u32) -> io::Result<File> {
        if mode & !0o777 != 0 || mode & 0o022 != 0 {
            return Err(io::Error::new(
                io::ErrorKind::InvalidInput,
                "unsafe file mode",
            ));
        }
        let (directory, name) = open_parent(path, true, true)?;
        let fd = unsafe {
            libc::openat(
                directory.as_raw_fd(),
                name.as_ptr(),
                libc::O_RDWR | libc::O_CREAT | libc::O_EXCL | libc::O_NOFOLLOW | libc::O_CLOEXEC,
                mode as libc::c_uint,
            )
        };
        if fd < 0 {
            Err(io::Error::last_os_error())
        } else {
            Ok(unsafe { File::from_raw_fd(fd) })
        }
    }

    pub(super) fn publish_directory(staging: &Path, destination: &Path) -> io::Result<()> {
        if staging.parent() != destination.parent() {
            return Err(io::Error::new(
                io::ErrorKind::InvalidInput,
                "staging and destination directories must be siblings",
            ));
        }
        let parent_path = destination.parent().ok_or_else(|| {
            io::Error::new(io::ErrorKind::InvalidInput, "destination has no parent")
        })?;
        let parent = open_directory_path(parent_path, false)?;
        validate_directory_metadata(&parent.metadata()?, true)?;
        let staging_name = c_string(staging.file_name().ok_or_else(|| {
            io::Error::new(io::ErrorKind::InvalidInput, "staging path has no name")
        })?)?;
        let destination_name = c_string(destination.file_name().ok_or_else(|| {
            io::Error::new(io::ErrorKind::InvalidInput, "destination path has no name")
        })?)?;
        let staging_directory = open_directory_at(&parent, &staging_name)?;
        let staging_metadata = staging_directory.metadata()?;
        validate_directory_metadata(&staging_metadata, true)?;
        if staging_metadata.mode() & 0o077 != 0 {
            return Err(io::Error::new(
                io::ErrorKind::PermissionDenied,
                "staging directory is not private",
            ));
        }
        let staging_identity = identity(&staging_metadata);
        match stat_at(&parent, &destination_name) {
            Ok(_) => {
                return Err(io::Error::new(
                    io::ErrorKind::AlreadyExists,
                    format!("{} already exists", destination.display()),
                ));
            }
            Err(error) if error.kind() == io::ErrorKind::NotFound => {}
            Err(error) => return Err(error),
        }
        let named_staging = stat_at(&parent, &staging_name)?;
        if !same_object(identity_from_stat(&named_staging), staging_identity) {
            return Err(io::Error::other(
                "staging directory was replaced before publication",
            ));
        }
        #[cfg(target_os = "linux")]
        let result = unsafe {
            libc::renameat2(
                parent.as_raw_fd(),
                staging_name.as_ptr(),
                parent.as_raw_fd(),
                destination_name.as_ptr(),
                libc::RENAME_NOREPLACE,
            )
        };
        #[cfg(target_os = "macos")]
        let result = unsafe {
            libc::renameatx_np(
                parent.as_raw_fd(),
                staging_name.as_ptr(),
                parent.as_raw_fd(),
                destination_name.as_ptr(),
                libc::RENAME_EXCL,
            )
        };
        #[cfg(not(any(target_os = "linux", target_os = "macos")))]
        let result = unsafe {
            libc::renameat(
                parent.as_raw_fd(),
                staging_name.as_ptr(),
                parent.as_raw_fd(),
                destination_name.as_ptr(),
            )
        };
        if result != 0 {
            return Err(io::Error::last_os_error());
        }
        let published = stat_at(&parent, &destination_name)?;
        if !same_object(identity_from_stat(&published), staging_identity) {
            return Err(io::Error::other(
                "published directory is not the completed staging directory",
            ));
        }
        parent.sync_all()
    }

    pub(super) fn remove_directory_tree(path: &Path) -> io::Result<()> {
        let (parent, name) = match open_parent(path, false, true) {
            Ok(value) => value,
            Err(error) if error.kind() == io::ErrorKind::NotFound => return Ok(()),
            Err(error) => return Err(error),
        };
        let directory = open_directory_at(&parent, &name)?;
        validate_directory_metadata(&directory.metadata()?, true)?;
        let identity = identity(&directory.metadata()?);
        let mut entries = 0usize;
        remove_directory_contents(&directory, 0, &mut entries)?;
        let named = stat_at(&parent, &name)?;
        if !same_object(identity_from_stat(&named), identity) {
            return Err(io::Error::other(
                "directory was replaced before it could be removed",
            ));
        }
        unlink_at_flags(&parent, &name, libc::AT_REMOVEDIR)?;
        parent.sync_all()
    }

    fn remove_directory_contents(
        directory: &File,
        depth: usize,
        entries: &mut usize,
    ) -> io::Result<()> {
        if depth > MAX_REMOVAL_DEPTH {
            return Err(io::Error::new(
                io::ErrorKind::InvalidData,
                "directory tree exceeded the removal depth limit",
            ));
        }
        let remaining = MAX_REMOVAL_ENTRIES.checked_sub(*entries).ok_or_else(|| {
            io::Error::new(
                io::ErrorKind::InvalidData,
                "directory tree exceeded the removal entry limit",
            )
        })?;
        let names = directory_entry_names(directory, remaining)?;
        *entries += names.len();
        for name in names {
            let stat = stat_at(directory, &name)?;
            if stat.st_uid != unsafe { libc::geteuid() } {
                return Err(io::Error::new(
                    io::ErrorKind::PermissionDenied,
                    "directory tree contains an entry owned by another user",
                ));
            }
            if stat.st_mode & libc::S_IFMT == libc::S_IFDIR {
                let child = open_directory_at(directory, &name)?;
                validate_directory_metadata(&child.metadata()?, true)?;
                let child_identity = identity(&child.metadata()?);
                remove_directory_contents(&child, depth + 1, entries)?;
                let named = stat_at(directory, &name)?;
                if !same_object(identity_from_stat(&named), child_identity) {
                    return Err(io::Error::other(
                        "child directory was replaced during removal",
                    ));
                }
                unlink_at_flags(directory, &name, libc::AT_REMOVEDIR)?;
            } else {
                unlink_at_flags(directory, &name, 0)?;
            }
        }
        directory.sync_all()
    }

    /// Enumerate through a duplicate of the already-validated directory
    /// descriptor. Unlike reopening `/dev/fd/<n>`, this works on macOS and
    /// cannot be redirected by replacing a pathname during cleanup.
    fn directory_entry_names(directory: &File, maximum: usize) -> io::Result<Vec<CString>> {
        let duplicate = unsafe { libc::fcntl(directory.as_raw_fd(), libc::F_DUPFD_CLOEXEC, 0) };
        if duplicate < 0 {
            return Err(io::Error::last_os_error());
        }
        let stream = unsafe { libc::fdopendir(duplicate) };
        if stream.is_null() {
            let error = io::Error::last_os_error();
            unsafe {
                libc::close(duplicate);
            }
            return Err(error);
        }

        let result = (|| {
            let mut names = Vec::new();
            loop {
                let entry = unsafe { libc::readdir(stream) };
                if entry.is_null() {
                    break;
                }
                let bytes = unsafe { CStr::from_ptr((*entry).d_name.as_ptr()) }.to_bytes();
                if bytes == b"." || bytes == b".." {
                    continue;
                }
                if names.len() == maximum {
                    return Err(io::Error::new(
                        io::ErrorKind::InvalidData,
                        "directory tree exceeded the removal entry limit",
                    ));
                }
                names.push(CString::new(bytes).map_err(|_| {
                    io::Error::new(io::ErrorKind::InvalidData, "directory entry contains NUL")
                })?);
            }
            Ok(names)
        })();
        let close_result = unsafe { libc::closedir(stream) };
        if close_result != 0 {
            return Err(io::Error::last_os_error());
        }
        result
    }

    fn open_parent(
        path: &Path,
        create: bool,
        require_current_owner: bool,
    ) -> io::Result<(File, CString)> {
        let parent = path.parent().ok_or_else(|| {
            io::Error::new(io::ErrorKind::InvalidInput, "path has no parent directory")
        })?;
        let name = path.file_name().ok_or_else(|| {
            io::Error::new(io::ErrorKind::InvalidInput, "path has no final component")
        })?;
        let directory = open_directory_path(parent, create)?;
        validate_directory_metadata(&directory.metadata()?, require_current_owner)?;
        Ok((directory, c_string(name)?))
    }

    fn open_directory_path(path: &Path, create: bool) -> io::Result<File> {
        let absolute = if path.is_absolute() {
            path.to_path_buf()
        } else {
            std::env::current_dir()?.join(path)
        };
        let mut directory = File::open("/")?;
        let components: Vec<_> = absolute.components().collect();
        if !matches!(components.first(), Some(Component::RootDir)) {
            return Err(io::Error::new(
                io::ErrorKind::InvalidInput,
                format!("{} is not an absolute path", absolute.display()),
            ));
        }
        for component in components.into_iter().skip(1) {
            let Component::Normal(component) = component else {
                return Err(io::Error::new(
                    io::ErrorKind::InvalidInput,
                    format!("{} contains an unsafe path component", absolute.display()),
                ));
            };
            let name = c_string(component)?;
            let next = match open_directory_at(&directory, &name) {
                Ok(file) => file,
                Err(error) if create && error.kind() == io::ErrorKind::NotFound => {
                    let result = unsafe {
                        libc::mkdirat(directory.as_raw_fd(), name.as_ptr(), SAFE_DIRECTORY_MODE)
                    };
                    if result != 0 {
                        let mkdir_error = io::Error::last_os_error();
                        if mkdir_error.kind() != io::ErrorKind::AlreadyExists {
                            return Err(mkdir_error);
                        }
                    }
                    open_directory_at(&directory, &name)?
                }
                Err(error) => return Err(error),
            };
            validate_directory_metadata(&next.metadata()?, false)?;
            directory = next;
        }
        Ok(directory)
    }

    fn open_directory_at(parent: &File, name: &CString) -> io::Result<File> {
        let fd = unsafe {
            libc::openat(
                parent.as_raw_fd(),
                name.as_ptr(),
                libc::O_RDONLY | libc::O_DIRECTORY | libc::O_NOFOLLOW | libc::O_CLOEXEC,
            )
        };
        if fd < 0 {
            Err(io::Error::last_os_error())
        } else {
            Ok(unsafe { File::from_raw_fd(fd) })
        }
    }

    fn open_regular_at(parent: &File, name: &CString) -> io::Result<File> {
        let fd = unsafe {
            libc::openat(
                parent.as_raw_fd(),
                name.as_ptr(),
                libc::O_RDONLY | libc::O_NOFOLLOW | libc::O_NONBLOCK | libc::O_CLOEXEC,
            )
        };
        if fd < 0 {
            Err(io::Error::last_os_error())
        } else {
            Ok(unsafe { File::from_raw_fd(fd) })
        }
    }

    fn create_temporary(parent: &File) -> io::Result<(File, CString)> {
        for _ in 0..64 {
            let mut random = [0u8; 16];
            getrandom::fill(&mut random)
                .map_err(|error| io::Error::other(format!("randomness unavailable: {error}")))?;
            let suffix: String = random.iter().map(|byte| format!("{byte:02x}")).collect();
            let name = CString::new(format!(".hyprcorrect.{suffix}"))
                .map_err(|_| io::Error::new(io::ErrorKind::InvalidInput, "invalid temp name"))?;
            let fd = unsafe {
                libc::openat(
                    parent.as_raw_fd(),
                    name.as_ptr(),
                    libc::O_RDWR
                        | libc::O_CREAT
                        | libc::O_EXCL
                        | libc::O_NOFOLLOW
                        | libc::O_CLOEXEC,
                    0o600 as libc::c_uint,
                )
            };
            if fd >= 0 {
                return Ok((unsafe { File::from_raw_fd(fd) }, name));
            }
            let error = io::Error::last_os_error();
            if error.kind() != io::ErrorKind::AlreadyExists {
                return Err(error);
            }
        }
        Err(io::Error::new(
            io::ErrorKind::AlreadyExists,
            "could not create a unique temporary file",
        ))
    }

    fn validate_regular(file: &File, allow_root_owner: bool) -> io::Result<FileIdentity> {
        let metadata = file.metadata()?;
        if !metadata.is_file() {
            return Err(io::Error::new(
                io::ErrorKind::InvalidData,
                "expected an owned regular file",
            ));
        }
        let uid = metadata.uid();
        if uid != unsafe { libc::geteuid() } && !(allow_root_owner && uid == 0) {
            return Err(io::Error::new(
                io::ErrorKind::PermissionDenied,
                "file is not owned by the current user",
            ));
        }
        if metadata.mode() & 0o022 != 0 {
            return Err(io::Error::new(
                io::ErrorKind::PermissionDenied,
                format!("file has unsafe mode {:04o}", metadata.mode() & 0o777),
            ));
        }
        Ok(identity(&metadata))
    }

    fn validate_directory_metadata(
        metadata: &std::fs::Metadata,
        require_current_owner: bool,
    ) -> io::Result<()> {
        if !metadata.is_dir() {
            return Err(io::Error::new(
                io::ErrorKind::InvalidData,
                "expected a directory",
            ));
        }
        let euid = unsafe { libc::geteuid() };
        if (require_current_owner && metadata.uid() != euid)
            || (!require_current_owner && metadata.uid() != euid && metadata.uid() != 0)
        {
            return Err(io::Error::new(
                io::ErrorKind::PermissionDenied,
                "directory has an unexpected owner",
            ));
        }
        let mode = metadata.mode();
        #[allow(clippy::unnecessary_cast)] // `mode_t` varies across Unix targets.
        let sticky_root = metadata.uid() == 0 && mode & libc::S_ISVTX as u32 != 0;
        if mode & 0o022 != 0 && !sticky_root {
            return Err(io::Error::new(
                io::ErrorKind::PermissionDenied,
                format!("directory has unsafe mode {:04o}", mode & 0o7777),
            ));
        }
        Ok(())
    }

    fn verify_destination(
        directory: &File,
        name: &CString,
        expected: Option<FileIdentity>,
    ) -> io::Result<()> {
        match stat_at(directory, name) {
            Ok(stat) if expected == Some(identity_from_stat(&stat)) => Ok(()),
            Err(error) if error.kind() == io::ErrorKind::NotFound && expected.is_none() => Ok(()),
            Ok(_) | Err(_) => Err(io::Error::new(
                io::ErrorKind::AlreadyExists,
                "destination changed while replacement was staged",
            )),
        }
    }

    fn stat_at(parent: &File, name: &CString) -> io::Result<libc::stat> {
        let mut value = MaybeUninit::<libc::stat>::uninit();
        let result = unsafe {
            libc::fstatat(
                parent.as_raw_fd(),
                name.as_ptr(),
                value.as_mut_ptr(),
                libc::AT_SYMLINK_NOFOLLOW,
            )
        };
        if result != 0 {
            Err(io::Error::last_os_error())
        } else {
            Ok(unsafe { value.assume_init() })
        }
    }

    fn unlink_at(parent: &File, name: &CString) -> io::Result<()> {
        unlink_at_flags(parent, name, 0)
    }

    fn unlink_at_flags(parent: &File, name: &CString, flags: libc::c_int) -> io::Result<()> {
        let result = unsafe { libc::unlinkat(parent.as_raw_fd(), name.as_ptr(), flags) };
        if result == 0 {
            Ok(())
        } else {
            Err(io::Error::last_os_error())
        }
    }

    fn c_string(value: &OsStr) -> io::Result<CString> {
        CString::new(value.as_bytes())
            .map_err(|_| io::Error::new(io::ErrorKind::InvalidInput, "path contains a NUL byte"))
    }

    fn identity(metadata: &std::fs::Metadata) -> FileIdentity {
        FileIdentity {
            dev: metadata.dev(),
            ino: metadata.ino(),
            size: metadata.size(),
            modified_seconds: metadata.mtime(),
            modified_nanoseconds: metadata.mtime_nsec(),
            changed_seconds: metadata.ctime(),
            changed_nanoseconds: metadata.ctime_nsec(),
        }
    }

    #[allow(clippy::unnecessary_cast)] // libc stat field widths vary across Unix targets.
    fn identity_from_stat(stat: &libc::stat) -> FileIdentity {
        FileIdentity {
            dev: stat.st_dev as u64,
            ino: stat.st_ino as u64,
            size: stat.st_size as u64,
            modified_seconds: stat.st_mtime as i64,
            modified_nanoseconds: stat.st_mtime_nsec as i64,
            changed_seconds: stat.st_ctime as i64,
            changed_nanoseconds: stat.st_ctime_nsec as i64,
        }
    }

    fn same_object(left: FileIdentity, right: FileIdentity) -> bool {
        left.dev == right.dev && left.ino == right.ino
    }
}

#[cfg(not(unix))]
mod imp {
    use super::{ExpectedFile, File, FileIdentity, Path, Read, Snapshot, Write, io};
    use std::fs;

    pub(super) fn read_limited(
        path: &Path,
        maximum: usize,
        _allow_root_owner: bool,
    ) -> io::Result<Option<Snapshot>> {
        let mut file = match File::open(path) {
            Ok(file) => file,
            Err(error) if error.kind() == io::ErrorKind::NotFound => return Ok(None),
            Err(error) => return Err(error),
        };
        let before = file.metadata()?;
        if !before.is_file() || before.len() > maximum as u64 {
            return Err(io::Error::new(
                io::ErrorKind::InvalidData,
                "unsafe or oversized file",
            ));
        }
        let mut bytes = Vec::new();
        (&mut file)
            .take(maximum as u64 + 1)
            .read_to_end(&mut bytes)?;
        if bytes.len() > maximum {
            return Err(io::Error::new(
                io::ErrorKind::InvalidData,
                "file exceeded limit",
            ));
        }
        let identity = FileIdentity {
            dev: 0,
            ino: 0,
            size: bytes.len() as u64,
            modified_seconds: 0,
            modified_nanoseconds: 0,
            changed_seconds: 0,
            changed_nanoseconds: 0,
        };
        Ok(Some(Snapshot { bytes, identity }))
    }

    pub(super) fn atomic_write(
        path: &Path,
        bytes: &[u8],
        _default_mode: u32,
        _expected: ExpectedFile,
    ) -> io::Result<()> {
        if let Some(parent) = path.parent() {
            fs::create_dir_all(parent)?;
        }
        let mut random = [0u8; 16];
        getrandom::fill(&mut random)
            .map_err(|error| io::Error::other(format!("randomness unavailable: {error}")))?;
        let suffix: String = random.iter().map(|byte| format!("{byte:02x}")).collect();
        let temporary = path.with_extension(format!("tmp.{suffix}"));
        let mut file = fs::OpenOptions::new()
            .write(true)
            .create_new(true)
            .open(&temporary)?;
        file.write_all(bytes)?;
        file.sync_all()?;
        fs::rename(&temporary, path)
    }

    pub(super) fn remove_file(path: &Path) -> io::Result<()> {
        match fs::remove_file(path) {
            Ok(()) => Ok(()),
            Err(error) if error.kind() == io::ErrorKind::NotFound => Ok(()),
            Err(error) => Err(error),
        }
    }

    pub(super) fn ensure_private_directory(path: &Path, create: bool) -> io::Result<()> {
        if create {
            fs::create_dir_all(path)?;
        }
        if fs::metadata(path)?.is_dir() {
            Ok(())
        } else {
            Err(io::Error::new(
                io::ErrorKind::InvalidData,
                "expected a directory",
            ))
        }
    }

    pub(super) fn create_new_file(path: &Path, _mode: u32) -> io::Result<File> {
        if let Some(parent) = path.parent() {
            fs::create_dir_all(parent)?;
        }
        fs::OpenOptions::new()
            .read(true)
            .write(true)
            .create_new(true)
            .open(path)
    }

    pub(super) fn publish_directory(staging: &Path, destination: &Path) -> io::Result<()> {
        if destination.exists() {
            return Err(io::Error::new(
                io::ErrorKind::AlreadyExists,
                "destination exists",
            ));
        }
        fs::rename(staging, destination)
    }

    pub(super) fn remove_directory_tree(path: &Path) -> io::Result<()> {
        match fs::symlink_metadata(path) {
            Ok(metadata) if metadata.is_dir() => fs::remove_dir_all(path),
            Ok(_) => Err(io::Error::new(
                io::ErrorKind::InvalidData,
                "expected a directory",
            )),
            Err(error) if error.kind() == io::ErrorKind::NotFound => Ok(()),
            Err(error) => Err(error),
        }
    }
}

#[cfg(all(test, unix))]
mod tests {
    use super::*;
    use std::fs;
    use std::os::unix::fs::{PermissionsExt, symlink};

    fn test_dir(name: &str) -> std::path::PathBuf {
        let mut random = [0u8; 8];
        getrandom::fill(&mut random).unwrap();
        let suffix: String = random.iter().map(|byte| format!("{byte:02x}")).collect();
        let root = std::env::temp_dir()
            .canonicalize()
            .unwrap_or_else(|_| std::env::temp_dir());
        let path = root.join(format!("hyprcorrect-{name}-{suffix}"));
        fs::create_dir(&path).unwrap();
        path
    }

    #[test]
    fn bounded_read_accepts_limit_and_rejects_limit_plus_one() {
        let dir = test_dir("bounded");
        let exact = dir.join("exact");
        let over = dir.join("over");
        atomic_write(&exact, b"1234", 0o600).unwrap();
        atomic_write(&over, b"12345", 0o600).unwrap();
        assert_eq!(read_limited(&exact, 4).unwrap().unwrap().bytes, b"1234");
        assert!(read_limited(&over, 4).is_err());
        assert_eq!(
            fs::metadata(&exact).unwrap().permissions().mode() & 0o777,
            0o600
        );
        fs::remove_dir_all(dir).unwrap();
    }

    #[test]
    fn read_and_write_refuse_symlinks_and_fifo_without_blocking() {
        let dir = test_dir("special");
        let target = dir.join("target");
        fs::write(&target, b"safe").unwrap();
        let link = dir.join("link");
        symlink(&target, &link).unwrap();
        assert!(read_limited(&link, 32).is_err());
        assert!(atomic_write(&link, b"unsafe", 0o600).is_err());
        assert_eq!(fs::read(&target).unwrap(), b"safe");

        let fifo = dir.join("fifo");
        let fifo_c = std::ffi::CString::new(fifo.as_os_str().as_encoded_bytes()).unwrap();
        assert_eq!(unsafe { libc::mkfifo(fifo_c.as_ptr(), 0o600) }, 0);
        assert!(read_limited(&fifo, 32).is_err());
        fs::remove_dir_all(dir).unwrap();
    }

    #[test]
    fn checked_write_rejects_a_replaced_source() {
        let dir = test_dir("replacement");
        let path = dir.join("state");
        atomic_write(&path, b"one", 0o600).unwrap();
        let snapshot = read_limited(&path, 32).unwrap().unwrap();
        atomic_write(&path, b"two", 0o600).unwrap();
        assert!(
            atomic_write_checked(
                &path,
                b"three",
                0o600,
                ExpectedFile::Matching(snapshot.identity),
            )
            .is_err()
        );
        assert_eq!(fs::read(&path).unwrap(), b"two");
        fs::remove_dir_all(dir).unwrap();
    }

    #[test]
    fn directory_publication_is_private_atomic_and_no_replace() {
        let dir = test_dir("publish");
        let staging = dir.join("staging");
        let destination = dir.join("destination");
        fs::create_dir(&staging).unwrap();
        fs::set_permissions(&staging, fs::Permissions::from_mode(0o700)).unwrap();
        atomic_write(&staging.join("ready"), b"yes", 0o600).unwrap();

        publish_directory(&staging, &destination).unwrap();
        assert!(!staging.exists());
        assert_eq!(fs::read(destination.join("ready")).unwrap(), b"yes");

        let second = dir.join("second");
        fs::create_dir(&second).unwrap();
        fs::set_permissions(&second, fs::Permissions::from_mode(0o700)).unwrap();
        assert!(publish_directory(&second, &destination).is_err());
        assert!(second.is_dir());
        fs::remove_dir_all(dir).unwrap();
    }

    #[test]
    fn recursive_removal_does_not_follow_symlinks() {
        let dir = test_dir("remove-tree");
        let tree = dir.join("tree");
        let nested = tree.join("nested");
        let outside = dir.join("outside");
        fs::create_dir(&tree).unwrap();
        fs::create_dir(&nested).unwrap();
        fs::create_dir(&outside).unwrap();
        fs::write(nested.join("data"), b"inside").unwrap();
        fs::write(outside.join("keep"), b"outside").unwrap();
        symlink(&outside, nested.join("link")).unwrap();

        remove_directory_tree(&tree).unwrap();
        assert!(!tree.exists());
        assert_eq!(fs::read(outside.join("keep")).unwrap(), b"outside");

        let direct_link = dir.join("direct-link");
        symlink(&outside, &direct_link).unwrap();
        assert!(remove_directory_tree(&direct_link).is_err());
        assert!(direct_link.is_symlink());
        fs::remove_dir_all(dir).unwrap();
    }
}
