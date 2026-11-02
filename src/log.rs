//! The runner log: one JSON line per claim, start, renew, end and stop, with
//! ids, counts and the outcome, never content. Each agent's own output goes
//! to one file per run under `runs/`, deleted after `log_keep_days`.

use std::io::Write as _;
use std::path::{Path, PathBuf};
use std::time::{Duration, SystemTime};

use serde_json::{Value, json};

#[derive(Clone, Debug)]
pub struct Log {
    dir: PathBuf,
}

impl Log {
    #[must_use]
    pub fn new(dir: &Path) -> Self {
        Self {
            dir: dir.to_path_buf(),
        }
    }

    /// The file of one run's agent output.
    #[must_use]
    pub fn run_output(&self, run_id: &str) -> PathBuf {
        self.dir.join("runs").join(format!("{run_id}.log"))
    }

    /// Writes one event line, and prints it.
    pub fn event(&self, event: &str, fields: Value) {
        let mut line = json!({"at": chrono::Utc::now().to_rfc3339(), "event": event});
        if let (Some(target), Value::Object(fields)) = (line.as_object_mut(), fields) {
            target.extend(fields);
        }
        println!("{line}");
        let _ = std::fs::create_dir_all(&self.dir);
        if let Ok(mut file) = std::fs::OpenOptions::new()
            .create(true)
            .append(true)
            .open(self.dir.join("octotrigger.log"))
        {
            let _ = writeln!(file, "{line}");
        }
    }

    /// Deletes the run outputs older than `keep_days`.
    pub fn sweep(&self, keep_days: u64) {
        let Ok(entries) = std::fs::read_dir(self.dir.join("runs")) else {
            return;
        };
        let keep = Duration::from_secs(keep_days.saturating_mul(86_400));
        for entry in entries.flatten() {
            let old = entry
                .metadata()
                .and_then(|meta| meta.modified())
                .ok()
                .and_then(|modified| SystemTime::now().duration_since(modified).ok())
                .is_some_and(|age| age > keep);
            if old {
                let _ = std::fs::remove_file(entry.path());
            }
        }
    }
}
