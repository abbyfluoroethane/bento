use std::collections::HashMap;
use std::ffi::OsString;
use std::future::Future;
use std::io;
use std::pin::Pin;
use std::sync::{Arc, Mutex};

use async_trait::async_trait;
use bento_types::{Image, ImageVersion};
use reqwest::{Request, Response, StatusCode};
use sha2::{Digest, Sha256};
use tracing::field::{Field, Visit};
use tracing::span::{Attributes, Id, Record};
use tracing::{Event, Metadata, Subscriber};

use crate::{DB, Doer, DynError, RunError, Runner};

pub(crate) fn sha256_hex(content: &str) -> String {
    hex::encode(Sha256::digest(content.as_bytes()))
}

#[derive(Clone, Default)]
pub(crate) struct FakeDb(Arc<Mutex<FakeDbState>>);

#[derive(Default)]
struct FakeDbState {
    images: Vec<Image>,
    versions: HashMap<String, ImageVersion>,
    in_use: HashMap<String, bool>,
    inserted: Vec<ImageVersion>,
    deleted: Vec<String>,
}

impl FakeDb {
    pub(crate) fn new() -> Self {
        Self::default()
    }

    pub(crate) fn set_images(&self, images: Vec<Image>) {
        self.0.lock().expect("db").images = images;
    }

    pub(crate) fn images_snapshot(&self) -> Vec<Image> {
        self.0.lock().expect("db").images.clone()
    }

    pub(crate) fn add_version(&self, version: ImageVersion) {
        self.0
            .lock()
            .expect("db")
            .versions
            .insert(version.checksum.clone(), version);
    }

    pub(crate) fn set_in_use(&self, checksum: &str, in_use: bool) {
        self.0
            .lock()
            .expect("db")
            .in_use
            .insert(checksum.to_owned(), in_use);
    }

    pub(crate) fn inserted(&self) -> Vec<ImageVersion> {
        self.0.lock().expect("db").inserted.clone()
    }

    pub(crate) fn deleted(&self) -> Vec<String> {
        self.0.lock().expect("db").deleted.clone()
    }

    pub(crate) fn has_version(&self, checksum: &str) -> bool {
        self.0.lock().expect("db").versions.contains_key(checksum)
    }
}

#[async_trait]
impl DB for FakeDb {
    async fn images(&self) -> std::result::Result<Vec<Image>, DynError> {
        Ok(self.images_snapshot())
    }

    async fn has_image_version(&self, checksum: &str) -> std::result::Result<bool, DynError> {
        Ok(self.has_version(checksum))
    }

    async fn insert_image_version(
        &self,
        version: ImageVersion,
    ) -> std::result::Result<(), DynError> {
        let mut state = self.0.lock().expect("db");
        if state.versions.contains_key(&version.checksum) {
            return Err(io::Error::other(format!("duplicate version {}", version.checksum)).into());
        }
        state
            .versions
            .insert(version.checksum.clone(), version.clone());
        state.inserted.push(version);
        Ok(())
    }

    async fn set_current_checksum(
        &self,
        image_name: &str,
        checksum: &str,
    ) -> std::result::Result<(), DynError> {
        let mut state = self.0.lock().expect("db");
        let image = state
            .images
            .iter_mut()
            .find(|image| image.name == image_name)
            .ok_or_else(|| io::Error::other(format!("no image {image_name}")))?;
        image.current_checksum = Some(checksum.to_owned());
        Ok(())
    }

    async fn image_versions(&self) -> std::result::Result<Vec<ImageVersion>, DynError> {
        Ok(self
            .0
            .lock()
            .expect("db")
            .versions
            .values()
            .cloned()
            .collect())
    }

    async fn delete_image_version(&self, checksum: &str) -> std::result::Result<(), DynError> {
        let mut state = self.0.lock().expect("db");
        state.versions.remove(checksum);
        state.deleted.push(checksum.to_owned());
        Ok(())
    }

    async fn checksum_in_use(&self, checksum: &str) -> std::result::Result<bool, DynError> {
        Ok(*self
            .0
            .lock()
            .expect("db")
            .in_use
            .get(checksum)
            .unwrap_or(&false))
    }
}

pub(crate) struct FakeResponse {
    status: StatusCode,
    body: String,
}

impl FakeResponse {
    pub(crate) fn ok(body: &str) -> Self {
        Self {
            status: StatusCode::OK,
            body: body.to_owned(),
        }
    }

    pub(crate) fn status(status: u16) -> Self {
        Self {
            status: StatusCode::from_u16(status).expect("valid status"),
            body: String::new(),
        }
    }

    fn into_response(self) -> Response {
        http::Response::builder()
            .status(self.status)
            .body(self.body)
            .expect("fake response")
            .into()
    }
}

type Callback =
    Arc<dyn Fn() -> Pin<Box<dyn Future<Output = crate::Result<()>> + Send>> + Send + Sync>;

#[derive(Clone)]
pub(crate) struct FakeClient {
    responses: Arc<Mutex<HashMap<String, FakeResponse>>>,
    callback: Option<Callback>,
}

impl FakeClient {
    pub(crate) fn new(responses: HashMap<String, FakeResponse>) -> Self {
        Self {
            responses: Arc::new(Mutex::new(responses)),
            callback: None,
        }
    }

    pub(crate) fn with_response(url: &str, response: FakeResponse) -> Self {
        Self::new(HashMap::from([(url.to_owned(), response)]))
    }

    pub(crate) fn with_callback<F, Fut>(url: &str, response: FakeResponse, callback: F) -> Self
    where
        F: Fn() -> Fut + Send + Sync + 'static,
        Fut: Future<Output = crate::Result<()>> + Send + 'static,
    {
        Self {
            responses: Arc::new(Mutex::new(HashMap::from([(url.to_owned(), response)]))),
            callback: Some(Arc::new(move || Box::pin(callback()))),
        }
    }
}

#[async_trait]
impl Doer for FakeClient {
    async fn do_request(&self, request: Request) -> std::result::Result<Response, DynError> {
        if let Some(callback) = &self.callback {
            callback()
                .await
                .map_err(|error| -> DynError { Box::new(error) })?;
        }
        let response = self
            .responses
            .lock()
            .expect("responses")
            .remove(request.url().as_str())
            .unwrap_or_else(|| FakeResponse::status(404));
        Ok(response.into_response())
    }
}

#[derive(Clone, Default)]
pub(crate) struct FakeRunner(Arc<Mutex<FakeRunnerState>>);

#[derive(Default)]
struct FakeRunnerState {
    calls: Vec<Vec<String>>,
    failure: Option<(usize, RunError)>,
}

impl FakeRunner {
    pub(crate) fn failing(index: usize, error: RunError) -> Self {
        Self(Arc::new(Mutex::new(FakeRunnerState {
            calls: Vec::new(),
            failure: Some((index, error)),
        })))
    }

    pub(crate) fn calls(&self) -> Vec<Vec<String>> {
        self.0.lock().expect("runner").calls.clone()
    }
}

#[async_trait]
impl Runner for FakeRunner {
    async fn run(&self, name: &str, args: &[OsString]) -> std::result::Result<Vec<u8>, RunError> {
        let mut state = self.0.lock().expect("runner");
        let mut call = vec![name.to_owned()];
        call.extend(args.iter().map(|arg| arg.to_string_lossy().into_owned()));
        let index = state.calls.len();
        state.calls.push(call);
        if state
            .failure
            .as_ref()
            .is_some_and(|(fail_index, _)| *fail_index == index)
        {
            return Err(state.failure.take().expect("failure checked").1);
        }
        Ok(Vec::new())
    }
}

pub(crate) struct RecordingSubscriber {
    logs: Arc<Mutex<Vec<String>>>,
}

impl RecordingSubscriber {
    pub(crate) fn new(logs: Arc<Mutex<Vec<String>>>) -> Self {
        Self { logs }
    }
}

struct FieldVisitor(String);

impl Visit for FieldVisitor {
    fn record_debug(&mut self, field: &Field, value: &dyn std::fmt::Debug) {
        self.0.push_str(&format!(" {}={value:?}", field.name()));
    }
}

impl Subscriber for RecordingSubscriber {
    fn enabled(&self, _metadata: &Metadata<'_>) -> bool {
        true
    }

    fn new_span(&self, _span: &Attributes<'_>) -> Id {
        Id::from_u64(1)
    }

    fn record(&self, _span: &Id, _values: &Record<'_>) {}

    fn record_follows_from(&self, _span: &Id, _follows: &Id) {}

    fn event(&self, event: &Event<'_>) {
        let mut visitor = FieldVisitor(format!("{}", event.metadata().level()));
        event.record(&mut visitor);
        self.logs.lock().expect("logs").push(visitor.0);
    }

    fn enter(&self, _span: &Id) {}

    fn exit(&self, _span: &Id) {}
}
