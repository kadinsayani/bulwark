use bulwark_config::Thresholds;
use bulwark_wasm_host::PluginExecutionError;
use bulwark_wasm_sdk::BodyChunk;
use tokio::task::JoinHandle;
use tracing::Span;

use {
    crate::{
        serialize_decision_sfv, serialize_tags_sfv, FilterProcessingError,
        MultiPluginInstantiationError,
    },
    bulwark_config::Config,
    bulwark_wasm_host::{
        DecisionComponents, Plugin, PluginInstance, PluginLoadError, RedisInfo, RequestContext,
        ScriptRegistry,
    },
    bulwark_wasm_sdk::{Decision, MassFunction},
    envoy_control_plane::envoy::{
        config::core::v3::{HeaderMap, HeaderValue, HeaderValueOption},
        r#type::v3::HttpStatus,
        service::ext_proc::v3::{
            external_processor_server::ExternalProcessor, processing_request, processing_response,
            CommonResponse, HeaderMutation, HeadersResponse, HttpHeaders, ImmediateResponse,
            ProcessingRequest, ProcessingResponse,
        },
    },
    futures::{
        channel::mpsc::{SendError, UnboundedSender},
        SinkExt, Stream,
    },
    matchit::Router,
    std::{
        collections::HashSet,
        pin::Pin,
        str,
        str::FromStr,
        sync::{Arc, Mutex},
        time::Duration,
    },
    tokio::{sync::RwLock, task::JoinSet, time::timeout},
    tonic::{Request, Response, Status, Streaming},
    tracing::{debug, error, info, instrument, trace, warn, Instrument},
};

extern crate redis;

type ExternalProcessorStream =
    Pin<Box<dyn Stream<Item = Result<ProcessingResponse, Status>> + Send>>;
type PluginList = Vec<Arc<Plugin>>;

struct RouteTarget {
    value: String,
    plugins: PluginList,
    timeout: Option<u64>,
}

// TODO: BulwarkProcessor::new should take a config root as a param, compile all the plugins and build a radix tree router that maps to them

#[derive(Clone)]
pub struct BulwarkProcessor {
    // TODO: may need to have a plugin registry at some point
    router: Arc<RwLock<Router<RouteTarget>>>,
    redis_info: Option<Arc<RedisInfo>>,
    thresholds: bulwark_config::Thresholds,
}

impl BulwarkProcessor {
    pub fn new(config: Config) -> Result<Self, PluginLoadError> {
        let redis_info = if let Some(service) = config.service.as_ref() {
            if let Some(remote_state_addr) = service.remote_state.as_ref() {
                // TODO: better error handling instead of unwrap/panic
                let client = redis::Client::open(remote_state_addr.as_str()).unwrap();
                // TODO: make pool size configurable
                Some(Arc::new(RedisInfo {
                    // TODO: better error handling instead of unwrap/panic
                    pool: r2d2::Pool::builder().max_size(16).build(client).unwrap(),
                    registry: ScriptRegistry::default(),
                }))
            } else {
                None
            }
        } else {
            None
        };

        // TODO: return an init error not a plugin load error
        let mut router: Router<RouteTarget> = Router::new();
        if let Some(resources) = config.resources.as_ref() {
            for resource in resources {
                let plugin_configs = resource.resolve_plugins(&config);
                let mut plugins: PluginList = Vec::with_capacity(plugin_configs.len());
                for plugin_config in &plugin_configs {
                    // TODO: pass in the plugin config
                    debug!(
                        plugin_path = plugin_config.path,
                        message = "loading plugin",
                        resource = resource.route
                    );
                    let plugin = Plugin::from_file(
                        plugin_config.path.clone(),
                        plugin_config.config_as_json(),
                    )?;
                    plugins.push(Arc::new(plugin));
                }
                router
                    .insert(
                        resource.route.clone(),
                        RouteTarget {
                            value: resource.route.clone(),
                            timeout: resource.timeout,
                            plugins,
                        },
                    )
                    .ok();
            }
        } else {
            // TODO: error handling
            panic!("no resources found");
        }
        Ok(Self {
            router: Arc::new(RwLock::new(router)),
            redis_info,
            thresholds: config.thresholds.unwrap_or_default(),
        })
    }
}

#[tonic::async_trait]
impl ExternalProcessor for BulwarkProcessor {
    type ProcessStream = ExternalProcessorStream;

    #[instrument(name = "request_handler", skip(self, request))]
    async fn process(
        &self,
        request: Request<Streaming<ProcessingRequest>>,
    ) -> Result<Response<ExternalProcessorStream>, Status> {
        let mut stream = request.into_inner();
        let thresholds = self.thresholds;
        if let Ok(http_req) = prepare_request(&mut stream).await {
            let redis_info = self.redis_info.clone();
            let http_req = Arc::new(http_req);
            let router = self.router.clone();

            info!(
                message = "handling request",
                method = http_req.method().to_string(),
                uri = http_req.uri().to_string(),
                user_agent = http_req
                    .headers()
                    .get("User-Agent")
                    .map(|ua: &http::HeaderValue| ua.to_str().unwrap_or_default())
            );

            let child_span = tracing::debug_span!("routing request");
            let (sender, receiver) = futures::channel::mpsc::unbounded();
            tokio::task::spawn(
                async move {
                    let http_req = http_req.clone();
                    let router = router.read().await;
                    let route_result = router.at(http_req.uri().path());
                    // TODO: router needs to point to a struct that bundles the plugin set and associated config like timeout duration
                    // TODO: put default timeout in a constant somewhere central
                    // TODO: figure out timeout from optional resource-specific timeout or central default
                    let mut timeout_duration = Duration::from_millis(10);
                    match route_result {
                        Ok(route_match) => {
                            // TODO: may want to expose params to logging after redaction
                            let route_target = route_match.value;
                            // TODO: figure out how to bubble the error out of the task and up to the parent
                            // TODO: figure out if tonic-error or some other option is the best way to convert to a tonic Status error
                            let plugin_instances = instantiate_plugins(
                                &route_target.plugins,
                                redis_info.clone(),
                                http_req.clone(),
                                route_match.params,
                            )
                            .unwrap();
                            if let Some(millis) = route_match.value.timeout {
                                timeout_duration = Duration::from_millis(millis);
                            }

                            let combined =
                                execute_request_phase(plugin_instances.clone(), timeout_duration)
                                    .await;

                            handle_request_phase_decision(
                                sender,
                                stream,
                                combined,
                                thresholds,
                                plugin_instances,
                                timeout_duration,
                            )
                            .await;
                        }
                        Err(err) => {
                            // TODO: figure out how to handle trailing slash errors, silent failure is probably undesirable
                            error!(uri = http_req.uri().to_string(), message = "match error");
                            panic!("match error");
                        }
                    };
                }
                .instrument(child_span.or_current()),
            );
            return Ok(Response::new(Box::pin(receiver)));
        }
        // By default, just close the stream.
        Ok(Response::new(Box::pin(futures::stream::empty())))
    }
}

// TODO: a bunch of these fns seem like they should probably be inside the BulwarkProcessor impl but currently aren't due to async/move

fn instantiate_plugins(
    plugins: &PluginList,
    redis_info: Option<Arc<RedisInfo>>,
    http_req: Arc<bulwark_wasm_sdk::Request>,
    params: matchit::Params,
) -> Result<Vec<Arc<Mutex<PluginInstance>>>, MultiPluginInstantiationError> {
    let mut plugin_instances = Vec::with_capacity(plugins.len());
    let mut shared_params = bulwark_wasm_sdk::Map::new();
    for (key, value) in params.iter() {
        let wrapped_value = bulwark_wasm_sdk::Value::String(value.to_string());
        shared_params.insert(key.to_string(), wrapped_value);
    }
    let shared_params = Arc::new(Mutex::new(shared_params));
    for plugin in plugins {
        let request_context = RequestContext::new(
            plugin.clone(),
            redis_info.clone(),
            shared_params.clone(),
            http_req.clone(),
        )?;

        plugin_instances.push(Arc::new(Mutex::new(PluginInstance::new(
            plugin.clone(),
            request_context,
        )?)));
    }
    Ok(plugin_instances)
}

async fn execute_request_phase(
    plugin_instances: Vec<Arc<Mutex<PluginInstance>>>,
    timeout_duration: std::time::Duration,
) -> DecisionComponents {
    execute_request_phase_one(plugin_instances.clone(), timeout_duration).await;
    execute_request_phase_two(plugin_instances.clone(), timeout_duration).await
}

async fn execute_request_phase_one(
    plugin_instances: Vec<Arc<Mutex<PluginInstance>>>,
    timeout_duration: std::time::Duration,
) {
    let mut phase_one_tasks = JoinSet::new();
    for plugin_instance in plugin_instances.clone() {
        let phase_one_child_span: Span;
        {
            let plugin_instance = plugin_instance.lock().unwrap();
            phase_one_child_span = tracing::debug_span!(
                "executing on_request phase",
                // TODO: figure out why this isn't displayed in the logs
                plugin = plugin_instance.plugin_reference(),
            );
        }
        phase_one_tasks.spawn(
            timeout(timeout_duration, async move {
                // TODO: avoid unwraps
                execute_plugin_initialization(plugin_instance.clone()).unwrap();
                execute_on_request(plugin_instance.clone()).unwrap();
            })
            .instrument(phase_one_child_span.or_current()),
        );
    }
    // efficiently hand execution off to the plugins
    tokio::task::yield_now().await;

    while let Some(r) = phase_one_tasks.join_next().await {
        match r {
            Ok(Ok(_)) => {}
            Ok(Err(e)) => {
                warn!(
                    message = "timeout on plugin execution",
                    elapsed = ?e,
                );
            }
            Err(e) => {
                warn!(
                    message = "join error on plugin execution",
                    error_message = ?e,
                );
            }
        }
    }
}

async fn execute_request_phase_two(
    plugin_instances: Vec<Arc<Mutex<PluginInstance>>>,
    timeout_duration: std::time::Duration,
) -> DecisionComponents {
    let decision_components = Arc::new(Mutex::new(Vec::with_capacity(plugin_instances.len())));
    let mut phase_two_tasks = JoinSet::new();
    for plugin_instance in plugin_instances.clone() {
        let phase_two_child_span: Span;
        {
            let plugin_instance = plugin_instance.lock().unwrap();
            phase_two_child_span = tracing::debug_span!(
                "executing on_request_decision phase",
                // TODO: figure out why this isn't displayed in the logs
                plugin = plugin_instance.plugin_reference(),
            );
        }
        let decision_components = decision_components.clone();
        phase_two_tasks.spawn(
            timeout(timeout_duration, async move {
                let decision_result = execute_on_request_decision(plugin_instance.clone());
                let decision_component = decision_result.unwrap();
                {
                    let decision = &decision_component.decision;
                    debug!(
                        message = "plugin decision result",
                        accept = decision.accept,
                        restrict = decision.restrict,
                        unknown = decision.unknown
                    );
                }
                let mut decision_components = decision_components.lock().unwrap();
                decision_components.push(decision_component);
            })
            .instrument(phase_two_child_span.or_current()),
        );
    }
    // efficiently hand execution off to the plugins
    tokio::task::yield_now().await;

    while let Some(r) = phase_two_tasks.join_next().await {
        match r {
            Ok(Ok(_)) => {}
            Ok(Err(e)) => {
                warn!(
                    message = "timeout on plugin execution",
                    elapsed = ?e,
                );
            }
            Err(e) => {
                warn!(
                    message = "join error on plugin execution",
                    error_message = ?e,
                );
            }
        }
    }
    let decision_vec: Vec<Decision>;
    let tags: HashSet<String>;
    {
        let decision_components = decision_components.lock().unwrap();
        decision_vec = decision_components.iter().map(|dc| dc.decision).collect();
        tags = decision_components
            .iter()
            .flat_map(|dc| dc.tags.clone())
            .collect();
    }
    let decision = Decision::combine(&decision_vec);

    info!(
        message = "decision combined",
        accept = decision.accept,
        restrict = decision.restrict,
        unknown = decision.unknown,
        // array values aren't handled well unfortunately, coercing to comma-separated values seems to be the best option
        tags = tags
            .iter()
            .map(|s| s.as_str())
            .collect::<Vec<&str>>()
            .to_vec()
            .join(","),
        count = decision_vec.len(),
    );

    DecisionComponents {
        decision,
        tags: tags.into_iter().collect(),
    }
}

async fn execute_response_phase(
    plugin_instances: Vec<Arc<Mutex<PluginInstance>>>,
    response: Arc<http::Response<BodyChunk>>,
    timeout_duration: std::time::Duration,
) -> DecisionComponents {
    let decision_components = Arc::new(Mutex::new(Vec::with_capacity(plugin_instances.len())));
    let mut response_phase_tasks = JoinSet::new();
    for plugin_instance in plugin_instances.clone() {
        let response_phase_child_span: Span;
        {
            let plugin_instance = plugin_instance.lock().unwrap();
            response_phase_child_span = tracing::debug_span!(
                "executing on_response_decision phase",
                // TODO: figure out why this isn't displayed in the logs
                plugin = plugin_instance.plugin_reference(),
            );
        }
        {
            // Make sure the plugin instance knows about the response
            let mut plugin_instance = plugin_instance.lock().unwrap();
            let response = response.clone();
            plugin_instance.set_response(response);
        }
        let decision_components = decision_components.clone();
        response_phase_tasks.spawn(
            timeout(timeout_duration, async move {
                let decision_result = execute_on_response_decision(plugin_instance.clone());
                let decision_component = decision_result.unwrap();
                {
                    let decision = &decision_component.decision;
                    debug!(
                        message = "plugin decision result",
                        accept = decision.accept,
                        restrict = decision.restrict,
                        unknown = decision.unknown
                    );
                }
                let mut decision_components = decision_components.lock().unwrap();
                decision_components.push(decision_component);
            })
            .instrument(response_phase_child_span.or_current()),
        );
    }
    // efficiently hand execution off to the plugins
    tokio::task::yield_now().await;

    while let Some(r) = response_phase_tasks.join_next().await {
        match r {
            Ok(Ok(_)) => {}
            Ok(Err(e)) => {
                warn!(
                    message = "timeout on plugin execution",
                    elapsed = ?e,
                );
            }
            Err(e) => {
                warn!(
                    message = "join error on plugin execution",
                    error_message = ?e,
                );
            }
        }
    }
    let decision_vec: Vec<Decision>;
    let tags: HashSet<String>;
    {
        let decision_components = decision_components.lock().unwrap();
        decision_vec = decision_components.iter().map(|dc| dc.decision).collect();
        tags = decision_components
            .iter()
            .flat_map(|dc| dc.tags.clone())
            .collect();
    }
    let decision = Decision::combine(&decision_vec);

    info!(
        message = "decision combined",
        accept = decision.accept,
        restrict = decision.restrict,
        unknown = decision.unknown,
        // array values aren't handled well unfortunately, coercing to comma-separated values seems to be the best option
        tags = tags
            .iter()
            .map(|s| s.as_str())
            .collect::<Vec<&str>>()
            .to_vec()
            .join(","),
        count = decision_vec.len(),
    );

    DecisionComponents {
        decision,
        tags: tags.into_iter().collect(),
    }
}

fn execute_plugin_initialization(
    plugin_instance: Arc<Mutex<PluginInstance>>,
) -> Result<(), PluginExecutionError> {
    let mut plugin_instance = plugin_instance.lock().unwrap();
    // unlike on_request, the _start/main function is mandatory
    plugin_instance.start()
}

fn execute_on_request(
    plugin_instance: Arc<Mutex<PluginInstance>>,
) -> Result<(), PluginExecutionError> {
    let mut plugin_instance = plugin_instance.lock().unwrap();
    let result = plugin_instance.handle_request();
    match result {
        Ok(_) => result,
        Err(e) => match e {
            // we can silence not implemented errors because they are expected and normal here
            PluginExecutionError::NotImplementedError { expected: _ } => Ok(()),
            // everything else will get passed along
            _ => Err(e),
        },
    }
}

fn execute_on_request_decision(
    plugin_instance: Arc<Mutex<PluginInstance>>,
) -> Result<DecisionComponents, PluginExecutionError> {
    let mut plugin_instance = plugin_instance.lock().unwrap();
    let result = plugin_instance.handle_request_decision();
    if let Err(e) = result {
        match e {
            // we can silence not implemented errors because they are expected and normal here
            PluginExecutionError::NotImplementedError { expected: _ } => (),
            // everything else will get passed along
            _ => Err(e)?,
        }
    }
    Ok(plugin_instance.get_decision())
}

fn execute_on_response_decision(
    plugin_instance: Arc<Mutex<PluginInstance>>,
) -> Result<DecisionComponents, PluginExecutionError> {
    let mut plugin_instance = plugin_instance.lock().unwrap();
    let result = plugin_instance.handle_response_decision();
    if let Err(e) = result {
        match e {
            // we can silence not implemented errors because they are expected and normal here
            PluginExecutionError::NotImplementedError { expected: _ } => (),
            // everything else will get passed along
            _ => Err(e)?,
        }
    }
    Ok(plugin_instance.get_decision())
}

fn execute_on_decision_feedback(
    plugin_instance: Arc<Mutex<PluginInstance>>,
) -> Result<(), PluginExecutionError> {
    let mut plugin_instance = plugin_instance.lock().unwrap();
    let result = plugin_instance.handle_decision_feedback();
    if let Err(e) = result {
        match e {
            // we can silence not implemented errors because they are expected and normal here
            PluginExecutionError::NotImplementedError { expected: _ } => (),
            // everything else will get passed along
            _ => Err(e)?,
        }
    }
    Ok(())
}

async fn prepare_request(
    stream: &mut Streaming<ProcessingRequest>,
) -> Result<bulwark_wasm_sdk::Request, FilterProcessingError> {
    // TODO: determine client IP address, pass it through as an extension
    if let Some(header_msg) = get_request_headers(stream).await {
        // TODO: currently this information isn't used and isn't accessible to the plugin environment yet
        let authority = get_header_value(&header_msg.headers, ":authority").ok_or_else(|| {
            FilterProcessingError::Error(anyhow::anyhow!("Missing HTTP authority"))
        })?;
        let scheme = get_header_value(&header_msg.headers, ":scheme")
            .ok_or_else(|| FilterProcessingError::Error(anyhow::anyhow!("Missing HTTP scheme")))?;

        let method =
            http::Method::from_str(get_header_value(&header_msg.headers, ":method").ok_or_else(
                || FilterProcessingError::Error(anyhow::anyhow!("Missing HTTP method")),
            )?)?;
        let request_uri = get_header_value(&header_msg.headers, ":path").ok_or_else(|| {
            FilterProcessingError::Error(anyhow::anyhow!("Missing HTTP request URI"))
        })?;
        let mut request = http::Request::builder();
        // TODO: read the request body
        let request_chunk = bulwark_wasm_sdk::BodyChunk {
            end_of_stream: header_msg.end_of_stream,
            size: 0,
            start: 0,
            content: vec![],
        };
        request = request.method(method).uri(request_uri);
        match &header_msg.headers {
            Some(headers) => {
                for header in &headers.headers {
                    // must not pass through Envoy pseudo headers here, http module treats them as invalid
                    if !header.key.starts_with(':') {
                        request = request.header(&header.key, &header.value);
                    }
                }
            }
            None => {}
        }
        return Ok(request.body(request_chunk).unwrap());
    }
    // TODO: what exactly should happen here?
    Err(FilterProcessingError::Error(anyhow::anyhow!(
        "Nothing useful happened"
    )))
}

async fn prepare_response(
    stream: &mut Streaming<ProcessingRequest>,
) -> Result<bulwark_wasm_sdk::Response, FilterProcessingError> {
    // TODO: determine client IP address, pass it through as an extension
    if let Some(header_msg) = get_response_headers(stream).await {
        // TODO: make this an int? status builder function might not care though?
        let status = get_header_value(&header_msg.headers, ":status")
            .ok_or_else(|| FilterProcessingError::Error(anyhow::anyhow!("Missing HTTP status")))?;

        let mut response = http::Response::builder();
        // TODO: read the response body
        let response_chunk = bulwark_wasm_sdk::BodyChunk {
            end_of_stream: header_msg.end_of_stream,
            size: 0,
            start: 0,
            content: vec![],
        };
        response = response.status(status);
        match &header_msg.headers {
            Some(headers) => {
                for header in &headers.headers {
                    // must not pass through Envoy pseudo headers here, http module treats them as invalid
                    if !header.key.starts_with(':') {
                        response = response.header(&header.key, &header.value);
                    }
                }
            }
            None => {}
        }
        return Ok(response.body(response_chunk).unwrap());
    }
    // TODO: what exactly should happen here?
    Err(FilterProcessingError::Error(anyhow::anyhow!(
        "Nothing useful happened"
    )))
}

async fn handle_request_phase_decision(
    sender: UnboundedSender<Result<ProcessingResponse, Status>>,
    mut stream: Streaming<ProcessingRequest>,
    decision_components: DecisionComponents,
    thresholds: Thresholds,
    plugin_instances: Vec<Arc<Mutex<PluginInstance>>>,
    timeout_duration: std::time::Duration,
) {
    let outcome = decision_components
        .decision
        .outcome(thresholds.trust, thresholds.suspicious, thresholds.restrict)
        .unwrap();

    match outcome {
            bulwark_wasm_sdk::Outcome::Trusted
            | bulwark_wasm_sdk::Outcome::Accepted
            // suspected requests are monitored but not rejected
            | bulwark_wasm_sdk::Outcome::Suspected => {
                let result = allow_request(&sender, &decision_components).await;
                // TODO: must perform error handling on sender results, sending can definitely fail
                debug!(message = "send result", result = result.is_ok());
               },
               bulwark_wasm_sdk::Outcome::Restricted => {
                let result = block_request(&sender, &decision_components).await;
                // TODO: must perform error handling on sender results, sending can definitely fail
                debug!(message = "send result", result = result.is_ok());
        },
    }

    if outcome == bulwark_wasm_sdk::Outcome::Restricted {
        // Normally we initiate feedback after the response phase, but if we skip the response phase
        // we need to do it here instead.
        handle_decision_feedback(
            decision_components,
            outcome,
            plugin_instances,
            timeout_duration,
        );
        // Short-circuit if restricted, we can skip the response phase
        return;
    }

    if let Ok(http_resp) = prepare_response(&mut stream).await {
        let http_resp = Arc::new(http_resp);

        handle_response_phase_decision(
            sender,
            execute_response_phase(plugin_instances.clone(), http_resp, timeout_duration).await,
            // TODO: get thresholds from config
            Thresholds::default(),
            plugin_instances.clone(),
            timeout_duration,
        )
        .await;
    }
}

async fn handle_response_phase_decision(
    sender: UnboundedSender<Result<ProcessingResponse, Status>>,
    decision_components: DecisionComponents,
    thresholds: Thresholds,
    plugin_instances: Vec<Arc<Mutex<PluginInstance>>>,
    timeout_duration: std::time::Duration,
) {
    let outcome = decision_components
        .decision
        .outcome(thresholds.trust, thresholds.suspicious, thresholds.restrict)
        .unwrap();
    match outcome {
        bulwark_wasm_sdk::Outcome::Trusted
        | bulwark_wasm_sdk::Outcome::Accepted
        // suspected requests are monitored but not rejected
        | bulwark_wasm_sdk::Outcome::Suspected => {
            let result = allow_response(&sender, &decision_components).await;
            // TODO: must perform error handling on sender results, sending can definitely fail
            debug!(message = "send result", result = result.is_ok());
        },
        bulwark_wasm_sdk::Outcome::Restricted => {
            let result = block_response(&sender, &decision_components).await;
            // TODO: must perform error handling on sender results, sending can definitely fail
            debug!(message = "send result", result = result.is_ok());
        }
    }

    handle_decision_feedback(
        decision_components,
        outcome,
        plugin_instances,
        timeout_duration,
    );
}

fn handle_decision_feedback(
    decision_components: DecisionComponents,
    outcome: bulwark_wasm_sdk::Outcome,
    plugin_instances: Vec<Arc<Mutex<PluginInstance>>>,
    timeout_duration: std::time::Duration,
) {
    for plugin_instance in plugin_instances.clone() {
        let response_phase_child_span: Span;
        {
            let plugin_instance = plugin_instance.lock().unwrap();
            response_phase_child_span = tracing::debug_span!(
                "executing on_decision_feedback phase",
                // TODO: figure out why this isn't displayed in the logs
                plugin = plugin_instance.plugin_reference(),
            );
        }
        {
            // Make sure the plugin instance knows about the final combined decision
            let mut plugin_instance = plugin_instance.lock().unwrap();
            plugin_instance.set_combined_decision(&decision_components, outcome);
        }
        tokio::spawn(
            timeout(timeout_duration, async move {
                execute_on_decision_feedback(plugin_instance.clone()).ok();
            })
            .instrument(response_phase_child_span.or_current()),
        );
    }
}

async fn allow_request(
    mut sender: &UnboundedSender<Result<ProcessingResponse, Status>>,
    decision_components: &DecisionComponents,
) -> Result<(), SendError> {
    // Send back a response that changes the request header for the HTTP target.
    let mut req_headers_cr = CommonResponse::default();
    add_set_header(
        &mut req_headers_cr,
        "Bulwark-Decision",
        &serialize_decision_sfv(decision_components.decision),
    );
    if !decision_components.tags.is_empty() {
        add_set_header(
            &mut req_headers_cr,
            "Bulwark-Tags",
            &serialize_tags_sfv(decision_components.tags.clone()),
        );
    }
    let req_headers_resp = ProcessingResponse {
        response: Some(processing_response::Response::RequestHeaders(
            HeadersResponse {
                response: Some(req_headers_cr),
            },
        )),
        ..Default::default()
    };
    sender.send(Ok(req_headers_resp)).await
}

async fn block_request(
    mut sender: &UnboundedSender<Result<ProcessingResponse, Status>>,
    // TODO: this will be used in the future
    _decision_components: &DecisionComponents,
) -> Result<(), SendError> {
    // Send back a response indicating the request has been blocked.
    let req_headers_resp = ProcessingResponse {
        response: Some(processing_response::Response::ImmediateResponse(
            ImmediateResponse {
                status: Some(HttpStatus { code: 403 }),
                // TODO: add decision debug
                details: "blocked by bulwark".to_string(),
                // TODO: better default response + customizability
                body: "Bulwark says no.".to_string(),
                headers: None,
                grpc_status: None,
            },
        )),
        ..Default::default()
    };
    sender.send(Ok(req_headers_resp)).await
}

async fn allow_response(
    mut sender: &UnboundedSender<Result<ProcessingResponse, Status>>,
    // TODO: this will be used in the future
    _decision_components: &DecisionComponents,
) -> Result<(), SendError> {
    let resp_headers_resp = ProcessingResponse {
        response: Some(processing_response::Response::RequestHeaders(
            HeadersResponse { response: None },
        )),
        ..Default::default()
    };
    sender.send(Ok(resp_headers_resp)).await
}

async fn block_response(
    mut sender: &UnboundedSender<Result<ProcessingResponse, Status>>,
    // TODO: this will be used in the future
    _decision_components: &DecisionComponents,
) -> Result<(), SendError> {
    // Send back a response indicating the request has been blocked.
    let resp_headers_resp = ProcessingResponse {
        response: Some(processing_response::Response::ImmediateResponse(
            ImmediateResponse {
                status: Some(HttpStatus { code: 403 }),
                // TODO: add decision debug
                details: "blocked by bulwark".to_string(),
                // TODO: better default response + customizability
                body: "Bulwark says no.".to_string(),
                headers: None,
                grpc_status: None,
            },
        )),
        ..Default::default()
    };
    sender.send(Ok(resp_headers_resp)).await
}

async fn get_request_headers(stream: &mut Streaming<ProcessingRequest>) -> Option<HttpHeaders> {
    if let Ok(Some(next_msg)) = stream.message().await {
        if let Some(processing_request::Request::RequestHeaders(hdrs)) = next_msg.request {
            return Some(hdrs);
        }
    }
    None
}

async fn get_response_headers(stream: &mut Streaming<ProcessingRequest>) -> Option<HttpHeaders> {
    if let Ok(Some(next_msg)) = stream.message().await {
        if let Some(processing_request::Request::ResponseHeaders(hdrs)) = next_msg.request {
            return Some(hdrs);
        }
    }
    None
}

fn get_header_value<'a>(header_map: &'a Option<HeaderMap>, name: &str) -> Option<&'a str> {
    match header_map {
        Some(headers) => {
            for header in &headers.headers {
                if header.key == name {
                    return Some(&header.value);
                }
            }
            None
        }
        None => None,
    }
}

fn add_set_header(cr: &mut CommonResponse, key: &str, value: &str) {
    let new_header = HeaderValueOption {
        header: Some(HeaderValue {
            key: key.into(),
            value: value.into(),
        }),
        ..Default::default()
    };
    match &mut cr.header_mutation {
        Some(hm) => hm.set_headers.push(new_header),
        None => {
            let mut new_hm = HeaderMutation::default();
            new_hm.set_headers.push(new_header);
            cr.header_mutation = Some(new_hm);
        }
    }
}
