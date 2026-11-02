//! The three claim calls of Toolscaled's `/surface`: claim a run, keep the
//! claim, end it. octotrigger calls nothing else on `/surface`. The same
//! sign-in gives each run's agent its Toolscaled token.

use std::path::PathBuf;
use std::time::Duration;

use anyhow::{Result, anyhow};
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};

/// One claimed run, as the claim answer gives it.
#[derive(Clone, Debug, Deserialize, Serialize, PartialEq, Eq)]
pub struct Claim {
    pub run_id: String,
    pub workflow_id: String,
    pub kind: String,
    /// The run's session, which the start prompt names.
    pub code: String,
    pub lease_until: String,
    /// The claim's secret for renew and release.
    pub claim_token: String,
    /// The whole text the agent starts with, as Toolscaled wrote it. The
    /// agent gets it word for word.
    pub start_prompt: String,
}

/// How the agent ended.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum Outcome {
    Done,
    GaveUp,
    Error,
}

/// The agent's model use, as octotrigger read it.
#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct Usage {
    pub input_tokens: u64,
    pub cache_read_tokens: u64,
    pub output_tokens: u64,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub model: Option<String>,
}

impl Usage {
    #[must_use]
    pub const fn total(&self) -> u64 {
        self.input_tokens + self.cache_read_tokens + self.output_tokens
    }
}

/// A refusal of a claim call: the server's code and its words.
#[derive(Debug)]
pub struct Refused {
    pub status: u16,
    pub code: String,
    pub detail: String,
}

impl std::fmt::Display for Refused {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(
            formatter,
            "{} ({}): {}",
            self.code, self.status, self.detail
        )
    }
}

impl std::error::Error for Refused {}

impl Refused {
    /// Whether the claim is gone for good: the run ended, waits for the
    /// person, was taken back, or the lease ran out.
    #[must_use]
    pub fn claim_ended(&self) -> bool {
        self.code == "claim_ended" || self.status == 404 || self.status == 409
    }
}

/// What the runner needs of Toolscaled.
pub trait Surface: Send + Sync {
    /// The oldest run that waits for an agent and fits; `None` when no run
    /// waits.
    ///
    /// # Errors
    ///
    /// A call that fails or is refused.
    fn claim(
        &self,
        workflow_ids: &[String],
        runner: &str,
        lease_seconds: u32,
    ) -> Result<Option<Claim>>;
    /// Moves the end of the lease.
    ///
    /// # Errors
    ///
    /// [`Refused`] with `claim_ended` once the claim is gone.
    fn renew(&self, claim_token: &str, lease_seconds: u32) -> Result<String>;
    /// Ends the claim; the answer says where the run stands.
    ///
    /// # Errors
    ///
    /// A call that fails or is refused.
    fn release(
        &self,
        claim_token: &str,
        outcome: Outcome,
        note: Option<&str>,
        usage: Option<&Usage>,
    ) -> Result<String>;
    /// The access token for one run's agent, with at least `seconds` left.
    ///
    /// # Errors
    ///
    /// A sign-in that ended, or a token that cannot live that long.
    fn agent_token(&self, seconds: i64) -> Result<String>;
}

/// The HTTP form of [`Surface`], with the sign-in of the auth file.
pub struct HttpSurface {
    base_url: String,
    auth_file: PathBuf,
    http: ureq::Agent,
}

impl HttpSurface {
    #[must_use]
    pub fn new(base_url: &str, auth_file: PathBuf) -> Self {
        Self {
            base_url: base_url.trim_end_matches('/').to_owned(),
            auth_file,
            http: ureq::AgentBuilder::new()
                .timeout(Duration::from_secs(60))
                .user_agent(concat!("octotrigger/", env!("CARGO_PKG_VERSION")))
                .build(),
        }
    }

    /// One call, with the version and the workspace added; a 401 refreshes
    /// the sign-in once and calls again.
    fn call(&self, action: &str, mut body: Value) -> Result<Value> {
        let mut forced = false;
        loop {
            let tokens = crate::auth::access_token(&self.auth_file, forced)?;
            body["version"] = json!("v1");
            body["organization_id"] = json!(tokens.organization_id);
            let result = self
                .http
                .post(&format!(
                    "{}/surface/automation_tool/{action}",
                    self.base_url
                ))
                .set("Authorization", &format!("Bearer {}", tokens.access_token))
                .send_json(body.clone());
            match result {
                Ok(response) => return Ok(response.into_json()?),
                Err(ureq::Error::Status(401, _)) if !forced => forced = true,
                Err(ureq::Error::Status(status, response)) => {
                    let answer: Value = response.into_json().unwrap_or(Value::Null);
                    let code = answer["error"]
                        .as_str()
                        .or_else(|| answer["code"].as_str())
                        .unwrap_or("refused")
                        .to_owned();
                    let detail = answer["detail"]
                        .as_str()
                        .or_else(|| answer["message"].as_str())
                        .unwrap_or_default()
                        .to_owned();
                    return Err(Refused {
                        status,
                        code,
                        detail,
                    }
                    .into());
                }
                Err(other) => return Err(anyhow!("{action}: {other}")),
            }
        }
    }
}

impl Surface for HttpSurface {
    fn claim(
        &self,
        workflow_ids: &[String],
        runner: &str,
        lease_seconds: u32,
    ) -> Result<Option<Claim>> {
        let answer = self.call(
            "claim_agent_work",
            json!({
                "kinds": ["agent_run"],
                "workflow_ids": workflow_ids,
                "runner": runner,
                "lease_seconds": lease_seconds,
            }),
        )?;
        match answer.get("claim") {
            None | Some(Value::Null) => Ok(None),
            Some(claim) => Ok(Some(serde_json::from_value(claim.clone())?)),
        }
    }

    fn renew(&self, claim_token: &str, lease_seconds: u32) -> Result<String> {
        let answer = self.call(
            "renew_agent_work",
            json!({"claim_token": claim_token, "lease_seconds": lease_seconds}),
        )?;
        Ok(answer["lease_until"]
            .as_str()
            .unwrap_or_default()
            .to_owned())
    }

    fn release(
        &self,
        claim_token: &str,
        outcome: Outcome,
        note: Option<&str>,
        usage: Option<&Usage>,
    ) -> Result<String> {
        let mut body = json!({"claim_token": claim_token, "outcome": outcome});
        if let Some(note) = note {
            body["note"] = json!(note.chars().take(4_000).collect::<String>());
        }
        if let Some(usage) = usage {
            body["usage"] = json!(usage);
        }
        let answer = self.call("release_agent_work", body)?;
        Ok(answer["run"].as_str().unwrap_or_default().to_owned())
    }

    fn agent_token(&self, seconds: i64) -> Result<String> {
        Ok(crate::auth::fresh_for(&self.auth_file, seconds)?.access_token)
    }
}

impl<T: Surface + ?Sized> Surface for std::sync::Arc<T> {
    fn claim(
        &self,
        workflow_ids: &[String],
        runner: &str,
        lease_seconds: u32,
    ) -> Result<Option<Claim>> {
        (**self).claim(workflow_ids, runner, lease_seconds)
    }

    fn renew(&self, claim_token: &str, lease_seconds: u32) -> Result<String> {
        (**self).renew(claim_token, lease_seconds)
    }

    fn release(
        &self,
        claim_token: &str,
        outcome: Outcome,
        note: Option<&str>,
        usage: Option<&Usage>,
    ) -> Result<String> {
        (**self).release(claim_token, outcome, note, usage)
    }

    fn agent_token(&self, seconds: i64) -> Result<String> {
        (**self).agent_token(seconds)
    }
}
