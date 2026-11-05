//! Open a regular file under a canonical allowed root without additional hard links.
//! The caller's path policy runs on the open file's actual path, before metadata checks.
//! This module grants no read permission and never logs file contents.

use std::fs::{File, Metadata, OpenOptions};
use std::io;
use std::os::unix::fs::{MetadataExt, OpenOptionsExt};
use std::path::{Path, PathBuf};

#[derive(Debug)]
pub(crate) enum Error<E> {
    Open(io::Error),
    PathUnavailable,
    OutsideRoot,
    Policy(E),
    Metadata(io::Error),
    NotFile,
    MultipleLinks,
}

/// `root` must be canonical. Checks use the file descriptor, so a parent directory
/// changed into a symlink before opening cannot move the read outside that root.
/// Callers retain their own path policy, error mapping and subsequent read semantics.
pub(crate) fn open<E>(
    path: &Path,
    root: &Path,
    policy: impl FnOnce(&Path) -> Result<(), E>,
) -> Result<(File, Metadata), Error<E>> {
    let file = OpenOptions::new()
        .read(true)
        .custom_flags(libc::O_NOFOLLOW | libc::O_NONBLOCK)
        .open(path)
        .map_err(Error::Open)?;
    let actual = open_path(&file).ok_or(Error::PathUnavailable)?;
    if !actual.starts_with(root) {
        return Err(Error::OutsideRoot);
    }
    policy(&actual).map_err(Error::Policy)?;
    let meta = file.metadata().map_err(Error::Metadata)?;
    if !meta.is_file() {
        return Err(Error::NotFile);
    }
    if meta.nlink() > 1 {
        return Err(Error::MultipleLinks);
    }
    Ok((file, meta))
}

/// Where the open file really lives; unsupported platforms refuse the read.
#[cfg(target_os = "macos")]
fn open_path(file: &File) -> Option<PathBuf> {
    use std::ffi::OsStr;
    use std::os::unix::ffi::OsStrExt;
    use std::os::unix::io::AsRawFd;
    let mut buf = vec![0u8; libc::PATH_MAX as usize];
    // SAFETY: F_GETPATH writes at most PATH_MAX bytes into `buf`, which has that length.
    if unsafe { libc::fcntl(file.as_raw_fd(), libc::F_GETPATH, buf.as_mut_ptr()) } == -1 {
        return None;
    }
    let len = buf.iter().position(|byte| *byte == 0)?;
    Some(PathBuf::from(OsStr::from_bytes(&buf[..len])))
}

#[cfg(target_os = "linux")]
fn open_path(file: &File) -> Option<PathBuf> {
    use std::os::unix::io::AsRawFd;
    std::fs::read_link(format!("/proc/self/fd/{}", file.as_raw_fd())).ok()
}

#[cfg(not(any(target_os = "macos", target_os = "linux")))]
fn open_path(_file: &File) -> Option<PathBuf> {
    None
}

#[cfg(test)]
mod tests {
    use std::io::Read;
    use std::path::PathBuf;

    use super::{Error, open};

    fn root() -> (tempfile::TempDir, PathBuf) {
        let dir = tempfile::tempdir().unwrap();
        let real = dir.path().canonicalize().unwrap();
        (dir, real)
    }

    #[test]
    fn reads_a_regular_file_through_the_verified_handle() {
        let (_dir, root) = root();
        let path = root.join("answer.txt");
        std::fs::write(&path, "café — Київ").unwrap();
        let (mut file, meta) = open(&path, &root, |_| Ok::<_, ()>(())).unwrap();
        let mut text = String::new();
        file.read_to_string(&mut text).unwrap();
        assert_eq!(text, "café — Київ");
        assert_eq!(meta.len(), text.len() as u64);
    }

    #[test]
    fn final_symlinks_are_not_followed_even_inside_the_root() {
        let (_dir, root) = root();
        std::fs::write(root.join("actual"), "answer").unwrap();
        std::os::unix::fs::symlink(root.join("actual"), root.join("link")).unwrap();
        assert!(matches!(
            open(&root.join("link"), &root, |_| Ok::<_, ()>(())),
            Err(Error::Open(_))
        ));
    }

    #[test]
    fn a_symlinked_parent_cannot_escape_the_allowed_root() {
        let (_dir, root) = root();
        let outside = tempfile::tempdir().unwrap();
        std::fs::write(outside.path().join("answer"), "outside").unwrap();
        std::os::unix::fs::symlink(outside.path(), root.join("parent")).unwrap();
        let mut policy_called = false;
        let result = open(&root.join("parent/answer"), &root, |_| {
            policy_called = true;
            Ok::<_, ()>(())
        });
        assert!(matches!(result, Err(Error::OutsideRoot)));
        assert!(!policy_called);
    }

    #[test]
    fn caller_policy_sees_the_open_files_actual_path() {
        let (_dir, root) = root();
        std::fs::create_dir(root.join("actual")).unwrap();
        std::fs::write(root.join("actual/answer"), "answer").unwrap();
        std::os::unix::fs::symlink(root.join("actual"), root.join("alias")).unwrap();
        let result = open(&root.join("alias/answer"), &root, |actual| {
            assert_eq!(actual, root.join("actual/answer"));
            Err("caller refused")
        });
        assert!(matches!(result, Err(Error::Policy("caller refused"))));
    }

    #[test]
    fn caller_policy_precedes_file_type_and_hard_link_errors() {
        let (_dir, root) = root();
        assert!(matches!(
            open(&root, &root, |_| Err("denied")),
            Err(Error::Policy("denied"))
        ));
        let path = root.join("answer");
        std::fs::write(&path, "answer").unwrap();
        std::fs::hard_link(&path, root.join("other-name")).unwrap();
        assert!(matches!(
            open(&path, &root, |_| Err("denied")),
            Err(Error::Policy("denied"))
        ));
    }

    #[test]
    fn directories_and_fifos_are_refused_without_waiting_for_a_writer() {
        use std::ffi::CString;
        use std::os::unix::ffi::OsStrExt;
        let (_dir, root) = root();
        assert!(matches!(
            open(&root, &root, |_| Ok::<_, ()>(())),
            Err(Error::NotFile)
        ));
        let path = root.join("fifo");
        let name = CString::new(path.as_os_str().as_bytes()).unwrap();
        // SAFETY: name is a valid NUL-terminated path retained across the call.
        assert_eq!(unsafe { libc::mkfifo(name.as_ptr(), 0o600) }, 0);
        assert!(matches!(
            open(&path, &root, |_| Ok::<_, ()>(())),
            Err(Error::NotFile)
        ));
    }

    #[test]
    fn additional_hard_links_are_refused() {
        let (_dir, root) = root();
        let outside = tempfile::tempdir().unwrap();
        std::fs::write(outside.path().join("actual"), "outside").unwrap();
        let path = root.join("answer");
        std::fs::hard_link(outside.path().join("actual"), &path).unwrap();
        assert!(matches!(
            open(&path, &root, |_| Ok::<_, ()>(())),
            Err(Error::MultipleLinks)
        ));
    }

    #[test]
    fn removing_the_path_after_verification_keeps_the_already_open_handle() {
        use std::os::unix::fs::MetadataExt;
        let (_dir, root) = root();
        let path = root.join("answer");
        std::fs::write(&path, "answer").unwrap();
        let (mut file, meta) = open(&path, &root, |_| {
            std::fs::remove_file(&path).unwrap();
            Ok::<_, ()>(())
        })
        .unwrap();
        let mut text = String::new();
        file.read_to_string(&mut text).unwrap();
        assert_eq!(text, "answer");
        assert_eq!(
            meta.nlink(),
            0,
            "no extra hard links; callers may require the path to stay linked"
        );
    }
}
