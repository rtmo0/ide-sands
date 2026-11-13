use std::{
    collections::HashMap,
    io::Read,
    net::SocketAddr,
    pin::Pin,
    sync::Arc,
    thread,
    time::{Instant, SystemTime},
};

use anyhow::{Context as _, Result};
use futures::AsyncReadExt as _;
use http_client::{AsyncBody, HttpClient, http};
use parking_lot::Mutex;
use serde::Serialize;

/// The address sandboxes use to reach the host.
pub const SANDBOX_HOST: &str = "host.docker.internal";

/// Requests bigger than this are refused; model requests with long context stay well below it.
const MAX_REQUEST_SIZE: u64 = 32 * 1024 * 1024;

/// A model API the gateway forwards to.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum Provider {
    Anthropic,
    OpenAi,
}

impl Provider {
    pub fn path_name(self) -> &'static str {
        match self {
            Provider::Anthropic => "anthropic",
            Provider::OpenAi => "openai",
        }
    }

    fn from_path_name(name: &str) -> Option<Self> {
        match name {
            "anthropic" => Some(Provider::Anthropic),
            "openai" => Some(Provider::OpenAi),
            _ => None,
        }
    }

    fn default_upstream(self) -> &'static str {
        match self {
            Provider::Anthropic => "https://api.anthropic.com",
            Provider::OpenAi => "https://api.openai.com",
        }
    }

    fn authorize(self, builder: http::request::Builder, key: &str) -> http::request::Builder {
        match self {
            Provider::Anthropic => builder.header("x-api-key", key),
            Provider::OpenAi => builder.header("authorization", format!("Bearer {key}")),
        }
    }

    fn rejection(self, message: &str) -> Vec<u8> {
        let body = match self {
            Provider::Anthropic => serde_json::json!({
                "type": "error",
                "error": { "type": "permission_error", "message": message },
            }),
            Provider::OpenAi => serde_json::json!({
                "error": { "type": "permission_error", "code": "model_not_allowed", "message": message },
            }),
        };
        body.to_string().into_bytes()
    }
}

/// What one project may do through the gateway.
#[derive(Clone, Debug, Default)]
pub struct Route {
    pub project: String,
    /// Models the project may call, as the API names them. `None` allows every model.
    pub allowed_models: Option<Vec<String>>,
    /// API keys kept on the host, by provider. Without one, the request goes out with the
    /// credentials the caller sent, such as a subscription sign-in.
    pub keys: HashMap<Provider, String>,
}

impl Route {
    fn allows(&self, model: &str) -> bool {
        match &self.allowed_models {
            None => true,
            Some(allowed) => allowed.iter().any(|allowed| {
                let name = allowed
                    .rsplit_once('/')
                    .map_or(allowed.as_str(), |(_, name)| name);
                // An alias such as `claude-sonnet-4-5` also covers its dated versions.
                model == name
                    || model
                        .strip_prefix(name)
                        .is_some_and(|rest| rest.starts_with('-'))
            }),
        }
    }
}

/// One call through the gateway, for the project's log.
#[derive(Clone, Debug, PartialEq, Eq, Serialize)]
pub struct Call {
    pub project: String,
    pub provider: &'static str,
    pub path: String,
    pub model: Option<String>,
    /// The upstream status, or the gateway's own when it refused or failed the request.
    pub status: u16,
    pub allowed: bool,
    pub duration_ms: u64,
    pub unix_time: u64,
}

pub type CallLog = Arc<dyn Fn(Call) + Send + Sync>;

struct State {
    routes: Mutex<HashMap<String, Route>>,
    http: Arc<dyn HttpClient>,
    upstreams: HashMap<Provider, String>,
    log: CallLog,
}

/// Forwards model API calls from sandboxes to the providers. Keys stay on the host, each
/// project only reaches the models it is allowed, and every call is logged.
pub struct Gateway {
    state: Arc<State>,
    address: SocketAddr,
    server: Arc<tiny_http::Server>,
}

impl Drop for Gateway {
    fn drop(&mut self) {
        self.server.unblock();
    }
}

impl Gateway {
    /// Listens on the loopback interface only; sandboxes reach it as [`SANDBOX_HOST`].
    pub fn start(port: u16, http: Arc<dyn HttpClient>, log: CallLog) -> Result<Self> {
        Self::start_with_upstreams(port, http, log, HashMap::new())
    }

    pub fn start_with_upstreams(
        port: u16,
        http: Arc<dyn HttpClient>,
        log: CallLog,
        upstreams: HashMap<Provider, String>,
    ) -> Result<Self> {
        let server = tiny_http::Server::http(("127.0.0.1", port)).map_err(|error| {
            anyhow::anyhow!("starting the model gateway on port {port}: {error}")
        })?;
        let address = server
            .server_addr()
            .to_ip()
            .context("the model gateway has no IP address")?;
        let server = Arc::new(server);
        let state = Arc::new(State {
            routes: Mutex::new(HashMap::new()),
            http,
            upstreams,
            log,
        });
        thread::Builder::new()
            .name("model-gateway".to_string())
            .spawn({
                let server = server.clone();
                let state = state.clone();
                move || {
                    for request in server.incoming_requests() {
                        let state = state.clone();
                        // Responses stream for as long as the model writes, so each request
                        // gets its own thread.
                        let spawned = thread::Builder::new()
                            .name("model-gateway-request".to_string())
                            .spawn(move || handle(&state, request));
                        if let Err(error) = spawned {
                            log::error!("model gateway could not start a request thread: {error}");
                        }
                    }
                }
            })
            .context("starting the model gateway thread")?;
        Ok(Self {
            state,
            address,
            server,
        })
    }

    pub fn port(&self) -> u16 {
        self.address.port()
    }

    /// Lets a project use the gateway and returns the secret part of its URLs.
    pub fn register(&self, route: Route) -> String {
        let token = uuid::Uuid::new_v4().simple().to_string();
        self.state.routes.lock().insert(token.clone(), route);
        token
    }

    pub fn update(&self, token: &str, route: Route) {
        if let Some(existing) = self.state.routes.lock().get_mut(token) {
            *existing = route;
        }
    }

    pub fn unregister(&self, token: &str) {
        self.state.routes.lock().remove(token);
    }

    /// The base URL an agent in a sandbox points its provider SDK at.
    pub fn base_url(&self, token: &str, provider: Provider) -> String {
        format!(
            "http://{SANDBOX_HOST}:{}/{token}/{}",
            self.port(),
            provider.path_name()
        )
    }
}

fn unix_time() -> u64 {
    SystemTime::now()
        .duration_since(SystemTime::UNIX_EPOCH)
        .map(|duration| duration.as_secs())
        .unwrap_or(0)
}

fn respond(request: tiny_http::Request, status: u16, body: Vec<u8>) {
    let response = tiny_http::Response::from_data(body)
        .with_status_code(status)
        .with_header(content_type_json());
    if let Err(error) = request.respond(response) {
        log::debug!("model gateway could not answer: {error}");
    }
}

fn content_type_json() -> tiny_http::Header {
    tiny_http::Header {
        field: "Content-Type".parse().expect("valid header name"),
        value: "application/json".parse().expect("valid header value"),
    }
}

/// Headers that belong to one hop, or carry credentials the gateway replaces.
fn is_hop_or_credential(name: &str) -> bool {
    matches!(
        name.to_ascii_lowercase().as_str(),
        "host"
            | "connection"
            | "content-length"
            | "transfer-encoding"
            | "keep-alive"
            | "proxy-authorization"
            | "proxy-connection"
            | "upgrade"
            | "te"
            | "trailer"
    )
}

fn is_credential(name: &str) -> bool {
    matches!(
        name.to_ascii_lowercase().as_str(),
        "authorization" | "x-api-key"
    )
}

struct Target<'a> {
    token: &'a str,
    provider: Provider,
    rest: &'a str,
}

fn parse_target(url: &str) -> Option<Target<'_>> {
    let mut parts = url.trim_start_matches('/').splitn(3, '/');
    let token = parts.next().filter(|token| !token.is_empty())?;
    let provider = Provider::from_path_name(parts.next()?)?;
    let rest = parts.next().unwrap_or("");
    Some(Target {
        token,
        provider,
        rest,
    })
}

fn requested_model(body: &[u8]) -> Option<String> {
    serde_json::from_slice::<serde_json::Value>(body)
        .ok()?
        .get("model")?
        .as_str()
        .map(str::to_string)
}

fn handle(state: &State, mut request: tiny_http::Request) {
    let started = Instant::now();
    let url = request.url().to_string();
    let Some(target) = parse_target(&url) else {
        return respond(request, 404, b"{\"error\":\"unknown route\"}".to_vec());
    };
    let Some(route) = state.routes.lock().get(target.token).cloned() else {
        return respond(request, 403, b"{\"error\":\"unknown project\"}".to_vec());
    };
    let provider = target.provider;
    let path = format!("/{}", target.rest);

    let mut body = Vec::new();
    if let Err(error) = request
        .as_reader()
        .take(MAX_REQUEST_SIZE + 1)
        .read_to_end(&mut body)
    {
        log::warn!("model gateway could not read a request: {error}");
        return respond(request, 400, b"{\"error\":\"unreadable request\"}".to_vec());
    }
    if body.len() as u64 > MAX_REQUEST_SIZE {
        return respond(request, 413, b"{\"error\":\"request too large\"}".to_vec());
    }
    let model = requested_model(&body);
    let mut call = Call {
        project: route.project.clone(),
        provider: provider.path_name(),
        path: path.split('?').next().unwrap_or(&path).to_string(),
        model: model.clone(),
        status: 0,
        allowed: true,
        duration_ms: 0,
        unix_time: unix_time(),
    };

    if let Some(model) = &model
        && !route.allows(model)
    {
        call.allowed = false;
        call.status = 403;
        (state.log)(call);
        let message = format!(
            "Sands: the project {} may not use the model {model}. Pick a model from its tiers.",
            route.project
        );
        return respond(request, 403, provider.rejection(&message));
    }

    let upstream = state
        .upstreams
        .get(&provider)
        .map(String::as_str)
        .unwrap_or(provider.default_upstream());
    let mut builder = http::Request::builder()
        .method(request.method().as_str())
        .uri(format!("{}{path}", upstream.trim_end_matches('/')));
    // A Claude subscription signs requests with a bearer token. Those pass as they are, so the
    // host key never moves subscription use onto API billing.
    let signed_by_caller = provider == Provider::Anthropic
        && request
            .headers()
            .iter()
            .any(|header| header.field.equiv("authorization"));
    let host_key = route.keys.get(&provider).filter(|_| !signed_by_caller);
    for header in request.headers() {
        let name = header.field.as_str().as_str();
        if is_hop_or_credential(name) || (host_key.is_some() && is_credential(name)) {
            continue;
        }
        builder = builder.header(name, header.value.as_str());
    }
    if let Some(key) = host_key {
        builder = provider.authorize(builder, key);
    }
    let upstream_request = match builder.body(AsyncBody::from(body)) {
        Ok(upstream_request) => upstream_request,
        Err(error) => {
            log::warn!("model gateway could not build a request: {error}");
            call.status = 400;
            (state.log)(call);
            return respond(request, 400, b"{\"error\":\"bad request\"}".to_vec());
        }
    };

    let response = futures::executor::block_on(state.http.send(upstream_request));
    let response = match response {
        Ok(response) => response,
        Err(error) => {
            log::warn!("model gateway could not reach {upstream}: {error:#}");
            call.status = 502;
            call.duration_ms = started.elapsed().as_millis() as u64;
            (state.log)(call);
            return respond(
                request,
                502,
                b"{\"error\":\"the model API is unreachable\"}".to_vec(),
            );
        }
    };
    let status = response.status().as_u16();
    let length = response
        .headers()
        .get(http::header::CONTENT_LENGTH)
        .and_then(|value| value.to_str().ok())
        .and_then(|value| value.parse::<usize>().ok());
    let headers = response
        .headers()
        .iter()
        .filter(|(name, _)| !is_hop_or_credential(name.as_str()))
        .filter_map(|(name, value)| {
            tiny_http::Header::from_bytes(name.as_str().as_bytes(), value.as_bytes()).ok()
        })
        .collect::<Vec<_>>();
    let body = BlockingReader(response.into_body());
    let reply =
        tiny_http::Response::new(tiny_http::StatusCode(status), headers, body, length, None);
    if let Err(error) = request.respond(reply) {
        log::debug!("model gateway stopped streaming a response: {error}");
    }
    call.status = status;
    call.duration_ms = started.elapsed().as_millis() as u64;
    (state.log)(call);
}

/// Hands an upstream body to `tiny_http`, which reads synchronously, chunk by chunk as the model
/// produces it.
struct BlockingReader(AsyncBody);

impl Read for BlockingReader {
    fn read(&mut self, buffer: &mut [u8]) -> std::io::Result<usize> {
        futures::executor::block_on(Pin::new(&mut self.0).read(buffer))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::{io::Write as _, time::Duration};

    fn record() -> (CallLog, Arc<Mutex<Vec<Call>>>) {
        let calls = Arc::new(Mutex::new(Vec::new()));
        let log: CallLog = Arc::new({
            let calls = calls.clone();
            move |call| calls.lock().push(call)
        });
        (log, calls)
    }

    /// A stand-in for the provider API: answers once and reports what it received.
    fn fake_upstream() -> (
        String,
        std::sync::mpsc::Receiver<(String, Vec<(String, String)>)>,
    ) {
        let server = tiny_http::Server::http("127.0.0.1:0").unwrap();
        let address = server.server_addr().to_ip().unwrap();
        let (sender, receiver) = std::sync::mpsc::channel();
        thread::spawn(move || {
            for request in server.incoming_requests() {
                let headers = request
                    .headers()
                    .iter()
                    .map(|header| {
                        (
                            header.field.as_str().as_str().to_ascii_lowercase(),
                            header.value.as_str().to_string(),
                        )
                    })
                    .collect();
                sender.send((request.url().to_string(), headers)).ok();
                request
                    .respond(tiny_http::Response::from_string("data: hello\n\n"))
                    .ok();
            }
        });
        (format!("http://{address}"), receiver)
    }

    fn post(url: &str, headers: &[(&str, &str)], body: &str) -> (u16, String) {
        let url = url.strip_prefix("http://").unwrap();
        let (authority, path) = url.split_once('/').unwrap();
        let mut stream = std::net::TcpStream::connect(authority).unwrap();
        let mut request = format!(
            "POST /{path} HTTP/1.1\r\nHost: {authority}\r\nContent-Length: {}\r\nConnection: close\r\n",
            body.len()
        );
        for (name, value) in headers {
            request.push_str(&format!("{name}: {value}\r\n"));
        }
        request.push_str("\r\n");
        request.push_str(body);
        stream.write_all(request.as_bytes()).unwrap();
        let mut response = String::new();
        stream.read_to_string(&mut response).unwrap();
        let status = response
            .split_whitespace()
            .nth(1)
            .and_then(|status| status.parse().ok())
            .unwrap();
        let body = response
            .split_once("\r\n\r\n")
            .map(|(_, body)| body.to_string())
            .unwrap_or_default();
        (status, body)
    }

    fn gateway(upstream: &str, log: CallLog) -> Gateway {
        let http: Arc<dyn HttpClient> = Arc::new(reqwest_client::ReqwestClient::new());
        Gateway::start_with_upstreams(
            0,
            http,
            log,
            HashMap::from_iter([(Provider::Anthropic, upstream.to_string())]),
        )
        .unwrap()
    }

    fn local_url(gateway: &Gateway, token: &str, rest: &str) -> String {
        format!(
            "http://127.0.0.1:{}/{token}/anthropic/{rest}",
            gateway.port()
        )
    }

    #[test]
    fn forwards_allowed_models_with_the_host_key() {
        let (upstream, received) = fake_upstream();
        let (log, calls) = record();
        let gateway = gateway(&upstream, log);
        let token = gateway.register(Route {
            project: "shop".to_string(),
            allowed_models: Some(vec!["anthropic/claude-work".to_string()]),
            keys: HashMap::from_iter([(Provider::Anthropic, "host-secret".to_string())]),
        });

        let (status, body) = post(
            &local_url(&gateway, &token, "v1/messages?beta=true"),
            &[
                ("x-api-key", "sandbox-placeholder"),
                ("anthropic-version", "2023-06-01"),
            ],
            r#"{"model":"claude-work","stream":true}"#,
        );
        assert_eq!(status, 200);
        assert!(body.contains("data: hello"), "{body}");
        let (path, headers) = received.recv_timeout(Duration::from_secs(5)).unwrap();
        assert_eq!(path, "/v1/messages?beta=true");
        let header = |name: &str| {
            headers
                .iter()
                .filter(|(header, _)| header == name)
                .map(|(_, value)| value.clone())
                .collect::<Vec<_>>()
        };
        assert_eq!(header("x-api-key"), ["host-secret"]);
        assert_eq!(header("anthropic-version"), ["2023-06-01"]);

        let call = calls.lock().first().cloned().unwrap();
        assert_eq!(call.project, "shop");
        assert_eq!(call.model.as_deref(), Some("claude-work"));
        assert_eq!(call.path, "/v1/messages");
        assert!(call.allowed);
        assert_eq!(call.status, 200);
    }

    #[test]
    fn refuses_models_outside_the_project_and_unknown_tokens() {
        let (upstream, received) = fake_upstream();
        let (log, calls) = record();
        let gateway = gateway(&upstream, log);
        let token = gateway.register(Route {
            project: "shop".to_string(),
            allowed_models: Some(vec!["anthropic/claude-work".to_string()]),
            keys: HashMap::new(),
        });

        let (status, body) = post(
            &local_url(&gateway, &token, "v1/messages"),
            &[],
            r#"{"model":"claude-plan"}"#,
        );
        assert_eq!(status, 403);
        assert!(body.contains("may not use the model claude-plan"), "{body}");
        assert!(!calls.lock()[0].allowed);

        let (status, _) = post(
            &local_url(&gateway, "not-a-token", "v1/messages"),
            &[],
            r#"{"model":"claude-work"}"#,
        );
        assert_eq!(status, 403);

        gateway.unregister(&token);
        let (status, _) = post(
            &local_url(&gateway, &token, "v1/messages"),
            &[],
            r#"{"model":"claude-work"}"#,
        );
        assert_eq!(status, 403);
        assert!(
            received.try_recv().is_err(),
            "nothing may reach the provider"
        );
    }

    #[test]
    fn without_a_host_key_the_callers_credentials_pass() {
        let (upstream, received) = fake_upstream();
        let (log, _) = record();
        let gateway = gateway(&upstream, log);
        let token = gateway.register(Route {
            project: "shop".to_string(),
            allowed_models: None,
            keys: HashMap::new(),
        });
        let (status, _) = post(
            &local_url(&gateway, &token, "v1/messages"),
            &[("authorization", "Bearer subscription")],
            r#"{"model":"anything"}"#,
        );
        assert_eq!(status, 200);
        let (_, headers) = received.recv_timeout(Duration::from_secs(5)).unwrap();
        assert!(
            headers
                .iter()
                .any(|(name, value)| name == "authorization" && value == "Bearer subscription")
        );
    }

    #[test]
    fn a_claude_subscription_keeps_its_own_credentials() {
        let (upstream, received) = fake_upstream();
        let (log, _) = record();
        let gateway = gateway(&upstream, log);
        let token = gateway.register(Route {
            project: "shop".to_string(),
            allowed_models: None,
            keys: HashMap::from_iter([(Provider::Anthropic, "host-secret".to_string())]),
        });
        let (status, _) = post(
            &local_url(&gateway, &token, "v1/messages"),
            &[
                ("authorization", "Bearer oauth"),
                ("x-api-key", "placeholder"),
            ],
            r#"{"model":"claude-work"}"#,
        );
        assert_eq!(status, 200);
        let (_, headers) = received.recv_timeout(Duration::from_secs(5)).unwrap();
        assert!(
            headers
                .iter()
                .any(|(name, value)| name == "authorization" && value == "Bearer oauth")
        );
        assert!(!headers.iter().any(|(_, value)| value == "host-secret"));
    }

    #[test]
    fn aliases_cover_dated_versions_only() {
        let route = Route {
            project: "shop".to_string(),
            allowed_models: Some(vec!["anthropic/claude-sonnet-4-5".to_string()]),
            keys: HashMap::new(),
        };
        assert!(route.allows("claude-sonnet-4-5"));
        assert!(route.allows("claude-sonnet-4-5-20250929"));
        assert!(!route.allows("claude-sonnet-4"));
        assert!(!route.allows("claude-sonnet-4-50"));
    }

    #[test]
    fn base_url_names_the_sandbox_host() {
        let (log, _) = record();
        let gateway = gateway("http://127.0.0.1:9", log);
        assert_eq!(
            gateway.base_url("abc", Provider::Anthropic),
            format!(
                "http://host.docker.internal:{}/abc/anthropic",
                gateway.port()
            )
        );
    }
}
