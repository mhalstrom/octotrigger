//! The loop of `octotrigger start`.
//!
//! 1. At start: the agents of the state file are stopped and their claims
//!    ended as `error`.
//! 2. The stop file: `stop` claims nothing new and ends when the agents are
//!    done; `now` stops them and ends their claims.
//! 3. With a free slot and the day's limits left: one claim call for all
//!    jobs with a free slot.
//! 4. A claim: the run's folder, a fresh Toolscaled token for the agent that
//!    outlives the job's time limit, the agent in its own process group with
//!    that limit, the claim in the state file, a renew every third of the
//!    lease.
//! 5. An agent that ends: its token use is read and counted, the claim is
//!    ended (`done` when it exited 0, else `error`) with the use, and the
//!    folder is deleted.
//! 6. The wait: `poll_seconds` after a claim, doubled up to 300 s while
//!    nothing comes.

use std::path::Path;
use std::process::Child;
use std::time::{Duration, Instant};

use anyhow::{Result, bail};
use serde_json::json;

use crate::agent::{self, Folder};
use crate::config::{Config, Job, Workflows, expand};
use crate::log::Log;
use crate::state::{Held, State, today};
use crate::surface::{Claim, Outcome, Refused, Surface, Usage};

/// The longest wait between claim calls while nothing comes.
pub const MAX_WAIT: Duration = Duration::from_secs(300);

/// What the stop file says.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Stop {
    /// Claim nothing new; end when the agents are done.
    Drain,
    /// Stop the agents now and end their claims.
    Now,
}

/// Reads the stop file: absent is no stop, `now` is [`Stop::Now`], any other
/// text is [`Stop::Drain`].
#[must_use]
pub fn read_stop(path: &Path) -> Option<Stop> {
    let text = std::fs::read_to_string(path).ok()?;
    Some(if text.trim() == "now" {
        Stop::Now
    } else {
        Stop::Drain
    })
}

/// Writes the stop file.
///
/// # Errors
///
/// A file that cannot be written.
pub fn write_stop(path: &Path, stop: Stop) -> Result<()> {
    if let Some(folder) = path.parent() {
        std::fs::create_dir_all(folder)?;
    }
    std::fs::write(
        path,
        match stop {
            Stop::Drain => "stop\n",
            Stop::Now => "now\n",
        },
    )?;
    Ok(())
}

struct Running {
    claim: Claim,
    job: usize,
    child: Child,
    folder: Folder,
    output: std::path::PathBuf,
    deadline: Instant,
    next_renew: Instant,
    renew_every: Duration,
}

pub struct Runner<S: Surface> {
    config: Config,
    surface: S,
    log: Log,
    state: State,
    running: Vec<Running>,
    next_claim: Instant,
    wait: Duration,
    /// A shorter renew interval, for tests.
    pub renew_every: Option<Duration>,
    /// A shorter time between loop steps, for tests.
    pub step: Duration,
}

impl<S: Surface> Runner<S> {
    /// # Errors
    ///
    /// A state file that cannot be read.
    pub fn new(config: Config, surface: S) -> Result<Self> {
        let log = Log::new(&expand(&config.log_dir));
        let state = State::load(&expand(&config.state_file))?;
        let wait = Duration::from_secs(config.poll_seconds);
        Ok(Self {
            config,
            surface,
            log,
            state,
            running: Vec::new(),
            next_claim: Instant::now(),
            wait,
            renew_every: None,
            step: Duration::from_secs(1),
        })
    }

    fn save(&self) -> Result<()> {
        self.state.save(&expand(&self.config.state_file))
    }

    /// Runs the loop until the stop file ends it.
    ///
    /// # Errors
    ///
    /// No token warning confirmed, or a state file that cannot be written.
    pub fn start(&mut self) -> Result<()> {
        if self.config.token_warning_confirmed.is_none() {
            bail!("token use is not confirmed: run `octotrigger setup` and read the warning first");
        }
        self.recover()?;
        self.log.sweep(self.config.log_keep_days);
        self.state.loop_pid = Some(std::process::id());
        self.save()?;
        self.log.event(
            "start",
            json!({"machine": self.config.machine, "jobs": self.config.jobs.len()}),
        );
        loop {
            if !self.step()? {
                break;
            }
            std::thread::sleep(self.step);
        }
        self.state.loop_pid = None;
        self.save()?;
        self.log.event("stopped", json!({}));
        Ok(())
    }

    /// Stops the agents a previous start left and ends their claims.
    ///
    /// # Errors
    ///
    /// A state file that cannot be written.
    pub fn recover(&mut self) -> Result<()> {
        let left = std::mem::take(&mut self.state.claims);
        for held in left {
            agent::kill_group(held.pid);
            let ended = self.surface.release(
                &held.claim_token,
                Outcome::Error,
                Some("octotrigger restarted while the agent worked; it stopped the agent."),
                None,
            );
            self.log.event(
                "recovered",
                json!({"run_id": held.run_id, "workflow_id": held.workflow_id, "job": held.job,
                       "released": ended.is_ok()}),
            );
            let _ = std::fs::remove_dir_all(&held.folder);
        }
        self.save()
    }

    /// One pass of the loop. `false` when the loop ends.
    ///
    /// # Errors
    ///
    /// A state file that cannot be written.
    pub fn step(&mut self) -> Result<bool> {
        let stop = read_stop(&expand(&self.config.stop_file));
        if stop == Some(Stop::Now) {
            self.stop_all("stopped with octotrigger stop --now")?;
            return Ok(false);
        }
        self.watch()?;
        if stop == Some(Stop::Drain) {
            return Ok(!self.running.is_empty());
        }
        if Instant::now() >= self.next_claim {
            self.try_claim()?;
        }
        Ok(true)
    }

    /// Ends each agent that exited or ran out of time, and renews the rest.
    fn watch(&mut self) -> Result<()> {
        let mut index = 0;
        while index < self.running.len() {
            let now = Instant::now();
            let exited = self.running[index].child.try_wait()?;
            if let Some(status) = exited {
                let running = self.running.remove(index);
                let (outcome, note) = if status.success() {
                    (Outcome::Done, None)
                } else {
                    (
                        Outcome::Error,
                        Some(format!("The agent stopped with {status}.")),
                    )
                };
                self.finish(running, outcome, note.as_deref())?;
                continue;
            }
            if now >= self.running[index].deadline {
                let running = self.running.remove(index);
                agent::kill_group(running.child.id());
                let minutes = self.config.jobs[running.job].run_timeout_minutes;
                self.finish(
                    running,
                    Outcome::Error,
                    Some(&format!(
                        "The agent ran longer than {minutes} minutes; octotrigger stopped it."
                    )),
                )?;
                continue;
            }
            if now >= self.running[index].next_renew {
                let lease = self.config.jobs[self.running[index].job].lease_seconds;
                let token = self.running[index].claim.claim_token.clone();
                match self.surface.renew(&token, lease) {
                    Ok(until) => {
                        let running = &mut self.running[index];
                        running.next_renew = now + running.renew_every;
                        self.log.event(
                            "renew",
                            json!({"run_id": running.claim.run_id, "until": until}),
                        );
                    }
                    Err(error)
                        if error
                            .downcast_ref::<Refused>()
                            .is_some_and(Refused::claim_ended) =>
                    {
                        // The run ended, waits for the person, or was taken
                        // back: the agent's token stops working, and there is
                        // no claim left to end.
                        let mut running = self.running.remove(index);
                        agent::kill_group(running.child.id());
                        let _ = running.child.wait();
                        self.log.event(
                            "claim_ended",
                            json!({"run_id": running.claim.run_id, "detail": error.to_string()}),
                        );
                        self.forget(&running)?;
                        continue;
                    }
                    Err(error) => {
                        let running = &mut self.running[index];
                        running.next_renew = now + Duration::from_secs(10);
                        self.log.event(
                            "renew_failed",
                            json!({"run_id": running.claim.run_id, "detail": error.to_string()}),
                        );
                    }
                }
            }
            index += 1;
        }
        Ok(())
    }

    /// Reads the use of an agent that ended, ends its claim, and deletes its
    /// folder.
    fn finish(&mut self, mut running: Running, outcome: Outcome, note: Option<&str>) -> Result<()> {
        let _ = running.child.wait();
        let usage = agent::read_usage(&running.folder, &running.output);
        self.count(usage.as_ref());
        let released =
            self.surface
                .release(&running.claim.claim_token, outcome, note, usage.as_ref());
        let job = &self.config.jobs[running.job];
        self.log.event(
            "end",
            json!({
                "run_id": running.claim.run_id,
                "workflow_id": running.claim.workflow_id,
                "job": job.name,
                "outcome": outcome,
                "tokens": usage.as_ref().map(Usage::total),
                "run": released.as_ref().ok(),
                "release_error": released.as_ref().err().map(ToString::to_string),
            }),
        );
        self.forget(&running)
    }

    fn forget(&mut self, running: &Running) -> Result<()> {
        let _ = std::fs::remove_dir_all(&running.folder.path);
        self.state
            .claims
            .retain(|held| held.run_id != running.claim.run_id);
        self.save()
    }

    fn count(&mut self, usage: Option<&Usage>) {
        self.state.roll(&today());
        match usage {
            Some(usage) => {
                self.state.tokens_today = self.state.tokens_today.saturating_add(usage.total());
            }
            None => self.state.unreported_today += 1,
        }
    }

    /// Whether the day's limits leave room for one more run.
    fn within_limits(&mut self) -> bool {
        self.state.roll(&today());
        let limits = &self.config.limits;
        limits
            .runs_per_day
            .is_none_or(|limit| self.state.runs_today < limit)
            && limits
                .tokens_per_day
                .is_none_or(|limit| self.state.tokens_today < limit)
    }

    fn running_of(&self, job: usize) -> usize {
        self.running
            .iter()
            .filter(|running| running.job == job)
            .count()
    }

    /// One claim call for the jobs with a free slot.
    fn try_claim(&mut self) -> Result<()> {
        let free: Vec<usize> = (0..self.config.jobs.len())
            .filter(|&job| self.running_of(job) < self.config.jobs[job].max_concurrent)
            .collect();
        if free.is_empty() || !self.within_limits() {
            self.next_claim = Instant::now() + Duration::from_secs(self.config.poll_seconds);
            return Ok(());
        }
        let workflow_ids: Vec<String> = if free
            .iter()
            .any(|&job| self.config.jobs[job].workflows == Workflows::All)
        {
            Vec::new()
        } else {
            let mut ids: Vec<String> = free
                .iter()
                .filter_map(|&job| match &self.config.jobs[job].workflows {
                    Workflows::Some(ids) => Some(ids.clone()),
                    Workflows::All => None,
                })
                .flatten()
                .collect();
            ids.sort();
            ids.dedup();
            ids
        };
        let first = &self.config.jobs[free[0]];
        let lease = free
            .iter()
            .map(|&job| self.config.jobs[job].lease_seconds)
            .max()
            .unwrap_or(first.lease_seconds);
        let runner = self.config.runner_name(first);
        match self.surface.claim(&workflow_ids, &runner, lease) {
            Ok(Some(claim)) => {
                self.wait = Duration::from_secs(self.config.poll_seconds);
                self.next_claim = Instant::now() + self.wait;
                let job = free
                    .iter()
                    .copied()
                    .find(|&job| self.config.jobs[job].workflows.takes(&claim.workflow_id));
                match job {
                    Some(job) => self.begin(claim, job)?,
                    None => {
                        let _ = self.surface.release(
                            &claim.claim_token,
                            Outcome::Error,
                            Some("No job of this octotrigger takes the run's workflow."),
                            None,
                        );
                    }
                }
            }
            Ok(None) => {
                self.next_claim = Instant::now() + self.wait;
                self.wait = (self.wait * 2).min(MAX_WAIT);
            }
            Err(error) => {
                self.log
                    .event("claim_failed", json!({"detail": error.to_string()}));
                self.next_claim = Instant::now() + self.wait;
                self.wait = (self.wait * 2).min(MAX_WAIT);
            }
        }
        Ok(())
    }

    /// Starts the job's agent on a claimed run.
    fn begin(&mut self, claim: Claim, job_index: usize) -> Result<()> {
        let job: Job = self.config.jobs[job_index].clone();
        let work_dir = expand(&self.config.work_dir);
        let output = self.log.run_output(&claim.run_id);
        // The token must outlive the run, with five minutes to spare.
        let token_seconds = i64::try_from(job.run_timeout_minutes * 60 + 300).unwrap_or(i64::MAX);
        let started = self.surface.agent_token(token_seconds).and_then(|token| {
            agent::prepare(&work_dir, &claim).and_then(|folder| {
                agent::spawn(&job, &folder, &output, &token).map(|child| (folder, child))
            })
        });
        let (folder, child) = match started {
            Ok(pair) => pair,
            Err(error) => {
                let _ = self.surface.release(
                    &claim.claim_token,
                    Outcome::Error,
                    Some(&format!("octotrigger could not start the agent: {error}")),
                    None,
                );
                self.log.event(
                    "start_failed",
                    json!({"run_id": claim.run_id, "job": job.name, "detail": error.to_string()}),
                );
                return Ok(());
            }
        };
        self.state.roll(&today());
        self.state.runs_today += 1;
        self.state.claims.push(Held {
            run_id: claim.run_id.clone(),
            workflow_id: claim.workflow_id.clone(),
            job: job.name.clone(),
            claim_token: claim.claim_token.clone(),
            pid: child.id(),
            started_at: chrono::Utc::now().to_rfc3339(),
            folder: folder.path.clone(),
        });
        self.save()?;
        self.log.event(
            "claim",
            json!({"run_id": claim.run_id, "workflow_id": claim.workflow_id, "kind": claim.kind,
                   "job": job.name, "agent": self.config.runner_name(&job)}),
        );
        let renew_every = self
            .renew_every
            .unwrap_or_else(|| Duration::from_secs(u64::from(job.lease_seconds) / 3));
        let now = Instant::now();
        self.running.push(Running {
            claim,
            job: job_index,
            child,
            folder,
            output,
            deadline: now + Duration::from_secs(job.run_timeout_minutes * 60),
            next_renew: now + renew_every,
            renew_every,
        });
        Ok(())
    }

    /// Stops every agent and ends its claim as `error`.
    fn stop_all(&mut self, why: &str) -> Result<()> {
        let running = std::mem::take(&mut self.running);
        for mut running in running {
            agent::kill_group(running.child.id());
            let _ = running.child.wait();
            let _ = self.surface.release(
                &running.claim.claim_token,
                Outcome::Error,
                Some(&format!("The agent was stopped: {why}.")),
                None,
            );
            self.log
                .event("stop_now", json!({"run_id": running.claim.run_id}));
            self.forget(&running)?;
        }
        Ok(())
    }

    /// The agents that work now, for tests.
    #[must_use]
    pub fn running(&self) -> usize {
        self.running.len()
    }

    /// The state, for tests and `status`.
    #[must_use]
    pub const fn state(&self) -> &State {
        &self.state
    }

    /// Sets when the next claim call may go, for tests.
    pub fn claim_now(&mut self) {
        self.next_claim = Instant::now();
    }

    /// Sets a time limit that has passed for every agent, for tests.
    pub fn expire_all(&mut self) {
        for running in &mut self.running {
            running.deadline = Instant::now();
        }
    }
}
