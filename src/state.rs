//! The state file: the claims that agents work now, and the day's counts.
//! A restart reads it to stop the agents of the last start and end their
//! claims. It holds each claim's secret, so only the person can read it.

use std::io::Write as _;
use std::path::{Path, PathBuf};

use anyhow::Result;
use serde::{Deserialize, Serialize};

#[derive(Clone, Debug, Default, Deserialize, Serialize, PartialEq, Eq)]
pub struct State {
    /// The process of the running `octotrigger start`, when one runs.
    #[serde(default)]
    pub loop_pid: Option<u32>,
    /// The day (UTC) of the counts.
    #[serde(default)]
    pub day: String,
    #[serde(default)]
    pub runs_today: u32,
    #[serde(default)]
    pub tokens_today: u64,
    /// Agents that ran today with no token report.
    #[serde(default)]
    pub unreported_today: u32,
    #[serde(default)]
    pub claims: Vec<Held>,
}

/// One claim an agent works.
#[derive(Clone, Debug, Deserialize, Serialize, PartialEq, Eq)]
pub struct Held {
    pub run_id: String,
    pub workflow_id: String,
    pub job: String,
    pub claim_token: String,
    /// The agent's process group.
    pub pid: u32,
    pub started_at: String,
    pub folder: PathBuf,
}

/// Today's day (UTC), as the counts name it.
#[must_use]
pub fn today() -> String {
    chrono::Utc::now().format("%Y-%m-%d").to_string()
}

impl State {
    /// Reads the state file; an absent file is an empty state.
    ///
    /// # Errors
    ///
    /// A file that cannot be read or holds no state.
    pub fn load(path: &Path) -> Result<Self> {
        match std::fs::read_to_string(path) {
            Ok(text) => Ok(serde_json::from_str(&text)?),
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(Self::default()),
            Err(error) => Err(error.into()),
        }
    }

    /// Writes the state file whole, readable by the person only.
    ///
    /// # Errors
    ///
    /// A file that cannot be written.
    pub fn save(&self, path: &Path) -> Result<()> {
        if let Some(folder) = path.parent() {
            std::fs::create_dir_all(folder)?;
        }
        let partial = path.with_extension("json.partial");
        {
            let mut file = std::fs::File::create(&partial)?;
            crate::platform::make_private(&partial, false)?;
            file.write_all(&serde_json::to_vec_pretty(self)?)?;
            file.sync_all()?;
        }
        std::fs::rename(&partial, path)?;
        Ok(())
    }

    /// Starts the counts of a new day.
    pub fn roll(&mut self, day: &str) {
        if self.day != day {
            day.clone_into(&mut self.day);
            self.runs_today = 0;
            self.tokens_today = 0;
            self.unreported_today = 0;
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_new_day_starts_the_counts_again() {
        let mut state = State {
            day: "2026-11-01".into(),
            runs_today: 4,
            tokens_today: 10,
            unreported_today: 1,
            ..State::default()
        };
        state.roll("2026-11-01");
        assert_eq!(state.runs_today, 4);
        state.roll("2026-11-02");
        assert_eq!(
            (state.runs_today, state.tokens_today, state.unreported_today),
            (0, 0, 0)
        );
    }

    #[test]
    fn the_file_round_trips_and_an_absent_file_is_empty() {
        let folder = tempfile::tempdir().expect("folder");
        let path = folder.path().join("s/claims.json");
        assert_eq!(State::load(&path).expect("empty"), State::default());
        let state = State {
            loop_pid: Some(7),
            claims: vec![Held {
                run_id: "r".into(),
                workflow_id: "w".into(),
                job: "j".into(),
                claim_token: "c".into(),
                pid: 9,
                started_at: "t".into(),
                folder: PathBuf::from("/tmp/x"),
            }],
            ..State::default()
        };
        state.save(&path).expect("save");
        assert_eq!(State::load(&path).expect("load"), state);
    }
}
