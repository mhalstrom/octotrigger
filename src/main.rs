use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};

use anyhow::{Context, Result, bail};
use clap::{Parser, Subcommand};

use octotrigger::agent::{self, alive};
use octotrigger::auth;
use octotrigger::config::{self, Config, expand};
use octotrigger::runner::{Runner, Stop, read_stop, write_stop};
use octotrigger::setup::{self, Paths, Terminal};
use octotrigger::state::{State, today};
use octotrigger::surface::{Claim, HttpSurface, Surface as _};

/// Claims Toolscaled runs that wait for an agent and starts your own agent
/// CLI on each.
#[derive(Parser)]
#[command(version)]
struct Cli {
    /// The config file.
    #[arg(long, global = true)]
    config: Option<PathBuf>,
    #[command(subcommand)]
    command: Command,
}

#[derive(Subcommand)]
enum Command {
    /// Sign in to Toolscaled in the browser.
    Login {
        /// Print the address only; do not open the browser.
        #[arg(long)]
        no_browser: bool,
    },
    /// A guided setup, from nothing to a running watcher: Hermes, the model
    /// login, the sign-in, the token warning, the check, and how to keep it
    /// running. Each step checks the machine first.
    Setup,
    /// Check the sign-in, the claim call, and each job's agent: one short
    /// task that reads the workspace in Toolscaled.
    Check {
        /// Start no agent: check the sign-in and the claim call only.
        #[arg(long)]
        no_agent: bool,
    },
    /// Claim runs and start agents until `octotrigger stop`.
    Start,
    /// Claim nothing new; running agents finish.
    Stop {
        /// Stop the running agents too, and end their claims.
        #[arg(long)]
        now: bool,
    },
    /// The claims held, the agents running and the day's counts.
    Status,
}

fn main() -> Result<()> {
    let cli = Cli::parse();
    let path = cli.config.unwrap_or_else(config::default_path);
    match cli.command {
        Command::Setup => {
            setup::need_terminal()?;
            let finished = setup::run(&mut Terminal, &paths(path)?)?;
            if !finished {
                std::process::exit(1);
            }
            Ok(())
        }
        Command::Login { no_browser } => {
            let config = load_or_defaults(&path)?;
            let tokens = auth::login(
                &config.base_url,
                &config.mcp_url,
                &expand(&config.auth_file),
                !no_browser,
            )?;
            println!(
                "Signed in: workspace {}, scope {}.",
                tokens.organization_id, tokens.scope
            );
            Ok(())
        }
        Command::Check { no_agent } => check(&Config::load(&path)?, !no_agent),
        Command::Start => {
            let config = Config::load(&path)?;
            let stop_file = expand(&config.stop_file);
            let state = State::load(&expand(&config.state_file))?;
            if let Some(pid) = state.loop_pid.filter(|pid| alive(*pid)) {
                bail!("octotrigger already runs (process {pid}); `octotrigger stop` ends it");
            }
            // A start is the person's choice: an old stop file does not hold it.
            let _ = std::fs::remove_file(&stop_file);
            let surface = HttpSurface::new(
                &config.toolscaled.base_url,
                expand(&config.toolscaled.auth_file),
            );
            Runner::new(config, surface)?.start()
        }
        Command::Stop { now } => {
            let config = Config::load(&path)?;
            write_stop(
                &expand(&config.stop_file),
                if now { Stop::Now } else { Stop::Drain },
            )?;
            println!(
                "{}",
                if now {
                    "octotrigger stops the agents now and ends their claims."
                } else {
                    "octotrigger claims nothing new; it ends when the running agents are done."
                }
            );
            Ok(())
        }
        Command::Status => status(&Config::load(&path)?),
    }
}

/// The two addresses and the auth file, from the config when there is one.
struct Endpoints {
    base_url: String,
    mcp_url: String,
    auth_file: PathBuf,
}

fn load_or_defaults(path: &Path) -> Result<Endpoints> {
    let toolscaled = if path.exists() {
        Config::load(path)?.toolscaled
    } else {
        config::Toolscaled::default()
    };
    Ok(Endpoints {
        base_url: toolscaled.base_url,
        mcp_url: toolscaled.mcp_url,
        auth_file: toolscaled.auth_file,
    })
}

/// The places of the guided setup.
fn paths(config: PathBuf) -> Result<Paths> {
    let machine = std::process::Command::new("hostname")
        .arg("-s")
        .output()
        .ok()
        .and_then(|out| String::from_utf8(out.stdout).ok())
        .map(|name| name.trim().to_owned())
        .filter(|name| !name.is_empty())
        .unwrap_or_else(|| "my-machine".to_owned());
    Ok(Paths {
        config,
        hermes_home: config::hermes_home(),
        home: config::home(),
        exe: std::env::current_exe()
            .context("find this program")?
            .display()
            .to_string(),
        path_env: std::env::var("PATH").unwrap_or_else(|_| "/usr/bin:/bin".to_owned()),
        machine,
    })
}

/// The task of the check: one read of the workspace, nothing changed.
const CHECK_PROMPT: &str = "\
This is a connection check of octotrigger. Call the Toolscaled tool inspect \
once, with no arguments. Then answer with one line: the value of \
state.identity.organization_id in its answer. Call no other tool and change \
nothing.";

/// The time limit of the check's agent.
const CHECK_MINUTES: u64 = 5;

fn check(config: &Config, with_agent: bool) -> Result<()> {
    let auth_file = expand(&config.toolscaled.auth_file);
    let tokens = auth::access_token(&auth_file, false).context("the sign-in")?;
    println!(
        "Sign-in: workspace {}, scope {}.",
        tokens.organization_id, tokens.scope
    );
    // A dry claim: a workflow id that names no workflow takes nothing.
    let surface = HttpSurface::new(&config.toolscaled.base_url, auth_file);
    let nothing = vec!["00000000-0000-0000-0000-000000000000".to_owned()];
    match surface.claim(
        &nothing,
        &format!("octotrigger check on {}", config.machine),
        60,
    )? {
        None => println!("Claim call: works (a dry claim took nothing)."),
        Some(_) => bail!("the dry claim took a run; this must not happen"),
    }
    let mut failed = Vec::new();
    for job in &config.jobs {
        let program = job.agent.split_whitespace().next().unwrap_or_default();
        let found = std::process::Command::new("/bin/sh")
            .arg("-c")
            .arg(format!("command -v {program}"))
            .output()
            .is_ok_and(|out| out.status.success());
        if !found {
            println!("Job {}: agent {program} NOT FOUND on PATH.", job.name);
            failed.push(job.name.clone());
            continue;
        }
        if !with_agent {
            println!("Job {}: agent {program} found.", job.name);
            continue;
        }
        println!(
            "Job {}: starting {program} with a short task (at most {CHECK_MINUTES} minutes)...",
            job.name
        );
        match agent_check(config, job, &surface, &tokens.organization_id) {
            Ok(seconds) => println!(
                "Job {}: the agent reached Toolscaled on octotrigger's sign-in and read \
                 workspace {} in {seconds} s.",
                job.name, tokens.organization_id
            ),
            Err(error) => {
                println!("Job {}: FAILED: {error:#}", job.name);
                failed.push(job.name.clone());
            }
        }
    }
    match &config.token_warning_confirmed {
        Some(day) => println!("Token warning: confirmed on {day}."),
        None => println!("Token warning: not confirmed; run `octotrigger setup`."),
    }
    if !failed.is_empty() {
        bail!("the check of job {} failed", failed.join(", "));
    }
    Ok(())
}

/// Starts the job's agent once on [`CHECK_PROMPT`], as a run starts it: an
/// empty folder, the job's environment, a fresh Toolscaled token. Passes
/// when its answer names the workspace of the sign-in. The seconds it took.
fn agent_check(
    config: &Config,
    job: &config::Job,
    surface: &HttpSurface,
    organization_id: &str,
) -> Result<u64> {
    let seconds = i64::try_from(CHECK_MINUTES * 60 + 300).unwrap_or(i64::MAX);
    let token = surface.agent_token(seconds)?;
    let probe = Claim {
        run_id: format!("check-{}", job.name),
        workflow_id: String::new(),
        kind: "check".into(),
        code: String::new(),
        lease_until: String::new(),
        claim_token: String::new(),
        start_prompt: CHECK_PROMPT.into(),
    };
    let folder = agent::prepare(&expand(&config.work_dir), &probe)?;
    let output = expand(&config.log_dir).join(format!("check-{}.log", job.name));
    let started = Instant::now();
    let result = (|| {
        let mut child = agent::spawn(job, &folder, &output, &token)?;
        let deadline = started + Duration::from_secs(CHECK_MINUTES * 60);
        let status = loop {
            if let Some(status) = child.try_wait()? {
                break status;
            }
            if Instant::now() > deadline {
                agent::kill_group(child.id());
                let _ = child.wait();
                bail!("the agent did not end in {CHECK_MINUTES} minutes");
            }
            std::thread::sleep(Duration::from_millis(250));
        };
        let text = std::fs::read_to_string(&output).unwrap_or_default();
        if status.success() && text.contains(organization_id) {
            return Ok(started.elapsed().as_secs());
        }
        let tail: Vec<&str> = text.lines().rev().take(15).collect();
        let tail: Vec<&str> = tail.into_iter().rev().collect();
        bail!(
            "the agent {} and its answer does not name workspace {organization_id}. \
             Its last lines ({}):\n{}",
            if status.success() { "ended" } else { "failed" },
            output.display(),
            tail.join("\n")
        )
    })();
    let _ = std::fs::remove_dir_all(&folder.path);
    result
}

fn status(config: &Config) -> Result<()> {
    let state = State::load(&expand(&config.state_file))?;
    match state.loop_pid.filter(|pid| alive(*pid)) {
        Some(pid) => println!("Running: process {pid}."),
        None => println!("Not running."),
    }
    match read_stop(&expand(&config.stop_file)) {
        Some(Stop::Drain) => println!("Stop asked: no new claims."),
        Some(Stop::Now) => println!("Stop now asked."),
        None => {}
    }
    let counts_of_today = state.day == today();
    println!(
        "Today: {} runs, {} tokens reported, {} runs with no report.",
        if counts_of_today { state.runs_today } else { 0 },
        if counts_of_today {
            state.tokens_today
        } else {
            0
        },
        if counts_of_today {
            state.unreported_today
        } else {
            0
        },
    );
    if state.claims.is_empty() {
        println!("No claims held.");
    }
    for held in &state.claims {
        println!(
            "Run {} of workflow {}: job {}, since {}, agent {}.",
            held.run_id,
            held.workflow_id,
            held.job,
            held.started_at,
            if alive(held.pid) {
                "running"
            } else {
                "not running"
            }
        );
    }
    Ok(())
}
