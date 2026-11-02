//! octotrigger claims Toolscaled runs that wait for an agent and starts the
//! person's own agent CLI on each, with the prompt Toolscaled gives.

pub mod agent;
pub mod auth;
pub mod config;
pub mod log;
pub mod runner;
pub mod state;
pub mod surface;

/// The warning `octotrigger setup` shows, and the person confirms once,
/// before the first start.
pub const TOKEN_WARNING: &str = "\
Token use can grow with no limit.

Each run that octotrigger claims starts your agent, and the agent's model
reads and writes tokens on your plan or your API account until the run ends.
A run can take a million tokens or more; most of them are the model reading
the same context again on each call.

octotrigger's own limits cap what it starts, set in its config:
  - the agent command and model of each job (a cheaper model for simple runs),
  - run_timeout_minutes: the time limit of one run,
  - max_concurrent: the runs at once,
  - limits.runs_per_day: the runs a day,
  - limits.tokens_per_day: the tokens a day of the agents that report them.
An agent that reports no token use counts against runs_per_day only.

The only hard stop is the limit of your plan or of your model provider. Set
it there before you start (for example, turn off extra usage), so a run that
goes wrong stops at that limit instead of costing more.";
