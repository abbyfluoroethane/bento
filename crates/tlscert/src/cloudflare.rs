use std::time::Duration;

use async_trait::async_trait;
use reqwest::header::{ACCEPT, AUTHORIZATION};
use serde::{Deserialize, Serialize};
use tokio::time::{Instant, sleep};

use crate::{DnsProvider, DnsRecord, ProviderError, install_ring_provider};

const API_BASE: &str = "https://api.cloudflare.com/client/v4";
const DNS_JSON_ENDPOINT: &str = "https://cloudflare-dns.com/dns-query";

/// A DNS-01 provider backed by the Cloudflare v4 API.
///
/// Use a scoped API token with `Zone.DNS:Write` on the one zone, never a
/// global API key. The token is sent only as an `Authorization: Bearer`
/// header and is deliberately omitted from debug output.
pub struct CloudflareProvider {
    client: reqwest::Client,
    token: String,
    api_base: String,
    dns_json_endpoint: String,
}

impl std::fmt::Debug for CloudflareProvider {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter
            .debug_struct("CloudflareProvider")
            .field("api_base", &self.api_base)
            .field("dns_json_endpoint", &self.dns_json_endpoint)
            .finish_non_exhaustive()
    }
}

impl CloudflareProvider {
    /// Creates a Cloudflare DNS provider using `api_token`.
    pub fn new(api_token: impl Into<String>) -> Self {
        install_ring_provider();
        Self {
            client: reqwest::Client::new(),
            token: api_token.into(),
            api_base: API_BASE.to_string(),
            dns_json_endpoint: DNS_JSON_ENDPOINT.to_string(),
        }
    }

    #[cfg(test)]
    fn with_endpoints(api_token: &str, api_base: String, dns_json_endpoint: String) -> Self {
        install_ring_provider();
        Self {
            client: reqwest::Client::new(),
            token: api_token.to_string(),
            api_base,
            dns_json_endpoint,
        }
    }

    async fn find_zone(&self, base_domain: &str) -> Result<Zone, ProviderError> {
        let mut candidate = base_domain;
        loop {
            let url = query_url(
                &format!("{}/zones", self.api_base),
                &[("name", candidate), ("status", "active"), ("per_page", "1")],
            )?;
            let response: Envelope<Vec<Zone>> = self
                .request(self.client.get(url))
                .send()
                .await?
                .json()
                .await?;
            let mut zones = response.into_result()?;
            if let Some(zone) = zones.pop() {
                return Ok(zone);
            }
            let Some((_, parent)) = candidate.split_once('.') else {
                return Err(
                    format!("Cloudflare has no active zone containing {base_domain}").into(),
                );
            };
            candidate = parent;
        }
    }

    fn request(&self, request: reqwest::RequestBuilder) -> reqwest::RequestBuilder {
        request.header(AUTHORIZATION, format!("Bearer {}", self.token))
    }

    async fn txt_is_visible(&self, name: &str, value: &str) -> Result<bool, ProviderError> {
        let url = query_url(&self.dns_json_endpoint, &[("name", name), ("type", "TXT")])?;
        let response: DnsJsonResponse = self
            .client
            .get(url)
            .header(ACCEPT, "application/dns-json")
            .send()
            .await?
            .error_for_status()?
            .json()
            .await?;
        if response.status != 0 {
            return Ok(false);
        }
        Ok(response
            .answer
            .unwrap_or_default()
            .iter()
            .any(|answer| answer.record_type == 16 && txt_data(&answer.data) == value))
    }
}

#[async_trait]
impl DnsProvider for CloudflareProvider {
    async fn present(
        &self,
        base_domain: &str,
        name: &str,
        value: &str,
    ) -> Result<DnsRecord, ProviderError> {
        let zone = self.find_zone(base_domain).await?;
        let response: Envelope<Record> = self
            .request(
                self.client
                    .post(format!("{}/zones/{}/dns_records", self.api_base, zone.id)),
            )
            .json(&CreateRecord {
                record_type: "TXT",
                name,
                content: value,
                ttl: 60,
            })
            .send()
            .await?
            .json()
            .await?;
        let record = response.into_result()?;
        Ok(DnsRecord::new(zone.id, record.id, name, value))
    }

    async fn wait_for_propagation(
        &self,
        record: &DnsRecord,
        timeout: Duration,
    ) -> Result<(), ProviderError> {
        let deadline = Instant::now() + timeout;
        loop {
            if self.txt_is_visible(record.name(), record.value()).await? {
                return Ok(());
            }
            if Instant::now() >= deadline {
                return Err(format!(
                    "timed out waiting for TXT record {} to propagate",
                    record.name()
                )
                .into());
            }
            sleep(Duration::from_secs(2).min(deadline.saturating_duration_since(Instant::now())))
                .await;
        }
    }

    async fn cleanup(&self, record: &DnsRecord) -> Result<(), ProviderError> {
        let response: Envelope<DeletedRecord> = self
            .request(self.client.delete(format!(
                "{}/zones/{}/dns_records/{}",
                self.api_base,
                record.provider_scope(),
                record.provider_id()
            )))
            .send()
            .await?
            .json()
            .await?;
        let deleted = response.into_result()?;
        if deleted.id != record.provider_id() {
            return Err("Cloudflare deleted an unexpected DNS record".into());
        }
        Ok(())
    }
}

#[derive(Deserialize)]
struct Envelope<T> {
    success: bool,
    result: T,
    #[serde(default)]
    errors: Vec<ApiMessage>,
}

impl<T> Envelope<T> {
    fn into_result(self) -> Result<T, ProviderError> {
        if self.success {
            return Ok(self.result);
        }
        let message = self
            .errors
            .into_iter()
            .map(|error| format!("{}: {}", error.code, error.message))
            .collect::<Vec<_>>()
            .join(", ");
        Err(format!("Cloudflare API error: {message}").into())
    }
}

#[derive(Deserialize)]
struct ApiMessage {
    code: u64,
    message: String,
}

#[derive(Deserialize)]
struct Zone {
    id: String,
}

#[derive(Deserialize)]
struct Record {
    id: String,
}

#[derive(Deserialize)]
struct DeletedRecord {
    id: String,
}

#[derive(Serialize)]
struct CreateRecord<'a> {
    #[serde(rename = "type")]
    record_type: &'static str,
    name: &'a str,
    content: &'a str,
    ttl: u32,
}

#[derive(Deserialize)]
#[serde(rename_all = "PascalCase")]
struct DnsJsonResponse {
    status: u16,
    answer: Option<Vec<DnsAnswer>>,
}

#[derive(Deserialize)]
struct DnsAnswer {
    #[serde(rename = "type")]
    record_type: u16,
    data: String,
}

fn txt_data(data: &str) -> &str {
    data.strip_prefix('"')
        .and_then(|value| value.strip_suffix('"'))
        .unwrap_or(data)
}

fn query_url(base: &str, pairs: &[(&str, &str)]) -> Result<url::Url, ProviderError> {
    let mut url = url::Url::parse(base)?;
    url.query_pairs_mut().extend_pairs(pairs.iter().copied());
    Ok(url)
}

#[cfg(test)]
mod tests {
    use std::sync::{Arc, Mutex};

    use tokio::io::{AsyncReadExt, AsyncWriteExt};
    use tokio::net::TcpListener;

    use super::{CloudflareProvider, DnsProvider, txt_data};

    #[test]
    fn parses_quoted_dns_json_txt_data() {
        assert_eq!(txt_data("\"challenge-value\""), "challenge-value");
        assert_eq!(txt_data("challenge-value"), "challenge-value");
    }

    #[test]
    fn parses_cloudflare_error_response() {
        let response: super::Envelope<Vec<super::Zone>> = serde_json::from_str(
            r#"{"success":false,"result":[],"errors":[{"code":9109,"message":"Invalid access token"}]}"#,
        )
        .unwrap();
        let error = match response.into_result() {
            Ok(_) => panic!("error response was accepted"),
            Err(error) => error,
        };
        assert_eq!(
            error.to_string(),
            "Cloudflare API error: 9109: Invalid access token"
        );
    }

    #[tokio::test]
    async fn shapes_zone_create_and_delete_requests() {
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let address = listener.local_addr().unwrap();
        let requests = Arc::new(Mutex::new(Vec::new()));
        let captured = requests.clone();
        let server = tokio::spawn(async move {
            let responses = [
                r#"{"success":true,"result":[],"errors":[]}"#,
                r#"{"success":true,"result":[{"id":"zone-id"}],"errors":[]}"#,
                r#"{"success":true,"result":{"id":"record-id"},"errors":[]}"#,
                r#"{"success":true,"result":{"id":"record-id"},"errors":[]}"#,
            ];
            for body in responses {
                let (mut stream, _) = listener.accept().await.unwrap();
                let request = read_request(&mut stream).await;
                captured.lock().unwrap().push(request);
                let response = format!(
                    "HTTP/1.1 200 OK\r\ncontent-type: application/json\r\ncontent-length: {}\r\nconnection: close\r\n\r\n{}",
                    body.len(),
                    body
                );
                stream.write_all(response.as_bytes()).await.unwrap();
            }
        });

        let endpoint = format!("http://{address}");
        let provider = CloudflareProvider::with_endpoints("token-123", endpoint.clone(), endpoint);
        let record = provider
            .present(
                "bento.example.org",
                "_acme-challenge.bento.example.org",
                "challenge-value",
            )
            .await
            .unwrap();
        provider.cleanup(&record).await.unwrap();
        server.await.unwrap();

        let requests = requests.lock().unwrap();
        assert!(
            requests[0]
                .starts_with("GET /zones?name=bento.example.org&status=active&per_page=1 HTTP/1.1")
        );
        assert!(
            requests[1]
                .starts_with("GET /zones?name=example.org&status=active&per_page=1 HTTP/1.1")
        );
        assert!(requests[2].starts_with("POST /zones/zone-id/dns_records HTTP/1.1"));
        assert!(
            requests[2]
                .to_ascii_lowercase()
                .contains("authorization: bearer token-123")
        );
        assert!(requests[2].contains(
            r#"{"type":"TXT","name":"_acme-challenge.bento.example.org","content":"challenge-value","ttl":60}"#
        ));
        assert!(requests[3].starts_with("DELETE /zones/zone-id/dns_records/record-id HTTP/1.1"));
    }

    async fn read_request(stream: &mut tokio::net::TcpStream) -> String {
        let mut data = Vec::new();
        let mut buffer = [0u8; 4096];
        loop {
            let count = stream.read(&mut buffer).await.unwrap();
            if count == 0 {
                break;
            }
            data.extend_from_slice(&buffer[..count]);
            if let Some(header_end) = data.windows(4).position(|window| window == b"\r\n\r\n") {
                let headers = String::from_utf8_lossy(&data[..header_end + 4]);
                let content_length = headers
                    .lines()
                    .find_map(|line| {
                        let (name, value) = line.split_once(':')?;
                        name.eq_ignore_ascii_case("content-length")
                            .then(|| value.trim().parse::<usize>().unwrap())
                    })
                    .unwrap_or(0);
                if data.len() >= header_end + 4 + content_length {
                    break;
                }
            }
        }
        String::from_utf8(data).unwrap()
    }
}
