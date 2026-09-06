//! The controller side of the runner transport (MULTI-NODE 11.2).

use crate::{Envelope, Refusal, Reply};

/// A client for one runner endpoint.
#[derive(Clone, Debug)]
pub struct RunnerClient {
    client: reqwest::Client,
    endpoint: String,
}

impl RunnerClient {
    pub fn new(endpoint: impl Into<String>) -> Self {
        Self {
            client: reqwest::Client::new(),
            endpoint: endpoint.into(),
        }
    }

    /// Sends one typed request to the runner's only endpoint.
    pub async fn call(&self, envelope: Envelope) -> Result<Reply, ClientError> {
        let url = format!("{}/rpc", self.endpoint.trim_end_matches('/'));
        let response = self.client.post(url).json(&envelope).send().await?;
        let status = response.status();
        let body = response.bytes().await?;

        decode_response(status, &body)
    }
}

fn decode_response(status: reqwest::StatusCode, body: &[u8]) -> Result<Reply, ClientError> {
    if status == reqwest::StatusCode::CONFLICT {
        let refusal = serde_json::from_slice(body)
            .map_err(|source| ClientError::InvalidBody { status, source })?;
        return Err(ClientError::Refused(refusal));
    }
    if status != reqwest::StatusCode::OK {
        return Err(ClientError::UnexpectedStatus {
            status,
            body: String::from_utf8_lossy(body).into_owned(),
        });
    }
    serde_json::from_slice(body).map_err(|source| ClientError::InvalidBody { status, source })
}

/// A failure to reach a runner is different from a refusal by that runner.
#[derive(Debug, thiserror::Error)]
pub enum ClientError {
    #[error("runner transport: {0}")]
    Transport(#[from] reqwest::Error),
    #[error("runner refused the request: {0}")]
    Refused(Refusal),
    #[error("runner answered HTTP {status}: {body}")]
    UnexpectedStatus {
        status: reqwest::StatusCode,
        body: String,
    },
    #[error("runner answered invalid JSON with HTTP {status}: {source}")]
    InvalidBody {
        status: reqwest::StatusCode,
        #[source]
        source: serde_json::Error,
    },
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_refusal_is_not_a_transport_failure() {
        let refusal = Refusal::WrongMachine {
            yours: "ebb80f403ef641deaa486417f2b6992a".into(),
            theirs: "167eeb6836c44115aa084e7780e4328c".into(),
        };
        let body = serde_json::to_vec(&refusal).unwrap();
        let result = decode_response(reqwest::StatusCode::CONFLICT, &body);

        assert!(matches!(result, Err(ClientError::Refused(got)) if got == refusal));
    }
}
