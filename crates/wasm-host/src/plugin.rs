mod bulwark_host {
    wasmtime::component::bindgen!({
        world: "bulwark:plugin/host-calls",
        async: true,
    });
}

mod request_handler {
    wasmtime::component::bindgen!({
        world: "bulwark:plugin/request-handler",
        async: true,
    });
}

mod request_decision_handler {
    wasmtime::component::bindgen!({
        world: "bulwark:plugin/request-decision-handler",
        async: true,
    });
}

mod response_decision_handler {
    wasmtime::component::bindgen!({
        world: "bulwark:plugin/response-decision-handler",
        async: true,
    });
}

mod decision_feedback_handler {
    wasmtime::component::bindgen!({
        world: "bulwark:plugin/decision-feedback-handler",
        async: true,
    });
}

use {
    crate::{
        ContextInstantiationError, PluginExecutionError, PluginInstantiationError, PluginLoadError,
    },
    async_trait::async_trait,
    bulwark_config::ConfigSerializationError,
    bulwark_host::{DecisionInterface, HeaderInterface, OutcomeInterface},
    bulwark_wasm_sdk::{Decision, Outcome},
    chrono::Utc,
    redis::Commands,
    std::{
        collections::{BTreeSet, HashMap},
        convert::From,
        net::IpAddr,
        ops::DerefMut,
        path::Path,
        sync::{Arc, Mutex, MutexGuard},
    },
    url::Url,
    wasmtime::component::{Component, Linker},
    wasmtime::{AsContextMut, Config, Engine, Store},
    wasmtime_wasi::preview2::{Table, WasiCtx, WasiCtxBuilder, WasiView},
};

extern crate redis;

/// Wraps an [`IpAddr`] representing the remote IP for the incoming request.
///
/// In an architecture with proxies or load balancers in front of Bulwark, this IP will belong to the immediately
/// exterior proxy or load balancer rather than the IP address of the client that originated the request.
pub struct RemoteIP(pub IpAddr);
/// Wraps an [`IpAddr`] representing the forwarded IP for the incoming request.
///
/// In an architecture with proxies or load balancers in front of Bulwark, this IP will belong to the IP address
/// of the client that originated the request rather than the immediately exterior proxy or load balancer.
pub struct ForwardedIP(pub IpAddr);

impl From<Arc<bulwark_wasm_sdk::Request>> for bulwark_host::RequestInterface {
    fn from(request: Arc<bulwark_wasm_sdk::Request>) -> Self {
        bulwark_host::RequestInterface {
            method: request.method().to_string(),
            uri: request.uri().to_string(),
            version: format!("{:?}", request.version()),
            headers: request
                .headers()
                .iter()
                .map(|(name, value)| bulwark_host::HeaderInterface {
                    name: name.to_string(),
                    value: value.as_bytes().to_vec(),
                })
                .collect(),
            chunk_start: request.body().start,
            chunk_length: request.body().size,
            end_of_stream: request.body().end_of_stream,
            // TODO: figure out how to avoid the copy
            chunk: request.body().content.clone(),
        }
    }
}

impl From<Arc<bulwark_wasm_sdk::Response>> for bulwark_host::ResponseInterface {
    fn from(response: Arc<bulwark_wasm_sdk::Response>) -> Self {
        bulwark_host::ResponseInterface {
            // this unwrap should be okay since a non-zero u16 should always be coercible to u32
            status: response.status().as_u16().try_into().unwrap(),
            headers: response
                .headers()
                .iter()
                .map(|(name, value)| bulwark_host::HeaderInterface {
                    name: name.to_string(),
                    value: value.as_bytes().to_vec(),
                })
                .collect(),
            chunk_start: response.body().start,
            chunk_length: response.body().size,
            end_of_stream: response.body().end_of_stream,
            // TODO: figure out how to avoid the copy
            chunk: response.body().content.clone(),
        }
    }
}

impl From<IpAddr> for bulwark_host::IpInterface {
    fn from(ip: IpAddr) -> Self {
        match ip {
            IpAddr::V4(v4) => {
                let octets = v4.octets();
                bulwark_host::IpInterface::V4((octets[0], octets[1], octets[2], octets[3]))
            }
            IpAddr::V6(v6) => {
                let segments = v6.segments();
                bulwark_host::IpInterface::V6((
                    segments[0],
                    segments[1],
                    segments[2],
                    segments[3],
                    segments[4],
                    segments[5],
                    segments[6],
                    segments[7],
                ))
            }
        }
    }
}

impl From<DecisionInterface> for Decision {
    fn from(decision: DecisionInterface) -> Self {
        Decision {
            accept: decision.accept,
            restrict: decision.restrict,
            unknown: decision.unknown,
        }
    }
}

impl From<Decision> for DecisionInterface {
    fn from(decision: Decision) -> Self {
        DecisionInterface {
            accept: decision.accept,
            restrict: decision.restrict,
            unknown: decision.unknown,
        }
    }
}

impl From<Outcome> for OutcomeInterface {
    fn from(outcome: Outcome) -> Self {
        match outcome {
            Outcome::Trusted => OutcomeInterface::Trusted,
            Outcome::Accepted => OutcomeInterface::Accepted,
            Outcome::Suspected => OutcomeInterface::Suspected,
            Outcome::Restricted => OutcomeInterface::Restricted,
        }
    }
}

/// The primary output of a [`PluginInstance`]'s execution. Combines a [`Decision`] and a list of tags together.
///
/// Both the output of individual plugins as well as the combined decision output of a group of plugins may be
/// represented by `DecisionComponents`. The latter is the result of applying Dempster-Shafer combination to each
/// `decision` value in a [`DecisionComponents`] list and then taking the union set of all `tags` lists and forming
/// a new [`DecisionComponents`] with both results.
pub struct DecisionComponents {
    /// A `Decision` made by a plugin or a group of plugins
    pub decision: Decision,
    /// The tags applied by plugins to annotate a [`Decision`]
    pub tags: Vec<String>,
}

/// Wraps a Redis connection pool and a registry of predefined Lua scripts.
pub struct RedisInfo {
    /// The connection pool
    pub pool: r2d2::Pool<redis::Client>,
    /// A Lua script registry
    pub registry: ScriptRegistry,
}

/// A registry of predefined Lua scripts for execution within Redis.
pub struct ScriptRegistry {
    /// Increments a Redis key's counter value if it has not yet expired.
    ///
    /// Uses the service's clock rather than Redis'. Uses Redis' TTL on a best-effort basis.
    increment_rate_limit: redis::Script,
    /// Checks a Redis key's counter value if it has not yet expired.
    ///
    /// Uses the service's clock rather than Redis'. Uses Redis' TTL on a best-effort basis.
    check_rate_limit: redis::Script,
    /// Increments a Redis key's counter value, corresponding to either success or failure, if it has not yet expired.
    ///
    /// Uses the service's clock rather than Redis'. Uses Redis' TTL on a best-effort basis.
    increment_breaker: redis::Script,
    /// Checks a Redis key's counter value, corresponding to either success or failure, if it has not yet expired.
    ///
    /// Uses the service's clock rather than Redis'. Uses Redis' TTL on a best-effort basis.
    check_breaker: redis::Script,
}

impl Default for ScriptRegistry {
    fn default() -> ScriptRegistry {
        ScriptRegistry {
            // TODO: handle overflow errors by expiring everything on overflow and returning nil?
            increment_rate_limit: redis::Script::new(
                r#"
                local counter_key = "bulwark:rl:" .. KEYS[1]
                local increment_delta = tonumber(ARGV[1])
                local expiration_window = tonumber(ARGV[2])
                local timestamp = tonumber(ARGV[3])
                local expiration_key = counter_key .. ":exp"
                local expiration = tonumber(redis.call("get", expiration_key))
                local next_expiration = timestamp + expiration_window
                if not expiration or timestamp > expiration then
                    redis.call("set", expiration_key, next_expiration)
                    redis.call("set", counter_key, 0)
                    redis.call("expireat", expiration_key, next_expiration + 1)
                    redis.call("expireat", counter_key, next_expiration + 1)
                    expiration = next_expiration
                end
                local attempts = redis.call("incrby", counter_key, increment_delta)
                return { attempts, expiration }
                "#,
            ),
            check_rate_limit: redis::Script::new(
                r#"
                local counter_key = "bulwark:rl:" .. KEYS[1]
                local expiration_key = counter_key .. ":exp"
                local timestamp = tonumber(ARGV[1])
                local attempts = tonumber(redis.call("get", counter_key))
                local expiration = nil
                if attempts then
                    expiration = tonumber(redis.call("get", expiration_key))
                    if not expiration or timestamp > expiration then
                        attempts = nil
                        expiration = nil
                    end
                end
                return { attempts, expiration }
                "#,
            ),
            increment_breaker: redis::Script::new(
                r#"
                local generation_key = "bulwark:bk:g:" .. KEYS[1]
                local success_key = "bulwark:bk:s:" .. KEYS[1]
                local failure_key = "bulwark:bk:f:" .. KEYS[1]
                local consec_success_key = "bulwark:bk:cs:" .. KEYS[1]
                local consec_failure_key = "bulwark:bk:cf:" .. KEYS[1]
                local success_delta = tonumber(ARGV[1])
                local failure_delta = tonumber(ARGV[2])
                local expiration_window = tonumber(ARGV[3])
                local timestamp = tonumber(ARGV[4])
                local expiration = timestamp + expiration_window
                local generation = redis.call("incrby", generation_key, 1)
                local successes = 0
                local failures = 0
                local consec_successes = 0
                local consec_failures = 0
                if success_delta > 0 then
                    successes = redis.call("incrby", success_key, success_delta)
                    failures = tonumber(redis.call("get", failure_key)) or 0
                    consec_successes = redis.call("incrby", consec_success_key, success_delta)
                    redis.call("set", consec_failure_key, 0)
                    consec_failures = 0
                else
                    successes = tonumber(redis.call("get", success_key))
                    failures = redis.call("incrby", failure_key, failure_delta) or 0
                    redis.call("set", consec_success_key, 0)
                    consec_successes = 0
                    consec_failures = redis.call("incrby", consec_failure_key, failure_delta)
                end
                redis.call("expireat", generation_key, expiration + 1)
                redis.call("expireat", success_key, expiration + 1)
                redis.call("expireat", failure_key, expiration + 1)
                redis.call("expireat", consec_success_key, expiration + 1)
                redis.call("expireat", consec_failure_key, expiration + 1)
                return { generation, successes, failures, consec_successes, consec_failures, expiration }
                "#,
            ),
            check_breaker: redis::Script::new(
                r#"
                local generation_key = "bulwark:bk:g:" .. KEYS[1]
                local success_key = "bulwark:bk:s:" .. KEYS[1]
                local failure_key = "bulwark:bk:f:" .. KEYS[1]
                local consec_success_key = "bulwark:bk:cs:" .. KEYS[1]
                local consec_failure_key = "bulwark:bk:cf:" .. KEYS[1]
                local generation = tonumber(redis.call("get", generation_key))
                if not generation then
                    return { nil, nil, nil, nil, nil, nil }
                end
                local successes = tonumber(redis.call("get", success_key)) or 0
                local failures = tonumber(redis.call("get", failure_key)) or 0
                local consec_successes = tonumber(redis.call("get", consec_success_key)) or 0
                local consec_failures = tonumber(redis.call("get", consec_failure_key)) or 0
                local expiration = tonumber(redis.call("expiretime", success_key)) - 1
                return { generation, successes, failures, consec_successes, consec_failures, expiration }
                "#,
            ),
        }
    }
}

/// The RequestContext provides a store of information that needs to cross the plugin sandbox boundary.
pub struct RequestContext {
    wasi_ctx: WasiCtx,
    wasi_table: Table,

    config: Arc<Vec<u8>>,
    /// The set of permissions granted to a plugin.
    permissions: bulwark_config::Permissions,
    /// The `params` are a key-value map shared between all plugin instances for a single request.
    params: Arc<Mutex<bulwark_wasm_sdk::Map<String, bulwark_wasm_sdk::Value>>>, // TODO: remove Arc? move to host mutable context?
    /// The HTTP request that the plugin is processing.
    request: bulwark_host::RequestInterface,
    /// The IP address of the client that originated the request, if available.
    client_ip: Option<bulwark_host::IpInterface>,
    /// The Redis connection pool and its associated Lua scripts.
    redis_info: Option<Arc<RedisInfo>>,
    /// A store of outbound requests being assembled by a plugin.
    ///
    /// Due to apparent limitations in WIT, a full request structure cannot be easily sent by a plugin as a single
    /// record. This is a work-around, but there may be better alternatives to achieve the same effect.
    outbound_http: Arc<Mutex<HashMap<u64, reqwest::blocking::RequestBuilder>>>,
    /// The HTTP client used to send outbound requests from plugins.
    http_client: reqwest::blocking::Client,

    // TODO: wrap these with `DecisionComponents`
    /// The `accept` component of a [`Decision`].
    accept: f64,
    /// The `restrict` component of a [`Decision`].
    restrict: f64,
    /// The `unknown` component of a [`Decision`].
    unknown: f64,
    /// The tags annotating a plugins decision.
    tags: Vec<String>,

    // TODO: should there be read-only context and guest-mutable context structs as well?
    /// Context values that will be mutated by the host environment.
    host_mutable_context: HostMutableContext,
}

impl RequestContext {
    /// Creates a new `RequestContext`.
    ///
    /// # Arguments
    ///
    /// * `plugin` - The [`Plugin`] and its associated configuration.
    /// * `redis_info` - The Redis connection pool.
    /// * `params` - A key-value map that plugins use to pass values within the context of a request.
    ///     Any parameters captured by the router will be added to this before plugin execution.
    /// * `request` - The [`Request`](bulwark_wasm_sdk::Request) that plugins will be operating on.
    pub fn new(
        plugin: Arc<Plugin>,
        redis_info: Option<Arc<RedisInfo>>,
        params: Arc<Mutex<bulwark_wasm_sdk::Map<String, bulwark_wasm_sdk::Value>>>,
        request: Arc<bulwark_wasm_sdk::Request>,
    ) -> Result<RequestContext, ContextInstantiationError> {
        let mut wasi_table = Table::new();
        let wasi_ctx = WasiCtxBuilder::new()
            .inherit_stdio()
            // TODO: assign stdio to something we can capture
            // TODO: figure out what to do with stdin, if anything?
            // .set_stdin(stdin)
            // .set_stdout(stdout)
            // .set_stderr(stderr)
            .build(&mut wasi_table)?;
        let client_ip = request
            .extensions()
            .get::<ForwardedIP>()
            .map(|forwarded_ip| bulwark_host::IpInterface::from(forwarded_ip.0));

        Ok(RequestContext {
            wasi_ctx,
            wasi_table,
            redis_info,
            config: Arc::new(plugin.guest_config()?),
            permissions: plugin.permissions(),
            params,
            request: bulwark_host::RequestInterface::from(request),
            client_ip,
            outbound_http: Arc::new(Mutex::new(HashMap::new())),
            http_client: reqwest::blocking::Client::new(),
            accept: 0.0,
            restrict: 0.0,
            unknown: 1.0,
            tags: vec![],
            host_mutable_context: HostMutableContext {
                response: Arc::new(Mutex::new(None)),
                combined_decision: Arc::new(Mutex::new(None)),
                outcome: Arc::new(Mutex::new(None)),
                combined_tags: Arc::new(Mutex::new(None)),
            },
        })
    }
}

impl WasiView for RequestContext {
    fn table(&self) -> &Table {
        &self.wasi_table
    }

    fn table_mut(&mut self) -> &mut Table {
        &mut self.wasi_table
    }

    fn ctx(&self) -> &WasiCtx {
        &self.wasi_ctx
    }

    fn ctx_mut(&mut self) -> &mut WasiCtx {
        &mut self.wasi_ctx
    }
}

/// A singular detection plugin and provides the interface between WASM host and guest.
///
/// One `Plugin` may spawn many [`PluginInstance`]s, which will handle the incoming request data.
#[derive(Clone)]
pub struct Plugin {
    reference: String,
    config: Arc<bulwark_config::Plugin>,
    engine: Engine,
    component: Component,
}

impl Plugin {
    /// Creates and compiles a new [`Plugin`] from a [`String`] of
    /// [WAT](https://webassembly.github.io/spec/core/text/index.html)-formatted WASM.
    pub fn from_wat(
        name: String,
        wat: &str,
        config: &bulwark_config::Plugin,
    ) -> Result<Self, PluginLoadError> {
        Self::from_component(
            name,
            config,
            |engine| -> Result<Component, PluginLoadError> {
                Ok(Component::new(engine, wat.as_bytes())?)
            },
        )
    }

    /// Creates and compiles a new [`Plugin`] from a byte slice of WASM.
    ///
    /// The bytes it expects are what you'd get if you read in a `*.wasm` file.
    /// See [`Module::from_binary`].
    pub fn from_bytes(
        name: String,
        bytes: &[u8],
        config: &bulwark_config::Plugin,
    ) -> Result<Self, PluginLoadError> {
        Self::from_component(
            name,
            config,
            |engine| -> Result<Component, PluginLoadError> {
                Ok(Component::from_binary(engine, bytes)?)
            },
        )
    }

    /// Creates and compiles a new [`Plugin`] by reading in a file in either `*.wasm` or `*.wat` format.
    ///
    /// See [`Module::from_file`].
    pub fn from_file(
        path: impl AsRef<Path>,
        config: &bulwark_config::Plugin,
    ) -> Result<Self, PluginLoadError> {
        let name = config.reference.clone();
        Self::from_component(
            name,
            config,
            |engine| -> Result<Component, PluginLoadError> {
                Ok(Component::from_file(engine, &path)?)
            },
        )
    }

    /// Helper method for the other `from_*` functions.
    fn from_component<F>(
        reference: String,
        config: &bulwark_config::Plugin,
        mut get_component: F,
    ) -> Result<Self, PluginLoadError>
    where
        F: FnMut(&Engine) -> Result<Component, PluginLoadError>,
    {
        let mut wasm_config = Config::new();
        wasm_config.wasm_backtrace_details(wasmtime::WasmBacktraceDetails::Enable);
        wasm_config.wasm_multi_memory(true);
        wasm_config.wasm_component_model(true);
        wasm_config.async_support(true);

        let engine = Engine::new(&wasm_config)?;
        let component = get_component(&engine)?;

        Ok(Plugin {
            reference,
            config: Arc::new(config.clone()),
            engine,
            component,
        })
    }

    /// Makes the guest's configuration available as serialized JSON bytes.
    fn guest_config(&self) -> Result<Vec<u8>, ConfigSerializationError> {
        // TODO: should guest config be required or optional?
        self.config.config_to_json()
    }

    /// Makes the permissions the plugin has been granted available to the guest environment.
    fn permissions(&self) -> bulwark_config::Permissions {
        self.config.permissions.clone()
    }
}

/// A collection of values that the host environment will mutate over the lifecycle of a request/response.
#[derive(Clone)]
struct HostMutableContext {
    /// The HTTP response received from the interior service.
    response: Arc<Mutex<Option<bulwark_host::ResponseInterface>>>,
    /// The combined decision of all plugins at the end of the request phase.
    ///
    /// Accessible to plugins in the response and feedback phases.
    combined_decision: Arc<Mutex<Option<bulwark_host::DecisionInterface>>>,
    /// The combined union set of all tags attached by plugins across all phases.
    combined_tags: Arc<Mutex<Option<Vec<String>>>>,
    /// The decision outcome after the decision has been checked against configured thresholds.
    outcome: Arc<Mutex<Option<bulwark_host::OutcomeInterface>>>,
}

/// An instance of a [`Plugin`], associated with a [`RequestContext`].
pub struct PluginInstance {
    /// A reference to the parent `Plugin` and its configuration.
    plugin: Arc<Plugin>,
    /// The WASM store that holds state associated with the incoming request.
    store: Store<RequestContext>,
    request_handler: request_handler::RequestHandler,
    request_decision_handler: request_decision_handler::RequestDecisionHandler,
    response_decision_handler: response_decision_handler::ResponseDecisionHandler,
    decision_feedback_handler: decision_feedback_handler::DecisionFeedbackHandler,
    /// All plugin-visible state that the host environment will mutate over the lifecycle of a request/response.
    host_mutable_context: HostMutableContext,
}

impl PluginInstance {
    /// Instantiates a [`Plugin`], creating a new `PluginInstance`.
    ///
    /// # Arguments
    ///
    /// * `plugin` - The plugin we are creating a `PluginInstance` for.
    /// * `request_context` - The request context stores all of the state associated with an incoming request and its corresponding response.
    pub async fn new(
        plugin: Arc<Plugin>,
        request_context: RequestContext,
    ) -> Result<PluginInstance, PluginInstantiationError> {
        // Clone the host mutable context so that we can make changes to the interior of our request context from the parent.
        let host_mutable_context = request_context.host_mutable_context.clone();

        // TODO: do we need to retain a reference to the linker value anywhere? explore how other wasm-based systems use it.
        // convert from normal request struct to wasm request interface
        let mut linker: Linker<RequestContext> = Linker::new(&plugin.engine);

        wasmtime_wasi::preview2::wasi::command::add_to_linker(&mut linker)?;

        let mut store = Store::new(&plugin.engine, request_context);
        bulwark_host::HostCalls::add_to_linker(&mut linker, |ctx: &mut RequestContext| ctx)?;

        // We discard the instance for all of these because we only use the generated interface to make calls

        let (request_handler, _) = request_handler::RequestHandler::instantiate_async(
            &mut store,
            &plugin.component,
            &linker,
        )
        .await?;

        let (request_decision_handler, _) =
            request_decision_handler::RequestDecisionHandler::instantiate_async(
                &mut store,
                &plugin.component,
                &linker,
            )
            .await?;

        let (response_decision_handler, _) =
            response_decision_handler::ResponseDecisionHandler::instantiate_async(
                &mut store,
                &plugin.component,
                &linker,
            )
            .await?;

        let (decision_feedback_handler, _) =
            decision_feedback_handler::DecisionFeedbackHandler::instantiate_async(
                &mut store,
                &plugin.component,
                &linker,
            )
            .await?;

        Ok(PluginInstance {
            plugin,
            store,
            request_handler,
            request_decision_handler,
            response_decision_handler,
            decision_feedback_handler,
            host_mutable_context,
        })
    }

    /// Returns the configured weight value for tuning [`Decision`] values.
    pub fn weight(&self) -> f64 {
        self.plugin.config.weight
    }

    /// Records a [`Response`](bulwark_wasm_sdk::Response) so that it will be accessible to the plugin guest
    /// environment.
    pub fn record_response(&mut self, response: Arc<bulwark_wasm_sdk::Response>) {
        let mut interior_response = self.host_mutable_context.response.lock().unwrap();
        *interior_response = Some(bulwark_host::ResponseInterface::from(response));
    }

    /// Records the combined [`Decision`], it's tags, and the associated [`Outcome`] so that they will be accessible
    /// to the plugin guest environment.
    pub fn record_combined_decision(
        &mut self,
        decision_components: &DecisionComponents,
        outcome: Outcome,
    ) {
        let mut interior_decision = self.host_mutable_context.combined_decision.lock().unwrap();
        *interior_decision = Some(decision_components.decision.into());
        let mut interior_outcome = self.host_mutable_context.outcome.lock().unwrap();
        *interior_outcome = Some(outcome.into());
    }

    /// Returns the plugin's identifier.
    pub fn plugin_reference(&self) -> String {
        self.plugin.reference.clone()
    }

    /// Executes the guest's `on_request` function.
    pub async fn handle_request(&mut self) -> Result<(), PluginExecutionError> {
        let _result = self
            .request_handler
            .call_on_request(self.store.as_context_mut())
            .await?;

        Ok(())
    }

    /// Executes the guest's `on_request_decision` function.
    pub async fn handle_request_decision(&mut self) -> Result<(), PluginExecutionError> {
        let _result = self
            .request_decision_handler
            .call_on_request_decision(self.store.as_context_mut())
            .await?;

        Ok(())
    }

    /// Executes the guest's `on_response_decision` function.
    pub async fn handle_response_decision(&mut self) -> Result<(), PluginExecutionError> {
        let _result = self
            .response_decision_handler
            .call_on_response_decision(self.store.as_context_mut())
            .await?;

        Ok(())
    }

    /// Executes the guest's `on_decision_feedback` function.
    pub async fn handle_decision_feedback(&mut self) -> Result<(), PluginExecutionError> {
        let _result = self
            .decision_feedback_handler
            .call_on_decision_feedback(self.store.as_context_mut())
            .await?;

        Ok(())
    }

    /// Returns the decision components from the [`RequestContext`].
    pub fn decision(&mut self) -> DecisionComponents {
        let ctx = self.store.data();

        DecisionComponents {
            decision: Decision {
                accept: ctx.accept,
                restrict: ctx.restrict,
                unknown: ctx.unknown,
            },
            tags: ctx.tags.clone(),
        }
    }
}

#[async_trait]
impl bulwark_host::HostCallsImports for RequestContext {
    /// Returns the guest environment's configuration value as serialized JSON.
    async fn get_config(&mut self) -> Result<Vec<u8>, wasmtime::Error> {
        Ok(self.config.to_vec())
    }

    /// Returns a named value from the request context's params.
    ///
    /// # Arguments
    ///
    /// * `key` - The key name corresponding to the param value.
    async fn get_param_value(&mut self, key: String) -> Result<Vec<u8>, wasmtime::Error> {
        let params = self.params.lock().unwrap();
        let value = params.get(&key).unwrap_or(&bulwark_wasm_sdk::Value::Null);
        Ok(serde_json::to_vec(value)?)
    }

    /// Set a named value in the request context's params.
    ///
    /// # Arguments
    ///
    /// * `key` - The key name corresponding to the param value.
    /// * `value` - The value to record. Values are serialized JSON.
    async fn set_param_value(
        &mut self,
        key: String,
        value: Vec<u8>,
    ) -> std::result::Result<(), wasmtime::Error> {
        let mut params = self.params.lock().unwrap();
        let value: bulwark_wasm_sdk::Value = serde_json::from_slice(&value)?;
        params.insert(key, value);
        Ok(())
    }

    /// Returns a named environment variable value as bytes.
    ///
    /// # Arguments
    ///
    /// * `key` - The environment variable name. Case-sensitive.
    async fn get_env_bytes(&mut self, key: String) -> Result<Vec<u8>, wasmtime::Error> {
        let allowed_env_vars = self
            .permissions
            .env
            .iter()
            .cloned()
            .collect::<BTreeSet<String>>();
        if !allowed_env_vars.contains(&key.to_string()) {
            // TODO: convert to error
            panic!("access to environment variable denied");
        }
        Ok(std::env::var(key)?.as_bytes().to_vec())
    }

    /// Returns the incoming request associated with the request context.
    async fn get_request(
        &mut self,
    ) -> std::result::Result<bulwark_host::RequestInterface, wasmtime::Error> {
        Ok(self.request.clone())
    }

    /// Returns the response received from the interior service.
    async fn get_response(
        &mut self,
    ) -> std::result::Result<bulwark_host::ResponseInterface, wasmtime::Error> {
        let response: MutexGuard<Option<bulwark_host::ResponseInterface>> =
            self.host_mutable_context.response.lock().unwrap();
        // TODO: remove unwrap
        Ok(response.to_owned().unwrap())
    }

    /// Returns the originating client's IP address, if available.
    async fn get_client_ip(
        &mut self,
    ) -> Result<Option<bulwark_host::IpInterface>, wasmtime::Error> {
        Ok(self.client_ip)
    }

    /// Begins an outbound request. Returns a request ID used by `add_request_header` and `set_request_body`.
    ///
    /// # Arguments
    ///
    /// * `method` - The HTTP method
    /// * `uri` - The absolute URI of the resource to request
    async fn prepare_request(
        &mut self,
        method: String,
        uri: String,
    ) -> Result<u64, wasmtime::Error> {
        let allowed_http_domains = self
            .permissions
            .http
            .iter()
            .cloned()
            .collect::<BTreeSet<String>>();
        let parsed_uri = Url::parse(&uri).unwrap();
        let requested_domain = parsed_uri.domain().unwrap();
        if !allowed_http_domains.contains(&requested_domain.to_string()) {
            // TODO: convert to error
            panic!("access to http resource denied");
        }
        let mut outbound_requests = self.outbound_http.lock().unwrap();
        let method = match method.to_ascii_uppercase().as_str() {
            "GET" => reqwest::Method::GET,
            "HEAD" => reqwest::Method::HEAD,
            "POST" => reqwest::Method::POST,
            "PUT" => reqwest::Method::PUT,
            "PATCH" => reqwest::Method::PATCH,
            "DELETE" => reqwest::Method::DELETE,
            "OPTIONS" => reqwest::Method::OPTIONS,
            "TRACE" => reqwest::Method::TRACE,
            _ => panic!("unsupported http method"),
        };
        let builder = self.http_client.request(method, uri);
        let index: u64 = outbound_requests.len().try_into().unwrap();
        outbound_requests.insert(index, builder);
        Ok((outbound_requests.len() - 1).try_into()?)
    }

    /// Adds a request header to an outbound HTTP request.
    ///
    /// # Arguments
    ///
    /// * `request_id` - The request ID received from `prepare_request`.
    /// * `name` - The header name.
    /// * `value` - The header value bytes.
    async fn add_request_header(
        &mut self,
        request_id: u64,
        name: String,
        value: Vec<u8>,
    ) -> Result<(), wasmtime::Error> {
        let mut outbound_requests = self.outbound_http.lock().unwrap();
        // remove/insert to avoid move issues
        let mut builder = outbound_requests.remove(&request_id).unwrap();
        builder = builder.header(name, value);
        outbound_requests.insert(request_id, builder);
        Ok(())
    }

    /// Sets the request body, if any. Returns the response.
    ///
    /// This function is still required even if the request does not have a body. An empty body is acceptable.
    ///
    /// # Arguments
    ///
    /// * `request_id` - The request ID received from `prepare_request`.
    /// * `body` - The request body in bytes or an empty slice for no body.
    async fn set_request_body(
        &mut self,
        request_id: u64,
        body: Vec<u8>,
    ) -> Result<bulwark_host::ResponseInterface, wasmtime::Error> {
        // TODO: handle basic error scenarios like timeouts
        // TODO: remove unwraps
        let mut outbound_requests = self.outbound_http.lock().unwrap();
        // remove/insert to avoid move issues
        let builder = outbound_requests.remove(&request_id).unwrap();
        let builder = builder.body(body);

        let response = builder.send().unwrap();
        let status: u32 = response.status().as_u16().try_into().unwrap();
        // need to read headers before body because retrieving body bytes will move the response
        let headers: Vec<HeaderInterface> = response
            .headers()
            .iter()
            .map(|(name, value)| HeaderInterface {
                name: name.to_string(),
                value: value.as_bytes().to_vec(),
            })
            .collect();
        let body = response.bytes().unwrap().to_vec();
        let content_length: u64 = body.len().try_into().unwrap();
        Ok(bulwark_host::ResponseInterface {
            status,
            headers,
            chunk: body,
            chunk_start: 0,
            chunk_length: content_length,
            end_of_stream: true,
        })
    }

    /// Records the decision value the plugin wants to return.
    ///
    /// # Arguments
    ///
    /// * `decision` - The [`Decision`] output of the plugin.
    async fn set_decision(
        &mut self,
        decision: bulwark_host::DecisionInterface,
    ) -> Result<(), wasmtime::Error> {
        let decision = Decision::from(decision);
        self.accept = decision.accept;
        self.restrict = decision.restrict;
        self.unknown = decision.unknown;
        // TODO: validate
        Ok(())
    }

    /// Records the tags the plugin wants to associate with its decision.
    ///
    /// # Arguments
    ///
    /// * `tags` - The list of tags to associate with a [`Decision`].
    async fn set_tags(&mut self, tags: Vec<String>) -> Result<(), wasmtime::Error> {
        self.tags = tags;
        Ok(())
    }

    /// Returns the combined decision, if available.
    ///
    /// Typically used in the feedback phase.
    async fn get_combined_decision(
        &mut self,
    ) -> Result<bulwark_host::DecisionInterface, wasmtime::Error> {
        let combined_decision: MutexGuard<Option<bulwark_host::DecisionInterface>> =
            self.host_mutable_context.combined_decision.lock().unwrap();
        // TODO: this should probably be an Option return type rather than unwrapping here
        Ok(combined_decision.to_owned().unwrap())
    }

    /// Returns the combined set of tags associated with a decision, if available.
    ///
    /// Typically used in the feedback phase.
    async fn get_combined_tags(&mut self) -> Result<Vec<String>, wasmtime::Error> {
        let combined_tags: MutexGuard<Option<Vec<String>>> =
            self.host_mutable_context.combined_tags.lock().unwrap();
        // TODO: this should probably be an Option return type rather than unwrapping here
        Ok(combined_tags.to_owned().unwrap())
    }

    /// Returns the outcome of the combined decision, if available.
    ///
    /// Typically used in the feedback phase.
    async fn get_outcome(&mut self) -> Result<bulwark_host::OutcomeInterface, wasmtime::Error> {
        let outcome: MutexGuard<Option<bulwark_host::OutcomeInterface>> =
            self.host_mutable_context.outcome.lock().unwrap();
        // TODO: this should probably be an Option return type rather than unwrapping here
        Ok(outcome.to_owned().unwrap())
    }

    /// Returns the named state value retrieved from Redis.
    ///
    /// Also used to retrieve a counter value.
    ///
    /// # Arguments
    ///
    /// * `key` - The key name corresponding to the state value.
    async fn get_remote_state(
        &mut self,
        key: String,
    ) -> Result<std::vec::Vec<u8>, wasmtime::Error> {
        // TODO: figure out how to extract to a helper function?
        let allowed_key_prefixes = self
            .permissions
            .state
            .iter()
            .cloned()
            .collect::<BTreeSet<String>>();
        if !allowed_key_prefixes
            .iter()
            .any(|prefix| key.starts_with(prefix))
        {
            // TODO: convert to error
            panic!("access to state value by prefix denied");
        }

        let pool = &self.redis_info.clone().unwrap().pool;
        let mut conn = pool.get().unwrap();
        Ok(conn.get(key)?)
    }

    /// Set a named value in Redis.
    ///
    /// # Arguments
    ///
    /// * `key` - The key name corresponding to the state value.
    /// * `value` - The value to record. Values are byte strings, but may be interpreted differently by Redis depending on context.
    async fn set_remote_state(
        &mut self,
        key: String,
        value: std::vec::Vec<u8>,
    ) -> std::result::Result<(), wasmtime::Error> {
        // TODO: figure out how to extract to a helper function?
        let allowed_key_prefixes = self
            .permissions
            .state
            .iter()
            .cloned()
            .collect::<BTreeSet<String>>();
        if !allowed_key_prefixes
            .iter()
            .any(|prefix| key.starts_with(prefix))
        {
            // TODO: convert to error
            panic!("access to state value by prefix denied");
        }

        let pool = &self.redis_info.clone().unwrap().pool;
        let mut conn = pool.get().unwrap();
        Ok(conn.set(key, value.to_vec())?)
    }

    /// Increments a named counter in Redis.
    ///
    /// # Arguments
    ///
    /// * `key` - The key name corresponding to the state counter.
    async fn increment_remote_state(
        &mut self,
        key: String,
    ) -> std::result::Result<i64, wasmtime::Error> {
        // TODO: figure out how to extract to a helper function?
        let allowed_key_prefixes = self
            .permissions
            .state
            .iter()
            .cloned()
            .collect::<BTreeSet<String>>();
        if !allowed_key_prefixes
            .iter()
            .any(|prefix| key.starts_with(prefix))
        {
            // TODO: convert to error
            panic!("access to state value by prefix denied");
        }

        let pool = &self.redis_info.clone().unwrap().pool;
        let mut conn = pool.get().unwrap();
        Ok(conn.incr(key, 1)?)
    }

    /// Increments a named counter in Redis by a specified delta value.
    ///
    /// # Arguments
    ///
    /// * `key` - The key name corresponding to the state counter.
    /// * `delta` - The amount to increase the counter by.
    async fn increment_remote_state_by(
        &mut self,
        key: String,
        delta: i64,
    ) -> std::result::Result<i64, wasmtime::Error> {
        // TODO: figure out how to extract to a helper function?
        let allowed_key_prefixes = self
            .permissions
            .state
            .iter()
            .cloned()
            .collect::<BTreeSet<String>>();
        if !allowed_key_prefixes
            .iter()
            .any(|prefix| key.starts_with(prefix))
        {
            // TODO: convert to error
            panic!("access to state value by prefix denied");
        }

        let pool = &self.redis_info.clone().unwrap().pool;
        let mut conn = pool.get().unwrap();
        Ok(conn.incr(key, delta)?)
    }

    /// Sets an expiration on a named value in Redis.
    ///
    /// # Arguments
    ///
    /// * `key` - The key name corresponding to the state value.
    /// * `ttl` - The time-to-live for the value in seconds.
    async fn set_remote_ttl(
        &mut self,
        key: String,
        ttl: i64,
    ) -> std::result::Result<(), wasmtime::Error> {
        // TODO: figure out how to extract to a helper function?
        let allowed_key_prefixes = self
            .permissions
            .state
            .iter()
            .cloned()
            .collect::<BTreeSet<String>>();
        if !allowed_key_prefixes
            .iter()
            .any(|prefix| key.starts_with(prefix))
        {
            // TODO: convert to error
            panic!("access to state value by prefix denied");
        }

        let pool = &self.redis_info.clone().unwrap().pool;
        let mut conn = pool.get().unwrap();
        Ok(conn.expire(key, ttl.try_into().unwrap())?)
    }

    /// Increments a rate limit, returning the number of attempts so far and the expiration time.
    ///
    /// The rate limiter is a counter over a period of time. At the end of the period, it will expire,
    /// beginning a new period. Window periods should be set to the longest amount of time that a client should
    /// be locked out for. The plugin is responsible for performing all rate-limiting logic with the counter
    /// value it receives.
    ///
    /// # Arguments
    ///
    /// * `key` - The key name corresponding to the state counter.
    /// * `delta` - The amount to increase the counter by.
    /// * `window` - How long each period should be in seconds.
    async fn increment_rate_limit(
        &mut self,
        key: String,
        delta: i64,
        window: i64,
    ) -> std::result::Result<bulwark_host::RateInterface, wasmtime::Error> {
        // TODO: figure out how to extract to a helper function?
        let allowed_key_prefixes = self
            .permissions
            .state
            .iter()
            .cloned()
            .collect::<BTreeSet<String>>();
        if !allowed_key_prefixes
            .iter()
            .any(|prefix| key.starts_with(prefix))
        {
            // TODO: convert to error
            panic!("access to state value by prefix denied");
        }

        let redis_info = self.redis_info.clone().unwrap();
        let mut conn = redis_info.pool.get()?;
        let dt = Utc::now();
        let timestamp: i64 = dt.timestamp();
        let script = redis_info.registry.increment_rate_limit.clone();
        let (attempts, expiration) = script
            .key(key)
            .arg(delta)
            .arg(window)
            .arg(timestamp)
            .invoke::<(i64, i64)>(conn.deref_mut())?;
        Ok(bulwark_host::RateInterface {
            attempts,
            expiration,
        })
    }

    /// Checks a rate limit, returning the number of attempts so far and the expiration time.
    ///
    /// See `increment_rate_limit`.
    ///
    /// # Arguments
    ///
    /// * `key` - The key name corresponding to the state counter.
    async fn check_rate_limit(
        &mut self,
        key: String,
    ) -> std::result::Result<bulwark_host::RateInterface, wasmtime::Error> {
        // TODO: figure out how to extract to a helper function?
        let allowed_key_prefixes = self
            .permissions
            .state
            .iter()
            .cloned()
            .collect::<BTreeSet<String>>();
        if !allowed_key_prefixes
            .iter()
            .any(|prefix| key.starts_with(prefix))
        {
            // TODO: convert to error
            panic!("access to state value by prefix denied");
        }

        let redis_info = self.redis_info.clone().unwrap();
        let mut conn = redis_info.pool.get().unwrap();
        let dt = Utc::now();
        let timestamp: i64 = dt.timestamp();
        let script = redis_info.registry.check_rate_limit.clone();
        let (attempts, expiration) = script
            .key(key)
            .arg(timestamp)
            .invoke::<(i64, i64)>(conn.deref_mut())?;
        Ok(bulwark_host::RateInterface {
            attempts,
            expiration,
        })
    }

    /// Increments a circuit breaker, returning the generation count, success count, failure count,
    /// consecutive success count, consecutive failure count, and expiration time.
    ///
    /// The plugin is responsible for performing all circuit-breaking logic with the counter
    /// values it receives. The host environment does as little as possible to maximize how much
    /// control the plugin has over the behavior of the breaker.
    ///
    /// # Arguments
    ///
    /// * `key` - The key name corresponding to the state counter.
    /// * `success_delta` - The amount to increase the success counter by. Generally zero on failure.
    /// * `failure_delta` - The amount to increase the failure counter by. Generally zero on success.
    /// * `window` - How long each period should be in seconds.
    async fn increment_breaker(
        &mut self,
        key: String,
        success_delta: i64,
        failure_delta: i64,
        window: i64,
    ) -> std::result::Result<bulwark_host::BreakerInterface, wasmtime::Error> {
        // TODO: figure out how to extract to a helper function?
        let allowed_key_prefixes = self
            .permissions
            .state
            .iter()
            .cloned()
            .collect::<BTreeSet<String>>();
        if !allowed_key_prefixes
            .iter()
            .any(|prefix| key.starts_with(prefix))
        {
            // TODO: convert to error
            panic!("access to state value by prefix denied");
        }

        let redis_info = self.redis_info.clone().unwrap();
        let mut conn = redis_info.pool.get()?;
        let dt = Utc::now();
        let timestamp: i64 = dt.timestamp();
        let script = redis_info.registry.increment_breaker.clone();
        let (
            generation,
            successes,
            failures,
            consecutive_successes,
            consecutive_failures,
            expiration,
        ) = script
            .key(key)
            .arg(success_delta)
            .arg(failure_delta)
            .arg(window)
            .arg(timestamp)
            .invoke::<(i64, i64, i64, i64, i64, i64)>(conn.deref_mut())?;
        Ok(bulwark_host::BreakerInterface {
            generation,
            successes,
            failures,
            consecutive_successes,
            consecutive_failures,
            expiration,
        })
    }

    /// Checks a circuit breaker, returning the generation count, success count, failure count,
    /// consecutive success count, consecutive failure count, and expiration time.
    ///
    /// See `increment_breaker`.
    ///
    /// # Arguments
    ///
    /// * `key` - The key name corresponding to the state counter.
    async fn check_breaker(
        &mut self,
        key: String,
    ) -> std::result::Result<bulwark_host::BreakerInterface, wasmtime::Error> {
        // TODO: figure out how to extract to a helper function?
        let allowed_key_prefixes = self
            .permissions
            .state
            .iter()
            .cloned()
            .collect::<BTreeSet<String>>();
        if !allowed_key_prefixes
            .iter()
            .any(|prefix| key.starts_with(prefix))
        {
            // TODO: convert to error
            panic!("access to state value by prefix denied");
        }

        let redis_info = self.redis_info.clone().unwrap();
        let mut conn = redis_info.pool.get()?;
        let dt = Utc::now();
        let timestamp: i64 = dt.timestamp();
        let script = redis_info.registry.check_breaker.clone();
        let (
            generation,
            successes,
            failures,
            consecutive_successes,
            consecutive_failures,
            expiration,
        ) = script
            .key(key)
            .arg(timestamp)
            .invoke::<(i64, i64, i64, i64, i64, i64)>(conn.deref_mut())?;
        Ok(bulwark_host::BreakerInterface {
            generation,
            successes,
            failures,
            consecutive_successes,
            consecutive_failures,
            expiration,
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_wasm_execution() -> Result<(), Box<dyn std::error::Error>> {
        let wasm_bytes = include_bytes!("../tests/bulwark-blank-slate.wasm");
        let plugin = Arc::new(Plugin::from_bytes(
            "bulwark-blank-slate.wasm".to_string(),
            wasm_bytes,
            &bulwark_config::Plugin::default(),
        )?);
        let request = Arc::new(
            http::Request::builder()
                .method("GET")
                .uri("/")
                .version(http::Version::HTTP_11)
                .body(bulwark_wasm_sdk::BodyChunk {
                    content: vec![],
                    start: 0,
                    size: 0,
                    end_of_stream: true,
                })?,
        );
        let params = Arc::new(Mutex::new(bulwark_wasm_sdk::Map::new()));
        let request_context = RequestContext::new(plugin.clone(), None, params, request)?;
        let mut plugin_instance =
            tokio_test::block_on(PluginInstance::new(plugin, request_context))?;
        let decision_components = plugin_instance.decision();
        assert_eq!(decision_components.decision.accept, 0.0);
        assert_eq!(decision_components.decision.restrict, 0.0);
        assert_eq!(decision_components.decision.unknown, 1.0);
        assert_eq!(decision_components.tags, vec![""; 0]);

        Ok(())
    }

    #[test]
    fn test_wasm_logic() -> Result<(), Box<dyn std::error::Error>> {
        let wasm_bytes = include_bytes!("../tests/bulwark-evil-bit.wasm");
        let plugin = Arc::new(Plugin::from_bytes(
            "bulwark-evil-bit.wasm".to_string(),
            wasm_bytes,
            &bulwark_config::Plugin::default(),
        )?);

        let request = Arc::new(
            http::Request::builder()
                .method("POST")
                .uri("/example")
                .version(http::Version::HTTP_11)
                .header("Content-Type", "application/json")
                .body(bulwark_wasm_sdk::BodyChunk {
                    content: "{\"number\": 42}".as_bytes().to_vec(),
                    start: 0,
                    size: 14,
                    end_of_stream: true,
                })?,
        );
        let params = Arc::new(Mutex::new(bulwark_wasm_sdk::Map::new()));
        let request_context = RequestContext::new(plugin.clone(), None, params, request)?;
        let mut typical_plugin_instance =
            tokio_test::block_on(PluginInstance::new(plugin.clone(), request_context))?;
        tokio_test::block_on(typical_plugin_instance.handle_request_decision())?;
        let typical_decision = typical_plugin_instance.decision();
        assert_eq!(typical_decision.decision.accept, 0.0);
        assert_eq!(typical_decision.decision.restrict, 0.0);
        assert_eq!(typical_decision.decision.unknown, 1.0);
        assert_eq!(typical_decision.tags, vec![""; 0]);

        let request = Arc::new(
            http::Request::builder()
                .method("POST")
                .uri("/example")
                .version(http::Version::HTTP_11)
                .header("Content-Type", "application/json")
                .header("Evil", "true")
                .body(bulwark_wasm_sdk::BodyChunk {
                    content: "{\"number\": 42}".as_bytes().to_vec(),
                    start: 0,
                    size: 14,
                    end_of_stream: true,
                })?,
        );
        let params = Arc::new(Mutex::new(bulwark_wasm_sdk::Map::new()));
        let request_context = RequestContext::new(plugin.clone(), None, params, request)?;
        let mut evil_plugin_instance =
            tokio_test::block_on(PluginInstance::new(plugin, request_context))?;
        tokio_test::block_on(evil_plugin_instance.handle_request_decision())?;
        let evil_decision = evil_plugin_instance.decision();
        assert_eq!(evil_decision.decision.accept, 0.0);
        assert_eq!(evil_decision.decision.restrict, 1.0);
        assert_eq!(evil_decision.decision.unknown, 0.0);
        assert_eq!(evil_decision.tags, vec!["evil"]);

        Ok(())
    }
}
