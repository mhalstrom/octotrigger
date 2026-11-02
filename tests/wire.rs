//! The claim calls on the wire, against a local server in the shape of
//! Toolscaled's `/surface`: the path, the bearer, the version and workspace
//! in each body, the answers, and a refusal.

use std::sync::mpsc;
use std::thread;

use octotrigger::auth::{Tokens, save};
use octotrigger::surface::{HttpSurface, Outcome, Refused, Surface, Usage};
use serde_json::{Value, json};

/// A server that answers each request with the next answer, and sends what
/// it got (path, authorization, body) back to the test.
fn serve(answers: Vec<(u16, Value)>) -> (String, mpsc::Receiver<(String, String, Value)>) {
    let server = tiny_http::Server::http("127.0.0.1:0").expect("server");
    let address = format!("http://{}", server.server_addr());
    let (sender, receiver) = mpsc::channel();
    thread::spawn(move || {
        for (status, answer) in answers {
            let Ok(mut request) = server.recv() else {
                return;
            };
            let mut body = String::new();
            let _ = request.as_reader().read_to_string(&mut body);
            let authorization = request
                .headers()
                .iter()
                .find(|header| header.field.equiv("Authorization"))
                .map(|header| header.value.to_string())
                .unwrap_or_default();
            let _ = sender.send((
                request.url().to_owned(),
                authorization,
                serde_json::from_str(&body).unwrap_or(Value::Null),
            ));
            let response = tiny_http::Response::from_string(answer.to_string())
                .with_status_code(status)
                .with_header(
                    "Content-Type: application/json"
                        .parse::<tiny_http::Header>()
                        .expect("header"),
                );
            let _ = request.respond(response);
        }
    });
    (address, receiver)
}

fn signed_in(dir: &std::path::Path) -> std::path::PathBuf {
    let path = dir.join("auth.json");
    save(
        &path,
        &Tokens {
            client_id: "client".into(),
            access_token: "access-1".into(),
            refresh_token: "refresh-1".into(),
            expires_at: chrono::Utc::now().timestamp() + 3600,
            scope: "mcp:connect".into(),
            organization_id: "org-1".into(),
            token_endpoint: "http://127.0.0.1:9/token".into(),
            resource: "http://127.0.0.1:9/mcp".into(),
        },
    )
    .expect("save");
    path
}

#[test]
fn claim_renew_and_release_go_to_surface_with_the_bearer_and_the_workspace() {
    let dir = tempfile::tempdir().expect("dir");
    let auth = signed_in(dir.path());
    let claim = json!({
        "run_id": "r1", "workflow_id": "w1", "kind": "agent_run", "code": "s1",
        "lease_until": "2026-11-02T10:10:00Z", "claim_token": "c1", "run_token": "t1",
        "prompt": "Connect."
    });
    let (address, got) = serve(vec![
        (
            200,
            json!({"version": "v1", "organization_id": "org-1", "claim": claim}),
        ),
        (
            200,
            json!({"version": "v1", "organization_id": "org-1", "run_id": "r1", "lease_until": "later"}),
        ),
        (
            200,
            json!({"version": "v1", "organization_id": "org-1", "run_id": "r1", "run": "finished"}),
        ),
        (
            200,
            json!({"version": "v1", "organization_id": "org-1", "claim": null}),
        ),
    ]);
    let surface = HttpSurface::new(&address, auth);

    let claimed = surface
        .claim(&["w1".to_owned()], "hermes on test", 600)
        .expect("claim")
        .expect("a run");
    assert_eq!(claimed.run_token, "t1");
    let (path, bearer, body) = got.recv().expect("request");
    assert_eq!(path, "/surface/automation_tool/claim_agent_work");
    assert_eq!(bearer, "Bearer access-1");
    assert_eq!(
        body,
        json!({"version": "v1", "organization_id": "org-1", "kinds": ["agent_run"],
               "workflow_ids": ["w1"], "runner": "hermes on test", "lease_seconds": 600})
    );

    assert_eq!(surface.renew("c1", 600).expect("renew"), "later");
    let (path, _, body) = got.recv().expect("request");
    assert_eq!(path, "/surface/automation_tool/renew_agent_work");
    assert_eq!(body["claim_token"], "c1");

    let usage = Usage {
        input_tokens: 1,
        cache_read_tokens: 2,
        output_tokens: 3,
        model: Some("m".into()),
    };
    let run = surface
        .release("c1", Outcome::GaveUp, Some("stopped"), Some(&usage))
        .expect("release");
    assert_eq!(run, "finished");
    let (path, _, body) = got.recv().expect("request");
    assert_eq!(path, "/surface/automation_tool/release_agent_work");
    assert_eq!(
        body,
        json!({"version": "v1", "organization_id": "org-1", "claim_token": "c1",
               "outcome": "gave_up", "note": "stopped",
               "usage": {"input_tokens": 1, "cache_read_tokens": 2, "output_tokens": 3, "model": "m"}})
    );

    assert!(
        surface
            .claim(&[], "hermes on test", 600)
            .expect("claim")
            .is_none()
    );
}

#[test]
fn a_refused_renew_says_the_claim_ended() {
    let dir = tempfile::tempdir().expect("dir");
    let auth = signed_in(dir.path());
    let (address, _got) = serve(vec![(
        409,
        json!({"error": "claim_ended", "detail": "The run waits for the person."}),
    )]);
    let surface = HttpSurface::new(&address, auth);
    let error = surface.renew("c1", 600).expect_err("refused");
    let refused = error.downcast_ref::<Refused>().expect("a refusal");
    assert!(refused.claim_ended());
    assert_eq!(refused.code, "claim_ended");
}
