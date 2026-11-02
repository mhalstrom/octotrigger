//! Sign-in to Toolscaled with OAuth: the authorization code flow with PKCE
//! and a loopback address, as an MCP client signs in, with the member's MCP
//! scope for the Toolscaled MCP server. The one sign-in serves octotrigger's
//! claim calls and the Toolscaled connection of the agents it starts: before
//! each run octotrigger gives that run's agent a fresh access token. The
//! tokens are kept in a file only the person can read; there is no API key
//! to make or paste.

use std::io::Write as _;
use std::path::Path;
use std::time::Duration;

use anyhow::{Context, Result, anyhow, bail};
use base64::Engine as _;
use base64::engine::general_purpose::URL_SAFE_NO_PAD;
use rand::RngCore as _;
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use sha2::{Digest as _, Sha256};

/// The member's MCP scope: the claim calls and the agent's MCP connection.
pub const MCP_SCOPE: &str = "mcp:connect";

/// What the auth file holds.
#[derive(Clone, Debug, Deserialize, Serialize)]
pub struct Tokens {
    pub client_id: String,
    pub access_token: String,
    pub refresh_token: String,
    /// Seconds since 1970 when the access token ends.
    pub expires_at: i64,
    pub scope: String,
    /// The workspace the person chose on the consent page.
    pub organization_id: String,
    pub token_endpoint: String,
    pub resource: String,
}

#[derive(Debug, Deserialize)]
struct Metadata {
    authorization_endpoint: String,
    token_endpoint: String,
    registration_endpoint: String,
}

#[derive(Debug, Deserialize)]
struct TokenAnswer {
    access_token: String,
    refresh_token: String,
    expires_in: i64,
    #[serde(default)]
    scope: Option<String>,
}

fn agent() -> ureq::Agent {
    ureq::AgentBuilder::new()
        .timeout(Duration::from_secs(30))
        .user_agent(concat!("octotrigger/", env!("CARGO_PKG_VERSION")))
        .build()
}

/// A random URL-safe text of `bytes` random bytes.
fn random(bytes: usize) -> String {
    let mut buffer = vec![0_u8; bytes];
    rand::thread_rng().fill_bytes(&mut buffer);
    URL_SAFE_NO_PAD.encode(buffer)
}

/// The S256 challenge of a PKCE verifier (RFC 7636).
#[must_use]
pub fn challenge(verifier: &str) -> String {
    URL_SAFE_NO_PAD.encode(Sha256::digest(verifier.as_bytes()))
}

fn now() -> i64 {
    chrono::Utc::now().timestamp()
}

/// Signs in through the browser and writes the auth file.
///
/// # Errors
///
/// A server that does not answer, a sign-in that is refused or not done in
/// five minutes, and a file that cannot be written.
pub fn login(
    base_url: &str,
    mcp_url: &str,
    auth_file: &Path,
    open_browser: bool,
) -> Result<Tokens> {
    let http = agent();
    let base = base_url.trim_end_matches('/');
    let metadata: Metadata = http
        .get(&format!("{base}/.well-known/oauth-authorization-server"))
        .call()
        .context("read the sign-in settings of Toolscaled")?
        .into_json()?;
    let scope = MCP_SCOPE;

    let server = tiny_http::Server::http("127.0.0.1:0")
        .map_err(|error| anyhow!("open a local address for the sign-in: {error}"))?;
    let port = server
        .server_addr()
        .to_ip()
        .ok_or_else(|| anyhow!("the local address has no port"))?
        .port();
    let redirect_uri = format!("http://127.0.0.1:{port}/callback");

    let registered: Value = http
        .post(&metadata.registration_endpoint)
        .send_json(json!({
            "client_name": "octotrigger",
            "redirect_uris": [redirect_uri],
            "grant_types": ["authorization_code", "refresh_token"],
            "response_types": ["code"],
            "token_endpoint_auth_method": "none",
        }))
        .context("register octotrigger with Toolscaled")?
        .into_json()?;
    let client_id = registered["client_id"]
        .as_str()
        .ok_or_else(|| anyhow!("the registration answer has no client_id"))?
        .to_owned();

    let verifier = random(48);
    let state = random(24);
    let mut address = url::Url::parse(&metadata.authorization_endpoint)?;
    address
        .query_pairs_mut()
        .append_pair("response_type", "code")
        .append_pair("client_id", &client_id)
        .append_pair("redirect_uri", &redirect_uri)
        .append_pair("code_challenge", &challenge(&verifier))
        .append_pair("code_challenge_method", "S256")
        .append_pair("resource", mcp_url)
        .append_pair("scope", scope)
        .append_pair("state", &state);
    println!("Sign in to Toolscaled in your browser:\n\n  {address}\n");
    if open_browser && open::that(address.as_str()).is_err() {
        println!("The browser did not open. Open the address above.");
    }

    let code = wait_for_code(&server, &state)?;
    let answer: TokenAnswer = http
        .post(&metadata.token_endpoint)
        .send_form(&[
            ("grant_type", "authorization_code"),
            ("code", &code),
            ("code_verifier", &verifier),
            ("client_id", &client_id),
            ("redirect_uri", &redirect_uri),
            ("resource", mcp_url),
        ])
        .context("exchange the sign-in code")?
        .into_json()?;
    let organization_id = workspace_of(&http, mcp_url, &answer.access_token)?;
    let tokens = Tokens {
        client_id,
        access_token: answer.access_token,
        refresh_token: answer.refresh_token,
        expires_at: now() + answer.expires_in,
        scope: answer.scope.unwrap_or_else(|| scope.to_owned()),
        organization_id,
        token_endpoint: metadata.token_endpoint,
        resource: mcp_url.to_owned(),
    };
    save(auth_file, &tokens)?;
    Ok(tokens)
}

/// Waits for the browser's redirect with the code, five minutes at most.
fn wait_for_code(server: &tiny_http::Server, state: &str) -> Result<String> {
    let deadline = std::time::Instant::now() + Duration::from_secs(300);
    loop {
        let left = deadline.saturating_duration_since(std::time::Instant::now());
        if left.is_zero() {
            bail!("the sign-in was not done in five minutes");
        }
        let Some(request) = server.recv_timeout(left)? else {
            continue;
        };
        let address = url::Url::parse(&format!("http://127.0.0.1{}", request.url()))?;
        if address.path() != "/callback" {
            let _ = request.respond(tiny_http::Response::empty(404));
            continue;
        }
        let pairs: std::collections::HashMap<String, String> =
            address.query_pairs().into_owned().collect();
        let page = |text: &str| {
            tiny_http::Response::from_string(format!(
                "<!doctype html><title>octotrigger</title><p>{text}</p>"
            ))
            .with_header(
                "Content-Type: text/html; charset=utf-8"
                    .parse::<tiny_http::Header>()
                    .expect("a header"),
            )
        };
        if pairs.get("state").map(String::as_str) != Some(state) {
            let _ = request.respond(page("This sign-in is not the one octotrigger started."));
            bail!("the sign-in answer does not match the request");
        }
        if let Some(error) = pairs.get("error") {
            let _ = request.respond(page("The sign-in was refused. You can close this tab."));
            bail!("the sign-in was refused: {error}");
        }
        let code = pairs
            .get("code")
            .cloned()
            .ok_or_else(|| anyhow!("the sign-in answer has no code"))?;
        let _ = request.respond(page("octotrigger is signed in. You can close this tab."));
        return Ok(code);
    }
}

/// The workspace of a token, read from the MCP `inspect` answer.
fn workspace_of(http: &ureq::Agent, mcp_url: &str, access_token: &str) -> Result<String> {
    let bearer = format!("Bearer {access_token}");
    let post = |body: Value, session: Option<&str>| -> Result<(Value, Option<String>)> {
        let mut request = http
            .post(mcp_url)
            .set("Authorization", &bearer)
            .set("Accept", "application/json, text/event-stream");
        if let Some(session) = session {
            request = request.set("Mcp-Session-Id", session);
        }
        let response = request.send_json(body)?;
        let session = response.header("Mcp-Session-Id").map(str::to_owned);
        let text = response.into_string()?;
        Ok((json_of(&text)?, session))
    };
    let (_, session) = post(
        json!({"jsonrpc": "2.0", "id": 1, "method": "initialize", "params": {
            "protocolVersion": "2025-06-18",
            "capabilities": {},
            "clientInfo": {"name": "octotrigger", "version": env!("CARGO_PKG_VERSION")}
        }}),
        None,
    )
    .context("connect to Toolscaled")?;
    let (answer, _) = post(
        json!({"jsonrpc": "2.0", "id": 2, "method": "tools/call",
               "params": {"name": "inspect", "arguments": {}}}),
        session.as_deref(),
    )
    .context("read the workspace of the sign-in")?;
    answer
        .pointer("/result/structuredContent/state/identity/organization_id")
        .and_then(Value::as_str)
        .map(str::to_owned)
        .ok_or_else(|| anyhow!("the sign-in names no workspace"))
}

/// A JSON answer, or the last `data:` line of an event stream.
fn json_of(text: &str) -> Result<Value> {
    if let Ok(value) = serde_json::from_str(text) {
        return Ok(value);
    }
    text.lines()
        .rev()
        .filter_map(|line| line.strip_prefix("data:"))
        .find_map(|data| serde_json::from_str(data.trim()).ok())
        .ok_or_else(|| anyhow!("the answer is not JSON"))
}

/// Writes the auth file, readable by the person only.
///
/// # Errors
///
/// A folder or file that cannot be written.
pub fn save(path: &Path, tokens: &Tokens) -> Result<()> {
    if let Some(folder) = path.parent() {
        std::fs::create_dir_all(folder)?;
        set_mode(folder, 0o700)?;
    }
    let partial = path.with_extension("json.partial");
    {
        let mut file = std::fs::File::create(&partial)?;
        set_mode(&partial, 0o600)?;
        file.write_all(&serde_json::to_vec_pretty(tokens)?)?;
        file.sync_all()?;
    }
    std::fs::rename(&partial, path)?;
    Ok(())
}

fn set_mode(path: &Path, mode: u32) -> Result<()> {
    use std::os::unix::fs::PermissionsExt as _;
    std::fs::set_permissions(path, std::fs::Permissions::from_mode(mode))?;
    Ok(())
}

/// Reads the auth file.
///
/// # Errors
///
/// No file (the person has not signed in) or a file that cannot be read.
pub fn load(path: &Path) -> Result<Tokens> {
    let text = std::fs::read_to_string(path).with_context(|| {
        format!(
            "no sign-in in {}: run `octotrigger login` first",
            path.display()
        )
    })?;
    Ok(serde_json::from_str(&text)?)
}

/// The access token, refreshed when it ends in less than a minute. The new
/// refresh token replaces the old one in the file.
///
/// # Errors
///
/// A refresh that the server refuses: the person signs in again.
pub fn access_token(path: &Path, force: bool) -> Result<Tokens> {
    fresh_for(path, if force { i64::MAX } else { 60 })
}

/// The access token with at least `seconds` left, refreshed when it ends
/// sooner. The refresh token rotates, so one refresh at a time: a second
/// octotrigger process (`octotrigger check` beside `start`) waits on the
/// lock and then reads the new token instead of using the old refresh token.
///
/// # Errors
///
/// A refresh that the server refuses (the person signs in again), and a
/// token that ends sooner than `seconds` even after a refresh.
pub fn fresh_for(path: &Path, seconds: i64) -> Result<Tokens> {
    let tokens = load(path)?;
    if tokens.expires_at - now() > seconds {
        return Ok(tokens);
    }
    let _lock = Lock::take(&path.with_extension("lock"))?;
    // Another process may have refreshed while this one waited.
    let mut tokens = load(path)?;
    if seconds != i64::MAX && tokens.expires_at - now() > seconds {
        return Ok(tokens);
    }
    let answer: TokenAnswer = agent()
        .post(&tokens.token_endpoint)
        .send_form(&[
            ("grant_type", "refresh_token"),
            ("refresh_token", &tokens.refresh_token),
            ("client_id", &tokens.client_id),
            ("resource", &tokens.resource),
        ])
        .map_err(|error| match error {
            ureq::Error::Status(400 | 401, _) => {
                anyhow!("the sign-in ended: run `octotrigger login` again")
            }
            other => anyhow!("refresh the sign-in: {other}"),
        })?
        .into_json()?;
    tokens.access_token = answer.access_token;
    tokens.refresh_token = answer.refresh_token;
    tokens.expires_at = now() + answer.expires_in;
    save(path, &tokens)?;
    if seconds != i64::MAX && tokens.expires_at - now() <= seconds {
        bail!(
            "the Toolscaled token lives {} seconds, less than the {seconds} a run needs",
            answer.expires_in
        );
    }
    Ok(tokens)
}

/// An exclusive lock on a file, held until it is dropped.
struct Lock(std::fs::File);

impl Lock {
    fn take(path: &Path) -> Result<Self> {
        use std::os::fd::AsRawFd as _;
        let file = std::fs::OpenOptions::new()
            .create(true)
            .truncate(false)
            .write(true)
            .open(path)
            .with_context(|| format!("open the lock {}", path.display()))?;
        set_mode(path, 0o600)?;
        // SAFETY: flock takes the descriptor of a file this value owns.
        if unsafe { libc::flock(file.as_raw_fd(), libc::LOCK_EX) } != 0 {
            bail!(
                "lock {}: {}",
                path.display(),
                std::io::Error::last_os_error()
            );
        }
        Ok(Self(file))
    }
}

impl Drop for Lock {
    fn drop(&mut self) {
        use std::os::fd::AsRawFd as _;
        // SAFETY: as in take; the file stays open until after this call.
        unsafe {
            libc::flock(self.0.as_raw_fd(), libc::LOCK_UN);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_challenge_is_the_url_safe_sha256_of_the_verifier() {
        // SHA-256 of "abc" (FIPS 180-2) in base64url with no padding.
        assert_eq!(
            challenge("abc"),
            "ungWv48Bz-pBQUDeXa4iI7ADYaOWF3qctBD_YfIAFa0"
        );
    }

    #[test]
    fn a_verifier_is_43_to_128_url_safe_characters() {
        let verifier = random(48);
        assert!((43..=128).contains(&verifier.len()), "{verifier}");
        assert!(
            verifier
                .chars()
                .all(|c| c.is_ascii_alphanumeric() || c == '-' || c == '_')
        );
    }

    #[test]
    fn an_event_stream_answer_reads_as_its_last_data_line() {
        let text = "event: message\ndata: {\"id\":1}\n\ndata: {\"id\":2}\n";
        assert_eq!(json_of(text).expect("json")["id"], 2);
        assert_eq!(json_of("{\"id\":3}").expect("json")["id"], 3);
    }

    #[test]
    fn the_auth_file_is_readable_by_the_person_only() {
        use std::os::unix::fs::PermissionsExt as _;
        let folder = tempfile::tempdir().expect("folder");
        let path = folder.path().join("conf/auth.json");
        let tokens = Tokens {
            client_id: "c".into(),
            access_token: "a".into(),
            refresh_token: "r".into(),
            expires_at: now() + 3600,
            scope: MCP_SCOPE.into(),
            organization_id: "o".into(),
            token_endpoint: "http://127.0.0.1:1/token".into(),
            resource: "http://127.0.0.1:1/mcp".into(),
        };
        save(&path, &tokens).expect("save");
        let mode = std::fs::metadata(&path).expect("file").permissions().mode() & 0o777;
        assert_eq!(mode, 0o600);
        let folder_mode = std::fs::metadata(path.parent().expect("parent"))
            .expect("folder")
            .permissions()
            .mode()
            & 0o777;
        assert_eq!(folder_mode, 0o700);
        // A token with more than a minute left is not refreshed.
        assert_eq!(access_token(&path, false).expect("token").access_token, "a");
        assert_eq!(fresh_for(&path, 1_500).expect("token").access_token, "a");
        // One that ends sooner than a run needs is refreshed; this token
        // endpoint does not answer, so the refresh fails.
        assert!(fresh_for(&path, 7_200).is_err());
    }
}
