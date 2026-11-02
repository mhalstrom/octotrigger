//! One agent process for one claimed run: its own empty folder, the run's
//! start prompt in it, the command in its own process group, and its token
//! use when it ends.
//!
//! The agent reaches Toolscaled on octotrigger's own sign-in: octotrigger
//! puts a fresh access token in the agent's environment
//! ([`AGENT_TOKEN_ENV`]), and the agent's MCP config names that variable.
//! The token is in no file of the run's folder and in no log line, and the
//! person's own chats keep their own Toolscaled connection.

use std::fs::File;
use std::io::Write as _;
use std::os::unix::process::CommandExt as _;
use std::path::{Path, PathBuf};
use std::process::{Child, Command, Stdio};

use anyhow::{Context, Result};
use serde_json::Value;

use crate::config::{AGENT_TOKEN_ENV, Job};
use crate::surface::{Claim, Usage};

/// The files of a run's folder.
pub struct Folder {
    pub path: PathBuf,
    pub prompt_file: PathBuf,
    pub usage_file: PathBuf,
}

/// Makes the run's folder, readable by the person only, with the start
/// prompt as Toolscaled wrote it, word for word.
///
/// # Errors
///
/// A folder or file that cannot be written.
pub fn prepare(work_dir: &Path, claim: &Claim) -> Result<Folder> {
    use std::os::unix::fs::PermissionsExt as _;
    std::fs::create_dir_all(work_dir)?;
    let path = work_dir.join(format!("run-{}", claim.run_id));
    if path.exists() {
        std::fs::remove_dir_all(&path)?;
    }
    std::fs::create_dir_all(&path)?;
    std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o700))?;
    let prompt_file = path.join("prompt.txt");
    let mut handle = File::create(&prompt_file)?;
    std::fs::set_permissions(&prompt_file, std::fs::Permissions::from_mode(0o600))?;
    handle.write_all(claim.start_prompt.as_bytes())?;
    Ok(Folder {
        usage_file: path.join("usage.json"),
        prompt_file,
        path,
    })
}

/// The job's command with the run's files in place of `{prompt_file}`,
/// `{folder}` and `{usage_file}`, each quoted for `sh`.
#[must_use]
pub fn command_line(job: &Job, folder: &Folder) -> String {
    job.agent
        .replace("{prompt_file}", &quote(&folder.prompt_file))
        .replace("{folder}", &quote(&folder.path))
        .replace("{usage_file}", &quote(&folder.usage_file))
}

fn quote(path: &Path) -> String {
    format!("'{}'", path.display().to_string().replace('\'', "'\\''"))
}

/// Starts the agent in the run's folder, in a process group of its own, with
/// its output in `output`, the job's `env`, and `token` as its Toolscaled
/// access token.
///
/// # Errors
///
/// A command that cannot start.
pub fn spawn(job: &Job, folder: &Folder, output: &Path, token: &str) -> Result<Child> {
    if let Some(parent) = output.parent() {
        std::fs::create_dir_all(parent)?;
    }
    let log = File::create(output)?;
    let mut command = Command::new("/bin/sh");
    command
        .arg("-c")
        .arg(command_line(job, folder))
        .current_dir(&folder.path)
        .stdin(Stdio::null())
        .stdout(log.try_clone()?)
        .stderr(log)
        .process_group(0);
    for name in &job.unset_env {
        command.env_remove(name);
    }
    command.envs(job.env_values());
    command.env(AGENT_TOKEN_ENV, token);
    command
        .spawn()
        .with_context(|| format!("start the agent of job {}", job.name))
}

/// Stops a process group: the agent and every process it started.
pub fn kill_group(pid: u32) {
    if let Ok(pid) = i32::try_from(pid) {
        // SAFETY: killpg sends a signal; it reads and writes no memory of ours.
        unsafe {
            libc::killpg(pid, libc::SIGKILL);
        }
    }
}

/// Whether a process still lives.
#[must_use]
pub fn alive(pid: u32) -> bool {
    i32::try_from(pid).is_ok_and(|pid| {
        // SAFETY: signal 0 checks the process and sends nothing.
        unsafe { libc::kill(pid, 0) == 0 }
    })
}

/// The agent's token use: the usage file when the agent wrote one, else the
/// last JSON object of its output (`claude -p --output-format json`).
/// `None` when it reported none.
#[must_use]
pub fn read_usage(folder: &Folder, output: &Path) -> Option<Usage> {
    let from_file = std::fs::read_to_string(&folder.usage_file)
        .ok()
        .and_then(|text| serde_json::from_str::<Value>(&text).ok())
        .and_then(|value| usage_in(&value));
    from_file.or_else(|| {
        let text = std::fs::read_to_string(output).ok()?;
        text.lines()
            .rev()
            .filter_map(|line| serde_json::from_str::<Value>(line.trim()).ok())
            .find_map(|value| usage_in(&value))
            .or_else(|| {
                serde_json::from_str::<Value>(text.trim())
                    .ok()
                    .and_then(|value| usage_in(&value))
            })
    })
}

/// The first object in `value` with input and output token counts, with
/// the cache reads and the model when it names them.
#[must_use]
pub fn usage_in(value: &Value) -> Option<Usage> {
    let count = |object: &serde_json::Map<String, Value>, keys: &[&str]| {
        keys.iter()
            .find_map(|key| object.get(*key).and_then(Value::as_u64))
    };
    match value {
        Value::Object(object) => {
            let input = count(object, &["input_tokens", "prompt_tokens"]);
            let output = count(object, &["output_tokens", "completion_tokens"]);
            if let (Some(input_tokens), Some(output_tokens)) = (input, output) {
                return Some(Usage {
                    input_tokens,
                    output_tokens,
                    cache_read_tokens: count(
                        object,
                        &[
                            "cache_read_input_tokens",
                            "cache_read_tokens",
                            "cached_tokens",
                        ],
                    )
                    .unwrap_or(0),
                    model: object
                        .get("model")
                        .and_then(Value::as_str)
                        .map(str::to_owned),
                });
            }
            let model = object.get("model").and_then(Value::as_str);
            ["usage", "total_usage", "tokens"]
                .iter()
                .filter_map(|key| object.get(*key))
                .chain(object.values())
                .find_map(usage_in)
                .map(|mut usage| {
                    if usage.model.is_none() {
                        usage.model = model.map(str::to_owned);
                    }
                    usage
                })
        }
        Value::Array(items) => items.iter().find_map(usage_in),
        _ => None,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn claim() -> Claim {
        Claim {
            run_id: "r1".into(),
            workflow_id: "w1".into(),
            kind: "agent_run".into(),
            code: "s1".into(),
            lease_until: String::new(),
            claim_token: "c1".into(),
            start_prompt: "You are running a Toolscaled agent run. Nobody reads this chat.".into(),
        }
    }

    fn job(agent: &str) -> Job {
        serde_yaml::from_str(&format!("name: j\nagent: \"{agent}\"\n")).expect("job")
    }

    #[test]
    fn the_folder_holds_the_start_prompt_word_for_word_and_nothing_else() {
        let work = tempfile::tempdir().expect("work");
        let folder = prepare(work.path(), &claim()).expect("folder");
        assert_eq!(
            std::fs::read_to_string(&folder.prompt_file).expect("prompt"),
            claim().start_prompt
        );
        let names: Vec<_> = std::fs::read_dir(&folder.path)
            .expect("folder")
            .map(|entry| entry.expect("entry").file_name())
            .collect();
        assert_eq!(names, ["prompt.txt"]);
        let line = command_line(&job("agent -z {prompt_file} --usage {usage_file}"), &folder);
        assert!(line.contains("prompt.txt'"), "{line}");
    }

    #[test]
    fn the_agent_runs_in_its_folder_without_the_unset_names() {
        let work = tempfile::tempdir().expect("work");
        let folder = prepare(work.path(), &claim()).expect("folder");
        let mut job = job(
            "pwd; echo key=${OCTO_TEST_KEY:-none} home=$OCTO_TEST_HOME token=$OCTOTRIGGER_TOOLSCALED_TOKEN",
        );
        job.unset_env = vec!["OCTO_TEST_KEY".into()];
        job.env
            .insert("OCTO_TEST_HOME".into(), "~/agent-home".into());
        // SAFETY: the test sets a name only it reads.
        unsafe { std::env::set_var("OCTO_TEST_KEY", "secret") };
        let output = work.path().join("out.log");
        let status = spawn(&job, &folder, &output, "run-token")
            .expect("spawn")
            .wait()
            .expect("wait");
        assert!(status.success());
        let text = std::fs::read_to_string(&output).expect("output");
        assert!(text.contains("run-r1"), "{text}");
        assert!(text.contains("key=none"), "{text}");
        let home = crate::config::home().join("agent-home");
        assert!(text.contains(&format!("home={}", home.display())), "{text}");
        assert!(text.contains("token=run-token"), "{text}");
        let names: Vec<_> = std::fs::read_dir(&folder.path)
            .expect("folder")
            .map(|entry| entry.expect("entry").file_name())
            .collect();
        assert_eq!(names, ["prompt.txt"], "no token file in the run's folder");
    }

    #[test]
    fn usage_is_read_from_the_file_or_the_last_json_of_the_output() {
        let claude = serde_json::json!({
            "type": "result",
            "usage": {"input_tokens": 120, "cache_read_input_tokens": 9000, "output_tokens": 300},
            "model": "claude-opus-5-5"
        });
        let usage = usage_in(&claude).expect("usage");
        assert_eq!(usage.input_tokens, 120);
        assert_eq!(usage.cache_read_tokens, 9000);
        assert_eq!(usage.output_tokens, 300);
        assert_eq!(usage.model.as_deref(), Some("claude-opus-5-5"));
        assert_eq!(usage.total(), 9420);
        assert!(usage_in(&serde_json::json!({"text": "no counts"})).is_none());

        let work = tempfile::tempdir().expect("work");
        let folder = prepare(work.path(), &claim()).expect("folder");
        let output = work.path().join("out.log");
        std::fs::write(&output, format!("some text\n{claude}\n")).expect("write");
        assert_eq!(
            read_usage(&folder, &output).expect("usage").output_tokens,
            300
        );
        std::fs::write(
            &folder.usage_file,
            r#"{"input_tokens":1,"output_tokens":2}"#,
        )
        .expect("write");
        assert_eq!(
            read_usage(&folder, &output).expect("usage").output_tokens,
            2
        );
    }
}
