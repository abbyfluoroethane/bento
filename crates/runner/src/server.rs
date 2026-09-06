//! The runner's one HTTP endpoint (MULTI-NODE 11.2).

use std::sync::Arc;

use axum::body::Body;
use axum::extract::State;
use axum::extract::rejection::JsonRejection;
use axum::http::{StatusCode, header};
use axum::response::{IntoResponse, Response};
use axum::routing::post;
use axum::{Json, Router};

use crate::{Envelope, Fence, Host, Outcome, serve_one};

#[derive(Clone)]
struct RunnerState {
    fence: Arc<Fence>,
    host: Arc<dyn Host>,
}

/// Builds the runner transport.
///
/// The protocol uses one typed enum. Separate paths would create a second
/// protocol surface with no extra type safety (MULTI-NODE 11.2).
pub fn router(fence: Arc<Fence>, host: Arc<dyn Host>) -> Router {
    Router::new()
        .route("/rpc", post(rpc))
        .with_state(RunnerState { fence, host })
}

async fn rpc(
    State(state): State<RunnerState>,
    envelope: Result<Json<Envelope>, JsonRejection>,
) -> Response {
    let Json(envelope) = match envelope {
        Ok(envelope) => envelope,
        Err(_) => return StatusCode::BAD_REQUEST.into_response(),
    };

    match serve_one(&state.fence, state.host.as_ref(), envelope).await {
        Ok(Outcome::Done(reply)) => (StatusCode::OK, Json(reply)).into_response(),
        Ok(Outcome::Replayed(recorded)) => Response::builder()
            .status(StatusCode::OK)
            .header(header::CONTENT_TYPE, "application/json")
            .body(Body::from(recorded))
            .expect("the static replay response is valid"),
        Ok(Outcome::Refused(refusal)) => (StatusCode::CONFLICT, Json(refusal)).into_response(),
        Err(error) => (
            StatusCode::INTERNAL_SERVER_ERROR,
            Json(serde_json::json!({ "error": error.to_string() })),
        )
            .into_response(),
    }
}

#[cfg(test)]
mod tests {
    use std::time::Duration;

    use axum::body::{Body, to_bytes};
    use axum::http::{Request, header};
    use time::OffsetDateTime;
    use tower::ServiceExt;

    use super::*;
    use crate::{
        Capabilities, Health, HostError, InstanceRef, Inventory, Operation, PROTOCOL_VERSION,
        Refusal, Reply, SqliteFence,
    };

    const MACHINE: &str = "167eeb6836c44115aa084e7780e4328c";
    const OTHER: &str = "ebb80f403ef641deaa486417f2b6992a";

    struct FakeHost;

    #[async_trait::async_trait]
    impl Host for FakeHost {
        async fn health(&self) -> Result<Health, HostError> {
            Ok(Health {
                protocol_version: PROTOCOL_VERSION,
                machine_id: "167eeb6836c44115aa084e7780e4328c".into(),
                hostname: "runner-a.example.org".into(),
                accepted_epoch: 0,
            })
        }

        async fn capabilities(&self) -> Result<Capabilities, HostError> {
            Ok(Capabilities {
                arch: "aarch64".into(),
                cpu_count: 8,
                memory_total_mib: 8192,
                storage_total_gib: 160,
                storage_available_gib: 120,
                hypervisor_version: "libvirt local RPC".into(),
            })
        }

        async fn sample(&self) -> Result<crate::Samples, HostError> {
            Ok(crate::Samples {
                host: crate::HostSample {
                    cpu: None,
                    memory_total_bytes: 8192 * 1024 * 1024,
                    memory_available_bytes: 4096 * 1024 * 1024,
                    storage_total_bytes: 160 * 1024 * 1024 * 1024,
                    storage_available_bytes: 120 * 1024 * 1024 * 1024,
                    cpu_count: 8,
                },
                domains: Vec::new(),
            })
        }

        async fn ensure_image(&self, _: &crate::ImageRequest) -> Result<crate::Reply, HostError> {
            Ok(crate::Reply::ImageReady {
                name: "debian-13".into(),
                checksum: "sha256-00".into(),
                already_present: true,
                size: 0,
            })
        }
        async fn provision(&self, _: &crate::ProvisionRequest) -> Result<crate::Reply, HostError> {
            Ok(crate::Reply::Provisioned {
                state: bento_types::State::Running,
            })
        }
        async fn apply_network(
            &self,
            _: &bento_network::MachineNetwork,
        ) -> Result<crate::Reply, HostError> {
            Ok(crate::Reply::NetworkApplied {
                bridges: 0,
                routes_added: 0,
                routes_removed: 0,
                routes_unchanged: 0,
            })
        }
        async fn start(&self, _: &InstanceRef) -> Result<bento_types::State, HostError> {
            Ok(bento_types::State::Running)
        }
        async fn stop(&self, _: &InstanceRef) -> Result<bento_types::State, HostError> {
            Ok(bento_types::State::Stopped)
        }
        async fn reboot(&self, _: &InstanceRef) -> Result<bento_types::State, HostError> {
            Ok(bento_types::State::Running)
        }
        async fn remove(&self, _: &InstanceRef) -> Result<bento_types::State, HostError> {
            Ok(bento_types::State::Stopped)
        }
        async fn inventory(&self) -> Result<Inventory, HostError> {
            Ok(Inventory { domains: vec![] })
        }
    }

    fn app() -> Router {
        let at = OffsetDateTime::UNIX_EPOCH + Duration::from_secs(1_000);
        let fence =
            Fence::new(MACHINE, Box::new(SqliteFence::in_memory().unwrap())).with_clock(move || at);
        router(Arc::new(fence), Arc::new(FakeHost))
    }

    fn envelope(target_machine_id: &str) -> Envelope {
        Envelope {
            protocol_version: PROTOCOL_VERSION,
            target_machine_id: Some(target_machine_id.into()),
            epoch: 1,
            holder_id: "controller-a".into(),
            lease_expires_at: OffsetDateTime::UNIX_EPOCH + Duration::from_secs(1_030),
            sent_at: OffsetDateTime::UNIX_EPOCH + Duration::from_secs(1_000),
            object: None,
            request_id: "request-1".into(),
            op: Operation::Capabilities,
        }
    }

    async fn post(body: impl Into<Body>) -> Response {
        app()
            .oneshot(
                Request::post("/rpc")
                    .header(header::CONTENT_TYPE, "application/json")
                    .body(body.into())
                    .unwrap(),
            )
            .await
            .unwrap()
    }

    #[tokio::test]
    async fn a_permitted_request_returns_the_typed_reply() {
        let response = post(serde_json::to_vec(&envelope(MACHINE)).unwrap()).await;
        assert_eq!(response.status(), StatusCode::OK);
        let body = to_bytes(response.into_body(), usize::MAX).await.unwrap();
        let reply: Reply = serde_json::from_slice(&body).unwrap();
        assert!(matches!(
            reply,
            Reply::Capabilities(Capabilities { cpu_count: 8, .. })
        ));
    }

    #[tokio::test]
    async fn a_refused_request_returns_conflict_and_the_refusal() {
        let response = post(serde_json::to_vec(&envelope(OTHER)).unwrap()).await;
        assert_eq!(response.status(), StatusCode::CONFLICT);
        let body = to_bytes(response.into_body(), usize::MAX).await.unwrap();
        let refusal: Refusal = serde_json::from_slice(&body).unwrap();
        assert_eq!(
            refusal,
            Refusal::WrongMachine {
                yours: OTHER.into(),
                theirs: MACHINE.into(),
            }
        );
    }

    #[tokio::test]
    async fn malformed_json_returns_bad_request() {
        let response = post("{not json").await;
        assert_eq!(response.status(), StatusCode::BAD_REQUEST);
    }
}
