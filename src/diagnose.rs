use std::fs::{File, OpenOptions};
use std::io::{Seek, SeekFrom, Write};
use std::path::PathBuf;
use std::sync::{Mutex, OnceLock};
use std::time::{SystemTime, UNIX_EPOCH};

struct DiagnoseState {
    path: PathBuf,
    file: Mutex<File>,
}

static DIAGNOSE_STATE: OnceLock<DiagnoseState> = OnceLock::new();

pub fn init() -> Result<PathBuf, String> {
    let path = std::env::temp_dir().join("claude-codex-usage-monitor.log");
    let file = OpenOptions::new()
        .create(true)
        .append(true)
        .open(&path)
        .map_err(|e| format!("Unable to open diagnostic log file {}: {e}", path.display()))?;

    let _ = DIAGNOSE_STATE.set(DiagnoseState {
        path: path.clone(),
        file: Mutex::new(file),
    });

    log("diagnostic logging enabled");
    Ok(path)
}

pub fn is_enabled() -> bool {
    DIAGNOSE_STATE.get().is_some()
}

pub fn log(message: impl AsRef<str>) {
    let Some(state) = DIAGNOSE_STATE.get() else {
        return;
    };

    let timestamp = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|duration| duration.as_secs())
        .unwrap_or(0);

    if let Ok(mut file) = state.file.lock() {
        // Keep the current and previous ~4 MiB rather than accumulating indefinitely.
        if file
            .metadata()
            .is_ok_and(|meta| meta.len() >= 4 * 1024 * 1024)
        {
            let _ = file.flush();
            if std::fs::copy(&state.path, state.path.with_extension("previous.log")).is_ok() {
                let _ = file.set_len(0);
                let _ = file.seek(SeekFrom::Start(0));
            }
        }
        let _ = writeln!(file, "[{timestamp}] {}", message.as_ref());
        let _ = file.flush();
    }
}

pub fn log_error(context: &str, error: impl std::fmt::Display) {
    log(format!("{context}: {error}"));
}
