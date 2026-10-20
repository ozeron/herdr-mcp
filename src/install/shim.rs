use std::fs;
use std::io;
use std::os::unix::fs::symlink;
use std::path::{Path, PathBuf};

use super::{InstallEnv, InstallError};

pub const SHIM_NAME: &str = "herdr-mcp";

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ShimState {
    Missing,
    Current,
    Moved,
    Dangling,
    Foreign,
}

fn resolve(link: &Path, target: PathBuf) -> PathBuf {
    if target.is_absolute() {
        target
    } else {
        link.parent()
            .map_or(target.clone(), |dir| dir.join(&target))
    }
}

pub fn inspect(env: &InstallEnv) -> ShimState {
    let path = env.shim_path();
    let meta = match fs::symlink_metadata(&path) {
        Ok(meta) => meta,
        Err(err) if err.kind() == io::ErrorKind::NotFound => return ShimState::Missing,
        Err(_) => return ShimState::Foreign,
    };
    if !meta.is_symlink() {
        return ShimState::Foreign;
    }
    let Ok(target) = fs::read_link(&path).map(|target| resolve(&path, target)) else {
        return ShimState::Foreign;
    };
    let Ok(real) = fs::canonicalize(&target) else {
        return ShimState::Dangling;
    };
    if fs::canonicalize(&env.exe).is_ok_and(|exe| exe == real) {
        ShimState::Current
    } else if target.file_name().is_some_and(|name| name == SHIM_NAME) {
        ShimState::Moved
    } else {
        ShimState::Foreign
    }
}

pub fn link(env: &InstallEnv) -> Result<(), InstallError> {
    let path = env.shim_path();
    let fail = |err: io::Error| InstallError::new(format!("{}: {err}", path.display()));
    if let Some(dir) = path.parent() {
        fs::create_dir_all(dir).map_err(fail)?;
    }
    if fs::symlink_metadata(&path).is_ok() {
        fs::remove_file(&path).map_err(fail)?;
    }
    symlink(&env.exe, &path).map_err(fail)
}

pub fn unlink(env: &InstallEnv) -> Result<(), InstallError> {
    let path = env.shim_path();
    fs::remove_file(&path).map_err(|err| InstallError::new(format!("{}: {err}", path.display())))
}
