#![allow(clippy::disallowed_types)]
/// This module contains the core state of the PocketIc server.
/// Axum handlers operate on a global state of type ApiState, whose
/// interface guarantees consistency and determinism.
use crate::pocket_ic::{
    AdvanceTimeAndTick, ApiResponse, EffectivePrincipal, PocketIc, ProcessCanisterHttpInternal,
    SetCertifiedTime,
};
use crate::state_api::canister_id::{self, DomainResolver, ResolvesDomain};
use crate::{InstanceId, OpId, Operation};
use async_trait::async_trait;
use axum::{
    extract::{Request as AxumRequest, State},
    response::{IntoResponse, Response},
    routing::{get, post},
    Router,
};
use axum_server::tls_rustls::RustlsConfig;
use axum_server::Handle;
use base64;
use fqdn::{fqdn, FQDN};
use futures::future::Shared;
use http::{
    header::{
        ACCEPT_RANGES, CACHE_CONTROL, CONTENT_LENGTH, CONTENT_RANGE, CONTENT_TYPE, COOKIE, DNT,
        IF_MODIFIED_SINCE, IF_NONE_MATCH, RANGE, USER_AGENT,
    },
    HeaderName, Method, StatusCode,
};
use ic_gateway::ic_bn_lib::http::{
    body::buffer_body,
    headers::{X_IC_CANISTER_ID, X_REQUESTED_WITH, X_REQUEST_ID},
    proxy::proxy,
    Client, Error as IcBnError,
};
use ic_http_gateway::{CanisterRequest, HttpGatewayClient, HttpGatewayRequestArgs};
use ic_types::{canister_http::CanisterHttpRequestId, CanisterId, NodeId, PrincipalId, SubnetId};
use itertools::Itertools;
use pocket_ic::common::rest::{
    CanisterHttpRequest, HttpGatewayBackend, HttpGatewayConfig, HttpGatewayDetails,
    HttpGatewayInfo, Topology,
};
use pocket_ic::RejectResponse;
use reqwest::Url;
use serde::{Deserialize, Serialize};
use std::{
    collections::HashMap,
    path::PathBuf,
    str::FromStr,
    sync::atomic::AtomicU64,
    sync::Arc,
    time::{Duration, SystemTime},
};
use strum::Display;
use tokio::{
    sync::mpsc::error::TryRecvError,
    sync::mpsc::Receiver,
    sync::{mpsc, Mutex, RwLock},
    task::{spawn, spawn_blocking, JoinHandle, JoinSet},
    time::{self, sleep},
};
use tower::ServiceExt;
use tower_http::cors::{Any, CorsLayer};
use tracing::{debug, error, trace};

// The maximum wait time for a computation to finish synchronously.
const DEFAULT_SYNC_WAIT_DURATION: Duration = Duration::from_secs(10);

// The timeout for executing an operation in auto progress mode.
const AUTO_PROGRESS_OPERATION_TIMEOUT: Duration = Duration::from_secs(10);
// The minimum delay between consecutive attempts to run an operation in auto progress mode.
const MIN_OPERATION_DELAY: Duration = Duration::from_millis(100);
// The minimum delay between consecutive attempts to read the graph in auto progress mode.
const READ_GRAPH_DELAY: Duration = Duration::from_millis(100);

pub const STATE_LABEL_HASH_SIZE: usize = 16;

/// Uniquely identifies a state.
#[derive(Clone, Eq, PartialEq, Ord, PartialOrd, Hash, Default)]
pub struct StateLabel(pub [u8; STATE_LABEL_HASH_SIZE]);

impl StateLabel {
    pub fn new(seed: u64) -> Self {
        let mut seq_no: u128 = seed.into();
        seq_no <<= 64;
        Self(seq_no.to_le_bytes())
    }

    pub fn bump(&mut self) {
        let mut seq_no: u128 = u128::from_le_bytes(self.0);
        seq_no += 1;
        self.0 = seq_no.to_le_bytes();
    }
}

// The only error condition is if the vector has the wrong size.
pub struct InvalidSize;

impl std::fmt::Debug for StateLabel {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "StateLabel(")?;
        self.0.iter().try_for_each(|b| write!(f, "{:02X}", b))?;
        write!(f, ")")
    }
}

impl std::convert::TryFrom<Vec<u8>> for StateLabel {
    // The input vector having the wrong size is the only possible error condition.
    type Error = InvalidSize;

    fn try_from(v: Vec<u8>) -> Result<StateLabel, InvalidSize> {
        if v.len() != STATE_LABEL_HASH_SIZE {
            return Err(InvalidSize);
        }

        let mut res = StateLabel::default();
        res.0[0..STATE_LABEL_HASH_SIZE].clone_from_slice(v.as_slice());
        Ok(res)
    }
}

struct ProgressThread {
    handle: JoinHandle<()>,
    sender: mpsc::Sender<()>,
}

struct Instance {
    progress_thread: Option<ProgressThread>,
    state: InstanceState,
}

struct HttpGateway {
    details: HttpGatewayDetails,
    shutdown_handle: Handle,
}

impl HttpGateway {
    fn new(details: HttpGatewayDetails, shutdown_handle: Handle) -> Self {
        Self {
            details,
            shutdown_handle,
        }
    }
}

impl Drop for HttpGateway {
    fn drop(&mut self) {
        self.shutdown_handle.shutdown();
    }
}

/// The state of the PocketIC API.
pub struct ApiState {
    // impl note: If locks are acquired on both fields, acquire first on `instances` and then on `graph`.
    instances: Arc<RwLock<Vec<Mutex<Instance>>>>,
    graph: Arc<RwLock<HashMap<StateLabel, Computations>>>,
    seed: AtomicU64,
    sync_wait_time: Duration,
    // PocketIC server port
    port: Option<u16>,
    // HTTP gateway infos (`None` = stopped)
    http_gateways: Arc<RwLock<Vec<Option<HttpGateway>>>>,
}

#[derive(Default)]
pub struct PocketIcApiStateBuilder {
    initial_instances: Vec<PocketIc>,
    sync_wait_time: Option<Duration>,
    port: Option<u16>,
}

impl PocketIcApiStateBuilder {
    pub fn new() -> Self {
        Default::default()
    }

    /// Computations are dispatched into background tasks. If a computation takes longer than
    /// [sync_wait_time], the update-operation returns, indicating that the given instance is busy.
    pub fn with_sync_wait_time(self, sync_wait_time: Duration) -> Self {
        Self {
            sync_wait_time: Some(sync_wait_time),
            ..self
        }
    }

    pub fn with_port(self, port: u16) -> Self {
        Self {
            port: Some(port),
            ..self
        }
    }

    /// Will make the given instance available in the initial state.
    pub fn add_initial_instance(mut self, instance: PocketIc) -> Self {
        self.initial_instances.push(instance);
        self
    }

    pub fn build(self) -> Arc<ApiState> {
        let graph: HashMap<StateLabel, Computations> = self
            .initial_instances
            .iter()
            .map(|i| (i.get_state_label(), Computations::default()))
            .collect();
        let graph = Arc::new(RwLock::new(graph));

        let instances: Vec<_> = self
            .initial_instances
            .into_iter()
            .map(|instance| {
                Mutex::new(Instance {
                    progress_thread: None,
                    state: InstanceState::Available(instance),
                })
            })
            .collect();
        let instances = Arc::new(RwLock::new(instances));

        let sync_wait_time = self.sync_wait_time.unwrap_or(DEFAULT_SYNC_WAIT_DURATION);

        Arc::new(ApiState {
            instances,
            graph,
            seed: AtomicU64::new(0),
            sync_wait_time,
            port: self.port,
            http_gateways: Arc::new(RwLock::new(Vec::new())),
        })
    }
}

#[derive(Clone)]
pub enum OpOut {
    NoOutput,
    Time(u64),
    CanisterResult(Result<Vec<u8>, RejectResponse>),
    CanisterId(CanisterId),
    Controllers(Vec<PrincipalId>),
    Cycles(u128),
    Bytes(Vec<u8>),
    StableMemBytes(Vec<u8>),
    MaybeSubnetId(Option<SubnetId>),
    Error(PocketIcError),
    RawResponse(Shared<ApiResponse>),
    MessageId((EffectivePrincipal, Vec<u8>)),
    Topology(Topology),
    CanisterHttp(Vec<CanisterHttpRequest>),
}

#[derive(Clone, Eq, PartialEq, Ord, PartialOrd, Debug, Deserialize, Serialize)]
pub enum PocketIcError {
    CanisterNotFound(CanisterId),
    CanisterIsEmpty(CanisterId),
    BadIngressMessage(String),
    SubnetNotFound(candid::Principal),
    RequestRoutingError(String),
    InvalidCanisterHttpRequestId((SubnetId, CanisterHttpRequestId)),
    InvalidMockCanisterHttpResponses((usize, usize)),
    InvalidRejectCode(u64),
    SettingTimeIntoPast((u64, u64)),
    Forbidden(String),
    BlockmakerNotFound(NodeId),
    BlockmakerContainedInFailed(NodeId),
}

impl std::fmt::Debug for OpOut {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            OpOut::NoOutput => write!(f, "NoOutput"),
            OpOut::Time(x) => write!(f, "Time({})", x),
            OpOut::Topology(t) => write!(f, "Topology({:?})", t),
            OpOut::CanisterId(cid) => write!(f, "CanisterId({})", cid),
            OpOut::Controllers(controllers) => write!(
                f,
                "Controllers({})",
                controllers.iter().map(|c| c.to_string()).join(",")
            ),
            OpOut::Cycles(x) => write!(f, "Cycles({})", x),
            OpOut::CanisterResult(Ok(x)) => write!(f, "CanisterResult: Ok({:?})", x),
            OpOut::CanisterResult(Err(x)) => write!(f, "CanisterResult: Err({})", x),
            OpOut::Error(PocketIcError::CanisterNotFound(cid)) => {
                write!(f, "CanisterNotFound({})", cid)
            }
            OpOut::Error(PocketIcError::CanisterIsEmpty(cid)) => {
                write!(f, "CanisterIsEmpty({})", cid)
            }
            OpOut::Error(PocketIcError::BadIngressMessage(msg)) => {
                write!(f, "BadIngressMessage({})", msg)
            }
            OpOut::Error(PocketIcError::SubnetNotFound(sid)) => {
                write!(f, "SubnetNotFound({})", sid)
            }
            OpOut::Error(PocketIcError::BlockmakerNotFound(nid)) => {
                write!(f, "BlockmakerNotFound({})", nid)
            }
            OpOut::Error(PocketIcError::BlockmakerContainedInFailed(nid)) => {
                write!(f, "BlockmakerContainedInFailed({})", nid)
            }
            OpOut::Error(PocketIcError::RequestRoutingError(msg)) => {
                write!(f, "RequestRoutingError({:?})", msg)
            }
            OpOut::Error(PocketIcError::InvalidCanisterHttpRequestId((
                subnet_id,
                canister_http_request_id,
            ))) => {
                write!(
                    f,
                    "InvalidCanisterHttpRequestId({},{:?})",
                    subnet_id, canister_http_request_id
                )
            }
            OpOut::Error(PocketIcError::InvalidMockCanisterHttpResponses((actual, expected))) => {
                write!(
                    f,
                    "InvalidMockCanisterHttpResponses(actual={},expected={})",
                    actual, expected
                )
            }
            OpOut::Error(PocketIcError::InvalidRejectCode(code)) => {
                write!(f, "InvalidRejectCode({})", code)
            }
            OpOut::Error(PocketIcError::SettingTimeIntoPast((current, set))) => {
                write!(f, "SettingTimeIntoPast(current={},set={})", current, set)
            }
            OpOut::Error(PocketIcError::Forbidden(msg)) => {
                write!(f, "Forbidden({})", msg)
            }
            OpOut::Bytes(bytes) => write!(f, "Bytes({})", base64::encode(bytes)),
            OpOut::StableMemBytes(bytes) => write!(f, "StableMemory({})", base64::encode(bytes)),
            OpOut::MaybeSubnetId(Some(subnet_id)) => write!(f, "SubnetId({})", subnet_id),
            OpOut::MaybeSubnetId(None) => write!(f, "NoSubnetId"),
            OpOut::RawResponse(fut) => {
                write!(
                    f,
                    "ApiResp({:?})",
                    fut.peek().map(|(status, headers, bytes)| format!(
                        "{}:{:?}:{}",
                        status,
                        headers,
                        base64::encode(bytes)
                    ))
                )
            }
            OpOut::MessageId((effective_principal, message_id)) => {
                write!(
                    f,
                    "MessageId({:?},{})",
                    effective_principal,
                    hex::encode(message_id)
                )
            }
            OpOut::CanisterHttp(canister_http_reqeusts) => {
                write!(f, "CanisterHttp({:?})", canister_http_reqeusts)
            }
        }
    }
}

pub type Computations = HashMap<OpId, (StateLabel, OpOut)>;

/// The PocketIcApiState has a vector with elements of InstanceState.
/// When an operation is bound to an instance, the corresponding element in the
/// vector is replaced by a Busy variant which contains information about the
/// computation that is currently running. Afterwards, the instance is put back as
/// Available.
pub enum InstanceState {
    Busy {
        state_label: StateLabel,
        op_id: OpId,
    },
    Available(PocketIc),
    Deleted,
}

#[derive(Clone, Debug, Deserialize, Serialize)]
pub struct UpdateError {
    message: String,
}

pub type UpdateResult = std::result::Result<UpdateReply, UpdateError>;

/// An operation bound to an instance can be dispatched, which updates the instance.
/// If the instance is already busy with an operation, the initial state and that operation
/// are returned.
/// If the result can be read from a cache, or if the computation is a fast read, an Output is
/// returned directly.
/// If the computation can be run and takes longer, a Started variant is returned, containing the
/// requested op and the initial state.
#[derive(Debug)]
pub enum UpdateReply {
    /// The requested instance is busy executing another update.
    Busy {
        state_label: StateLabel,
        op_id: OpId,
    },
    /// The requested instance is busy executing this current update.
    Started {
        state_label: StateLabel,
        op_id: OpId,
    },
    // This request is either cached or quickly executable, so we return
    // the output immediately.
    Output(OpOut),
}

impl UpdateReply {
    pub fn get_in_progress(&self) -> Option<(StateLabel, OpId)> {
        match self {
            Self::Busy { state_label, op_id } => Some((state_label.clone(), op_id.clone())),
            Self::Started { state_label, op_id } => Some((state_label.clone(), op_id.clone())),
            _ => None,
        }
    }
}

/// This trait lets us put a mock of the pocket_ic into the PocketIcApiState.
pub trait HasStateLabel {
    fn get_state_label(&self) -> StateLabel;
}

fn received_stop_signal(rx: &mut Receiver<()>) -> bool {
    match rx.try_recv() {
        Ok(_) | Err(TryRecvError::Disconnected) => true,
        Err(TryRecvError::Empty) => false,
    }
}

// ADAPTED from ic-gateway

const MAX_REQUEST_BODY_SIZE: usize = 10 * 1_048_576;
const MINUTE: Duration = Duration::from_secs(60);

const X_OC_JWT: HeaderName = HeaderName::from_static("x-oc-jwt");
const X_OC_API_KEY: HeaderName = HeaderName::from_static("x-oc-api-key");

fn layer(methods: &[Method]) -> CorsLayer {
    CorsLayer::new()
        .allow_origin(Any)
        .allow_methods(methods.to_vec())
        .expose_headers([
            ACCEPT_RANGES,
            CONTENT_LENGTH,
            CONTENT_RANGE,
            X_REQUEST_ID,
            X_IC_CANISTER_ID,
        ])
        .allow_headers([
            USER_AGENT,
            DNT,
            IF_NONE_MATCH,
            IF_MODIFIED_SINCE,
            CACHE_CONTROL,
            CONTENT_TYPE,
            RANGE,
            COOKIE,
            X_REQUESTED_WITH,
            X_IC_CANISTER_ID,
        ])
        .max_age(10 * MINUTE)
}

fn http_gw_layer(methods: &[Method]) -> CorsLayer {
    CorsLayer::new()
        .allow_origin(Any)
        .allow_methods(methods.to_vec())
        .expose_headers([
            ACCEPT_RANGES,
            CONTENT_LENGTH,
            CONTENT_RANGE,
            X_REQUEST_ID,
            X_IC_CANISTER_ID,
        ])
        .allow_headers([
            USER_AGENT,
            DNT,
            IF_NONE_MATCH,
            IF_MODIFIED_SINCE,
            CACHE_CONTROL,
            CONTENT_TYPE,
            RANGE,
            COOKIE,
            X_REQUESTED_WITH,
            X_IC_CANISTER_ID,
            X_OC_JWT,
            X_OC_API_KEY,
        ])
        .max_age(10 * MINUTE)
}

// Categorized possible causes for request processing failures
// Not using Error as inner type since it's not cloneable
#[derive(Clone, Debug, Display)]
enum ErrorCause {
    ClientBodyTooLarge,
    ClientBodyTimeout,
    ClientBodyError(String),
    ConnectionFailure(String),
    CanisterIdNotFound,
    Other(String),
}

impl ErrorCause {
    const fn status_code(&self) -> StatusCode {
        match self {
            Self::ClientBodyTooLarge => StatusCode::PAYLOAD_TOO_LARGE,
            Self::ClientBodyTimeout => StatusCode::REQUEST_TIMEOUT,
            Self::ClientBodyError(_) => StatusCode::BAD_REQUEST,
            Self::ConnectionFailure(_) => StatusCode::BAD_GATEWAY,
            Self::CanisterIdNotFound => StatusCode::BAD_REQUEST,
            Self::Other(_) => StatusCode::INTERNAL_SERVER_ERROR,
        }
    }

    fn details(&self) -> Option<String> {
        match self {
            Self::ClientBodyError(x) => Some(x.clone()),
            Self::ConnectionFailure(x) => Some(x.clone()),
            Self::Other(x) => Some(x.clone()),
            _ => None,
        }
    }

    // Convert from client-side error
    pub fn from_client_error(e: IcBnError) -> Self {
        match e {
            IcBnError::BodyReadingFailed(v) => Self::ClientBodyError(v),
            IcBnError::BodyTimedOut => Self::ClientBodyTimeout,
            IcBnError::BodyTooBig => Self::ClientBodyTooLarge,
            _ => Self::Other(e.to_string()),
        }
    }
}

// Creates the response from ErrorCause and injects itself into extensions to be visible by middleware
impl IntoResponse for ErrorCause {
    fn into_response(self) -> Response {
        let body = self
            .details()
            .map_or_else(|| self.to_string(), |x| format!("{self}: {x}\n"));

        let mut resp = (self.status_code(), body).into_response();

        resp.extensions_mut().insert(self);
        resp
    }
}

#[derive(Debug)]
struct ReqwestClient(reqwest::Client);

impl ReqwestClient {
    pub fn new(client: reqwest::Client) -> Self {
        Self(client)
    }
}

#[async_trait]
impl Client for ReqwestClient {
    async fn execute(&self, req: reqwest::Request) -> Result<reqwest::Response, reqwest::Error> {
        self.0.execute(req).await
    }
}

pub(crate) struct HandlerState {
    http_gateway_client: HttpGatewayClient,
    backend_client: Arc<dyn Client>,
    resolver: DomainResolver,
    replica_url: String,
}

impl HandlerState {
    fn new(
        http_gateway_client: HttpGatewayClient,
        backend_client: Arc<dyn Client>,
        resolver: DomainResolver,
        replica_url: String,
    ) -> Self {
        Self {
            http_gateway_client,
            backend_client,
            resolver,
            replica_url,
        }
    }

    pub(crate) fn resolver(&self) -> &DomainResolver {
        &self.resolver
    }
}

// Main HTTP->IC request handler
async fn handler(
    State(state): State<Arc<HandlerState>>,
    host_canister_id: Option<canister_id::HostHeader>,
    query_param_canister_id: Option<canister_id::QueryParam>,
    referer_host_canister_id: Option<canister_id::RefererHeaderHost>,
    referer_query_param_canister_id: Option<canister_id::RefererHeaderQueryParam>,
    request: AxumRequest,
) -> Result<impl IntoResponse, ErrorCause> {
    // Resolve the domain
    let lookup =
        extract_authority(&request).and_then(|authority| state.resolver.resolve(&authority));

    let canister_id = lookup.as_ref().and_then(|lookup| lookup.canister_id);
    let host_canister_id = host_canister_id.map(|v| v.0);
    let query_param_canister_id = query_param_canister_id.map(|v| v.0);
    let referer_host_canister_id = referer_host_canister_id.map(|v| v.0);
    let referer_query_param_canister_id = referer_query_param_canister_id.map(|v| v.0);
    let canister_id = canister_id
        .or(host_canister_id)
        .or(query_param_canister_id)
        .or(referer_host_canister_id)
        .or(referer_query_param_canister_id)
        .ok_or(ErrorCause::CanisterIdNotFound);

    if request.uri().path().starts_with("/_/") && canister_id.is_err() {
        let url = Url::parse(&format!(
            "{}{}",
            state.replica_url,
            request
                .uri()
                .path_and_query()
                .map(|p| p.as_str())
                .unwrap_or_default()
        ))
        .unwrap();
        proxy(url, request, &state.backend_client.clone())
            .await
            .map_err(|e| ErrorCause::ConnectionFailure(e.to_string()))
    } else {
        let (parts, body) = request.into_parts();

        // Collect the request body up to the limit
        let body = buffer_body(body, MAX_REQUEST_BODY_SIZE, Duration::from_secs(60)).await;
        let body = match body {
            Ok(v) => v,
            Err(e) => {
                return Err(ErrorCause::from_client_error(e));
            }
        };

        let args = HttpGatewayRequestArgs {
            canister_request: CanisterRequest::from_parts(parts, body),
            canister_id: canister_id?,
        };

        let resp = {
            // Execute the request
            let mut req = state.http_gateway_client.request(args);
            // Skip verification if it is a "raw" request.
            req.unsafe_set_skip_verification(lookup.map(|v| !v.verify).unwrap_or_default());
            req.send().await
        };

        Ok(resp.canister_response.into_response())
    }
}

// Attempts to extract host from HTTP2 "authority" pseudo-header or from HTTP/1.1 "Host" header
fn extract_authority(request: &AxumRequest) -> Option<FQDN> {
    // Try HTTP2 first, then Host header
    request
        .uri()
        .authority()
        .map(|x| x.host())
        .or_else(|| {
            request
                .headers()
                .get(http::header::HOST)
                .and_then(|x| x.to_str().ok())
                // Split if it has a port
                .and_then(|x| x.split(':').next())
        })
        .and_then(|x| FQDN::from_str(x).ok())
}

// END ADAPTED from ic-gateway

impl ApiState {
    // Helper function for auto progress mode.
    // Executes an operation to completion and returns its `OpOut`
    // or `None` if the auto progress mode received a stop signal.
    async fn execute_operation(
        instances: Arc<RwLock<Vec<Mutex<Instance>>>>,
        graph: Arc<RwLock<HashMap<StateLabel, Computations>>>,
        instance_id: InstanceId,
        op: impl Operation + Send + Sync + 'static,
        rx: &mut Receiver<()>,
    ) -> Option<OpOut> {
        let op = Arc::new(op);
        loop {
            // It is safe to unwrap as there can only be an error if the instance does not exist
            // and there cannot be a progress thread for a non-existing instance (progress threads
            // are stopped before an instance is deleted).
            match Self::update_instances_with_timeout(
                instances.clone(),
                graph.clone(),
                op.clone(),
                instance_id,
                AUTO_PROGRESS_OPERATION_TIMEOUT,
            )
            .await
            .unwrap()
            {
                UpdateReply::Started { state_label, op_id } => {
                    break loop {
                        sleep(READ_GRAPH_DELAY).await;
                        if let Some((_, op_out)) =
                            Self::read_result(graph.clone(), &state_label, &op_id)
                        {
                            break Some(op_out);
                        }
                        if received_stop_signal(rx) {
                            break None;
                        }
                    }
                }
                UpdateReply::Busy { .. } => {}
                UpdateReply::Output(op_out) => break Some(op_out),
            };
            if received_stop_signal(rx) {
                break None;
            }
        }
    }

    /// For polling:
    /// The client lib dispatches a long running operation and gets a Started {state_label, op_id}.
    /// It then polls on that via this state tree api function.
    fn read_result(
        graph: Arc<RwLock<HashMap<StateLabel, Computations>>>,
        state_label: &StateLabel,
        op_id: &OpId,
    ) -> Option<(StateLabel, OpOut)> {
        if let Some((new_state_label, op_out)) = graph.try_read().ok()?.get(state_label)?.get(op_id)
        {
            Some((new_state_label.clone(), op_out.clone()))
        } else {
            None
        }
    }

    pub fn read_graph(
        &self,
        state_label: &StateLabel,
        op_id: &OpId,
    ) -> Option<(StateLabel, OpOut)> {
        Self::read_result(self.graph.clone(), state_label, op_id)
    }

    pub async fn add_instance<F>(&self, f: F) -> Result<(InstanceId, Topology), String>
    where
        F: FnOnce(u64) -> Result<PocketIc, String> + std::marker::Send + 'static,
    {
        let seed = self.seed.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
        // create the instance using `spawn_blocking` before acquiring a lock
        let instance = tokio::task::spawn_blocking(move || f(seed))
            .await
            .expect("Failed to create PocketIC instance")?;
        let topology = instance.topology();
        let mut instances = self.instances.write().await;
        let instance_id = instances.len();
        instances.push(Mutex::new(Instance {
            progress_thread: None,
            state: InstanceState::Available(instance),
        }));
        Ok((instance_id, topology))
    }

    pub async fn delete_instance(&self, instance_id: InstanceId) {
        self.stop_progress(instance_id).await;
        loop {
            let instances = self.instances.read().await;
            let mut instance = instances[instance_id].lock().await;
            match &instance.state {
                InstanceState::Available(_) => {
                    let _ = std::mem::replace(&mut instance.state, InstanceState::Deleted);
                    break;
                }
                InstanceState::Deleted => {
                    break;
                }
                InstanceState::Busy { .. } => {}
            }
            drop(instance);
            drop(instances);
            tokio::time::sleep(Duration::from_secs(1)).await;
        }
    }

    pub async fn delete_all_instances(arc_self: Arc<ApiState>) {
        let mut tasks = JoinSet::new();
        let instances = arc_self.instances.read().await;
        let num_instances = instances.len();
        drop(instances);
        for instance_id in 0..num_instances {
            let arc_self_clone = arc_self.clone();
            tasks.spawn(async move { arc_self_clone.delete_instance(instance_id).await });
        }
        tasks.join_all().await;
    }

    pub async fn create_http_gateway(
        &self,
        http_gateway_config: HttpGatewayConfig,
    ) -> Result<HttpGatewayInfo, String> {
        async fn proxy_handler(
            State((replica_url, client)): State<(String, Arc<dyn Client>)>,
            request: AxumRequest,
        ) -> Result<Response, ErrorCause> {
            let url = format!(
                "{}{}",
                replica_url,
                request
                    .uri()
                    .path_and_query()
                    .map(|p| p.as_str())
                    .unwrap_or_default()
            );
            proxy(Url::parse(&url).unwrap(), request, &client)
                .await
                .map_err(|e| ErrorCause::ConnectionFailure(e.to_string()))
        }

        let https_config = if let Some(ref https_config) = http_gateway_config.https_config {
            Some(
                RustlsConfig::from_pem_file(
                    PathBuf::from(https_config.cert_path.clone()),
                    PathBuf::from(https_config.key_path.clone()),
                )
                .await
                .map_err(|e| format!("TLS config could not be created: {}", e))?,
            )
        } else {
            None
        };

        let ip_addr = http_gateway_config
            .ip_addr
            .clone()
            .unwrap_or("127.0.0.1".to_string());
        let port = http_gateway_config.port.unwrap_or_default();
        let addr = format!("{}:{}", ip_addr, port);
        let listener = std::net::TcpListener::bind(&addr)
            .map_err(|e| format!("Failed to bind to address {}: {}", addr, e))?;

        let pocket_ic_server_port = self.port.unwrap();
        let replica_url = match http_gateway_config.forward_to {
            HttpGatewayBackend::Replica(ref replica_url) => replica_url.clone(),
            HttpGatewayBackend::PocketIcInstance(instance_id) => {
                format!(
                    "http://localhost:{}/instances/{}/",
                    pocket_ic_server_port, instance_id
                )
            }
        };
        let agent = ic_agent::Agent::builder()
            .with_url(replica_url.clone())
            .build()
            .unwrap();
        time::timeout(DEFAULT_SYNC_WAIT_DURATION, agent.fetch_root_key())
            .await
            .map_err(|_| format!("Timed out fetching root key from {}", replica_url))?
            .map_err(|e| e.to_string())?;

        let replica_url = replica_url.trim_end_matches('/').to_string();

        let handle = Handle::new();
        let axum_handle = handle.clone();
        let domains = http_gateway_config
            .domains
            .clone()
            .unwrap_or(vec!["localhost".to_string()])
            .iter()
            .map(|d| fqdn!(d))
            .collect();
        spawn(async move {
            let http_gateway_client = ic_http_gateway::HttpGatewayClientBuilder::new()
                .with_agent(agent)
                .build()
                .unwrap();
            let domain_resolver = DomainResolver::new(domains);
            let backend_client = Arc::new(ReqwestClient::new(reqwest::Client::new()));
            let state_handler = Arc::new(HandlerState::new(
                http_gateway_client,
                backend_client.clone(),
                domain_resolver,
                replica_url.clone(),
            ));

            // ADAPTED from ic-gateway
            let cors_post = layer(&[Method::POST]);
            let cors_get = layer(&[Method::HEAD, Method::GET]);
            // IC API proxy routers
            let router_api_v2 = Router::new()
                .route(
                    "/canister/{principal}/query",
                    post(proxy_handler).layer(cors_post.clone()),
                )
                .route(
                    "/canister/{principal}/call",
                    post(proxy_handler).layer(cors_post.clone()),
                )
                .route(
                    "/canister/{principal}/read_state",
                    post(proxy_handler).layer(cors_post.clone()),
                )
                .route(
                    "/subnet/{principal}/read_state",
                    post(proxy_handler).layer(cors_post.clone()),
                )
                .route("/status", get(proxy_handler).layer(cors_get.clone()))
                .fallback(|| async { (StatusCode::NOT_FOUND, "") })
                .with_state((format!("{}/api/v2", replica_url), backend_client.clone()));
            let router_api_v3 = Router::new()
                .route(
                    "/canister/{principal}/call",
                    post(proxy_handler).layer(cors_post.clone()),
                )
                .fallback(|| async { (StatusCode::NOT_FOUND, "") })
                .with_state((format!("{}/api/v3", replica_url), backend_client.clone()));

            let router_http = Router::new().fallback(
                post(handler)
                    .get(handler)
                    .put(handler)
                    .delete(handler)
                    .patch(handler)
                    .layer(http_gw_layer(&[
                        Method::HEAD,
                        Method::GET,
                        Method::POST,
                        Method::PUT,
                        Method::DELETE,
                        Method::PATCH,
                    ]))
                    .with_state(state_handler),
            );
            // Top-level router
            let router = Router::new()
                .nest("/api/v2", router_api_v2)
                .nest("/api/v3", router_api_v3)
                .fallback(|request: AxumRequest| async move { router_http.oneshot(request).await })
                .into_make_service();
            // END ADAPTED from ic-gateway

            match https_config {
                Some(config) => {
                    axum_server::from_tcp_rustls(listener, config)
                        .handle(axum_handle)
                        .serve(router)
                        .await
                        .unwrap();
                }
                None => {
                    axum_server::from_tcp(listener)
                        .handle(axum_handle)
                        .serve(router)
                        .await
                        .unwrap();
                }
            }

            debug!("Terminating HTTP gateway.");
        });

        // Wait until the HTTP gateway starts listening.
        let real_port = loop {
            if let Some(socket_addr) = handle.listening().await {
                break socket_addr.port();
            }
            sleep(Duration::from_millis(20)).await;
        };

        let mut http_gateways = self.http_gateways.write().await;
        let instance_id = http_gateways.len();
        let http_gateway_details = HttpGatewayDetails {
            instance_id,
            port: real_port,
            forward_to: http_gateway_config.forward_to.clone(),
            domains: http_gateway_config.domains.clone(),
            https_config: http_gateway_config.https_config.clone(),
        };
        let shutdown_handle = handle.clone();
        let http_gateway = HttpGateway::new(http_gateway_details, shutdown_handle);
        http_gateways.push(Some(http_gateway));
        drop(http_gateways);

        Ok(HttpGatewayInfo {
            instance_id,
            port: real_port,
        })
    }

    pub async fn stop_http_gateway(&self, instance_id: InstanceId) {
        let mut http_gateways = self.http_gateways.write().await;
        if instance_id < http_gateways.len() {
            http_gateways[instance_id] = None;
        }
    }

    pub async fn stop_all_http_gateways(&self) {
        let mut http_gateways = self.http_gateways.write().await;
        for i in 0..http_gateways.len() {
            http_gateways[i] = None;
        }
    }

    pub async fn auto_progress(
        &self,
        instance_id: InstanceId,
        artificial_delay_ms: Option<u64>,
    ) -> Result<(), String> {
        let artificial_delay = Duration::from_millis(artificial_delay_ms.unwrap_or_default());
        let instances_clone = self.instances.clone();
        let graph = self.graph.clone();
        let instances = self.instances.read().await;
        let mut instance = instances[instance_id].lock().await;
        if instance.progress_thread.is_none() {
            let (tx, mut rx) = mpsc::channel::<()>(1);
            let handle = spawn(async move {
                let mut now = SystemTime::now();
                let time = ic_types::Time::from_nanos_since_unix_epoch(
                    now.duration_since(SystemTime::UNIX_EPOCH)
                        .unwrap_or_default()
                        .as_nanos() as u64,
                );
                let op = SetCertifiedTime { time };
                if Self::execute_operation(
                    instances_clone.clone(),
                    graph.clone(),
                    instance_id,
                    op,
                    &mut rx,
                )
                .await
                .is_some()
                {
                    debug!("Starting auto progress for instance {}.", instance_id);
                    loop {
                        let old = std::mem::replace(&mut now, SystemTime::now());
                        let op = AdvanceTimeAndTick(now.duration_since(old).unwrap_or_default());
                        if Self::execute_operation(
                            instances_clone.clone(),
                            graph.clone(),
                            instance_id,
                            op,
                            &mut rx,
                        )
                        .await
                        .is_none()
                        {
                            break;
                        }
                        let op = ProcessCanisterHttpInternal;
                        if Self::execute_operation(
                            instances_clone.clone(),
                            graph.clone(),
                            instance_id,
                            op,
                            &mut rx,
                        )
                        .await
                        .is_none()
                        {
                            break;
                        }
                        let sleep_duration = std::cmp::max(artificial_delay, MIN_OPERATION_DELAY);
                        sleep(sleep_duration).await;
                        if received_stop_signal(&mut rx) {
                            break;
                        }
                    }
                    debug!("Stopping auto progress for instance {}.", instance_id);
                }
            });
            instance.progress_thread = Some(ProgressThread { handle, sender: tx });
            Ok(())
        } else {
            Err("Auto progress mode has already been enabled.".to_string())
        }
    }

    pub async fn get_auto_progress(&self, instance_id: InstanceId) -> bool {
        let instances = self.instances.read().await;
        let instance = instances[instance_id].lock().await;
        instance.progress_thread.is_some()
    }

    pub async fn stop_progress(&self, instance_id: InstanceId) {
        let instances = self.instances.read().await;
        let mut instance = instances[instance_id].lock().await;
        let progress_thread = instance.progress_thread.take();
        // drop locks otherwise we might end up with a deadlock
        drop(instance);
        drop(instances);
        if let Some(t) = progress_thread {
            t.sender.send(()).await.unwrap();
            t.handle.await.unwrap();
        }
    }

    pub async fn list_instance_states(&self) -> Vec<String> {
        let instances = self.instances.read().await;
        let mut res = vec![];

        for instance in &*instances {
            let instance = &*instance.lock().await;
            match &instance.state {
                InstanceState::Busy { state_label, op_id } => {
                    res.push(format!("Busy({:?}, {:?})", state_label, op_id))
                }
                InstanceState::Available(_) => res.push("Available".to_string()),
                InstanceState::Deleted => res.push("Deleted".to_string()),
            }
        }
        res
    }

    pub async fn list_http_gateways(&self) -> Vec<HttpGatewayDetails> {
        self.http_gateways
            .read()
            .await
            .iter()
            .filter_map(|gateway| gateway.as_ref().map(|g| g.details.clone()))
            .collect()
    }

    /// An operation bound to an instance (a Computation) can update the PocketIC state.
    ///
    /// * If the instance is busy executing an operation, the call returns [UpdateReply::Busy]
    ///   immediately. In that case, the state label and operation id contained in the result
    ///   indicate that the instance is busy with a previous operation.
    ///
    /// * If the instance is available and the computation exceeds a (short) timeout,
    ///   [UpdateReply::Busy] is returned.
    ///
    /// * If the computation finished within the timeout, [UpdateReply::Output] is returned
    ///   containing the result.
    ///
    /// Operations are _not_ queued by default. Thus, if the instance is busy with an existing operation,
    /// the client has to retry until the operation is done. Some operations for which the client
    /// might be unable to retry are exceptions to this rule and they are queued up implicitly
    /// by a retry mechanism inside PocketIc.
    pub async fn update<O>(&self, op: Arc<O>, instance_id: InstanceId) -> UpdateResult
    where
        O: Operation + Send + Sync + 'static,
    {
        self.update_with_timeout(op, instance_id, None).await
    }

    /// Same as [Self::update] except that the timeout can be specified manually. This is useful in
    /// cases when clients want to enforce a long-running blocking call.
    pub async fn update_with_timeout<O>(
        &self,
        op: Arc<O>,
        instance_id: InstanceId,
        sync_wait_time: Option<Duration>,
    ) -> UpdateResult
    where
        O: Operation + Send + Sync + 'static,
    {
        let sync_wait_time = sync_wait_time.unwrap_or(self.sync_wait_time);
        Self::update_instances_with_timeout(
            self.instances.clone(),
            self.graph.clone(),
            op,
            instance_id,
            sync_wait_time,
        )
        .await
    }

    /// Same as [Self::update] except that the timeout can be specified manually. This is useful in
    /// cases when clients want to enforce a long-running blocking call.
    async fn update_instances_with_timeout<O>(
        instances: Arc<RwLock<Vec<Mutex<Instance>>>>,
        graph: Arc<RwLock<HashMap<StateLabel, Computations>>>,
        op: Arc<O>,
        instance_id: InstanceId,
        sync_wait_time: Duration,
    ) -> UpdateResult
    where
        O: Operation + Send + Sync + 'static,
    {
        let op_id = op.id().0;
        trace!(
            "update_with_timeout::start instance_id={} op_id={}",
            instance_id,
            op_id,
        );
        let instances_cloned = instances.clone();
        let instances_locked = instances_cloned.read().await;
        let (bg_task, busy_outcome) = if let Some(instance_mutex) =
            instances_locked.get(instance_id)
        {
            let mut instance = instance_mutex.lock().await;
            // If this instance is busy, return the running op and initial state
            match &instance.state {
                InstanceState::Deleted => {
                    return Err(UpdateError {
                        message: "Instance was deleted".to_string(),
                    });
                }
                // TODO: cache lookup possible with this state_label and our own op_id
                InstanceState::Busy { state_label, op_id } => {
                    return Ok(UpdateReply::Busy {
                        state_label: state_label.clone(),
                        op_id: op_id.clone(),
                    });
                }
                InstanceState::Available(pocket_ic) => {
                    // move pocket_ic out

                    let state_label = pocket_ic.get_state_label();
                    let op_id = op.id();
                    let busy = InstanceState::Busy {
                        state_label: state_label.clone(),
                        op_id: op_id.clone(),
                    };
                    let InstanceState::Available(mut pocket_ic) =
                        std::mem::replace(&mut instance.state, busy)
                    else {
                        unreachable!()
                    };

                    let bg_task = {
                        let old_state_label = state_label.clone();
                        let op_id = op_id.clone();
                        let graph = graph.clone();
                        move || {
                            trace!(
                                "bg_task::start instance_id={} state_label={:?} op_id={}",
                                instance_id,
                                old_state_label,
                                op_id.0,
                            );
                            let result = op.compute(&mut pocket_ic);
                            pocket_ic.bump_state_label();
                            let new_state_label = pocket_ic.get_state_label();
                            // add result to graph, but grab instance lock first!
                            let instances = instances.blocking_read();
                            let mut graph_guard = graph.blocking_write();
                            let cached_computations =
                                graph_guard.entry(old_state_label.clone()).or_default();
                            cached_computations
                                .insert(op_id.clone(), (new_state_label, result.clone()));
                            drop(graph_guard);
                            let mut instance = instances[instance_id].blocking_lock();
                            if let InstanceState::Deleted = &instance.state {
                                error!("The instance is deleted immediately after an operation. This is a bug!");
                                std::mem::drop(pocket_ic);
                            } else {
                                instance.state = InstanceState::Available(pocket_ic);
                            }
                            trace!("bg_task::end instance_id={} op_id={}", instance_id, op_id.0);
                            result
                        }
                    };

                    // cache miss: replace pocket_ic instance in the vector with Busy
                    (bg_task, UpdateReply::Started { state_label, op_id })
                }
            }
        } else {
            return Err(UpdateError {
                message: "Instance not found".to_string(),
            });
        };
        // drop lock, otherwise we end up with a deadlock
        std::mem::drop(instances_locked);

        // We schedule a blocking background task on the tokio runtime. Note that if all
        // blocking workers are busy, the task is put on a queue (which is what we want).
        //
        // Note: One issue here is that we drop the join handle "on the floor". Threads
        // that are not awaited upon before exiting the process are known to cause spurios
        // issues. This should not be a problem as the tokio Executor will wait
        // indefinitively for threads to return, unless a shutdown timeout is configured.
        //
        // See: https://docs.rs/tokio/latest/tokio/task/fn.spawn_blocking.html
        let bg_handle = spawn_blocking(bg_task);

        // if the operation returns "in time", we return the result, otherwise we indicate to the
        // client that the instance is busy.
        //
        // note: this assumes that cancelling the JoinHandle does not stop the execution of the
        // background task. This only works because the background thread, in this case, is a
        // kernel thread.
        if let Ok(Ok(op_out)) = time::timeout(sync_wait_time, bg_handle).await {
            trace!(
                "update_with_timeout::synchronous instance_id={} op_id={}",
                instance_id,
                op_id,
            );
            return Ok(UpdateReply::Output(op_out));
        }

        trace!(
            "update_with_timeout::timeout instance_id={} op_id={}",
            instance_id,
            op_id,
        );
        Ok(busy_outcome)
    }
}
