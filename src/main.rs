use std::io::BufRead as _;
use std::path::{Path, PathBuf};

use anyhow::{Context, Result, bail};
use clap::{Parser, Subcommand};

use octotrigger::agent::alive;
use octotrigger::config::{self, Config, expand};
use octotrigger::runner::{Runner, Stop, read_stop, write_stop};
use octotrigger::state::{State, today};
use octotrigger::surface::{HttpSurface, Surface as _};
use octotrigger::{TOKEN_WARNING, auth};

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
    /// Write a starter config, and read and confirm the token warning.
    Setup,
    /// Check the sign-in, the claim call and the agent commands.
    Check,
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
        Command::Setup => setup(&path),
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
        Command::Check => check(&Config::load(&path)?),
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

fn setup(path: &Path) -> Result<()> {
    if !path.exists() {
        let machine = std::process::Command::new("hostname")
            .arg("-s")
            .output()
            .ok()
            .and_then(|out| String::from_utf8(out.stdout).ok())
            .map(|name| name.trim().to_owned())
            .filter(|name| !name.is_empty())
            .unwrap_or_else(|| "my-machine".to_owned());
        if let Some(folder) = path.parent() {
            std::fs::create_dir_all(folder)?;
        }
        std::fs::write(path, config::starter(&machine))?;
        println!(
            "Wrote a starter config to {}. Change its agent command and limits.\n",
            path.display()
        );
    }
    let config = Config::load(path)?;
    if let Some(day) = &config.token_warning_confirmed {
        println!("The token warning was confirmed on {day}.");
        return Ok(());
    }
    println!("{TOKEN_WARNING}\n");
    println!("Type yes to confirm that you read this:");
    let mut answer = String::new();
    std::io::stdin().lock().read_line(&mut answer)?;
    if answer.trim() != "yes" {
        bail!("not confirmed: octotrigger start refuses until you confirm");
    }
    confirm_warning(path, &today())?;
    println!(
        "Confirmed. Next: `octotrigger login`, `octotrigger check`, then `octotrigger start`."
    );
    Ok(())
}

/// Writes `token_warning_confirmed` into the config, keeping its comments.
fn confirm_warning(path: &Path, day: &str) -> Result<()> {
    let text = std::fs::read_to_string(path)?;
    let line = format!("token_warning_confirmed: {day}");
    let mut found = false;
    let mut lines: Vec<String> = text
        .lines()
        .map(|current| {
            if current.starts_with("token_warning_confirmed:") {
                found = true;
                line.clone()
            } else {
                current.to_owned()
            }
        })
        .collect();
    if !found {
        lines.push(line);
    }
    std::fs::write(path, lines.join("\n") + "\n")?;
    Config::load(path).map(|_| ())
}

fn check(config: &Config) -> Result<()> {
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
    for job in &config.jobs {
        let program = job.agent.split_whitespace().next().unwrap_or_default();
        let found = std::process::Command::new("/bin/sh")
            .arg("-c")
            .arg(format!("command -v {program}"))
            .output()
            .is_ok_and(|out| out.status.success());
        println!(
            "Job {}: agent {program} {}.",
            job.name,
            if found { "found" } else { "NOT FOUND on PATH" }
        );
    }
    match &config.token_warning_confirmed {
        Some(day) => println!("Token warning: confirmed on {day}."),
        None => println!("Token warning: not confirmed; run `octotrigger setup`."),
    }
    Ok(())
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
