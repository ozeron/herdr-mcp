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

/// Herdr keeps every managed plugin build under a directory with this name.
const HERDR_PLUGINS_DIR: &str = "plugins";

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Relink {
    Relinked,
    Current,
    NotInstalled,
    Left,
}

/// Re-points a shim that names an older Herdr-managed build at this one, because
/// `herdr plugin update` builds into a new directory. A missing shim stays missing, and a shim
/// pointing outside Herdr's plugin builds is left alone.
pub fn relink(env: &InstallEnv) -> Result<Relink, InstallError> {
    match inspect(env) {
        ShimState::Missing => return Ok(Relink::NotInstalled),
        ShimState::Current => return Ok(Relink::Current),
        ShimState::Foreign => return Ok(Relink::Left),
        ShimState::Moved | ShimState::Dangling => {}
    }
    let path = env.shim_path();
    let Ok(target) = fs::read_link(&path).map(|target| resolve(&path, target)) else {
        return Ok(Relink::Left);
    };
    let plugins = env.exe.ancestors().find(|dir| {
        dir.file_name()
            .is_some_and(|name| name == HERDR_PLUGINS_DIR)
    });
    let ours = plugins.is_some_and(|plugins| target.starts_with(plugins))
        && target.file_name().is_some_and(|name| name == SHIM_NAME);
    if !ours {
        return Ok(Relink::Left);
    }
    link(env)?;
    Ok(Relink::Relinked)
}

pub fn unlink(env: &InstallEnv) -> Result<(), InstallError> {
    let path = env.shim_path();
    fs::remove_file(&path).map_err(|err| InstallError::new(format!("{}: {err}", path.display())))
}
