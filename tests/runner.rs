//! The loop against a fake `/surface`: a claim, a renew, a release, a
//! restart that stops the agent of the last start, the stop file, the day's
//! limit, and a claim that ended on the server.

use std::collections::VecDeque;
use std::path::Path;
use std::sync::{Arc, Mutex};
use std::time::Duration;

use anyhow::Result;
use octotrigger::config::Config;
use octotrigger::runner::{Runner, Stop, write_stop};
use octotrigger::state::{Held, State};
use octotrigger::surface::{Claim, Outcome, Refused, Surface, Usage};

#[derive(Debug, Clone, PartialEq)]
struct Released {
    token: String,
    outcome: Outcome,
    note: Option<String>,
    usage: Option<Usage>,
}

#[derive(Default)]
struct Fake {
    waiting: Mutex<VecDeque<Claim>>,
    claims: Mutex<Vec<(Vec<String>, String, u32)>>,
    renewed: Mutex<Vec<String>>,
    released: Mutex<Vec<Released>>,
    claim_ended: Mutex<bool>,
}

impl Surface for Fake {
    fn claim(&self, workflow_ids: &[String], runner: &str, lease: u32) -> Result<Option<Claim>> {
        self.claims
            .lock()
            .expect("lock")
            .push((workflow_ids.to_vec(), runner.to_owned(), lease));
        Ok(self.waiting.lock().expect("lock").pop_front())
    }

    fn renew(&self, claim_token: &str, _lease: u32) -> Result<String> {
        if *self.claim_ended.lock().expect("lock") {
            return Err(Refused {
                status: 409,
                code: "claim_ended".into(),
                detail: "the run was taken back".into(),
            }
            .into());
        }
        self.renewed
            .lock()
            .expect("lock")
            .push(claim_token.to_owned());
        Ok("later".into())
    }

    fn release(
        &self,
        claim_token: &str,
        outcome: Outcome,
        note: Option<&str>,
        usage: Option<&Usage>,
    ) -> Result<String> {
        self.released.lock().expect("lock").push(Released {
            token: claim_token.to_owned(),
            outcome,
            note: note.map(str::to_owned),
            usage: usage.cloned(),
        });
        Ok("finished".into())
    }
}

fn claim(n: u32) -> Claim {
    Claim {
        run_id: format!("run-{n}"),
        workflow_id: "wf-a".into(),
        kind: "agent_run".into(),
        code: format!("code-{n}"),
        lease_until: String::new(),
        claim_token: format!("claim-{n}"),
        run_token: format!("token-{n}"),
        prompt: "Connect with the code and do the run.".into(),
    }
}

fn config(dir: &Path, agent: &str, extra: &str) -> Config {
    let text = format!(
        "machine: test\n\
         state_file: {d}/state/claims.json\n\
         stop_file: {d}/stop\n\
         work_dir: {d}/work\n\
         log_dir: {d}/log\n\
         poll_seconds: 1\n\
         token_warning_confirmed: 2026-11-02\n\
         {extra}\
         jobs:\n\
         \x20 - name: runs\n\
         \x20   max_concurrent: 1\n\
         \x20   lease_seconds: 60\n\
         \x20   run_timeout_minutes: 5\n\
         \x20   agent: '{agent}'\n",
        d = dir.display()
    );
    let config: Config = serde_yaml::from_str(&text).expect("config");
    config.check().expect("check");
    config
}

fn runner(dir: &Path, agent: &str, extra: &str, fake: &Arc<Fake>) -> Runner<Arc<Fake>> {
    let mut runner = Runner::new(config(dir, agent, extra), Arc::clone(fake)).expect("runner");
    runner.renew_every = Some(Duration::from_millis(50));
    runner.step = Duration::from_millis(20);
    runner
}

/// Steps the loop until `done` or two seconds pass.
fn step_until(runner: &mut Runner<Arc<Fake>>, done: impl Fn(&Runner<Arc<Fake>>) -> bool) {
    for _ in 0..200 {
        runner.step().expect("step");
        if done(runner) {
            return;
        }
        std::thread::sleep(Duration::from_millis(10));
    }
    panic!("the loop did not get there in two seconds");
}

#[test]
fn a_claimed_run_starts_the_agent_and_ends_with_its_usage() {
    let dir = tempfile::tempdir().expect("dir");
    let fake = Arc::new(Fake::default());
    fake.waiting.lock().expect("lock").push_back(claim(1));
    let agent = r#"cat {prompt_file} > /dev/null && test -f {folder}/mcp.json && echo "{\"usage\":{\"input_tokens\":5,\"cache_read_input_tokens\":100,\"output_tokens\":7}}""#;
    let mut runner = runner(dir.path(), agent, "", &fake);

    runner.step().expect("step");
    assert_eq!(runner.running(), 1);
    let (workflows, name, lease) = fake.claims.lock().expect("lock")[0].clone();
    assert!(workflows.is_empty(), "a job of all workflows names none");
    assert_eq!(name, "cat on test");
    assert_eq!(lease, 60);
    assert_eq!(runner.state().claims.len(), 1);

    step_until(&mut runner, |runner| runner.running() == 0);
    let released = fake.released.lock().expect("lock").clone();
    assert_eq!(released.len(), 1, "{released:?}");
    assert_eq!(released[0].token, "claim-1");
    assert_eq!(released[0].outcome, Outcome::Done);
    assert_eq!(released[0].usage.as_ref().map(Usage::total), Some(112));
    assert_eq!(runner.state().claims, Vec::new());
    assert_eq!(runner.state().runs_today, 1);
    assert_eq!(runner.state().tokens_today, 112);
    assert!(
        !dir.path().join("work/run-run-1").exists(),
        "the folder is deleted"
    );
    assert!(
        dir.path().join("log/runs/run-1.log").exists(),
        "the output is kept"
    );
}

#[test]
fn a_working_agent_renews_and_one_past_its_time_is_stopped() {
    let dir = tempfile::tempdir().expect("dir");
    let fake = Arc::new(Fake::default());
    fake.waiting.lock().expect("lock").push_back(claim(2));
    let mut runner = runner(dir.path(), "sleep 30", "", &fake);

    runner.step().expect("step");
    step_until(&mut runner, |_| {
        fake.renewed.lock().expect("lock").len() >= 2
    });
    assert_eq!(fake.renewed.lock().expect("lock")[0], "claim-2");

    let pid = runner.state().claims[0].pid;
    runner.expire_all();
    runner.step().expect("step");
    assert_eq!(runner.running(), 0);
    let released = fake.released.lock().expect("lock").clone();
    assert_eq!(released[0].outcome, Outcome::Error);
    assert!(
        released[0]
            .note
            .as_deref()
            .is_some_and(|note| note.contains("ran longer than 5 minutes")),
        "{released:?}"
    );
    assert!(
        !octotrigger::agent::alive(pid),
        "the agent's group is stopped"
    );
}

#[test]
fn a_restart_stops_the_agent_of_the_last_start_and_ends_its_claim() {
    let dir = tempfile::tempdir().expect("dir");
    let fake = Arc::new(Fake::default());
    // The agent a previous start left: a process group of its own.
    let mut left = {
        use std::os::unix::process::CommandExt as _;
        std::process::Command::new("sleep")
            .arg("60")
            .process_group(0)
            .spawn()
            .expect("spawn")
    };
    let folder = dir.path().join("work/run-old");
    std::fs::create_dir_all(&folder).expect("folder");
    let state = State {
        loop_pid: Some(1),
        claims: vec![Held {
            run_id: "old".into(),
            workflow_id: "wf-a".into(),
            job: "runs".into(),
            claim_token: "claim-old".into(),
            pid: left.id(),
            started_at: "then".into(),
            folder: folder.clone(),
        }],
        ..State::default()
    };
    state
        .save(&dir.path().join("state/claims.json"))
        .expect("save");

    let mut runner = runner(dir.path(), "true", "", &fake);
    runner.recover().expect("recover");
    let status = left.wait().expect("wait");
    assert!(!status.success(), "the old agent was stopped");
    let released = fake.released.lock().expect("lock").clone();
    assert_eq!(released.len(), 1);
    assert_eq!(released[0].token, "claim-old");
    assert_eq!(released[0].outcome, Outcome::Error);
    assert_eq!(runner.state().claims, Vec::new());
    assert!(!folder.exists());
}

#[test]
fn the_stop_file_drains_and_stop_now_stops_the_agents() {
    let dir = tempfile::tempdir().expect("dir");
    let fake = Arc::new(Fake::default());
    fake.waiting.lock().expect("lock").push_back(claim(3));
    fake.waiting.lock().expect("lock").push_back(claim(4));
    let mut runner = runner(dir.path(), "sleep 30", "", &fake);
    let stop_file = dir.path().join("stop");

    runner.step().expect("step");
    assert_eq!(runner.running(), 1);
    write_stop(&stop_file, Stop::Drain).expect("stop");
    runner.claim_now();
    assert!(
        runner.step().expect("step"),
        "it waits for the running agent"
    );
    assert_eq!(
        fake.claims.lock().expect("lock").len(),
        1,
        "no new claim after stop"
    );

    write_stop(&stop_file, Stop::Now).expect("stop now");
    assert!(!runner.step().expect("step"), "stop now ends the loop");
    assert_eq!(runner.running(), 0);
    let released = fake.released.lock().expect("lock").clone();
    assert_eq!(released[0].token, "claim-3");
    assert_eq!(released[0].outcome, Outcome::Error);

    // With no agent left, a stop ends the loop at once.
    write_stop(&stop_file, Stop::Drain).expect("stop");
    assert!(!runner.step().expect("step"));
}

#[test]
fn the_day_limit_holds_further_claims() {
    let dir = tempfile::tempdir().expect("dir");
    let fake = Arc::new(Fake::default());
    fake.waiting.lock().expect("lock").push_back(claim(5));
    fake.waiting.lock().expect("lock").push_back(claim(6));
    let mut runner = runner(dir.path(), "true", "limits:\n  runs_per_day: 1\n", &fake);

    runner.step().expect("step");
    step_until(&mut runner, |runner| runner.running() == 0);
    runner.claim_now();
    runner.step().expect("step");
    assert_eq!(
        fake.claims.lock().expect("lock").len(),
        1,
        "the second claim waits for tomorrow"
    );
}

#[test]
fn a_claim_that_ended_on_the_server_stops_its_agent_without_a_release() {
    let dir = tempfile::tempdir().expect("dir");
    let fake = Arc::new(Fake::default());
    fake.waiting.lock().expect("lock").push_back(claim(7));
    let mut runner = runner(dir.path(), "sleep 30", "", &fake);

    runner.step().expect("step");
    let pid = runner.state().claims[0].pid;
    *fake.claim_ended.lock().expect("lock") = true;
    step_until(&mut runner, |runner| runner.running() == 0);
    assert!(fake.released.lock().expect("lock").is_empty());
    assert!(!octotrigger::agent::alive(pid));
    assert_eq!(runner.state().claims, Vec::new());
}

#[test]
fn no_start_before_the_token_warning_is_confirmed() {
    let dir = tempfile::tempdir().expect("dir");
    let fake = Arc::new(Fake::default());
    let mut config = config(dir.path(), "true", "");
    config.token_warning_confirmed = None;
    let mut runner = Runner::new(config, Arc::clone(&fake)).expect("runner");
    let refused = runner.start().expect_err("refused");
    assert!(
        refused.to_string().contains("octotrigger setup"),
        "{refused}"
    );
    assert!(fake.claims.lock().expect("lock").is_empty());
}
