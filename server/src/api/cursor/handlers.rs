//! Implements Cursor HTTP endpoints outside the Agent Run stream.
use axum::{
    body::{Body, Bytes},
    extract::{DefaultBodyLimit, Extension, State},
    http::{header, HeaderMap, HeaderValue, Request, Response, StatusCode},
    response::IntoResponse,
    routing::{get, post},
    Router,
};
use tower_http::decompression::RequestDecompressionLayer;

use crate::{
    api::cursor::{
        bidi, entry,
        proxy::{self, CursorProxy},
        run_sse,
    },
    cursor::{
        protocol::{
            connect,
            proto::{agent::v1 as agent, aiserver::v1 as ai},
        },
        services::{
            account, analytics, commit_message, compatibility, entitlement::FreeEntitlementCache,
            knowledge, model_catalog, server_config, tab,
        },
        transport::{TransportParent, TransportRegistry},
    },
    Result,
};

pub fn router(
    registry: TransportRegistry,
    clients: crate::network::NetworkClients,
    harness: crate::local_app::CursorHarness,
) -> Result<Router> {
    let proxy = CursorProxy::cursor(clients);
    let knowledge = knowledge::KnowledgeService::managed()?;
    Ok(router_with_proxy(registry, proxy, knowledge, harness))
}

fn router_with_proxy(
    registry: TransportRegistry,
    proxy: CursorProxy,
    knowledge_service: knowledge::KnowledgeService,
    harness: crate::local_app::CursorHarness,
) -> Router {
    let web_cache = registry.web_cache().router();
    let free_entitlements = FreeEntitlementCache::default();
    Router::new()
        .route("/__byok-api__/healthz", get(health))
        .route("/agent.v1.AgentService/RunSSE", post(run_sse_handler))
        .route("/aiserver.v1.BidiService/BidiAppend", post(bidi_handler))
        .route(
            "/aiserver.v1.AiService/AvailableDocs",
            post(compatibility::available_docs),
        )
        .route(
            "/aiserver.v1.DashboardService/GetEffectiveUserPlugins",
            post(compatibility::effective_user_plugins),
        )
        .route(
            "/aiserver.v1.DashboardService/GetUserPrivacyMode",
            post(compatibility::user_privacy_mode),
        )
        .route(
            "/agent.v1.AgentService/UpdateConversationMetadata",
            post(compatibility::update_conversation_metadata),
        )
        .route(
            "/aiserver.v1.AiService/GetServerConfig",
            post(server_config::get),
        )
        .route(
            "/aiserver.v1.ServerConfigService/GetServerConfig",
            post(server_config::get),
        )
        .route(
            "/aiserver.v1.AiService/AvailableModels",
            post(model_catalog::available_models),
        )
        .route(
            "/agent.v1.AgentService/GetUsableModels",
            post(model_catalog::usable_models),
        )
        .route(
            "/aiserver.v1.AiService/GetUsableModels",
            post(model_catalog::usable_models),
        )
        .route(
            "/aiserver.v1.AiService/WriteGitCommitMessage",
            post(commit_message::write_git_commit_message),
        )
        .route(
            "/aiserver.v1.NetworkService/IsConnected",
            post(is_connected),
        )
        .route(
            "/agent.v1.AgentService/GetDefaultModelForCli",
            post(model_catalog::default_model_for_cli),
        )
        .route(
            "/aiserver.v1.AiService/GetDefaultModelForCli",
            post(model_catalog::default_model_for_cli),
        )
        .route(
            "/aiserver.v1.AiService/GetDefaultModel",
            post(model_catalog::default_model),
        )
        .route(
            "/aiserver.v1.AiService/GetDefaultModelNudgeData",
            post(model_catalog::default_model_nudge),
        )
        .route(
            "/aiserver.v1.AuthService/GetEmail",
            post(account::get_email),
        )
        .route(
            "/aiserver.v1.AuthService/GetUserMeta",
            post(account::get_user_meta),
        )
        .route("/aiserver.v1.DashboardService/GetMe", post(account::get_me))
        .route(
            "/aiserver.v1.DashboardService/GetTeams",
            post(account::get_teams),
        )
        .route(
            "/aiserver.v1.DashboardService/GetUserProfile",
            post(account::get_user_profile),
        )
        .route(
            "/aiserver.v1.DashboardService/GetCurrentPeriodUsage",
            post(account::current_period_usage),
        )
        .route(
            "/aiserver.v1.DashboardService/GetUsageLimitStatusAndActiveGrants",
            post(account::usage_limit_status),
        )
        .route(
            "/aiserver.v1.AiService/KnowledgeBaseAdd",
            post(knowledge::add),
        )
        .route(
            "/aiserver.v1.AiService/KnowledgeBaseList",
            post(knowledge::list),
        )
        .route(
            "/aiserver.v1.AiService/KnowledgeBaseUpdate",
            post(knowledge::update),
        )
        .route(
            "/aiserver.v1.AiService/KnowledgeBaseRemove",
            post(knowledge::remove),
        )
        .route(
            analytics::BOOTSTRAP_STATSIG_PATH,
            post(analytics::bootstrap_statsig),
        )
        .route("/auth/full_stripe_profile", get(account::stripe_profile))
        .route("/auth/stripe_profile", get(account::stripe_profile))
        .merge(tab::router())
        .route_layer(DefaultBodyLimit::max(super::MAX_REQUEST_BODY))
        .route_layer(RequestDecompressionLayer::new())
        .fallback(proxy::forward)
        .method_not_allowed_fallback(proxy::forward)
        .layer(Extension(proxy))
        .layer(Extension(knowledge_service))
        .layer(Extension(free_entitlements))
        .layer(Extension(harness))
        // 经本地代理进来的请求必须出示调用方身份（判据与两道防线的分工见 `entry`）。
        .layer(axum::middleware::from_fn(
            entry::require_local_cursor_caller,
        ))
        .with_state(registry)
        .merge(web_cache)
}

async fn health() -> StatusCode {
    StatusCode::NO_CONTENT
}

/// `NetworkService/IsConnected` probe. Cursor's always-local extension checks
/// connectivity roughly 10s after any slow request starts; a 404/error here is
/// treated as "network disconnected" and aborts in-flight work (e.g. commit
/// message generation) even while the model is still streaming. Always answer
/// connected with an empty `IsConnectedResponse` so local BYOK generation is
/// never cancelled by this probe.
async fn is_connected() -> Result<Response<Body>> {
    let payload = connect::encode_message(&ai::IsConnectedResponse {})?;
    let mut response = Response::new(Body::from(payload));
    *response.status_mut() = StatusCode::OK;
    response.headers_mut().insert(
        header::CONTENT_TYPE,
        HeaderValue::from_static("application/proto"),
    );
    Ok(response)
}

async fn run_sse_handler(
    State(registry): State<TransportRegistry>,
    Extension(proxy): Extension<CursorProxy>,
    request: Request<Body>,
) -> Result<Response<Body>> {
    let (parts, body) = match buffered(request, super::MAX_REQUEST_BODY).await {
        Ok(buffered) => buffered,
        Err(response) => return Ok(*response),
    };
    let request: agent::BidiRequestId = connect::decode_unary(&body)?;
    let route = registry.wait_route(&request.request_id).await?;
    let trace = registry.trace(&request.request_id);
    trace.resume();
    trace.request(
        "run_sse_request",
        body.clone(),
        serde_json::json!({"request_id": request.request_id}),
    );
    match route {
        crate::cursor::transport::TransportRoute::Local => {
            run_sse::stream(&registry, &request.request_id).await
        }
        crate::cursor::transport::TransportRoute::Upstream(generation) => {
            let response = proxy::forward(
                Extension(proxy),
                Request::from_parts(parts, Body::from(body)),
            )
            .await?;
            Ok(run_sse::upstream(
                registry,
                request.request_id,
                generation,
                response,
                Some(trace),
            )
            .await)
        }
    }
}

async fn bidi_handler(
    State(registry): State<TransportRegistry>,
    Extension(proxy): Extension<CursorProxy>,
    request: Request<Body>,
) -> Result<Response<Body>> {
    let (parts, body) = match buffered(request, super::MAX_REQUEST_BODY).await {
        Ok(buffered) => buffered,
        Err(response) => return Ok(*response),
    };
    let request: ai::BidiAppendRequest = connect::decode_unary(&body)?;
    let decoded = bidi::decode(&request)?;
    let action = decoded.action_kind();
    if !decoded.is_routine() {
        // 后台任务完成、取消、计划、总结这类低频动作必须留痕：子代理唤醒链路里
        // "子代理结束的那一刻客户端到底发了什么" 只能靠这条日志回答。
        tracing::info!(
            request_id = decoded.request_id,
            seqno = decoded.seqno,
            action,
            "Cursor append received"
        );
    }
    let first_model = decoded.model_id().map(str::to_owned);
    let conversation_id = decoded.conversation_id().map(str::to_owned);
    let trace_metadata = decoded.trace_metadata();
    let trace = registry.trace(&decoded.request_id);
    let requested_model = decoded.model_id();
    let local = match model_route(
        requested_model,
        |model_id| is_local_model(&registry, model_id),
        || registry.store().cursor_mode(),
    )
    .await?
    {
        Some(ModelRoute::Local) => {
            tracing::info!(
                request_id = decoded.request_id,
                model_id = requested_model.unwrap_or_default(),
                "routing Cursor Run to BYOK provider"
            );
            true
        }
        Some(ModelRoute::Upstream) => {
            tracing::info!(
                request_id = decoded.request_id,
                model_id = requested_model.unwrap_or_default(),
                "routing Cursor Run to Cursor upstream"
            );
            false
        }
        None if registry.local(&decoded.request_id).await.is_some() => true,
        None if registry.upstream(&decoded.request_id).await => false,
        None => {
            trace.resume();
            trace.request(
                "bidi_request",
                body.clone(),
                trace_outcome(trace_metadata, false, "missing_transport", None),
            );
            // 这条路径上没有模型、也没有已知 transport。以前它只留一句协议错误，
            // 说不出被拒的是什么动作——而"子代理完成通知"恰好可能长这个样子。
            tracing::warn!(
                request_id = decoded.request_id,
                action,
                "rejecting an append with neither a model nor a known transport"
            );
            return Err(crate::Error::Protocol(format!(
                "first BidiAppend message must select a model (action: {action})"
            )));
        }
    };
    if first_model.is_some() {
        trace.begin(
            conversation_id.as_deref(),
            if local {
                "local_byok"
            } else {
                "cursor_official"
            },
            first_model.as_deref(),
        );
    } else {
        trace.resume();
    }
    if !local {
        // 这次 append 刚把路由钉到官方上游。标记的收尾不能只挂在 RunSSE 的响应体 guard
        // 上：guard 属于"转发成功之后才有的那份响应体"，转发失败时它根本不会存在，
        // 标记就永久留在 registry 里——同 id 的后续 RunSSE 被送去官方，
        // `maintain_storage_if_idle` 的门禁也永久失效。失败就地还回去。
        let mark = if first_model.is_some() {
            Some(registry.mark_upstream(&decoded.request_id).await)
        } else {
            None
        };
        trace.request(
            "bidi_request",
            body.clone(),
            trace_outcome(trace_metadata, true, "upstream", None),
        );
        return match proxy::forward(
            Extension(proxy),
            Request::from_parts(parts, Body::from(body)),
        )
        .await
        {
            Ok(response) => Ok(response),
            Err(error) => {
                if let Some(generation) = mark {
                    registry.finish_upstream(decoded.request_id.clone(), generation);
                }
                Err(error)
            }
        };
    }
    let parent = match parent_headers(&parts.headers) {
        Ok(parent) => parent,
        Err(error) => {
            trace.request(
                "bidi_request",
                body,
                trace_outcome(
                    trace_metadata,
                    false,
                    "invalid_parent",
                    Some(error.to_string()),
                ),
            );
            return Err(error);
        }
    };
    match bidi::append(&registry, decoded, parent).await {
        Ok(_) => trace.request(
            "bidi_request",
            body,
            trace_outcome(trace_metadata, true, "local", None),
        ),
        Err(error) => {
            trace.request(
                "bidi_request",
                body,
                trace_outcome(
                    trace_metadata,
                    false,
                    "command_rejected",
                    Some(error.to_string()),
                ),
            );
            return Err(error);
        }
    }
    let mut response = Response::new(axum::body::Body::empty());
    *response.status_mut() = StatusCode::OK;
    response.headers_mut().insert(
        header::CONTENT_TYPE,
        HeaderValue::from_static("application/proto"),
    );
    Ok(response)
}

/// 这个模型 ID 是不是由我们回答的（模型库里的模型，或插件模型）。
async fn is_local_model(registry: &TransportRegistry, model_id: &str) -> Result<bool> {
    if model_id.starts_with(crate::plugin::ADAPTER_ID_PREFIX) {
        return Ok(true);
    }
    Ok(registry.store().model(model_id).await?.is_some())
}

fn trace_outcome(
    mut metadata: serde_json::Value,
    accepted: bool,
    route_outcome: &str,
    error: Option<String>,
) -> serde_json::Value {
    if let Some(metadata) = metadata.as_object_mut() {
        metadata.insert("accepted".into(), accepted.into());
        metadata.insert("route_outcome".into(), route_outcome.into());
        if let Some(error) = error {
            metadata.insert("error".into(), error.into());
        }
    }
    metadata
}

async fn buffered(
    request: Request<Body>,
    limit: usize,
) -> Result<(axum::http::request::Parts, Bytes), Box<Response<Body>>> {
    let (parts, body) = request.into_parts();
    match super::read_request_body(body, limit).await {
        Ok(body) => Ok((parts, body)),
        // 超限(413)与传输中断(400)都从这里出去:调用方直接把响应交回客户端,
        // 不经过 `Error`(它只会给 400,分不出"请求太大")。
        Err(status) => {
            tracing::warn!(
                status = status.as_u16(),
                limit,
                "rejecting a Cursor request whose body could not be read"
            );
            Err(Box::new(if status == StatusCode::PAYLOAD_TOO_LARGE {
                super::body_too_large_response(limit)
            } else {
                (status, "cannot read request body").into_response()
            }))
        }
    }
}

fn parent_headers(headers: &HeaderMap) -> Result<Option<TransportParent>> {
    let request_id = header_text(headers, "x-parent-request-id")?;
    let tool_call_id = header_text(headers, "x-parent-agent-tool-call-id")?;
    match (request_id, tool_call_id) {
        (None, None) => Ok(None),
        // Cursor 这两个头是分别发的：子代理请求可能只带父请求 ID，或者只带父工具调用 ID。
        // 只有"两个都没有"才表示这次请求没有父元数据；只有一个也是合法客户端行为。
        (request_id, tool_call_id) => Ok(Some(TransportParent {
            request_id: request_id.map(str::to_owned),
            tool_call_id: tool_call_id.map(str::to_owned),
        })),
    }
}

/// 带模型的 append 由谁回答。
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum ModelRoute {
    /// 本地 BYOK provider：模型在本地库里（插件模型 ID 也在这条路上）。
    Local,
    /// Cursor 官方上游：模型不在库里，而当前模式允许官方模型。
    Upstream,
}

/// 只看"带模型的那次判断"。没有模型时返回 `None`，调用方接着看已知的 transport 标记。
///
/// 两个读数都从参数进来（惰性求值），因为**什么时候必须读库**本身就是这条审计的要求：
/// 以前每个 BidiAppend 都先无条件读一次 `cursor_mode`，可它只用来回答"官方模型该不该
/// 拒绝"。心跳、工具结果、子代理通知这些没有模型的 append 占绝大多数，它们既不选模型
/// 也不看模式，不该为它们各查一次库。参数化之后这一点可以直接断言（用例里传入会 panic
/// 的闭包），不必为了测它去数 SQL。
async fn model_route<'model, F, G, Fut, Gut>(
    model_id: Option<&'model str>,
    is_local_model: F,
    cursor_mode: G,
) -> Result<Option<ModelRoute>>
where
    F: FnOnce(&'model str) -> Fut,
    Fut: std::future::Future<Output = Result<bool>>,
    G: FnOnce() -> Gut,
    Gut: std::future::Future<Output = Result<crate::store::CursorMode>>,
{
    let Some(model_id) = model_id else {
        return Ok(None);
    };
    // 插件模型 ID 只在本地有意义,永远不转发到 Cursor 官方上游。
    if is_local_model(model_id).await? {
        return Ok(Some(ModelRoute::Local));
    }
    if !cursor_mode().await?.includes_official_models() {
        // 本地模式只认模型库里的模型。走到这里说明客户端手里还是上一份目录
        // （模式刚切过来、列表还没刷新）。明确拒绝比悄悄转发到官方诚实：静默
        // 转发会让用户以为在用本机模型，实际花的是官方额度。
        tracing::warn!(
            model_id,
            "rejecting a model that is not configured in local mode"
        );
        return Err(crate::Error::Protocol(format!(
            "model {model_id} is not in the model library; local mode answers only configured models. Reload the Cursor window to refresh the model list, or switch to mixed mode to use official models."
        )));
    }
    Ok(Some(ModelRoute::Upstream))
}

fn header_text<'a>(headers: &'a HeaderMap, name: &str) -> Result<Option<&'a str>> {
    headers
        .get(name)
        .map(|value| value.to_str())
        .transpose()
        .map_err(|error| crate::Error::Protocol(format!("invalid {name} header: {error}")))
}

#[cfg(test)]
mod tests {
    use prost::Message;

    use super::*;
    use crate::{
        api::cursor::proxy::{CursorProxy, UPSTREAM_URL_HEADER},
        cursor::transport::test_registry,
        network::NetworkClients,
        store::{ProxyMode, ProxySettingsInput},
    };

    /// 一条会走到"官方上游"分支的 append：模型不在本地模型库里，动作是 user_message。
    fn upstream_append(request_id: &str, model_id: &str, upstream: &str) -> Request<Body> {
        let message = agent::AgentClientMessage {
            message: Some(agent::agent_client_message::Message::RunRequest(
                agent::AgentRunRequest {
                    requested_model: Some(agent::RequestedModel {
                        model_id: model_id.into(),
                        ..Default::default()
                    }),
                    action: Some(agent::ConversationAction {
                        action: Some(agent::conversation_action::Action::UserMessageAction(
                            Default::default(),
                        )),
                        ..Default::default()
                    }),
                    ..Default::default()
                },
            )),
        };
        let append = ai::BidiAppendRequest {
            data: hex::encode(message.encode_to_vec()),
            request_id: Some(ai::BidiRequestId {
                request_id: request_id.into(),
            }),
            ..Default::default()
        };
        Request::builder()
            .method("POST")
            .uri("/aiserver.v1.AiService/BidiAppend")
            .header(UPSTREAM_URL_HEADER, upstream)
            .header(header::CONTENT_TYPE, "application/proto")
            .body(Body::from(
                connect::encode_message(&append).expect("test request encodes"),
            ))
            .unwrap()
    }

    /// 接受连接后立刻断开：把它设成出网代理，转发就必然失败，而且不用真的出网
    /// （也不依赖 DNS 或外网可达性）。
    async fn dropping_proxy() -> String {
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let address = listener.local_addr().unwrap();
        tokio::spawn(async move {
            while let Ok((socket, _)) = listener.accept().await {
                drop(socket);
            }
        });
        format!("http://{address}")
    }

    /// 转发失败必须当场把上游标记还回去。
    ///
    /// 标记的收尾不能只挂在 RunSSE 的响应体 guard 上：guard 属于"转发成功之后才有的那份
    /// 响应体"，转发失败时它根本不存在，标记会永久留在 registry 里——同 id 的后续 RunSSE
    /// 被送去官方、`maintain_storage_if_idle` 的门禁永久失效。这条路径以前没有任何用例经过
    /// （两条既有用例都停在"标记已经写下"或"转发成功"）。
    #[tokio::test]
    async fn a_failed_upstream_forward_releases_the_route_mark() {
        let (_directory, registry) = test_registry().await;
        let store = registry.store().clone();
        // 前置条件：这个模型不在模型库里，模式也允许官方模型，所以这次 append 必定走
        // 上游分支（否则用例会"因为没打标记"而假绿）。
        assert!(
            store.model("official-model").await.unwrap().is_none(),
            "模型必须不在本地库里，才会被转发到官方上游"
        );
        assert!(
            store
                .cursor_mode()
                .await
                .unwrap()
                .includes_official_models(),
            "默认模式必须包含官方模型"
        );
        // 出网代理指向"接受即断开"的本地桩：转发失败是确定的，而且不出网。
        store
            .set_proxy_settings(ProxySettingsInput {
                mode: ProxyMode::Custom,
                address: dropping_proxy().await,
                auth_enabled: false,
                username: String::new(),
                password: None,
            })
            .await
            .unwrap();
        let proxy = CursorProxy::cursor(NetworkClients::new(store));
        let request_id = "handler-upstream-failure";

        let error = bidi_handler(
            State(registry.clone()),
            Extension(proxy),
            upstream_append(request_id, "official-model", "https://api2.cursor.sh"),
        )
        .await
        .expect_err("官方上游连不上时这次 append 必须失败");

        assert!(!error.to_string().is_empty(), "{error}");
        // `finish_upstream` 是同步接口、内部 spawn 摘标记，所以效果不是"返回时就没了"：
        // 条件等待到它真的消失（有上界，避免守卫坏掉时挂住）。
        let released = tokio::time::timeout(std::time::Duration::from_secs(5), async {
            loop {
                if !registry.upstream(request_id).await {
                    break;
                }
                tokio::time::sleep(std::time::Duration::from_millis(5)).await;
            }
        })
        .await;
        assert!(
            released.is_ok(),
            "转发失败之后路由不许继续钉在官方上游（标记必须被还回去）"
        );
    }

    /// 没有模型的 append（心跳、工具结果、子代理通知）不读模式：`cursor_mode` 只用来回答
    /// "官方模型该不该拒绝"。这条用例把两个读数都换成会 panic 的闭包——真读了就是 panic。
    #[tokio::test]
    async fn an_append_without_a_model_never_reads_the_cursor_mode() {
        #[allow(clippy::unreachable)]
        let route = model_route(
            None,
            |_| async { unreachable!("没有模型时不该查本地模型库") },
            || async { unreachable!("没有模型时不该读 cursor_mode") },
        )
        .await
        .unwrap();

        assert_eq!(route, None, "没有模型时要交给调用方看 transport 标记");
    }

    /// 命中本地模型时同样不读模式：模式只在"不是本地模型"之后才需要。
    #[tokio::test]
    async fn a_locally_answered_model_never_reads_the_cursor_mode() {
        let route = model_route(
            Some("byok-model"),
            |_| async { Ok(true) },
            || async { unreachable!("本地模型不需要读 cursor_mode") },
        )
        .await
        .unwrap();

        assert_eq!(route, Some(ModelRoute::Local));
    }

    /// 本地模式 + 不在库里的模型：拒绝，而不是悄悄转发到官方（那会花掉官方额度）。
    #[tokio::test]
    async fn an_official_model_is_refused_in_local_mode() {
        let error = model_route(
            Some("claude-4.5-sonnet"),
            |_| async { Ok(false) },
            || async { Ok(crate::store::CursorMode::Local) },
        )
        .await
        .unwrap_err();

        assert!(
            error.to_string().contains("not in the model library"),
            "{error}"
        );
    }

    /// 允许官方模型的模式：转发到官方上游。
    #[tokio::test]
    async fn an_official_model_is_forwarded_when_the_mode_allows_it() {
        let route = model_route(
            Some("claude-4.5-sonnet"),
            |_| async { Ok(false) },
            || async { Ok(crate::store::CursorMode::Mixed) },
        )
        .await
        .unwrap();

        assert_eq!(route, Some(ModelRoute::Upstream));
    }

    /// 上限之内：正文完整交给处理器（边界包含：正好等于上限也要通过）。
    #[tokio::test]
    async fn a_body_within_the_limit_reaches_the_handler() {
        let request = Request::builder()
            .method("POST")
            .uri("/aiserver.v1.AiService/BidiAppend")
            .body(Body::from(vec![b'y'; 32]))
            .unwrap();

        let (parts, body) = buffered(request, 32).await.expect("32 字节是上限之内");

        assert_eq!(parts.method, "POST");
        assert_eq!(body.len(), 32);
    }

    /// 超限：413 直接回给客户端，处理器不再往下走（`to_bytes` 时代这里是 400，
    /// 与真实原因不符——请求是"太大"，不是"坏了"）。
    #[tokio::test]
    async fn a_body_over_the_limit_is_answered_with_413_before_the_handler_runs() {
        let request = Request::builder()
            .method("POST")
            .uri("/aiserver.v1.AiService/BidiAppend")
            .body(Body::from(vec![b'x'; 64]))
            .unwrap();

        let response = buffered(request, 32).await.unwrap_err();

        assert_eq!(response.status(), StatusCode::PAYLOAD_TOO_LARGE);
    }
}
