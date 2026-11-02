//! The config file: which agent command takes which runs, on which machine,
//! how many at once, and the limits. It holds no instruction text: the
//! prompt of each run comes from Toolscaled.

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};

use anyhow::{Context, Result, bail};
use serde::{Deserialize, Serialize};

/// Where the config lives when `--config` names no file.
#[must_use]
pub fn default_path() -> PathBuf {
    expand(Path::new("~/.config/octotrigger/config.yaml"))
}

/// The environment variable that holds octotrigger's Toolscaled access
/// token for one run's agent. octotrigger sets it on the agent process only,
/// fresh for each run; the agent's MCP config names it, never the token.
pub const AGENT_TOKEN_ENV: &str = "OCTOTRIGGER_TOOLSCALED_TOKEN";

/// octotrigger's own Hermes Agent, installed in one folder of its own: its
/// code, its Hermes home (config, model login, runtime), and the home folder
/// its installer writes shell files to. Nothing outside the folder changes,
/// and the person's own Hermes (`~/.hermes`) is never read or written.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct OwnHermes {
    pub root: PathBuf,
}

/// The folder of [`OwnHermes`], under the home folder.
pub const OWN_HERMES_DIR: &str = ".octotrigger/agents/hermes";

impl OwnHermes {
    /// `~/.octotrigger/agents/hermes`.
    #[must_use]
    pub fn new() -> Self {
        Self {
            root: expand(Path::new(&format!("~/{OWN_HERMES_DIR}"))),
        }
    }

    /// The Hermes code (a git checkout at the pinned commit).
    #[must_use]
    pub fn code(&self) -> PathBuf {
        self.root.join("hermes-agent")
    }

    /// The `hermes` command of this install. On Windows the installer
    /// writes `hermes.exe`, or `hermes.cmd` when it cannot make an `.exe`.
    #[must_use]
    pub fn program(&self) -> PathBuf {
        let bin = self.code().join(".hermes").join("bin");
        if cfg!(windows) {
            let cmd = bin.join("hermes.cmd");
            if cmd.exists() && !bin.join("hermes.exe").exists() {
                return cmd;
            }
            return bin.join("hermes.exe");
        }
        bin.join("hermes")
    }

    /// Its `HERMES_HOME`.
    #[must_use]
    pub fn home(&self) -> PathBuf {
        self.root.join("home")
    }

    /// The `HOME` the installer runs with, so its shell lines land here.
    #[must_use]
    pub fn installer_home(&self) -> PathBuf {
        self.root.join("installer-home")
    }
}

impl Default for OwnHermes {
    fn default() -> Self {
        Self::new()
    }
}

/// The person's home folder.
#[must_use]
pub fn home() -> PathBuf {
    crate::platform::home()
}

/// A path with a leading `~/` read as the home folder.
#[must_use]
pub fn expand(path: &Path) -> PathBuf {
    match path.to_str().and_then(|text| text.strip_prefix("~/")) {
        // Each part on its own, so a Windows path has one kind of separator.
        Some(rest) => rest
            .split('/')
            .filter(|part| !part.is_empty())
            .fold(home(), |path, part| path.join(part)),
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
    /// The Toolscaled MCP server: the resource that octotrigger's sign-in
    /// asks for.
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
    /// The command that runs one task and exits, in the run's folder.
    /// `{prompt}` is the run's start prompt as one word; `{prompt_file}`,
    /// `{folder}` and `{usage_file}` are the run's files. On macOS and Linux
    /// it runs with `sh -c`. On Windows it runs with no shell: octotrigger
    /// splits it into words as Windows programs do (double quotes group a
    /// word), replaces the names in each word, and starts the first word,
    /// with `~/` read as the home folder.
    pub agent: String,
    /// Names removed from the agent's environment, such as
    /// `OPENAI_API_KEY` when the plan login must pay.
    #[serde(default)]
    pub unset_env: Vec<String>,
    /// Names set in the agent's environment, such as `HERMES_HOME`. A value
    /// that starts with `~/` is read from the home folder.
    #[serde(default)]
    pub env: BTreeMap<String, String>,
}

impl Job {
    /// The job's `env`, with `~/` read as the home folder.
    #[must_use]
    pub fn env_values(&self) -> Vec<(String, String)> {
        self.env
            .iter()
            .map(|(name, value)| (name.clone(), expand(Path::new(value)).display().to_string()))
            .collect()
    }
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

/// The longest run: the agent's access token (one hour) must outlive it,
/// with five minutes to spare.
pub const MAX_RUN_MINUTES: u64 = 50;

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
            if !(1..=MAX_RUN_MINUTES).contains(&job.run_timeout_minutes) {
                bail!(
                    "job {}: run_timeout_minutes is 1 to {MAX_RUN_MINUTES}: the agent's \
                     Toolscaled token lives one hour",
                    job.name
                );
            }
            if job.env.contains_key(AGENT_TOKEN_ENV) {
                bail!(
                    "job {}: octotrigger sets {AGENT_TOKEN_ENV} itself",
                    job.name
                );
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
        let agent = crate::agent::program_name(&job.agent).unwrap_or_else(|| "agent".to_owned());
        let name = format!("{agent} on {}", self.machine.trim());
        name.chars().take(120).collect()
    }
}

/// The model plan that pays for the agent.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Plan {
    /// A ChatGPT plan, through Hermes' `openai-codex` login.
    ChatGpt,
}

impl Plan {
    /// The plan of a job command, from its `--provider`.
    #[must_use]
    pub fn of_command(agent: &str) -> Option<Self> {
        agent.contains("openai-codex").then_some(Self::ChatGpt)
    }

    /// The Hermes command of a job on this plan, with `program` as the
    /// `hermes` command.
    #[must_use]
    pub fn agent_command(self, program: &str) -> String {
        match self {
            Self::ChatGpt => format!(
                "{program} -z {{prompt}}\n\
                 \x20     --provider openai-codex -m gpt-6.1-terra\n\
                 \x20     -t toolscaled,file --usage-file {{usage_file}}"
            ),
        }
    }

    /// The API key that must not reach the agent, so the plan pays.
    #[must_use]
    pub const fn api_key(self) -> &'static str {
        match self {
            Self::ChatGpt => "OPENAI_API_KEY",
        }
    }
}

/// Which Hermes runs the agents.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Agent {
    /// octotrigger's own Hermes ([`OwnHermes`]); the default.
    Own,
    /// The person's own Hermes setup: `hermes` on the PATH and `~/.hermes`,
    /// with its own Toolscaled connection.
    Mine,
}

impl Agent {
    /// The Hermes of a job: [`Agent::Own`] when its `HERMES_HOME` is
    /// octotrigger's own, [`Agent::Mine`] for another Hermes job, `None` for
    /// a job that does not run Hermes.
    #[must_use]
    pub fn of_job(job: &Job, own: &OwnHermes) -> Option<Self> {
        let home = own.home();
        if job
            .env_values()
            .iter()
            .any(|(name, value)| name == "HERMES_HOME" && Path::new(value) == home)
        {
            return Some(Self::Own);
        }
        (crate::agent::program_name(&job.agent).as_deref() == Some("hermes")).then_some(Self::Mine)
    }
}

/// The config `octotrigger setup` writes when there is none: one job with
/// the Hermes command of the plan, for the person to change.
#[must_use]
pub fn starter(machine: &str, plan: Plan, agent: Agent) -> String {
    let (program, env) = match agent {
        Agent::Own => (
            format!("~/{OWN_HERMES_DIR}/hermes-agent/.hermes/bin/hermes"),
            format!("\x20   env:\n\x20     HERMES_HOME: ~/{OWN_HERMES_DIR}/home\n"),
        ),
        Agent::Mine => ("hermes".to_owned(), String::new()),
    };
    let connection = match agent {
        Agent::Own => format!(
            "# writes its token use. The agent reaches Toolscaled on octotrigger's\n\
             # sign-in: octotrigger puts a fresh token in {AGENT_TOKEN_ENV} for each run.\n"
        ),
        Agent::Mine => "# writes its token use. The agent uses your own Hermes and its own\n\
                        # Toolscaled connection.\n"
            .to_owned(),
    };
    format!(
        "# octotrigger config. The agent command runs one task and exits.\n\
         # {{prompt}}: the run's start prompt from Toolscaled, word for word, as one word.\n\
         # {{folder}}: the run's own empty folder. {{usage_file}}: where the agent\n\
         {connection}\
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
         \x20   unset_env: [{api_key}]\n\
         {env}\
         \x20   agent: >-\n\
         \x20     {command}\n",
        api_key = plan.api_key(),
        command = plan.agent_command(&program),
    )
}

/// The `config.yaml` of octotrigger's Hermes home: the Toolscaled server
/// with the token of the environment, no borrowed logins, no memory across
/// runs.
#[must_use]
pub fn hermes_config(mcp_url: &str) -> String {
    format!(
        "# Written by octotrigger setup. The Hermes of the agents that octotrigger\n\
         # starts. Your own Hermes (~/.hermes) is not changed.\n\
         cli:\n\
         \x20 # No `hermes` command on your PATH: octotrigger calls this one by its path.\n\
         \x20 expose_on_path: false\n\
         mcp_servers:\n\
         \x20 toolscaled:\n\
         \x20   url: {mcp_url}\n\
         \x20   headers:\n\
         \x20     Authorization: Bearer ${{{AGENT_TOKEN_ENV}}}\n\
         \x20   enabled: true\n\
         auth:\n\
         \x20 # Only this home's own logins (hermes auth add): a login shared with\n\
         \x20 # another program can log the two out.\n\
         \x20 adopt_external_logins: false\n\
         memory:\n\
         \x20 # Nothing of one run reaches the next.\n\
         \x20 memory_enabled: false\n\
         \x20 user_profile_enabled: false\n"
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_starter_config_reads_and_checks() {
        {
            let plan = Plan::ChatGpt;
            let config: Config =
                serde_yaml::from_str(&starter("test-machine", plan, Agent::Own)).expect("reads");
            config.check().expect("checks");
            let job = &config.jobs[0];
            assert_eq!(job.workflows, Workflows::All);
            assert_eq!(config.runner_name(job), "hermes on test-machine");
            assert!(config.token_warning_confirmed.is_none());
            assert_eq!(Plan::of_command(&job.agent), Some(plan));
            assert_eq!(job.unset_env, [plan.api_key()]);
            let env = job.env_values();
            assert_eq!(env.len(), 1);
            assert_eq!(env[0].0, "HERMES_HOME");
            assert_eq!(env[0].1, OwnHermes::new().home().display().to_string());
            assert!(job.agent.contains("-t toolscaled,file"), "{}", job.agent);
            assert!(
                job.agent
                    .starts_with("~/.octotrigger/agents/hermes/hermes-agent/.hermes/bin/hermes -z"),
                "{}",
                job.agent
            );
            assert_eq!(Agent::of_job(job, &OwnHermes::new()), Some(Agent::Own));

            let mine: Config =
                serde_yaml::from_str(&starter("m", plan, Agent::Mine)).expect("reads");
            mine.check().expect("checks");
            let job = &mine.jobs[0];
            assert!(job.env.is_empty());
            assert!(job.agent.starts_with("hermes -z"), "{}", job.agent);
            assert_eq!(Agent::of_job(job, &OwnHermes::new()), Some(Agent::Mine));
        }
    }

    #[test]
    fn a_job_with_repair_types_or_a_long_lease_is_refused() {
        let base = starter("m", Plan::ChatGpt, Agent::Own);
        let repair = base.replace("types: [1, 2]", "types: [3]");
        let config: Config = serde_yaml::from_str(&repair).expect("reads");
        assert!(config.check().is_err());
        let long = base.replace("lease_seconds: 600", "lease_seconds: 5000");
        let config: Config = serde_yaml::from_str(&long).expect("reads");
        assert!(config.check().is_err());
        let slow = base.replace("run_timeout_minutes: 20", "run_timeout_minutes: 51");
        let config: Config = serde_yaml::from_str(&slow).expect("reads");
        assert!(config.check().is_err(), "a run longer than its token");
        let token = base.replace(
            "HERMES_HOME: ~/.octotrigger/agents/hermes/home",
            "OCTOTRIGGER_TOOLSCALED_TOKEN: x",
        );
        let config: Config = serde_yaml::from_str(&token).expect("reads");
        assert!(config.check().is_err(), "octotrigger sets the token itself");
    }

    #[test]
    fn the_hermes_config_names_the_token_variable_and_no_token() {
        let text = hermes_config("https://mcp.example/mcp");
        let value: serde_yaml::Value = serde_yaml::from_str(&text).expect("yaml");
        let server = &value["mcp_servers"]["toolscaled"];
        assert_eq!(server["url"].as_str(), Some("https://mcp.example/mcp"));
        assert_eq!(
            server["headers"]["Authorization"].as_str(),
            Some("Bearer ${OCTOTRIGGER_TOOLSCALED_TOKEN}")
        );
        assert_eq!(
            value["auth"]["adopt_external_logins"].as_bool(),
            Some(false)
        );
        assert_eq!(value["memory"]["memory_enabled"].as_bool(), Some(false));
        assert_eq!(value["cli"]["expose_on_path"].as_bool(), Some(false));
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
