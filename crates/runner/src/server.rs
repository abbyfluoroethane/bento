//! The runner's one HTTP endpoint (MULTI-NODE 11.2).

use std::sync::Arc;

use axum::body::Body;
use axum::extract::State;
use axum::extract::rejection::JsonRejection;
use axum::http::{StatusCode, header};
use axum::response::{IntoResponse, Response};
use axum::routing::post;
use axum::{Json, Router};

use crate::{Envelope, Fence, Host, HostError, Outcome, serve_one};

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

    // The work runs in its own task. The controller drops a request on a
    // timeout, and axum then drops this future; a change cut off between
    // the host act and its record would leak files or run again on the
    // retry (MULTI-NODE 11.3, 14.2).
    let task =
        tokio::spawn(async move { serve_one(&state.fence, state.host.as_ref(), envelope).await });
    let served = match task.await {
        Ok(served) => served,
        Err(error) => Err(HostError::new(format!("the request task failed: {error}"))),
    };
    match served {
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
        RedefineRequest, Refusal, Reply, SqliteFence,
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
                boot_id: "boot-1".into(),
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
        async fn redefine(&self, _: &RedefineRequest) -> Result<bento_types::State, HostError> {
            Ok(bento_types::State::Stopped)
        }
        async fn resize_disk(
            &self,
            _: &InstanceRef,
            _: u64,
        ) -> Result<bento_types::State, HostError> {
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

    /// A host whose start waits for a release, so a test can drop the
    /// request while the host acts.
    struct Gated {
        gate: tokio::sync::Notify,
        finished: std::sync::atomic::AtomicBool,
    }

    #[async_trait::async_trait]
    impl Host for Gated {
        async fn health(&self) -> Result<Health, HostError> {
            FakeHost.health().await
        }
        async fn capabilities(&self) -> Result<Capabilities, HostError> {
            FakeHost.capabilities().await
        }
        async fn sample(&self) -> Result<crate::Samples, HostError> {
            FakeHost.sample().await
        }
        async fn ensure_image(&self, i: &crate::ImageRequest) -> Result<crate::Reply, HostError> {
            FakeHost.ensure_image(i).await
        }
        async fn provision(&self, p: &crate::ProvisionRequest) -> Result<crate::Reply, HostError> {
            FakeHost.provision(p).await
        }
        async fn apply_network(
            &self,
            n: &bento_network::MachineNetwork,
        ) -> Result<crate::Reply, HostError> {
            FakeHost.apply_network(n).await
        }
        async fn start(&self, _: &InstanceRef) -> Result<bento_types::State, HostError> {
            self.gate.notified().await;
            self.finished
                .store(true, std::sync::atomic::Ordering::SeqCst);
            Ok(bento_types::State::Running)
        }
        async fn stop(&self, i: &InstanceRef) -> Result<bento_types::State, HostError> {
            FakeHost.stop(i).await
        }
        async fn reboot(&self, i: &InstanceRef) -> Result<bento_types::State, HostError> {
            FakeHost.reboot(i).await
        }
        async fn remove(&self, i: &InstanceRef) -> Result<bento_types::State, HostError> {
            FakeHost.remove(i).await
        }
        async fn redefine(&self, r: &RedefineRequest) -> Result<bento_types::State, HostError> {
            FakeHost.redefine(r).await
        }
        async fn resize_disk(
            &self,
            i: &InstanceRef,
            g: u64,
        ) -> Result<bento_types::State, HostError> {
            FakeHost.resize_disk(i, g).await
        }
        async fn inventory(&self) -> Result<Inventory, HostError> {
            FakeHost.inventory().await
        }
    }

    #[tokio::test]
    async fn a_change_finishes_and_is_recorded_when_the_controller_goes_away() {
        // The controller drops a request on a timeout. The host act and
        // its record must not stop half-way (MULTI-NODE 11.3).
        let at = OffsetDateTime::UNIX_EPOCH + Duration::from_secs(1_000);
        let fence = Arc::new(
            Fence::new(MACHINE, Box::new(SqliteFence::in_memory().unwrap())).with_clock(move || at),
        );
        let host = Arc::new(Gated {
            gate: tokio::sync::Notify::new(),
            finished: std::sync::atomic::AtomicBool::new(false),
        });
        let app = router(fence.clone(), host.clone());

        let mut change = envelope(MACHINE);
        change.request_id = "request-gone".into();
        change.object = Some(crate::ObjectFence {
            object_id: "uuid-web".into(),
            generation: 1,
            digest: "digest-a".into(),
        });
        change.op = Operation::StartInstance {
            instance: InstanceRef {
                uuid: "uuid-web".into(),
                name: "web".into(),
            },
        };
        let request = Request::post("/rpc")
            .header(header::CONTENT_TYPE, "application/json")
            .body(Body::from(serde_json::to_vec(&change).unwrap()))
            .unwrap();

        let gone = tokio::time::timeout(Duration::from_millis(100), app.oneshot(request)).await;
        assert!(gone.is_err(), "the host must still be waiting");

        host.gate.notify_one();
        for _ in 0..100 {
            if fence.outcome("request-gone").unwrap().is_some() {
                break;
            }
            tokio::time::sleep(Duration::from_millis(20)).await;
        }
        assert!(host.finished.load(std::sync::atomic::Ordering::SeqCst));
        assert!(fence.outcome("request-gone").unwrap().is_some());
    }
}
