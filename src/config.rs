//! The config file: which agent command takes which runs, on which machine,
//! how many at once, and the limits. It holds no instruction text: the
//! prompt of each run comes from Toolscaled.

use std::path::{Path, PathBuf};

use anyhow::{Context, Result, bail};
use serde::{Deserialize, Serialize};

/// Where the config lives when `--config` names no file.
#[must_use]
pub fn default_path() -> PathBuf {
    home().join(".config/octotrigger/config.yaml")
}

/// The person's home folder.
#[must_use]
pub fn home() -> PathBuf {
    std::env::var_os("HOME").map_or_else(|| PathBuf::from("."), PathBuf::from)
}

/// A path with a leading `~/` read as the home folder.
#[must_use]
pub fn expand(path: &Path) -> PathBuf {
    match path.to_str().and_then(|text| text.strip_prefix("~/")) {
        Some(rest) => home().join(rest),
        None => path.to_path_buf(),
    }
}

#[derive(Clone, Debug, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub struct Config {
    #[serde(default)]
    pub toolscaled: Toolscaled,
    /// The machine's name, which the run page shows beside the agent.
    pub machine: String,
    #[serde(default = "default_state_file")]
    pub state_file: PathBuf,
    #[serde(default = "default_stop_file")]
    pub stop_file: PathBuf,
    /// Where each run's folder is made; it is deleted when the run ends.
    #[serde(default = "default_work_dir")]
    pub work_dir: PathBuf,
    /// The wait after a claim; it doubles up to 300 s while nothing comes.
    #[serde(default = "default_poll_seconds")]
    pub poll_seconds: u64,
    #[serde(default = "default_log_dir")]
    pub log_dir: PathBuf,
    #[serde(default = "default_log_keep_days")]
    pub log_keep_days: u64,
    /// The day the person confirmed the token warning (`octotrigger setup`).
    /// No agent starts while it is empty.
    #[serde(default)]
    pub token_warning_confirmed: Option<String>,
    #[serde(default)]
    pub limits: Limits,
    pub jobs: Vec<Job>,
}

#[derive(Clone, Debug, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub struct Toolscaled {
    /// The Toolscaled app: sign-in and the claim calls.
    #[serde(default = "default_base_url")]
    pub base_url: String,
    /// The MCP server the agent connects to with the run's token.
    #[serde(default = "default_mcp_url")]
    pub mcp_url: String,
    #[serde(default = "default_auth_file")]
    pub auth_file: PathBuf,
}

impl Default for Toolscaled {
    fn default() -> Self {
        Self {
            base_url: default_base_url(),
            mcp_url: default_mcp_url(),
            auth_file: default_auth_file(),
        }
    }
}

#[derive(Clone, Debug, Default, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub struct Limits {
    /// Runs started in one day (UTC); none when absent.
    #[serde(default)]
    pub runs_per_day: Option<u32>,
    /// Tokens in one day (UTC), from the agents that report their use. An
    /// agent that reports none counts against `runs_per_day` only.
    #[serde(default)]
    pub tokens_per_day: Option<u64>,
}

#[derive(Clone, Debug, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub struct Job {
    pub name: String,
    /// The run types this job takes: 1 (the procedure) and 2 (recorded
    /// steps). Toolscaled claims both as one kind today; the run's own
    /// connect answer says which it is.
    #[serde(default = "default_types")]
    pub types: Vec<u8>,
    /// `all`, or a list of workflow ids.
    #[serde(default)]
    pub workflows: Workflows,
    #[serde(default = "default_max_concurrent")]
    pub max_concurrent: usize,
    #[serde(default = "default_lease_seconds")]
    pub lease_seconds: u32,
    #[serde(default = "default_run_timeout_minutes")]
    pub run_timeout_minutes: u64,
    /// The command that runs one task and exits. `{prompt_file}`,
    /// `{folder}` and `{usage_file}` are replaced; it runs with `sh -c` in
    /// the run's folder.
    pub agent: String,
    /// Names removed from the agent's environment, such as
    /// `OPENAI_API_KEY` when the plan login must pay.
    #[serde(default)]
    pub unset_env: Vec<String>,
}

#[derive(Clone, Debug, Default, Deserialize, Serialize, PartialEq, Eq)]
#[serde(untagged)]
pub enum Workflows {
    #[default]
    #[serde(with = "all")]
    All,
    Some(Vec<String>),
}

mod all {
    use serde::{Deserialize, Deserializer, Serializer};

    pub fn serialize<S: Serializer>(serializer: S) -> Result<S::Ok, S::Error> {
        serializer.serialize_str("all")
    }

    pub fn deserialize<'de, D: Deserializer<'de>>(deserializer: D) -> Result<(), D::Error> {
        let text = String::deserialize(deserializer)?;
        if text == "all" {
            Ok(())
        } else {
            Err(serde::de::Error::custom(
                "workflows is `all` or a list of ids",
            ))
        }
    }
}

impl Workflows {
    /// Whether a run of this workflow fits the job.
    #[must_use]
    pub fn takes(&self, workflow_id: &str) -> bool {
        match self {
            Self::All => true,
            Self::Some(ids) => ids.iter().any(|id| id.eq_ignore_ascii_case(workflow_id)),
        }
    }
}

fn default_base_url() -> String {
    "https://app.toolscaled.com".to_owned()
}
fn default_mcp_url() -> String {
    "https://mcp.toolscaled.com/mcp".to_owned()
}
fn default_auth_file() -> PathBuf {
    PathBuf::from("~/.config/octotrigger/auth.json")
}
fn default_state_file() -> PathBuf {
    PathBuf::from("~/.local/state/octotrigger/claims.json")
}
fn default_stop_file() -> PathBuf {
    PathBuf::from("~/.config/octotrigger/stop")
}
fn default_work_dir() -> PathBuf {
    PathBuf::from("~/.local/state/octotrigger/work")
}
fn default_log_dir() -> PathBuf {
    PathBuf::from("~/.local/state/octotrigger")
}
const fn default_poll_seconds() -> u64 {
    30
}
const fn default_log_keep_days() -> u64 {
    30
}
fn default_types() -> Vec<u8> {
    vec![1, 2]
}
const fn default_max_concurrent() -> usize {
    1
}
const fn default_lease_seconds() -> u32 {
    600
}
const fn default_run_timeout_minutes() -> u64 {
    20
}

impl Config {
    /// Reads and checks the config file.
    ///
    /// # Errors
    ///
    /// A file that cannot be read, a field that is not known, and a value
    /// out of its range.
    pub fn load(path: &Path) -> Result<Self> {
        let text = std::fs::read_to_string(path)
            .with_context(|| format!("read the config {}", path.display()))?;
        let config: Self = serde_yaml::from_str(&text)
            .with_context(|| format!("the config {} is not valid", path.display()))?;
        config.check()?;
        Ok(config)
    }

    /// The rules of each field.
    ///
    /// # Errors
    ///
    /// The first field that breaks its rule.
    pub fn check(&self) -> Result<()> {
        if self.machine.trim().is_empty() || self.machine.chars().count() > 60 {
            bail!("machine is 1 to 60 characters");
        }
        if self.jobs.is_empty() {
            bail!("jobs holds at least one job");
        }
        if self.poll_seconds == 0 {
            bail!("poll_seconds is at least 1");
        }
        for job in &self.jobs {
            if job.name.trim().is_empty() {
                bail!("each job has a name");
            }
            if !(60..=1_800).contains(&job.lease_seconds) {
                bail!("job {}: lease_seconds is 60 to 1800", job.name);
            }
            if job.max_concurrent == 0 {
                bail!("job {}: max_concurrent is at least 1", job.name);
            }
            if job.run_timeout_minutes == 0 {
                bail!("job {}: run_timeout_minutes is at least 1", job.name);
            }
            if job.agent.trim().is_empty() {
                bail!("job {}: agent is the command that runs one task", job.name);
            }
            if job.types.is_empty() || job.types.iter().any(|kind| !matches!(kind, 1 | 2)) {
                bail!(
                    "job {}: types holds 1, 2 or both; repair work (3a, 3b, 3c) comes later",
                    job.name
                );
            }
        }
        Ok(())
    }

    /// The runner's name on the run page: the job's agent and the machine.
    #[must_use]
    pub fn runner_name(&self, job: &Job) -> String {
        let agent = job
            .agent
            .split_whitespace()
            .next()
            .and_then(|program| Path::new(program).file_name())
            .and_then(|name| name.to_str())
            .unwrap_or("agent");
        let name = format!("{agent} on {}", self.machine.trim());
        name.chars().take(120).collect()
    }
}

/// The config `octotrigger setup` writes when there is none: one job with
/// the Hermes command, for the person to change.
#[must_use]
pub fn starter(machine: &str) -> String {
    format!(
        "# octotrigger config. The agent command runs one task and exits.\n\
         # {{prompt_file}}: the run's prompt from Toolscaled. {{folder}}: the run's own\n\
         # folder, with mcp.json (Toolscaled and the run's token) and hermes/config.yaml.\n\
         # {{usage_file}}: where the agent writes its token use.\n\
         machine: {machine}\n\
         poll_seconds: 30\n\
         limits:\n\
         \x20 runs_per_day: 20\n\
         \x20 tokens_per_day: 20000000\n\
         jobs:\n\
         \x20 - name: runs\n\
         \x20   types: [1, 2]\n\
         \x20   workflows: all\n\
         \x20   max_concurrent: 1\n\
         \x20   lease_seconds: 600\n\
         \x20   run_timeout_minutes: 20\n\
         \x20   unset_env: [OPENAI_API_KEY]\n\
         \x20   agent: >-\n\
         \x20     hermes -z \"$(cat {{prompt_file}})\" --provider openai-codex -m gpt-6.1-terra\n\
         \x20     -t toolscaled,file --usage-file {{usage_file}}\n"
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_starter_config_reads_and_checks() {
        let config: Config = serde_yaml::from_str(&starter("test-machine")).expect("reads");
        config.check().expect("checks");
        assert_eq!(config.jobs[0].workflows, Workflows::All);
        assert_eq!(
            config.runner_name(&config.jobs[0]),
            "hermes on test-machine"
        );
        assert!(config.token_warning_confirmed.is_none());
    }

    #[test]
    fn a_job_with_repair_types_or_a_long_lease_is_refused() {
        let base = starter("m");
        let repair = base.replace("types: [1, 2]", "types: [3]");
        let config: Config = serde_yaml::from_str(&repair).expect("reads");
        assert!(config.check().is_err());
        let long = base.replace("lease_seconds: 600", "lease_seconds: 5000");
        let config: Config = serde_yaml::from_str(&long).expect("reads");
        assert!(config.check().is_err());
    }

    #[test]
    fn workflows_takes_all_or_the_listed_ids() {
        assert!(Workflows::All.takes("any"));
        let some = Workflows::Some(vec!["AbC".to_owned()]);
        assert!(some.takes("abc"));
        assert!(!some.takes("other"));
        let listed: Workflows = serde_yaml::from_str("[a, b]").expect("reads");
        assert_eq!(
            listed,
            Workflows::Some(vec!["a".to_owned(), "b".to_owned()])
        );
        assert!(serde_yaml::from_str::<Workflows>("some").is_err());
    }
}
