use std::fs::{self, File, OpenOptions};
use std::io::{self, Write};
use std::os::unix::fs::{DirBuilderExt, OpenOptionsExt, PermissionsExt};
use std::path::PathBuf;
use std::sync::{Arc, Mutex};
use std::time::{Duration, SystemTime, UNIX_EPOCH};

const LOG_NAME: &str = "herdr-mcp.log";
const MAX_LOG_BYTES: u64 = 256 * 1024;
const MAX_FIELD_LEN: usize = 64;

struct LogFile {
    dir: PathBuf,
    file: File,
    size: u64,
}

/// Lines carry only the tool name, an outcome code and a duration.
/// Each field is restricted to ID-shaped characters, so free text cannot reach the log.
#[derive(Clone, Default)]
pub struct Logger {
    file: Option<Arc<Mutex<LogFile>>>,
}

impl Logger {
    pub fn stderr_only() -> Self {
        Self::default()
    }

    pub fn to_dir(dir: PathBuf) -> io::Result<Self> {
        fs::DirBuilder::new()
            .recursive(true)
            .mode(0o700)
            .create(&dir)?;
        fs::set_permissions(&dir, fs::Permissions::from_mode(0o700))?;
        let file = open_log(&dir)?;
        let size = file.metadata()?.len();
        Ok(Self {
            file: Some(Arc::new(Mutex::new(LogFile { dir, file, size }))),
        })
    }

    pub fn call(&self, tool: &str, outcome: &str, elapsed: Duration) {
        let secs = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .map_or(0, |d| d.as_secs());
        let line = format!(
            "{secs} tool={} outcome={} ms={}\n",
            clean(tool),
            clean(outcome),
            elapsed.as_millis()
        );
        eprint!("{line}");
        if let Some(file) = &self.file {
            let mut file = file.lock().unwrap_or_else(|e| e.into_inner());
            let _ = file.append(line.as_bytes());
        }
    }
}

impl LogFile {
    fn append(&mut self, bytes: &[u8]) -> io::Result<()> {
        if self.size + bytes.len() as u64 > MAX_LOG_BYTES {
            let current = self.dir.join(LOG_NAME);
            fs::rename(&current, self.dir.join(format!("{LOG_NAME}.1")))?;
            self.file = open_log(&self.dir)?;
            self.size = 0;
        }
        self.file.write_all(bytes)?;
        self.size += bytes.len() as u64;
        Ok(())
    }
}

fn open_log(dir: &std::path::Path) -> io::Result<File> {
    let file = OpenOptions::new()
        .create(true)
        .append(true)
        .mode(0o600)
        .open(dir.join(LOG_NAME))?;
    file.set_permissions(fs::Permissions::from_mode(0o600))?;
    Ok(file)
}

fn clean(value: &str) -> &str {
    let ok = !value.is_empty()
        && value.len() <= MAX_FIELD_LEN
        && value
            .bytes()
            .all(|b| b.is_ascii_alphanumeric() || matches!(b, b':' | b'_' | b'.' | b'-'));
    if ok { value } else { "-" }
}
