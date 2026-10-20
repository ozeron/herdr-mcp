use std::ffi::OsString;

/// An environment variable, where empty means unset.
pub fn env_nonempty(name: &str) -> Option<OsString> {
    std::env::var_os(name).filter(|value| !value.is_empty())
}

/// Like [`env_nonempty`], but a value that is not valid UTF-8 also counts as unset.
pub fn env_nonempty_string(name: &str) -> Option<String> {
    env_nonempty(name).and_then(|value| value.into_string().ok())
}
