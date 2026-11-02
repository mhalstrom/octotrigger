//! `octotrigger setup`: a guided setup from nothing to a running watcher, and
//! `octotrigger uninstall`.
//!
//! Each step checks the machine. When the step is done, setup says so and
//! goes on. When it is not, setup prints the exact command, runs it after
//! the person's yes, and checks again. A step that stays not done ends the
//! setup with what to do; the next `octotrigger setup` starts again at the
//! first step and passes the done ones.
//!
//! By default the agents run on octotrigger's own Hermes Agent, installed in
//! `~/.octotrigger/agents/hermes` with Hermes' own installer at a pinned
//! commit: no `hermes` command on the PATH, no shell file changed, and the
//! person's own `~/.hermes` never read or written. Its Toolscaled
//! connection is octotrigger's sign-in. The second choice, on the person's
//! pick only, is their own Hermes setup.
//!
//! The steps: Hermes (two steps for either choice), the model login (a
//! ChatGPT plan), the config,
//! octotrigger's sign-in to Toolscaled, the token warning, the check, and
//! how to keep the watcher running.

use std::path::{Path, PathBuf};

use anyhow::{Result, bail};

use crate::TOKEN_WARNING;
use crate::agent::alive;
use crate::auth::{self, MCP_SCOPE};
use crate::config::{
    self, AGENT_TOKEN_ENV, Agent, Config, OwnHermes, Plan, Toolscaled, expand, hermes_config,
};
use crate::state::{State, today};

/// The oldest Hermes Agent that octotrigger runs.
pub const HERMES_MIN: (u32, u32, u32) = (0, 21, 4);
/// The Hermes commit that octotrigger installs: the one tested with
/// Toolscaled (Hermes Agent 0.21.7, 2026-09-24).
pub const HERMES_PIN: &str = "71d34bad1045610252eda02ee79549436896e784";
/// Hermes' own install command (its README), for a person's own setup.
pub const HERMES_INSTALL: &str =
    "curl -fsSL https://hermes-agent.nousresearch.com/install.sh | bash";
/// The Hermes plugin that signs in with a Claude plan. Anthropic does not
/// permit a third-party tool to route requests through a Claude plan's
/// sign-in, so setup offers no Claude plan and names the plugin in a job of
/// an older config.
const CLAUDE_PLAN_PLUGIN: &str = "claude-subscription-directsdk";

/// Why setup offers no Claude plan, with the words of Anthropic's page.
const NO_CLAUDE_PLAN: &str = "A Claude plan is not offered: Anthropic's terms for Claude Code \
     say \"Anthropic does not permit third-party developers to offer Claude.ai login into their \
     own applications, or to route requests through Free, Pro, Max, or Ultra plan credentials \
     on behalf of their users\" (code.claude.com/docs/en/legal-and-compliance).";
/// Hermes' own install command on Windows (its README: "Windows (native,
/// PowerShell)").
pub const HERMES_INSTALL_WINDOWS: &str =
    "iex (irm https://hermes-agent.nousresearch.com/install.ps1)";
/// The name of the Task Scheduler task on Windows.
pub const TASK_NAME: &str = "octotrigger";
/// The label of the launchd file.
pub const LAUNCHD_LABEL: &str = "com.octotrigger.watcher";

/// What a command printed, and whether it exited 0.
#[derive(Clone, Debug, Default)]
pub struct Output {
    pub ok: bool,
    pub text: String,
}

/// The operating systems with a way to start octotrigger at login. The
/// command lines that setup prints and runs are `sh` lines, and on Windows
/// PowerShell lines.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Os {
    MacOs,
    Linux,
    Windows,
    Other,
}

impl Os {
    #[must_use]
    pub const fn this() -> Self {
        if cfg!(target_os = "macos") {
            Self::MacOs
        } else if cfg!(target_os = "linux") {
            Self::Linux
        } else if cfg!(windows) {
            Self::Windows
        } else {
            Self::Other
        }
    }

    /// A word quoted for this system's shell: `sh`, or PowerShell, where a
    /// word in single quotes is plain text.
    #[must_use]
    pub fn quote(self, word: &str) -> String {
        if self == Self::Windows {
            format!("'{}'", word.replace('\'', "''"))
        } else {
            shell_quote(word)
        }
    }

    /// A program at the start of a command line: PowerShell runs a quoted
    /// path only after `&`.
    #[must_use]
    pub fn program(self, path: &str) -> String {
        if self == Self::Windows {
            format!("& {}", self.quote(path))
        } else {
            shell_quote(path)
        }
    }
}

/// What setup needs of the machine and the person, so a test can stand in.
pub trait Machine {
    /// Runs a program with extra environment names and reads its output;
    /// `None` when it cannot start (not installed).
    fn output(&self, program: &str, args: &[&str], env: &[(&str, &str)]) -> Option<Output>;
    /// Runs a shell command line on the person's terminal; whether it
    /// exited 0.
    fn run(&mut self, line: &str) -> bool;
    /// Asks the person and reads one line.
    fn ask(&mut self, question: &str) -> String;
    /// Shows a line to the person.
    fn say(&mut self, text: &str);
    fn os(&self) -> Os;
    /// The user id, for `launchctl` (`gui/<uid>`).
    fn uid(&self) -> u32;
}

/// The real machine: the terminal and the programs on the PATH.
pub struct Terminal;

impl Machine for Terminal {
    fn output(&self, program: &str, args: &[&str], env: &[(&str, &str)]) -> Option<Output> {
        let out = std::process::Command::new(program)
            .args(args)
            .envs(env.iter().copied())
            .stdin(std::process::Stdio::null())
            .output()
            .ok()?;
        let mut text = String::from_utf8_lossy(&out.stdout).into_owned();
        text.push_str(&String::from_utf8_lossy(&out.stderr));
        Some(Output {
            ok: out.status.success(),
            text,
        })
    }

    fn run(&mut self, line: &str) -> bool {
        crate::platform::shell(line)
            .status()
            .is_ok_and(|status| status.success())
    }

    fn ask(&mut self, question: &str) -> String {
        use std::io::{BufRead as _, Write as _};
        print!("{question} ");
        let _ = std::io::stdout().flush();
        let mut answer = String::new();
        let _ = std::io::stdin().lock().read_line(&mut answer);
        answer.trim().to_owned()
    }

    fn say(&mut self, text: &str) {
        println!("{text}");
    }

    fn os(&self) -> Os {
        Os::this()
    }

    fn uid(&self) -> u32 {
        #[cfg(unix)]
        {
            // SAFETY: getuid reads the process's user id and cannot fail.
            unsafe { libc::getuid() }
        }
        #[cfg(not(unix))]
        {
            0
        }
    }
}

/// The places setup reads and writes.
#[derive(Clone, Debug)]
pub struct Paths {
    /// octotrigger's config.
    pub config: PathBuf,
    /// octotrigger's own Hermes.
    pub own: OwnHermes,
    /// The person's home folder (service files).
    pub home: PathBuf,
    /// This program, as the service file and the printed commands name it.
    pub exe: String,
    /// The PATH the service file gives the watcher, so it finds the agent.
    pub path_env: String,
    /// The machine's short name, for a new config.
    pub machine: String,
    /// The Windows account (`DOMAIN\\USER`) that the Task Scheduler task
    /// runs as; empty elsewhere.
    pub user: String,
}

/// The Hermes that setup works on: its command and its home.
struct Hermes {
    program: String,
    /// `None`: the person's own default home (`~/.hermes`).
    home: Option<String>,
}

impl Hermes {
    fn of(agent: Agent, own: &OwnHermes) -> Self {
        match agent {
            Agent::Own => Self {
                program: own.program().display().to_string(),
                home: Some(own.home().display().to_string()),
            },
            Agent::Mine => Self {
                program: "hermes".to_owned(),
                home: None,
            },
        }
    }

    /// The command line of `args`, with this Hermes and its home.
    fn line(&self, os: Os, args: &str) -> String {
        match &self.home {
            Some(home) if os == Os::Windows => format!(
                "$env:HERMES_HOME = {}; {} {args}",
                os.quote(home),
                os.program(&self.program)
            ),
            Some(home) => format!(
                "HERMES_HOME={} {} {args}",
                shell_quote(home),
                shell_quote(&self.program)
            ),
            None => format!("hermes {args}"),
        }
    }

    fn output(&self, m: &impl Machine, args: &[&str]) -> Option<Output> {
        match &self.home {
            Some(home) => m.output(&self.program, args, &[("HERMES_HOME", home)]),
            None => m.output(&self.program, args, &[]),
        }
    }

    fn version(&self, m: &impl Machine) -> Option<(u32, u32, u32)> {
        self.output(m, &["--version"])
            .filter(|out| out.ok)
            .and_then(|out| parse_version(&out.text))
    }

    fn codex_login(&self, m: &impl Machine) -> bool {
        self.output(m, &["auth", "list"])
            .is_some_and(|out| out.ok && has_codex_login(&out.text))
    }
}

/// How a step ended.
enum Step {
    Done,
    /// Not done: setup ends here.
    Stop,
}

/// What the person decided before the setup, for a run with no terminal
/// (a chat that runs the commands for the person, #4165).
#[derive(Clone, Copy, Debug, Default)]
pub struct Choices {
    /// Yes to each "Run it now?" and "Write ...?" question, and the default
    /// of each choice that the flags and the config leave open.
    pub yes: bool,
    pub agent: Option<Agent>,
    pub plan: Option<Plan>,
    /// The person read the token warning and confirms it.
    pub accept_token_warning: bool,
}

/// The guided setup; whether it got to the end.
///
/// # Errors
///
/// A file that cannot be written.
pub fn run(machine: &mut impl Machine, paths: &Paths, choices: Choices) -> Result<bool> {
    Setup {
        m: machine,
        p: paths,
        c: choices,
    }
    .go()
}

/// `octotrigger uninstall`: stops the watcher's service and removes its
/// file, removes octotrigger's own Hermes after a yes, and with `all` also
/// octotrigger's config, sign-in and logs after a second yes. Whether it got
/// to the end.
///
/// # Errors
///
/// A file or folder that cannot be removed.
pub fn uninstall(machine: &mut impl Machine, paths: &Paths, all: bool) -> Result<bool> {
    Setup {
        m: machine,
        p: paths,
        c: Choices::default(),
    }
    .uninstall(all)
}

struct Setup<'a, M: Machine> {
    m: &'a mut M,
    p: &'a Paths,
    c: Choices,
}

const STEPS: usize = 8;

impl<M: Machine> Setup<'_, M> {
    fn go(&mut self) -> Result<bool> {
        self.m
            .say("octotrigger setup: each step checks your machine first.\n");
        let agent = self.agent();
        let steps = match agent {
            Agent::Own => {
                matches!(self.own_home()?, Step::Done) && matches!(self.own_install(), Step::Done)
            }
            Agent::Mine => {
                matches!(self.mine_install(), Step::Done)
                    && matches!(self.mine_connection(), Step::Done)
            }
        };
        if !steps {
            return Ok(false);
        }
        let hermes = Hermes::of(agent, &self.p.own);
        let plan = self.plan();
        if matches!(self.model_login(plan, &hermes), Step::Stop) {
            return Ok(false);
        }
        if matches!(self.config(plan, agent)?, Step::Stop) {
            return Ok(false);
        }
        let config = Config::load(&self.p.config)?;
        if matches!(self.sign_in(&config.toolscaled), Step::Stop) {
            return Ok(false);
        }
        if matches!(self.warning(&config)?, Step::Stop) {
            return Ok(false);
        }
        if matches!(self.check(), Step::Stop) {
            return Ok(false);
        }
        self.keep_running(&config)?;
        Ok(true)
    }

    fn title(&mut self, n: usize, text: &str) {
        self.m.say(&format!("[{n}/{STEPS}] {text}"));
    }

    fn done(&mut self, text: &str) -> Step {
        self.m.say(&format!("  Done: {text}\n"));
        Step::Done
    }

    fn yes(&mut self, question: &str, default_yes: bool) -> bool {
        if self.c.yes {
            self.m.say(&format!("  {question} yes (--yes)"));
            return true;
        }
        let answer = self
            .m
            .ask(&format!(
                "  {question} [{}]",
                if default_yes { "Y/n" } else { "y/N" }
            ))
            .to_ascii_lowercase();
        match answer.as_str() {
            "" => default_yes,
            "y" | "yes" => true,
            _ => false,
        }
    }

    /// Prints the command, runs it after a yes, and checks again with
    /// `check`. `Stop` when the person says no or the step stays not done.
    fn fix(
        &mut self,
        not_done: &str,
        command: &str,
        check: impl Fn(&Self) -> Option<String>,
    ) -> Step {
        self.m.say(&format!("  Not done: {not_done}"));
        self.m.say(&format!("  Run: {command}"));
        if !self.yes("Run it now?", true) {
            self.m.say("  Run it, then run `octotrigger setup` again.");
            return Step::Stop;
        }
        let ran = self.m.run(command);
        match check(self) {
            Some(text) => self.done(&text),
            None => {
                self.m.say(&format!(
                    "  Still not done{}. Do the step, then run `octotrigger setup` again.",
                    if ran { "" } else { " (the command failed)" }
                ));
                Step::Stop
            }
        }
    }

    /// The choice of Hermes: from the config when it has a Hermes job, else
    /// the person's pick, octotrigger's own by default.
    fn agent(&mut self) -> Agent {
        let from_config = Config::load(&self.p.config).ok().and_then(|config| {
            config
                .jobs
                .iter()
                .find_map(|job| Agent::of_job(job, &self.p.own))
        });
        if let Some(agent) = from_config.or(self.c.agent) {
            return agent;
        }
        if self.c.yes {
            return Agent::Own;
        }
        self.m.say("Which Hermes Agent runs your agents?");
        self.m.say(&format!(
            "  1  octotrigger's own, installed in {} (recommended). Your own Hermes, if you \
             have one, is not read or changed.",
            home_relative(&self.p.own.root, &self.p.home)
        ));
        self.m.say(
            "  2  My own Hermes setup: the `hermes` on my PATH, with ~/.hermes and its own \
             Toolscaled connection.",
        );
        loop {
            match self.m.ask("  Type 1 or 2 [1]").as_str() {
                "" | "1" => return Agent::Own,
                "2" => return Agent::Mine,
                _ => {}
            }
        }
    }

    // 1, octotrigger's own Hermes. Before the install: the installer keeps
    // a config it finds, so `cli.expose_on_path: false` holds from the start.
    fn own_home(&mut self) -> Result<Step> {
        self.title(1, "octotrigger's own Hermes home");
        let file = self.p.own.home().join("config.yaml");
        match std::fs::read_to_string(&file).ok() {
            Some(text) if text.contains(&format!("${{{AGENT_TOKEN_ENV}}}")) => {
                Ok(self.done(&format!(
                    "{} names Toolscaled on octotrigger's sign-in.",
                    file.display()
                )))
            }
            Some(_) => {
                self.m.say(&format!(
                    "  Not done: {} has no Toolscaled server on octotrigger's sign-in.",
                    file.display()
                ));
                self.m.say(&format!(
                    "  Add this to it, then run `octotrigger setup` again:\n\n{}",
                    hermes_config(&self.mcp_url()?)
                ));
                Ok(Step::Stop)
            }
            None => {
                write_private(&file, &hermes_config(&self.mcp_url()?))?;
                Ok(self.done(&format!(
                    "wrote {}: Toolscaled on octotrigger's sign-in (a fresh token for each \
                     run), a model login of its own, no memory across runs, no `hermes` on \
                     your PATH.",
                    file.display()
                )))
            }
        }
    }

    fn mcp_url(&self) -> Result<String> {
        Ok(if self.p.config.exists() {
            Config::load(&self.p.config)?.toolscaled.mcp_url
        } else {
            Toolscaled::default().mcp_url
        })
    }

    // 2, octotrigger's own Hermes.
    fn own_install(&mut self) -> Step {
        self.title(2, "octotrigger's own Hermes Agent");
        let hermes = Hermes::of(Agent::Own, &self.p.own);
        let ready = |me: &Self| {
            hermes
                .version(&*me.m)
                .filter(|v| *v >= HERMES_MIN)
                .map(|v| {
                    format!(
                        "Hermes Agent {} in {}.",
                        show_version(v),
                        home_relative(&me.p.own.root, &me.p.home)
                    )
                })
        };
        if let Some(text) = ready(self) {
            return self.done(&text);
        }
        self.m.say(if self.m.os() == Os::Windows {
            "  Hermes' own installer, at the commit tested with Toolscaled, into this one \
             folder. It takes about 2 GB and a few minutes. It says that it added `hermes` to \
             your PATH: the same command takes that entry out again, because octotrigger calls \
             this Hermes by its path. Setup goes on by itself."
        } else {
            "  Hermes' own installer, at the commit tested with Toolscaled, into this one \
             folder. It takes about 2 GB and a few minutes. Its last line asks you to reload \
             your shell (`source ~/.zshrc`): that is not for this Hermes, which octotrigger \
             calls by its path. Setup goes on by itself."
        });
        let command = own_install_command(self.m.os(), &self.p.own);
        self.fix(
            "octotrigger's own Hermes Agent is not installed.",
            &command,
            ready,
        )
    }

    // 1, the person's own Hermes.
    fn mine_install(&mut self) -> Step {
        self.title(1, "Your Hermes Agent, 0.21.4 or newer");
        let hermes = Hermes::of(Agent::Mine, &self.p.own);
        let ready = |me: &Self| {
            hermes
                .version(&*me.m)
                .filter(|v| *v >= HERMES_MIN)
                .map(|v| format!("Hermes Agent {}.", show_version(v)))
        };
        if let Some(text) = ready(self) {
            return self.done(&text);
        }
        let install = if self.m.os() == Os::Windows {
            HERMES_INSTALL_WINDOWS
        } else {
            HERMES_INSTALL
        };
        let (not_done, command) = match hermes.version(&*self.m) {
            Some(v) => (
                format!("Hermes Agent {} is older than 0.21.4.", show_version(v)),
                "hermes update",
            ),
            None => ("Hermes Agent is not installed.".to_owned(), install),
        };
        let step = self.fix(&not_done, command, ready);
        if matches!(step, Step::Stop) && command == install {
            self.m.say(
                "  When the install says it is done, open a new terminal, so that `hermes` is \
                 on your PATH.",
            );
        }
        step
    }

    // 2, the person's own Hermes.
    fn mine_connection(&mut self) -> Step {
        self.title(2, "Your Hermes' Toolscaled connection");
        self.m.say(
            "  The agents use your own Hermes' Toolscaled login. Toolscaled cannot tell their \
             calls from your own Hermes chats, so a chat of yours during a run is held to the \
             run's limits.",
        );
        let connected = |me: &Self| {
            me.m.output("hermes", &["mcp", "test", "toolscaled"], &[])
                .filter(|out| out.ok && out.text.contains("Connected"))
                .map(|_| "your Hermes connects to Toolscaled.".to_owned())
        };
        if let Some(text) = connected(self) {
            return self.done(&text);
        }
        let command = format!(
            "hermes mcp add toolscaled --url {} --auth oauth",
            Toolscaled::default().mcp_url
        );
        self.fix(
            "your Hermes has no working Toolscaled server named toolscaled.",
            &command,
            connected,
        )
    }

    fn plan(&mut self) -> Plan {
        let config = Config::load(&self.p.config).ok();
        // A job of an older config that signs in with a Claude plan.
        if config.as_ref().is_some_and(|config| {
            config
                .jobs
                .iter()
                .any(|job| job.agent.contains(CLAUDE_PLAN_PLUGIN))
        }) {
            self.m.say(&format!(
                "  Your config has a job on a Claude plan. {NO_CLAUDE_PLAN} Change the job's \
                 agent to a ChatGPT plan or to your own API key."
            ));
        }
        let from_config = config.and_then(|config| {
            config
                .jobs
                .iter()
                .find_map(|job| Plan::of_command(&job.agent))
        });
        if let Some(plan) = from_config.or(self.c.plan) {
            return plan;
        }
        self.m.say(
            "The agent's model: GPT-6.1 Terra on your ChatGPT plan (tested with octotrigger).",
        );
        self.m.say(&format!("  {NO_CLAUDE_PLAN}"));
        Plan::ChatGpt
    }

    // 3
    fn model_login(&mut self, plan: Plan, hermes: &Hermes) -> Step {
        match plan {
            Plan::ChatGpt => {
                self.title(3, "The ChatGPT login of the agents' Hermes");
                if hermes.codex_login(&*self.m) {
                    return self.done("Hermes has an openai-codex login.");
                }
                if hermes.home.is_some() {
                    self.m.say(
                        "  Give octotrigger's Hermes a ChatGPT login of its own: two programs \
                         on one login can log each other out.",
                    );
                }
                self.m.say("  Type the code it shows in your browser.");
                let command = hermes.line(self.m.os(), "auth add openai-codex");
                self.fix("Hermes has no openai-codex login.", &command, |me| {
                    hermes
                        .codex_login(&*me.m)
                        .then(|| "Hermes has an openai-codex login.".to_owned())
                })
            }
        }
    }

    // 4
    fn config(&mut self, plan: Plan, agent: Agent) -> Result<Step> {
        self.title(4, "octotrigger's config");
        if !self.p.config.exists() {
            write_private(
                &self.p.config,
                &config::starter(&self.p.machine, plan, agent),
            )?;
            return Ok(self.done(&format!(
                "wrote {}: one job, {}, one run at once, 20 runs a day. Change it when you like.",
                self.p.config.display(),
                match plan {
                    Plan::ChatGpt => "GPT-6.1 Terra on your ChatGPT plan",
                }
            )));
        }
        let config = match Config::load(&self.p.config) {
            Ok(config) => config,
            Err(error) => {
                self.m.say(&format!("  Not done: {error:#}"));
                self.m
                    .say("  Correct the config, then run `octotrigger setup` again.");
                return Ok(Step::Stop);
            }
        };
        if agent == Agent::Own {
            let missing: Vec<&str> = config
                .jobs
                .iter()
                .filter(|job| Agent::of_job(job, &self.p.own) == Some(Agent::Mine))
                .map(|job| job.name.as_str())
                .collect();
            if !missing.is_empty() {
                self.m.say(&format!(
                    "  Not done: job {} runs a Hermes without octotrigger's Hermes home, so the \
                     agent has no Toolscaled connection.",
                    missing.join(", ")
                ));
                self.m.say(&format!(
                    "  In each such job, run `{}` with these lines, then run `octotrigger setup` \
                     again:\n\n    env:\n      HERMES_HOME: {}\n",
                    home_relative(&self.p.own.program(), &self.p.home),
                    home_relative(&self.p.own.home(), &self.p.home)
                ));
                return Ok(Step::Stop);
            }
        }
        Ok(self.done(&format!("{} reads and checks.", self.p.config.display())))
    }

    // 5
    fn sign_in(&mut self, toolscaled: &Toolscaled) -> Step {
        self.title(5, "octotrigger's sign-in to Toolscaled");
        let file = expand(&toolscaled.auth_file);
        let login = self.octotrigger("login");
        let signed_in = |file: &Path| {
            auth::load(file)
                .ok()
                .filter(|tokens| {
                    tokens.scope.split_whitespace().any(|s| s == MCP_SCOPE)
                        && tokens.resource == toolscaled.mcp_url
                })
                .map(|tokens| match &tokens.organization_name {
                    Some(_) => format!(
                        "signed in to workspace {}. To use another workspace, sign in again: \
                         {login}",
                        tokens.workspace(),
                    ),
                    None => format!(
                        "signed in to workspace {id}: its page {base}/org/{id}/workflows shows \
                         its name. To use another workspace, sign in again: {login}",
                        id = tokens.organization_id,
                        login = login,
                        base = toolscaled.base_url.trim_end_matches('/'),
                    ),
                })
        };
        if let Some(text) = signed_in(&file) {
            return self.done(&text);
        }
        self.m.say(
            "  Your browser opens; sign in and approve. With octotrigger's own Hermes, this \
             one sign-in also serves the agents: before each run, octotrigger gives the agent \
             a fresh Toolscaled token.",
        );
        self.fix(
            "octotrigger is not signed in to Toolscaled.",
            &login,
            |_| signed_in(&file),
        )
    }

    fn octotrigger(&self, command: &str) -> String {
        let os = self.m.os();
        if self.p.config == config::default_path() {
            format!("{} {command}", os.program(&self.p.exe))
        } else {
            format!(
                "{} --config {} {command}",
                os.program(&self.p.exe),
                os.quote(&self.p.config.display().to_string())
            )
        }
    }

    // 6
    fn warning(&mut self, config: &Config) -> Result<Step> {
        self.title(6, "The token warning");
        if let Some(day) = &config.token_warning_confirmed {
            return Ok(self.done(&format!("you confirmed it on {day}.")));
        }
        self.m.say(&format!("\n{TOKEN_WARNING}\n"));
        if self.c.accept_token_warning {
            self.m.say("  Confirmed with --accept-token-warning.");
            confirm_warning(&self.p.config, &today())?;
            return Ok(self.done("confirmed."));
        }
        if self.c.yes {
            self.m.say(
                "  Not confirmed: --yes does not confirm it. When the person has read it, \
                 run setup again with --accept-token-warning.",
            );
            return Ok(Step::Stop);
        }
        if self.m.ask("  Type yes to confirm that you read this:") != "yes" {
            self.m
                .say("  Not confirmed: octotrigger starts no agent until you confirm it.");
            return Ok(Step::Stop);
        }
        confirm_warning(&self.p.config, &today())?;
        Ok(self.done("confirmed."))
    }

    // 7
    fn check(&mut self) -> Step {
        self.title(7, "The check");
        self.m.say(
            "  It makes one claim call that takes no run, and starts each job's agent once \
             with a short task: read your workspace in Toolscaled. That is one small model \
             call on your plan.",
        );
        let command = self.octotrigger("check");
        self.m.say(&format!("  Run: {command}"));
        if !self.yes("Run it now?", true) {
            self.m.say("  Run it, then run `octotrigger setup` again.");
            return Step::Stop;
        }
        if self.m.run(&command) {
            return self.done("octotrigger and its agents reach Toolscaled.");
        }
        self.m
            .say("  The check failed. Do what it says, then run `octotrigger setup` again.");
        Step::Stop
    }

    /// How this machine starts octotrigger at login: the files to write
    /// (the first one shows that it is set up), the commands that start and
    /// stop it, and the service manager's name.
    fn service(&self, config: &Config) -> Option<Service> {
        match self.m.os() {
            Os::MacOs => {
                let file = self
                    .p
                    .home
                    .join(format!("Library/LaunchAgents/{LAUNCHD_LABEL}.plist"));
                Some(Service {
                    files: vec![(
                        file.clone(),
                        launchd_plist(
                            &self.p.exe,
                            &self.p.config,
                            &self.p.path_env,
                            &expand(&config.log_dir),
                        )
                        .into_bytes(),
                    )],
                    load: format!(
                        "launchctl bootstrap gui/{} {}",
                        self.m.uid(),
                        shell_quote(&file.display().to_string())
                    ),
                    unload: format!("launchctl bootout gui/{}/{LAUNCHD_LABEL}", self.m.uid()),
                    name: "launchd",
                })
            }
            Os::Linux => Some(Service {
                files: vec![(
                    self.p.home.join(".config/systemd/user/octotrigger.service"),
                    systemd_unit(&self.p.exe, &self.p.config, &self.p.path_env).into_bytes(),
                )],
                load: "systemctl --user daemon-reload && systemctl --user enable --now \
                       octotrigger.service"
                    .to_owned(),
                unload: "systemctl --user disable --now octotrigger.service".to_owned(),
                name: "systemd",
            }),
            Os::Windows => {
                let folder = self.p.home.join(".octotrigger");
                let launcher = folder.join("octotrigger.vbs");
                let task = folder.join("octotrigger.task.xml");
                let q = |path: &Path| Os::Windows.quote(&path.display().to_string());
                Some(Service {
                    files: vec![
                        (task.clone(), utf16_file(&task_xml(&launcher, &self.p.user))),
                        (
                            launcher.clone(),
                            windows_launcher(&self.p.exe, &self.p.config).into_bytes(),
                        ),
                    ],
                    load: format!(
                        "schtasks /Create /F /TN {TASK_NAME} /XML {xml} /RU {user} /NP /IT; \
                         if ($LASTEXITCODE -ne 0) {{ schtasks /Create /F /TN {TASK_NAME} /XML {xml} }}; \
                         if ($LASTEXITCODE -ne 0) {{ exit 1 }}; \
                         schtasks /Run /TN {TASK_NAME}; exit $LASTEXITCODE",
                        xml = q(&task),
                        user = Os::Windows.quote(&self.p.user),
                    ),
                    unload: format!(
                        "schtasks /Delete /F /TN {TASK_NAME}; Remove-Item -ErrorAction \
                         SilentlyContinue (Join-Path ([Environment]::GetFolderPath('Startup')) \
                         'octotrigger.vbs')"
                    ),
                    name: "Task Scheduler",
                })
            }
            Os::Other => None,
        }
    }

    // 8
    fn keep_running(&mut self, config: &Config) -> Result<()> {
        self.title(8, "Start the watcher, and keep it running");
        let start = self.octotrigger("start");
        let Some(Service {
            files,
            load,
            unload,
            name,
        }) = self.service(config)
        else {
            self.m.say(&format!("  Start it: {start}"));
            return Ok(());
        };
        let marker = files[0].0.clone();
        if marker.exists() {
            self.done(&format!(
                "{name} starts octotrigger at login and after a crash ({}). To turn that off: \
                 {unload}",
                marker.display()
            ));
        } else if self.yes(
            &format!(
                "Write a {name} file, so that octotrigger starts at login and again after a \
                 crash?"
            ),
            true,
        ) {
            for (file, bytes) in &files {
                write_private_bytes(file, bytes)?;
                self.m.say(&format!("  Wrote {}.", file.display()));
            }
            self.m.say(&format!("  Run: {load}"));
            if self.yes("Start it now?", true) && self.m.run(&load) {
                self.done(&format!(
                    "{name} runs octotrigger. To turn that off: {unload}"
                ));
            } else if self.m.os() == Os::Windows {
                self.startup_folder(&files[1].0);
            } else {
                self.m.say("  Run that command to start it.\n");
            }
            if self.m.os() == Os::Linux {
                self.m.say(
                    "  To keep it running when you are not signed in: loginctl enable-linger \
                     $USER",
                );
            }
        } else {
            self.m
                .say(&format!("  Start it in this terminal: {start}\n"));
        }
        self.m.say(&format!(
            "See what it does: {}. Stop it: {} (running agents finish) or `stop --now`.",
            self.octotrigger("status"),
            self.octotrigger("stop")
        ));
        Ok(())
    }

    /// Windows, when Task Scheduler refuses the task (a policy of the
    /// computer): the launcher in the person's Startup folder starts
    /// octotrigger at sign-in instead, with no start again after a crash.
    fn startup_folder(&mut self, launcher: &Path) {
        let command = format!(
            "Copy-Item {} -Destination ([Environment]::GetFolderPath('Startup'))",
            Os::Windows.quote(&launcher.display().to_string())
        );
        self.m.say(
            "  Task Scheduler did not take the task. Your Startup folder can start octotrigger \
             at sign-in instead (not again after a crash).",
        );
        self.m.say(&format!("  Run: {command}"));
        if self.yes("Put it in your Startup folder?", true) && self.m.run(&command) {
            self.done(&format!(
                "your Startup folder starts octotrigger at sign-in. Start it now: {}",
                self.octotrigger("start")
            ));
        } else {
            self.m
                .say("  Run that command to start octotrigger at sign-in.\n");
        }
    }

    fn uninstall(&mut self, all: bool) -> Result<bool> {
        let config = Config::load(&self.p.config).ok();
        if let Some(config) = &config
            && let Some(Service {
                files,
                unload,
                name,
                ..
            }) = self.service(config)
            && files[0].0.exists()
        {
            self.m.say(&format!("Stop {name}'s octotrigger: {unload}"));
            self.m.run(&unload);
            for (file, _) in &files {
                if file.exists() {
                    std::fs::remove_file(file)?;
                    self.m.say(&format!("Removed {}.", file.display()));
                }
            }
        }
        if let Some(config) = &config {
            let state = State::load(&expand(&config.state_file)).unwrap_or_default();
            if let Some(pid) = state.loop_pid.filter(|pid| alive(*pid)) {
                self.m.say(&format!(
                    "octotrigger still runs (process {pid}). Run `{}` first, then uninstall \
                     again.",
                    self.octotrigger("stop --now")
                ));
                return Ok(false);
            }
        }
        let root = &self.p.own.root;
        if root.exists() {
            if self.yes(
                &format!(
                    "Remove {} (octotrigger's own Hermes, its model login and its history, \
                     about 2 GB)?",
                    root.display()
                ),
                false,
            ) {
                std::fs::remove_dir_all(root)?;
                // The folders above it, when they hold nothing else.
                for folder in root.ancestors().skip(1).take(2) {
                    if folder.starts_with(&self.p.home) && folder != self.p.home {
                        let _ = std::fs::remove_dir(folder);
                    }
                }
                self.m.say(&format!("Removed {}.", root.display()));
            } else {
                self.m.say(&format!("Kept {}.", root.display()));
            }
        }
        if all {
            let mut folders: Vec<PathBuf> = Vec::new();
            let mut add = |path: PathBuf| {
                // Only folders of octotrigger's own, never a folder such as ~.
                if path.components().any(|c| c.as_os_str() == "octotrigger")
                    && path.exists()
                    && !folders.iter().any(|f| path.starts_with(f))
                {
                    folders.retain(|f| !f.starts_with(&path));
                    folders.push(path);
                }
            };
            if let Some(folder) = self.p.config.parent() {
                add(folder.to_path_buf());
            }
            let config = config.unwrap_or_else(|| {
                serde_yaml::from_str(&config::starter("m", Plan::ChatGpt, Agent::Own))
                    .expect("the starter config reads")
            });
            for path in [
                expand(&config.toolscaled.auth_file),
                expand(&config.state_file),
                expand(&config.stop_file),
            ] {
                if let Some(folder) = path.parent() {
                    add(folder.to_path_buf());
                }
            }
            add(expand(&config.log_dir));
            add(expand(&config.work_dir));
            if !folders.is_empty() {
                let names: Vec<String> = folders.iter().map(|f| f.display().to_string()).collect();
                if self.yes(
                    &format!(
                        "Also remove octotrigger's config, sign-in and logs ({})?",
                        names.join(", ")
                    ),
                    false,
                ) {
                    for folder in &folders {
                        std::fs::remove_dir_all(folder)?;
                    }
                    self.m.say(&format!("Removed {}.", names.join(", ")));
                    self.m.say(
                        "The sign-in ends on Toolscaled's side by itself; you can also revoke it \
                         on the website.",
                    );
                }
            }
        }
        let program = if self.m.os() == Os::Windows {
            let folder = Path::new(&self.p.exe)
                .parent()
                .map_or_else(|| self.p.exe.clone(), |f| f.display().to_string());
            format!(
                "Remove-Item -Recurse -Force {q}; {}",
                remove_from_user_path(&folder),
                q = Os::Windows.quote(&folder)
            )
        } else {
            "cargo uninstall octotrigger".to_owned()
        };
        self.m
            .say(&format!("To remove the program itself: {program}"));
        Ok(true)
    }
}

/// The command that installs octotrigger's own Hermes: Hermes' installer at
/// the pinned commit, its code and home in the one folder, with no setup
/// questions, no browser tools and no computer-use driver. On Unix its `HOME`
/// is there too (its shell lines go to that folder, not to the person's
/// shell files). On Windows the installer (`install.ps1`) always adds its
/// `bin` folder to the person's PATH, so the same command takes that entry
/// out again.
#[must_use]
pub fn own_install_command(os: Os, own: &OwnHermes) -> String {
    if os == Os::Windows {
        let q = |path: PathBuf| os.quote(&path.display().to_string());
        return format!(
            "& ([scriptblock]::Create((irm \
             https://raw.githubusercontent.com/NousResearch/hermes-agent/{HERMES_PIN}/scripts/install.ps1))) \
             -Commit {HERMES_PIN} -InstallDir {code} -HermesHome {home} -NonInteractive \
             -SkipBrowser -SkipComputerUse; {path}",
            code = q(own.code()),
            home = q(own.home()),
            path = remove_from_user_path(&own.home().join("bin").display().to_string()),
        );
    }
    let q = |path: PathBuf| shell_quote(&path.display().to_string());
    format!(
        "mkdir -p {installer_home} && curl -fsSL \
         https://raw.githubusercontent.com/NousResearch/hermes-agent/{HERMES_PIN}/scripts/install.sh \
         | HOME={installer_home} bash -s -- --dir {code} --hermes-home {home} \
         --commit {HERMES_PIN} --non-interactive --skip-browser --skip-computer-use",
        installer_home = q(own.installer_home()),
        code = q(own.code()),
        home = q(own.home()),
    )
}

/// Hermes' version from `hermes --version`: the first `x.y.z` of its first
/// line ("Hermes Agent v0.21.6+777.g71d34ba (2026.9.24) · stable").
#[must_use]
pub fn parse_version(text: &str) -> Option<(u32, u32, u32)> {
    let line = text.lines().find(|line| line.contains("Hermes"))?;
    line.split(|c: char| !(c.is_ascii_digit() || c == '.'))
        .find_map(|word| {
            let mut parts = word.split('.');
            let major = parts.next()?.parse().ok()?;
            let minor = parts.next()?.parse().ok()?;
            let patch = parts.next()?.parse().ok()?;
            Some((major, minor, patch))
        })
}

fn show_version((major, minor, patch): (u32, u32, u32)) -> String {
    format!("{major}.{minor}.{patch}")
}

/// Whether `hermes auth list` shows an `openai-codex` login of kind oauth.
#[must_use]
pub fn has_codex_login(text: &str) -> bool {
    let mut in_codex = false;
    for line in text.lines() {
        if !line.starts_with(char::is_whitespace) && !line.trim().is_empty() {
            in_codex = line.starts_with("openai-codex ");
        } else if in_codex && line.split_whitespace().any(|word| word == "oauth") {
            return true;
        }
    }
    false
}

/// A PowerShell line that takes `folder` out of the person's PATH (the
/// user part, which a program may change without an administrator).
fn remove_from_user_path(folder: &str) -> String {
    format!(
        "[Environment]::SetEnvironmentVariable('Path', (([Environment]::GetEnvironmentVariable(\
         'Path', 'User') -split ';' | Where-Object {{ $_ -and $_.TrimEnd('\\') -ne {} }}) -join \
         ';'), 'User')",
        Os::Windows.quote(folder.trim_end_matches('\\'))
    )
}

/// How a machine starts octotrigger at login.
struct Service {
    /// The files to write; the first one shows that it is set up.
    files: Vec<(PathBuf, Vec<u8>)>,
    load: String,
    unload: String,
    name: &'static str,
}

/// A text as a UTF-16 file with its byte order mark, as Task Scheduler
/// reads a task file.
fn utf16_file(text: &str) -> Vec<u8> {
    let mut bytes = vec![0xFF, 0xFE];
    for unit in text.replace('\n', "\r\n").encode_utf16() {
        bytes.extend_from_slice(&unit.to_le_bytes());
    }
    bytes
}

/// The Task Scheduler task on Windows: at the person's sign-in (30 s
/// later), with the person's rights only, no time limit, on battery too, and
/// again a minute after a failure. It runs the launcher with `wscript`, which
/// has no window (as Hermes Agent starts its own gateway).
#[must_use]
pub fn task_xml(launcher: &Path, user: &str) -> String {
    format!(
        r#"<?xml version="1.0" encoding="UTF-16"?>
<Task version="1.4" xmlns="http://schemas.microsoft.com/windows/2004/02/mit/task">
  <RegistrationInfo>
    <Description>octotrigger: claims Toolscaled runs for your agent</Description>
  </RegistrationInfo>
  <Triggers>
    <LogonTrigger>
      <Enabled>true</Enabled>
      <UserId>{user}</UserId>
      <Delay>PT30S</Delay>
    </LogonTrigger>
  </Triggers>
  <Principals>
    <Principal id="Author">
      <UserId>{user}</UserId>
      <LogonType>InteractiveToken</LogonType>
      <RunLevel>LeastPrivilege</RunLevel>
    </Principal>
  </Principals>
  <Settings>
    <MultipleInstancesPolicy>IgnoreNew</MultipleInstancesPolicy>
    <DisallowStartIfOnBatteries>false</DisallowStartIfOnBatteries>
    <StopIfGoingOnBatteries>false</StopIfGoingOnBatteries>
    <AllowHardTerminate>true</AllowHardTerminate>
    <StartWhenAvailable>true</StartWhenAvailable>
    <RunOnlyIfNetworkAvailable>false</RunOnlyIfNetworkAvailable>
    <IdleSettings>
      <StopOnIdleEnd>false</StopOnIdleEnd>
      <RestartOnIdle>false</RestartOnIdle>
    </IdleSettings>
    <AllowStartOnDemand>true</AllowStartOnDemand>
    <Enabled>true</Enabled>
    <Hidden>false</Hidden>
    <RunOnlyIfIdle>false</RunOnlyIfIdle>
    <WakeToRun>false</WakeToRun>
    <ExecutionTimeLimit>PT0S</ExecutionTimeLimit>
    <Priority>7</Priority>
    <RestartOnFailure>
      <Interval>PT1M</Interval>
      <Count>999</Count>
    </RestartOnFailure>
  </Settings>
  <Actions Context="Author">
    <Exec>
      <Command>wscript.exe</Command>
      <Arguments>//B //Nologo "{launcher}"</Arguments>
    </Exec>
  </Actions>
</Task>
"#,
        user = xml(user),
        launcher = xml(&launcher.display().to_string()),
    )
}

/// The launcher the task runs on Windows: `octotrigger start` with this
/// config, in a hidden window, and its exit code as the task's, so a crash
/// counts as a failure.
#[must_use]
pub fn windows_launcher(exe: &str, config: &Path) -> String {
    let line = format!("\"{exe}\" --config \"{}\" start", config.display());
    format!(
        "' octotrigger: starts the watcher at sign-in, with no window. Written by octotrigger \
         setup.\r\n\
         Option Explicit\r\n\
         Dim sh\r\n\
         Set sh = CreateObject(\"WScript.Shell\")\r\n\
         WScript.Quit sh.Run(\"{}\", 0, True)\r\n",
        line.replace('"', "\"\"")
    )
}

/// A path with the home folder written as `~`.
fn home_relative(path: &Path, home: &Path) -> String {
    path.strip_prefix(home).map_or_else(
        |_| path.display().to_string(),
        |rest| format!("~/{}", rest.display()),
    )
}

/// A word quoted for `sh` when it needs it.
#[must_use]
pub fn shell_quote(word: &str) -> String {
    if !word.is_empty()
        && word
            .chars()
            .all(|c| c.is_ascii_alphanumeric() || "/._-+=:@".contains(c))
    {
        word.to_owned()
    } else {
        format!("'{}'", word.replace('\'', "'\\''"))
    }
}

/// Writes a file only the person can read, in a folder only they can open.
fn write_private(path: &Path, text: &str) -> Result<()> {
    write_private_bytes(path, text.as_bytes())
}

fn write_private_bytes(path: &Path, bytes: &[u8]) -> Result<()> {
    if let Some(folder) = path.parent() {
        let made = !folder.exists();
        std::fs::create_dir_all(folder)?;
        if made {
            crate::platform::make_private(folder, true)?;
        }
    }
    std::fs::write(path, bytes)?;
    crate::platform::make_private(path, false)?;
    Ok(())
}

/// Writes `token_warning_confirmed` into the config, keeping its comments.
///
/// # Errors
///
/// A config that cannot be read, written, or read back.
pub fn confirm_warning(path: &Path, day: &str) -> Result<()> {
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

fn xml(text: &str) -> String {
    text.replace('&', "&amp;")
        .replace('<', "&lt;")
        .replace('>', "&gt;")
        .replace('"', "&quot;")
}

/// The launchd file: `octotrigger start` at login, again after a crash (not
/// after `octotrigger stop`, which ends with exit 0), at most once a minute.
#[must_use]
pub fn launchd_plist(exe: &str, config: &Path, path_env: &str, log_dir: &Path) -> String {
    let log = log_dir.join("watcher.log");
    format!(
        r#"<?xml version="1.0" encoding="UTF-8"?>
<!DOCTYPE plist PUBLIC "-//Apple//DTD PLIST 1.0//EN" "http://www.apple.com/DTDs/PropertyList-1.0.dtd">
<plist version="1.0">
<dict>
  <key>Label</key>
  <string>{LAUNCHD_LABEL}</string>
  <key>ProgramArguments</key>
  <array>
    <string>{exe}</string>
    <string>--config</string>
    <string>{config}</string>
    <string>start</string>
  </array>
  <key>EnvironmentVariables</key>
  <dict>
    <key>PATH</key>
    <string>{path}</string>
  </dict>
  <key>RunAtLoad</key>
  <true/>
  <key>KeepAlive</key>
  <dict>
    <key>SuccessfulExit</key>
    <false/>
  </dict>
  <key>ThrottleInterval</key>
  <integer>60</integer>
  <key>StandardOutPath</key>
  <string>{log}</string>
  <key>StandardErrorPath</key>
  <string>{log}</string>
</dict>
</plist>
"#,
        exe = xml(exe),
        config = xml(&config.display().to_string()),
        path = xml(path_env),
        log = xml(&log.display().to_string()),
    )
}

/// The systemd user unit: `octotrigger start` at login, again after a crash.
#[must_use]
pub fn systemd_unit(exe: &str, config: &Path, path_env: &str) -> String {
    let quote = |text: &str| format!("\"{}\"", text.replace('\\', "\\\\").replace('"', "\\\""));
    format!(
        "[Unit]\n\
         Description=octotrigger: claims Toolscaled runs for your agent\n\
         After=network-online.target\n\
         \n\
         [Service]\n\
         ExecStart={} --config {} start\n\
         Environment={}\n\
         Restart=on-failure\n\
         RestartSec=60\n\
         \n\
         [Install]\n\
         WantedBy=default.target\n",
        quote(exe),
        quote(&config.display().to_string()),
        quote(&format!("PATH={path_env}")),
    )
}

/// Refuses a setup that cannot ask: it needs a person at the terminal.
///
/// # Errors
///
/// Standard input is not a terminal.
pub fn need_terminal() -> Result<()> {
    use std::io::IsTerminal as _;
    if !std::io::stdin().is_terminal() {
        bail!(
            "this command asks questions: run it in a terminal, or give the answers as flags \
             (octotrigger setup --yes [--agent own|mine] [--plan chatgpt] \
             [--accept-token-warning])"
        );
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::cell::RefCell;
    use std::collections::VecDeque;

    #[test]
    fn the_version_is_the_first_x_y_z_of_the_hermes_line() {
        let text = "Hermes Agent v0.21.6+777.g71d34ba (2026.9.24) · stable · local 71d34bad1045\n\
                    Install directory: /x\n";
        assert_eq!(parse_version(text), Some((0, 21, 6)));
        assert!(parse_version(text).expect("v") >= HERMES_MIN);
        assert!(parse_version("Hermes Agent v0.21.3").expect("v") < HERMES_MIN);
        assert_eq!(parse_version("no version here"), None);
    }

    #[test]
    fn a_codex_login_is_an_oauth_entry_under_openai_codex() {
        let with = "copilot (1 credentials):\n  #1  gh auth token        api_key id=1 gh_cli ←\n\n\
                    openai-codex (1 credentials):\n  #1  openai-codex-oauth-1 oauth   id=2 device_code ←\n";
        assert!(has_codex_login(with));
        let without = "copilot (1 credentials):\n  #1  gh auth token  api_key id=1 ←\n\n\
                       nous (1 credentials):\n  #1  someone oauth   id=3 device_code ←\n";
        assert!(!has_codex_login(without));
    }

    #[cfg(unix)]
    #[test]
    fn the_own_install_stays_in_its_folder_at_the_pinned_commit() {
        let own = OwnHermes {
            root: PathBuf::from("/h/.octotrigger/agents/hermes"),
        };
        let line = own_install_command(Os::MacOs, &own);
        assert!(
            line.contains(&format!("hermes-agent/{HERMES_PIN}/scripts/install.sh")),
            "{line}"
        );
        assert!(
            line.contains("| HOME=/h/.octotrigger/agents/hermes/installer-home bash -s --"),
            "{line}"
        );
        assert!(
            line.contains("--dir /h/.octotrigger/agents/hermes/hermes-agent"),
            "{line}"
        );
        assert!(
            line.contains("--hermes-home /h/.octotrigger/agents/hermes/home"),
            "{line}"
        );
        assert!(line.contains(&format!("--commit {HERMES_PIN}")), "{line}");
        assert!(line.contains("--non-interactive"), "{line}");
        assert_eq!(
            own.program(),
            Path::new("/h/.octotrigger/agents/hermes/hermes-agent/.hermes/bin/hermes")
        );
    }

    #[cfg(unix)]
    #[test]
    fn the_service_files_start_the_watcher_with_this_config() {
        let plist = launchd_plist(
            "/usr/local/bin/octotrigger",
            Path::new("/Users/a b/.config/octotrigger/config.yaml"),
            "/usr/bin:/bin",
            Path::new("/Users/a b/.local/state/octotrigger"),
        );
        assert!(plist.contains("<string>/usr/local/bin/octotrigger</string>"));
        assert!(plist.contains("<string>/Users/a b/.config/octotrigger/config.yaml</string>"));
        assert!(plist.contains("<key>SuccessfulExit</key>\n    <false/>"));
        let unit = systemd_unit(
            "/usr/bin/octotrigger",
            Path::new("/home/a/.config/octotrigger/config.yaml"),
            "/usr/bin",
        );
        assert!(unit.contains(
            "ExecStart=\"/usr/bin/octotrigger\" --config \"/home/a/.config/octotrigger/config.yaml\" start"
        ));
        assert!(unit.contains("Restart=on-failure"));
    }

    #[test]
    fn shell_quote_leaves_plain_words_and_quotes_the_rest() {
        assert_eq!(shell_quote("/usr/bin/octotrigger"), "/usr/bin/octotrigger");
        assert_eq!(shell_quote("/a b/c"), "'/a b/c'");
        assert_eq!(shell_quote("it's"), "'it'\\''s'");
    }

    /// A machine with nothing installed, where each step's result appears
    /// once its command ran.
    struct Fake {
        answers: VecDeque<&'static str>,
        said: Vec<String>,
        ran: RefCell<Vec<String>>,
        own: OwnHermes,
        auth_file: PathBuf,
        os: Os,
    }

    impl Fake {
        fn new(paths: &Paths, auth_file: PathBuf, answers: &[&'static str]) -> Self {
            Self {
                answers: answers.iter().copied().collect(),
                said: Vec::new(),
                ran: RefCell::new(Vec::new()),
                own: paths.own.clone(),
                auth_file,
                os: Os::MacOs,
            }
        }

        fn ran_with(&self, text: &str) -> bool {
            self.ran.borrow().iter().any(|line| line.contains(text))
        }
    }

    impl Machine for Fake {
        fn output(&self, program: &str, args: &[&str], env: &[(&str, &str)]) -> Option<Output> {
            let own = program == self.own.program().display().to_string();
            if own {
                assert_eq!(
                    env,
                    [("HERMES_HOME", self.own.home().to_str().expect("utf-8"))],
                    "octotrigger's Hermes always runs with its own home"
                );
            }
            match (program, args) {
                (_, ["--version"])
                    if own && (self.ran_with("install.sh") || self.ran_with("install.ps1")) =>
                {
                    Some(Output {
                        ok: true,
                        text: "Hermes Agent v0.21.7 (2026.9.24)\n".into(),
                    })
                }
                (_, ["auth", "list"]) if own => Some(Output {
                    ok: true,
                    text: if self.ran_with("auth add openai-codex") {
                        "openai-codex (1 credentials):\n  #1  x oauth id=1 device_code ←\n"
                    } else {
                        ""
                    }
                    .into(),
                }),
                _ => None,
            }
        }

        fn run(&mut self, line: &str) -> bool {
            self.ran.borrow_mut().push(line.to_owned());
            if line.ends_with(" login") {
                let tokens = auth::Tokens {
                    client_id: "c".into(),
                    access_token: "a".into(),
                    refresh_token: "r".into(),
                    expires_at: chrono::Utc::now().timestamp() + 3600,
                    scope: MCP_SCOPE.into(),
                    organization_id: "org-1".into(),
                    organization_name: None,
                    token_endpoint: "http://127.0.0.1:1/token".into(),
                    resource: Toolscaled::default().mcp_url,
                };
                auth::save(&self.auth_file, &tokens).expect("save");
            }
            true
        }

        fn ask(&mut self, question: &str) -> String {
            self.said.push(format!("? {question}"));
            self.answers.pop_front().unwrap_or("").to_owned()
        }

        fn say(&mut self, text: &str) {
            self.said.push(text.to_owned());
        }

        fn os(&self) -> Os {
            self.os
        }

        fn uid(&self) -> u32 {
            501
        }
    }

    /// Paths in a test folder, with a config written up front that holds
    /// this test's auth file and folders (setup keeps a config it finds).
    fn test_paths(home: &Path) -> (Paths, PathBuf) {
        let config_path = home.join(".config/octotrigger/config.yaml");
        let auth_file = home.join(".config/octotrigger/auth.json");
        let paths = Paths {
            config: config_path.clone(),
            own: OwnHermes {
                root: home.join(".octotrigger/agents/hermes"),
            },
            home: home.to_path_buf(),
            exe: "/bin/octotrigger".into(),
            path_env: "/usr/bin:/bin".into(),
            machine: "test-mac".into(),
            user: String::new(),
        };
        let own = &paths.own;
        let starter = config::starter("test-mac", Plan::ChatGpt, Agent::Own)
            .replace(
                "machine: test-mac\n",
                &format!(
                    "machine: test-mac\ntoolscaled:\n  auth_file: {}\nlog_dir: {}\nstate_file: {}\n\
                     stop_file: {}\nwork_dir: {}\n",
                    auth_file.display(),
                    home.join(".local/state/octotrigger").display(),
                    home.join(".local/state/octotrigger/claims.json").display(),
                    home.join(".config/octotrigger/stop").display(),
                    home.join(".local/state/octotrigger/work").display(),
                ),
            )
            .replace(
                "HERMES_HOME: ~/.octotrigger/agents/hermes/home",
                &format!("HERMES_HOME: {}", own.home().display()),
            );
        std::fs::create_dir_all(config_path.parent().expect("parent")).expect("dir");
        std::fs::write(&config_path, starter).expect("config");
        (paths, auth_file)
    }

    #[cfg(unix)]
    #[test]
    fn setup_takes_a_new_machine_from_nothing_to_a_running_watcher() {
        let dir = tempfile::tempdir().expect("dir");
        let home = dir.path();
        let (paths, auth_file) = test_paths(home);
        // The install, the ChatGPT login, the sign-in, the warning, the
        // check, the launchd file, its start. Hermes and the plan come from
        // the config, so setup asks neither.
        let mut fake = Fake::new(&paths, auth_file.clone(), &["", "", "", "yes", "", "", ""]);
        let finished = run(&mut fake, &paths, Choices::default()).expect("setup");
        let said = fake.said.join("\n");
        assert!(finished, "{said}");
        let ran = fake.ran.borrow().clone();
        let own = &paths.own;
        let config = paths.config.display();
        assert_eq!(
            ran,
            [
                own_install_command(Os::MacOs, own),
                format!(
                    "HERMES_HOME={} {} auth add openai-codex",
                    own.home().display(),
                    own.program().display()
                ),
                format!("/bin/octotrigger --config {config} login"),
                format!("/bin/octotrigger --config {config} check"),
                format!(
                    "launchctl bootstrap gui/501 {}",
                    home.join("Library/LaunchAgents/com.octotrigger.watcher.plist")
                        .display()
                ),
            ],
            "{said}"
        );
        assert!(said.contains("Done: Hermes Agent 0.21.7 in"), "{said}");
        let hermes = std::fs::read_to_string(own.home().join("config.yaml")).expect("hermes");
        assert!(hermes.contains("Bearer ${OCTOTRIGGER_TOOLSCALED_TOKEN}"));
        assert!(hermes.contains("expose_on_path: false"));
        assert!(
            Config::load(&paths.config)
                .expect("config")
                .token_warning_confirmed
                .is_some()
        );
        assert!(
            home.join("Library/LaunchAgents/com.octotrigger.watcher.plist")
                .exists()
        );

        // A second setup finds every step done; only the check runs again.
        let mut again = Fake::new(&paths, auth_file, &[""]);
        *again.ran.borrow_mut() = ran;
        let before = again.ran.borrow().len();
        assert!(run(&mut again, &paths, Choices::default()).expect("setup"));
        let ran = again.ran.borrow().clone();
        assert_eq!(ran.len(), before + 1, "{ran:?}");
        assert!(ran.last().expect("ran").ends_with(" check"));

        // Uninstall: the service stops, its file and octotrigger's Hermes go,
        // and with --all the config, sign-in and logs too.
        std::fs::create_dir_all(own.code()).expect("code");
        std::fs::create_dir_all(home.join(".local/state/octotrigger/runs")).expect("logs");
        std::fs::create_dir_all(home.join(".local/state/other")).expect("another program");
        let mut out = Fake::new(&paths, home.join("unused"), &["y", "y"]);
        assert!(uninstall(&mut out, &paths, true).expect("uninstall"));
        let said = out.said.join("\n");
        assert!(
            out.ran_with("launchctl bootout gui/501/com.octotrigger.watcher"),
            "{said}"
        );
        assert!(
            !home
                .join("Library/LaunchAgents/com.octotrigger.watcher.plist")
                .exists()
        );
        assert!(!home.join(".octotrigger").exists(), "{said}");
        assert!(!home.join(".config/octotrigger").exists(), "{said}");
        assert!(!home.join(".local/state/octotrigger").exists(), "{said}");
        assert!(
            home.join(".local/state").exists(),
            "only octotrigger's folders"
        );
    }

    /// The same setup on Windows: PowerShell lines, Hermes' Windows
    /// installer with the PATH entry taken out again, and a Task Scheduler
    /// task that runs a hidden launcher.
    #[test]
    fn setup_on_windows_uses_powershell_and_a_task_at_sign_in() {
        let dir = tempfile::tempdir().expect("dir");
        let home = dir.path();
        let (mut paths, auth_file) = test_paths(home);
        paths.exe = r"C:\Users\ann\AppData\Local\Programs\octotrigger\octotrigger.exe".into();
        paths.user = r"PC\ann".into();
        let mut fake = Fake::new(&paths, auth_file, &["", "", "", "yes", "", "", ""]);
        fake.os = Os::Windows;
        let finished = run(&mut fake, &paths, Choices::default()).expect("setup");
        let said = fake.said.join("\n");
        assert!(finished, "{said}");
        let ran = fake.ran.borrow().clone();
        let own = &paths.own;
        let q = |path: &Path| Os::Windows.quote(&path.display().to_string());
        let exe = format!("& '{}'", paths.exe);
        assert_eq!(ran.len(), 5, "{ran:#?}");
        assert!(ran[0].contains(&format!(
            "/{HERMES_PIN}/scripts/install.ps1))) -Commit {HERMES_PIN} -InstallDir {} \
             -HermesHome {} -NonInteractive -SkipBrowser -SkipComputerUse;",
            q(&own.code()),
            q(&own.home())
        )));
        assert!(
            ran[0].contains("SetEnvironmentVariable('Path'")
                && ran[0].contains(&q(&own.home().join("bin"))),
            "the installer's PATH entry goes again: {}",
            ran[0]
        );
        assert_eq!(
            ran[1],
            format!(
                "$env:HERMES_HOME = {}; & {} auth add openai-codex",
                q(&own.home()),
                q(&own.program())
            )
        );
        assert_eq!(ran[2], format!("{exe} --config {} login", q(&paths.config)));
        assert_eq!(ran[3], format!("{exe} --config {} check", q(&paths.config)));
        let task = home.join(".octotrigger").join("octotrigger.task.xml");
        let launcher = home.join(".octotrigger").join("octotrigger.vbs");
        assert!(
            ran[4].starts_with(&format!(
                "schtasks /Create /F /TN octotrigger /XML {} /RU 'PC\\ann' /NP /IT;",
                q(&task)
            )),
            "{}",
            ran[4]
        );
        assert!(ran[4].ends_with("schtasks /Run /TN octotrigger; exit $LASTEXITCODE"));
        let bytes = std::fs::read(&task).expect("task");
        assert_eq!(&bytes[..2], [0xFF, 0xFE], "UTF-16 with its byte order mark");
        let units: Vec<u16> = bytes[2..]
            .chunks(2)
            .map(|pair| u16::from_le_bytes([pair[0], pair[1]]))
            .collect();
        let text = String::from_utf16(&units).expect("utf-16");
        assert!(text.contains("<UserId>PC\\ann</UserId>"), "{text}");
        assert!(
            text.contains("<RunLevel>LeastPrivilege</RunLevel>"),
            "{text}"
        );
        assert!(text.contains(&format!("//B //Nologo \"{}\"", launcher.display())));
        let vbs = std::fs::read_to_string(&launcher).expect("launcher");
        assert!(
            vbs.contains(&format!(
                "WScript.Quit sh.Run(\"\"\"{}\"\" --config \"\"{}\"\" start\", 0, True)",
                paths.exe,
                paths.config.display()
            )),
            "{vbs}"
        );

        // Task Scheduler refuses: the Startup folder instead.
        let other = tempfile::tempdir().expect("dir");
        let (mut paths, auth_file) = test_paths(other.path());
        paths.user = r"PC\ann".into();
        std::fs::write(&auth_file, "").expect("auth");
        let mut refused = Refusing(Fake::new(
            &paths,
            auth_file,
            &["", "", "", "yes", "", "", "", ""],
        ));
        refused.0.os = Os::Windows;
        assert!(run(&mut refused, &paths, Choices::default()).expect("setup"));
        assert!(
            refused
                .0
                .ran_with("[Environment]::GetFolderPath('Startup')")
        );

        // Uninstall: the task and the Startup copy go, with both files.
        let mut out = Fake::new(&paths, other.path().join("unused"), &["n"]);
        out.os = Os::Windows;
        assert!(uninstall(&mut out, &paths, false).expect("uninstall"));
        assert!(out.ran_with("schtasks /Delete /F /TN octotrigger"));
        assert!(
            !other
                .path()
                .join(".octotrigger/octotrigger.task.xml")
                .exists()
        );
        assert!(!other.path().join(".octotrigger/octotrigger.vbs").exists());
        assert!(out.said.join("\n").contains("Remove-Item -Recurse -Force"));
    }

    /// Task Scheduler takes the task file that setup writes. It registers a
    /// real task (and deletes it), so only CI runs it:
    /// `cargo test -- --ignored`.
    #[cfg(windows)]
    #[test]
    #[ignore = "registers a real Task Scheduler task"]
    fn task_scheduler_takes_the_task_file() {
        let dir = tempfile::tempdir().expect("dir");
        let user = format!(
            "{}\\{}",
            std::env::var("USERDOMAIN").expect("USERDOMAIN"),
            std::env::var("USERNAME").expect("USERNAME")
        );
        let launcher = dir.path().join("octotrigger.vbs");
        std::fs::write(
            &launcher,
            windows_launcher(r"C:\none\octotrigger.exe", Path::new("c.yaml")),
        )
        .expect("launcher");
        let task = dir.path().join("octotrigger.task.xml");
        std::fs::write(&task, utf16_file(&task_xml(&launcher, &user))).expect("task");
        let name = "octotrigger-test";
        let schtasks = |args: &[&str]| {
            std::process::Command::new("schtasks")
                .args(args)
                .output()
                .expect("schtasks")
        };
        let xml = task.display().to_string();
        let mut made = schtasks(&[
            "/Create", "/F", "/TN", name, "/XML", &xml, "/RU", &user, "/NP", "/IT",
        ]);
        if !made.status.success() {
            made = schtasks(&["/Create", "/F", "/TN", name, "/XML", &xml]);
        }
        assert!(
            made.status.success(),
            "{}{}",
            String::from_utf8_lossy(&made.stdout),
            String::from_utf8_lossy(&made.stderr)
        );
        let query = schtasks(&["/Query", "/TN", name, "/XML"]);
        let _ = schtasks(&["/Delete", "/F", "/TN", name]);
        assert!(query.status.success());
        let text = String::from_utf8_lossy(&query.stdout).replace('\0', "");
        assert!(text.contains("LogonTrigger"), "{text}");
    }

    /// A machine whose Task Scheduler refuses the task.
    struct Refusing(Fake);

    impl Machine for Refusing {
        fn output(&self, program: &str, args: &[&str], env: &[(&str, &str)]) -> Option<Output> {
            self.0.output(program, args, env)
        }
        fn run(&mut self, line: &str) -> bool {
            let ok = self.0.run(line);
            ok && !line.starts_with("schtasks /Create")
        }
        fn ask(&mut self, question: &str) -> String {
            self.0.ask(question)
        }
        fn say(&mut self, text: &str) {
            self.0.say(text);
        }
        fn os(&self) -> Os {
            self.0.os()
        }
        fn uid(&self) -> u32 {
            0
        }
    }

    #[cfg(unix)]
    #[test]
    fn setup_asks_which_hermes_and_stops_with_the_command_on_a_no() {
        let dir = tempfile::tempdir().expect("dir");
        let (paths, auth_file) = test_paths(dir.path());
        std::fs::remove_file(&paths.config).expect("no config");
        // octotrigger's own Hermes, then no to the install.
        let mut fake = Fake::new(&paths, auth_file, &["", "n"]);
        assert!(!run(&mut fake, &paths, Choices::default()).expect("setup"));
        let said = fake.said.join("\n");
        assert!(
            said.contains("Which Hermes Agent runs your agents?"),
            "{said}"
        );
        assert!(
            said.contains(&format!(
                "Run: {}",
                own_install_command(Os::MacOs, &paths.own)
            )),
            "{said}"
        );
        assert!(
            said.contains("then run `octotrigger setup` again"),
            "{said}"
        );
        assert!(fake.ran.borrow().is_empty());
        assert!(!paths.config.exists(), "no config before Hermes is there");
        assert!(
            paths.own.home().join("config.yaml").exists(),
            "the home's config comes before the install"
        );
    }

    #[cfg(unix)]
    #[test]
    fn a_chat_with_no_terminal_drives_setup_with_flags() {
        let dir = tempfile::tempdir().expect("dir");
        let (paths, auth_file) = test_paths(dir.path());
        // --yes alone stops at the token warning: only the person confirms it.
        let mut fake = Fake::new(&paths, auth_file.clone(), &[]);
        let yes = Choices {
            yes: true,
            ..Choices::default()
        };
        assert!(!run(&mut fake, &paths, yes).expect("setup"));
        let said = fake.said.join("\n");
        assert!(
            !said.lines().any(|line| line.starts_with("? ")),
            "no question is asked: {said}"
        );
        assert!(said.contains("Run it now? yes (--yes)"), "{said}");
        assert!(
            said.contains("run setup again with --accept-token-warning"),
            "{said}"
        );
        assert!(
            said.contains("/org/org-1/workflows shows its name"),
            "{said}"
        );
        assert!(
            said.contains("that is not for this Hermes, which octotrigger calls by its path"),
            "{said}"
        );
        assert!(
            Config::load(&paths.config)
                .expect("config")
                .token_warning_confirmed
                .is_none()
        );
        // With the person's confirmation it goes to the end.
        let mut again = Fake::new(&paths, auth_file, &[]);
        *again.ran.borrow_mut() = fake.ran.borrow().clone();
        let confirmed = Choices {
            yes: true,
            accept_token_warning: true,
            ..Choices::default()
        };
        assert!(run(&mut again, &paths, confirmed).expect("setup"));
        let said = again.said.join("\n");
        assert!(
            !said.lines().any(|line| line.starts_with("? ")),
            "no question is asked: {said}"
        );
        assert!(
            said.contains("Confirmed with --accept-token-warning."),
            "{said}"
        );
        assert!(
            Config::load(&paths.config)
                .expect("config")
                .token_warning_confirmed
                .is_some()
        );
    }

    #[cfg(unix)]
    #[test]
    fn a_persons_own_hermes_is_used_as_it_is() {
        let dir = tempfile::tempdir().expect("dir");
        let (paths, auth_file) = test_paths(dir.path());
        std::fs::remove_file(&paths.config).expect("no config");
        // My own Hermes, then no to its install.
        let mut fake = Fake::new(&paths, auth_file, &["2", "n"]);
        assert!(!run(&mut fake, &paths, Choices::default()).expect("setup"));
        let said = fake.said.join("\n");
        assert!(said.contains(&format!("Run: {HERMES_INSTALL}")), "{said}");
        assert!(
            !paths.own.root.exists(),
            "nothing of octotrigger's own Hermes is written"
        );
    }
}
