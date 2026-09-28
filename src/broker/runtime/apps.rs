//! Workspace app requests, handled inside the runtime loop. Every request is
//! checked against the host grant and the run phase before any Docker call.

use super::*;
use crate::broker::resources::ProbeKind;
use crate::docker::{AppBuild, AppInputs, OwnedProbeError, PreflightError};
use serde_json::{Value, json};
use std::collections::BTreeMap;

/// Per-run app state that is not in the durable manifest: built images and
/// finished build/stop answers for request-ID replay.
#[derive(Default)]
pub(super) struct AppRegistry {
    images: BTreeMap<String, ImmutableImageId>,
    answers: BTreeMap<String, (String, (u16, Value))>,
}

pub(super) enum Replay {
    New,
    Answered((u16, Value)),
    Conflict,
}

impl AppRegistry {
    /// An identical retry gets the recorded answer; a reused ID with other
    /// content is a conflict. Nothing is executed twice.
    pub(super) fn replay(&self, request_id: &str, digest: &str) -> Replay {
        match self.answers.get(request_id) {
            None => Replay::New,
            Some((recorded, answer)) if recorded == digest => Replay::Answered(answer.clone()),
            Some(_) => Replay::Conflict,
        }
    }
    pub(super) fn record(&mut self, request_id: &str, digest: String, answer: (u16, Value)) {
        self.answers.insert(request_id.to_owned(), (digest, answer));
    }
}

fn error(status: u16, code: &str) -> (u16, Value) {
    (status, json!({ "error": code }))
}

/// Manifest request IDs for app runs, disjoint from the runtime's own.
fn run_request(request_id: &str) -> String {
    format!("app-{request_id}")
}

impl BrokerRuntime {
    pub(super) fn app_request(&mut self, request: ApiRequest) -> (u16, Value) {
        let action = match &request {
            ApiRequest::Status => Action::Status,
            ApiRequest::Build { .. } => Action::Build,
            ApiRequest::Run { .. } => Action::Run,
            ApiRequest::AppStatus { .. } => Action::Readiness,
            ApiRequest::Logs { .. } => Action::Logs,
            ApiRequest::Stop { .. } => Action::Stop,
        };
        // Run alone is the managed-Pi grant; app work needs the workspace
        // ceiling, which is the only grant that includes Build.
        if !self.grant.permits(Action::Build) || !self.grant.permits(action) {
            return error(403, "forbidden");
        }
        if self.phase != RuntimePhase::Ready || self.shutdown.is_requested() {
            return error(503, "not_ready");
        }
        match request {
            ApiRequest::Status => error(400, "bad_request"),
            ApiRequest::Build {
                request_id,
                app,
                dockerfile,
                context,
            } => {
                let digest = format!("build\0{app}\0{dockerfile}\0{context}");
                match self.apps.replay(&request_id, &digest) {
                    Replay::Answered(answer) => return answer,
                    Replay::Conflict => return error(409, "request_id_reused"),
                    Replay::New => {}
                }
                let answer = self.build(&app, &dockerfile, &context);
                self.apps.record(&request_id, digest, answer.clone());
                answer
            }
            ApiRequest::Run {
                request_id,
                app,
                command,
            } => self.run(&request_id, &app, &command),
            ApiRequest::AppStatus { app } => self.status(&app),
            ApiRequest::Logs { app, tail } => self.logs(&app, tail),
            ApiRequest::Stop { request_id, app } => {
                let digest = format!("stop\0{app}");
                match self.apps.replay(&request_id, &digest) {
                    Replay::Answered(answer) => return answer,
                    Replay::Conflict => return error(409, "request_id_reused"),
                    Replay::New => {}
                }
                let answer = self.stop(&app);
                self.apps.record(&request_id, digest, answer.clone());
                answer
            }
        }
    }

    fn build(&mut self, app: &str, dockerfile: &str, context: &str) -> (u16, Value) {
        let (Some(docker), Some(stage_root)) = (self.docker.as_mut(), &self.setup.stage_root)
        else {
            return error(503, "not_ready");
        };
        match docker.build_app(
            AppBuild {
                workspace: &self.setup.workspace,
                run_id: &self.setup.run_id,
                app,
                dockerfile,
                context,
            },
            stage_root,
        ) {
            Ok(image) => {
                let answer = (200, json!({"app": app, "image": image.as_str()}));
                self.apps.images.insert(app.to_owned(), image);
                answer
            }
            Err(PreflightError::InvalidInput) => error(400, "invalid_request"),
            Err(PreflightError::Unsupported) => error(422, "image_rejected"),
            Err(e) => (
                422,
                json!({"error": "build_failed", "detail": format!("{e:?}")}),
            ),
        }
    }

    /// The latest recorded run of `app`, if any.
    fn latest_run(&self, app: &str) -> Option<crate::broker::resources::Resource> {
        self.manifest
            .as_ref()?
            .resources()
            .iter()
            .rev()
            .find_map(|r| match &r.spec.operation {
                ProbeKind::App { logical, .. } if logical == app => Some(r.clone()),
                _ => None,
            })
    }

    fn run(&mut self, request_id: &str, app: &str, command: &[String]) -> (u16, Value) {
        let request = run_request(request_id);
        // A durable run record makes retries idempotent across the run.
        if let Some(existing) = self.manifest.as_ref().and_then(|m| {
            m.resources()
                .iter()
                .find(|r| r.request_id == request)
                .cloned()
        }) {
            return match &existing.spec.operation {
                ProbeKind::App { logical, .. } if logical == app => self.status(app),
                _ => error(409, "request_id_reused"),
            };
        }
        let Some(image) = self.apps.images.get(app).cloned() else {
            return error(404, "not_built");
        };
        if self.latest_run(app).is_some_and(|r| !r.removed) {
            return error(409, "already_running");
        }
        let (Some(docker), Some(manifest), Some(network)) = (
            self.docker.as_mut(),
            self.manifest.as_mut(),
            self.network.as_ref(),
        ) else {
            return error(503, "not_ready");
        };
        match docker.start_app(
            manifest,
            &request,
            AppInputs {
                image: &image,
                app,
                network,
                command,
            },
        ) {
            Ok(host) => (200, json!({"app": app, "host": host, "running": true})),
            Err(OwnedProbeError::Docker(PreflightError::InvalidInput)) => {
                error(400, "invalid_request")
            }
            Err(_)
                if self
                    .latest_run(app)
                    .is_some_and(|r| r.request_id == request) =>
            {
                // Recorded but not running: report what it did.
                let (status, mut body) = self.status(app);
                if status == 200 {
                    body["error"] = "not_running".into();
                    return (422, body);
                }
                (status, body)
            }
            // Static diagnostic labels only; never daemon output or paths.
            Err(e) => (
                422,
                json!({"error": "run_failed", "detail": format!("{e:?}")}),
            ),
        }
    }

    fn status(&mut self, app: &str) -> (u16, Value) {
        let Some(record) = self.latest_run(app) else {
            return error(404, "not_running");
        };
        let ProbeKind::App { host, .. } = &record.spec.operation else {
            return error(404, "not_running");
        };
        if record.removed {
            return (
                200,
                json!({"app": app, "host": host, "running": false, "stopped": true}),
            );
        }
        let (Some(docker), Some(manifest)) = (self.docker.as_mut(), self.manifest.as_ref()) else {
            return error(503, "not_ready");
        };
        match docker.app_status(manifest, &record.request_id) {
            Ok(state) => (
                200,
                json!({
                    "app": app,
                    "host": host,
                    "running": state.running,
                    "exit_code": state.exit_code,
                    "health": state.health,
                }),
            ),
            Err(_) => error(422, "status_unavailable"),
        }
    }

    fn logs(&mut self, app: &str, tail: u16) -> (u16, Value) {
        let Some(record) = self.latest_run(app).filter(|r| !r.removed) else {
            return error(404, "not_running");
        };
        let (Some(docker), Some(manifest)) = (self.docker.as_mut(), self.manifest.as_ref()) else {
            return error(503, "not_ready");
        };
        match docker.app_logs(manifest, &record.request_id, tail) {
            Ok(logs) => (
                200,
                json!({"app": app, "text": logs.text, "truncated": logs.truncated}),
            ),
            Err(_) => error(422, "logs_unavailable"),
        }
    }

    fn stop(&mut self, app: &str) -> (u16, Value) {
        let Some(record) = self.latest_run(app) else {
            return error(404, "not_running");
        };
        let (Some(docker), Some(manifest)) = (self.docker.as_mut(), self.manifest.as_mut()) else {
            return error(503, "not_ready");
        };
        match docker.stop_app(manifest, &record.request_id) {
            Ok(()) => (200, json!({"app": app, "stopped": true})),
            Err(_) => error(422, "stop_failed"),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn identical_retries_replay_and_reused_ids_conflict() {
        let mut registry = AppRegistry::default();
        assert!(matches!(registry.replay("b-1", "build\0api"), Replay::New));
        registry.record("b-1", "build\0api".into(), (200, json!({"ok": true})));
        assert!(matches!(
            registry.replay("b-1", "build\0api"),
            Replay::Answered((200, _))
        ));
        assert!(matches!(
            registry.replay("b-1", "build\0web"),
            Replay::Conflict
        ));
    }
}
