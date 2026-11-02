//! One agent process for one claimed run: its own empty folder, the run's
//! start prompt in it, the command in a process tree of its own (a process
//! group on Unix, a job object on Windows), and its token use when it ends.
//!
//! The agent reaches Toolscaled on octotrigger's own sign-in: octotrigger
//! puts a fresh access token in the agent's environment
//! ([`AGENT_TOKEN_ENV`]), and the agent's MCP config names that variable.
//! The token is in no file of the run's folder and in no log line, and the
//! person's own chats keep their own Toolscaled connection.

use std::fs::File;
use std::io::Write as _;
use std::path::{Path, PathBuf};
use std::process::{Child, Command, ExitStatus, Stdio};

use anyhow::{Context, Result};
use serde_json::Value;

use crate::config::{AGENT_TOKEN_ENV, Job};
use crate::platform::{self, Tree};
use crate::surface::{Claim, Usage};

pub use crate::platform::{alive, kill_group};

/// The files of a run's folder.
pub struct Folder {
    pub path: PathBuf,
    pub prompt_file: PathBuf,
    pub usage_file: PathBuf,
    /// The start prompt, as `{prompt}` gives it.
    pub prompt: String,
}

/// Makes the run's folder, readable by the person only, with the start
/// prompt as Toolscaled wrote it, word for word.
///
/// # Errors
///
/// A folder or file that cannot be written.
pub fn prepare(work_dir: &Path, claim: &Claim) -> Result<Folder> {
    std::fs::create_dir_all(work_dir)?;
    let path = work_dir.join(format!("run-{}", claim.run_id));
    if path.exists() {
        std::fs::remove_dir_all(&path)?;
    }
    std::fs::create_dir_all(&path)?;
    platform::make_private(&path, true)?;
    let prompt_file = path.join("prompt.txt");
    let mut handle = File::create(&prompt_file)?;
    platform::make_private(&prompt_file, false)?;
    handle.write_all(claim.start_prompt.as_bytes())?;
    Ok(Folder {
        usage_file: path.join("usage.json"),
        prompt_file,
        path,
        prompt: claim.start_prompt.clone(),
    })
}

/// The job's command for `sh -c` (Unix): `{prompt}` as one quoted word, and
/// the run's files in place of `{prompt_file}`, `{folder}` and
/// `{usage_file}`, each quoted.
#[must_use]
pub fn command_line(job: &Job, folder: &Folder) -> String {
    job.agent
        .replace(
            "{prompt_file}",
            &quote(&folder.prompt_file.display().to_string()),
        )
        .replace("{folder}", &quote(&folder.path.display().to_string()))
        .replace(
            "{usage_file}",
            &quote(&folder.usage_file.display().to_string()),
        )
        .replace("{prompt}", &quote(&folder.prompt))
}

fn quote(text: &str) -> String {
    format!("'{}'", text.replace('\'', "'\\''"))
}

/// The job's command as a program and its words (Windows): the line split
/// as Windows programs read it, the names replaced in each word with no
/// quoting, and the first word found as a program (`~/` read as the home
/// folder; `.exe`, `.cmd` or `.bat` added when it has none).
///
/// # Errors
///
/// A line with no word.
#[cfg(windows)]
pub fn command_words(job: &Job, folder: &Folder) -> Result<(PathBuf, Vec<String>)> {
    let words = platform::split_words(&job.agent)?;
    let fill = |word: &str| {
        word.replace("{prompt_file}", &folder.prompt_file.display().to_string())
            .replace("{folder}", &folder.path.display().to_string())
            .replace("{usage_file}", &folder.usage_file.display().to_string())
            .replace("{prompt}", &folder.prompt)
    };
    let (program, args) = words.split_first().context("the agent command is empty")?;
    let program = fill(program);
    let found = platform::find_program(&program)
        .unwrap_or_else(|| crate::config::expand(Path::new(&program)));
    Ok((found, args.iter().map(|word| fill(word)).collect()))
}

/// The first word of an agent command: the program, as the person wrote it.
#[must_use]
pub fn program_word(line: &str) -> String {
    #[cfg(windows)]
    {
        platform::split_words(line)
            .ok()
            .and_then(|words| words.into_iter().next())
            .unwrap_or_default()
    }
    #[cfg(not(windows))]
    {
        line.split_whitespace()
            .next()
            .unwrap_or_default()
            .to_owned()
    }
}

/// The name of an agent command's program: its file name, with no `.exe`,
/// `.cmd` or `.bat`.
#[must_use]
pub fn program_name(line: &str) -> Option<String> {
    let word = program_word(line);
    let path = Path::new(&word);
    let windows = path
        .extension()
        .and_then(|e| e.to_str())
        .is_some_and(|e| ["exe", "cmd", "bat"].contains(&e.to_ascii_lowercase().as_str()));
    let name = if windows {
        path.file_stem()
    } else {
        path.file_name()
    };
    name.and_then(|name| name.to_str()).map(str::to_owned)
}

/// A started agent: its process and its process tree.
pub struct AgentProcess {
    child: Child,
    tree: Tree,
}

impl AgentProcess {
    #[must_use]
    pub fn id(&self) -> u32 {
        self.child.id()
    }

    /// Its exit, when it has ended.
    ///
    /// # Errors
    ///
    /// A process the system cannot read.
    pub fn try_wait(&mut self) -> std::io::Result<Option<ExitStatus>> {
        self.child.try_wait()
    }

    /// Waits for its end.
    ///
    /// # Errors
    ///
    /// A process the system cannot read.
    pub fn wait(&mut self) -> std::io::Result<ExitStatus> {
        self.child.wait()
    }

    /// Kills the agent and every process it started.
    pub fn kill(&self) {
        self.tree.kill();
    }
}

/// Starts the agent in the run's folder, in a process tree of its own, with
/// its output in `output`, the job's `env`, and `token` as its Toolscaled
/// access token. On Unix the command runs with `sh -c`; on Windows with no
/// shell ([`command_words`]).
///
/// # Errors
///
/// A command that cannot start.
pub fn spawn(job: &Job, folder: &Folder, output: &Path, token: &str) -> Result<AgentProcess> {
    if let Some(parent) = output.parent() {
        std::fs::create_dir_all(parent)?;
    }
    let log = File::create(output)?;
    #[cfg(windows)]
    let mut command = {
        let (program, args) = command_words(job, folder)?;
        let mut command = Command::new(program);
        command.args(args);
        command
    };
    #[cfg(not(windows))]
    let mut command = {
        let mut command = Command::new("/bin/sh");
        command.arg("-c").arg(command_line(job, folder));
        command
    };
    command
        .current_dir(&folder.path)
        .stdin(Stdio::null())
        .stdout(log.try_clone()?)
        .stderr(log);
    platform::own_tree(&mut command);
    for name in &job.unset_env {
        command.env_remove(name);
    }
    command.envs(job.env_values());
    command.env(AGENT_TOKEN_ENV, token);
    let child = command
        .spawn()
        .with_context(|| format!("start the agent of job {}", job.name))?;
    match Tree::of(&child) {
        Ok(tree) => Ok(AgentProcess { child, tree }),
        Err(error) => {
            let mut child = child;
            let _ = child.kill();
            let _ = child.wait();
            Err(error).with_context(|| format!("hold the agent of job {}", job.name))
        }
    }
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
        serde_json::from_value(serde_json::json!({"name": "j", "agent": agent})).expect("job")
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
    fn the_prompt_is_one_word_however_it_is_written() {
        let work = tempfile::tempdir().expect("work");
        let mut claim = claim();
        claim.start_prompt = "It's \"quoted\", $HOME and `x`.\nLine two.".into();
        let folder = prepare(work.path(), &claim).expect("folder");
        #[cfg(unix)]
        {
            let output = work.path().join("out.log");
            let status = spawn(&job("printf '%s|' {prompt}"), &folder, &output, "t")
                .expect("spawn")
                .wait()
                .expect("wait");
            assert!(status.success());
            let text = std::fs::read_to_string(&output).expect("output");
            assert_eq!(text, format!("{}|", claim.start_prompt), "{text}");
        }
        #[cfg(windows)]
        {
            let (_, args) = command_words(
                &job(r#"hermes -z {prompt} -m "claude-sonnet-5-5[1m]" --usage-file {usage_file}"#),
                &folder,
            )
            .expect("words");
            assert_eq!(
                args,
                [
                    "-z".to_owned(),
                    claim.start_prompt.clone(),
                    "-m".to_owned(),
                    "claude-sonnet-5-5[1m]".to_owned(),
                    "--usage-file".to_owned(),
                    folder.usage_file.display().to_string(),
                ]
            );
        }
    }

    #[cfg(windows)]
    #[test]
    fn the_agent_runs_in_its_folder_without_the_unset_names() {
        let work = tempfile::tempdir().expect("work");
        let folder = prepare(work.path(), &claim()).expect("folder");
        let mut job = job(
            r#"powershell.exe -NoProfile -Command "(Get-Location).Path; 'key=' + $env:OCTO_TEST_KEY + ' home=' + $env:OCTO_TEST_HOME + ' token=' + $env:OCTOTRIGGER_TOOLSCALED_TOKEN""#,
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
        assert!(text.contains("key= "), "{text}");
        let home = crate::config::home().join("agent-home");
        assert!(text.contains(&format!("home={}", home.display())), "{text}");
        assert!(text.contains("token=run-token"), "{text}");
    }

    #[cfg(unix)]
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
