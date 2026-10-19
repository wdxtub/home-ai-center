//! 启动装配。
//!
//! 顺序是有讲究的：
//! 1. 配置 → 目录
//! 2. 建库 → 跑迁移（迁移必须先于任何查询）
//! 3. 崩溃对账（清残留冻结）
//! 4. 初始化管理员
//! 5. 装快照（此时**读路径**才可用）
//! 6. 挂路由 → 起后台任务
//!
//! 任何一步失败都直接退出，不做「降级启动」——配置不全的网关
//! 会以很难排查的方式错账，不如起不来。

use std::sync::Arc;

use axum::extract::{FromRequestParts, State};
use axum::http::StatusCode;
use axum::response::IntoResponse;
use axum::routing::{delete, get, post, put};
use axum::Router;
use serde_json::json;
use tower_http::trace::TraceLayer;

use hac::api::{admin, auth, images, v1};
use hac::billing::ledger;
use hac::config::Config;
use hac::state::{AppState, Snapshot};

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    init_tracing();

    let cfg = Config::from_env();
    cfg.ensure_dirs()?;
    tracing::info!(
        data_dir = %cfg.data_dir.display(),
        bind = %cfg.bind,
        timezone = %cfg.timezone,
        "home-ai-center 启动"
    );

    let pool = hac::db::connect(&cfg.db_path()).await?;
    hac::db::migrate::MIGRATOR.run(&pool).await?;
    tracing::info!("数据库迁移完成");

    // 崩溃残留的预扣一律清零：宁可少收，不可超收
    match ledger::reconcile_holds(&pool).await {
        Ok(0) => {}
        Ok(n) => tracing::warn!(n, "已清理崩溃残留的预扣冻结"),
        Err(e) => tracing::error!(error = %e, "预扣对账失败"),
    }

    auth::ensure_admin(&pool, cfg.admin_token.as_deref()).await?;

    let state = Arc::new(AppState {
        keys: Arc::new(hac::keys::KeyRing::new(pool.clone())),
        health: Arc::new(hac::gate::NodeHealth::default()),
        cfg,
        pool,
        snapshot: arc_swap::ArcSwap::from_pointee(Snapshot::default()),
        node_gates: Default::default(),
        key_gates: Default::default(),
        comfy_gates: Default::default(),
        login_throttle: Default::default(),
    });
    // 快照必须在挂路由之前就绪，否则第一个请求会读到空配置
    state.reload().await?;
    {
        let snap = state.snapshot();
        tracing::info!(
            nodes = snap.nodes.len(),
            comfy = snap.comfy_nodes.len(),
            models = snap.models().len(),
            workflows = snap.workflows.len(),
            "运行时快照已装载"
        );
    }

    let app = router(state.clone());

    let listener = tokio::net::TcpListener::bind(&state.cfg.bind).await?;
    tracing::info!("监听 {}", state.cfg.bind);

    hac::tasks::spawn_all(state);

    axum::serve(
        listener,
        app.into_make_service_with_connect_info::<std::net::SocketAddr>(),
    )
    .with_graceful_shutdown(shutdown_signal())
    .await?;
    tracing::info!("已退出");
    Ok(())
}

fn init_tracing() {
    use tracing_subscriber::{fmt, EnvFilter};
    let filter = EnvFilter::try_from_env("HOME_AI_LOG")
        .or_else(|_| EnvFilter::try_new("info,hac=debug,sqlx=warn"))
        .unwrap_or_else(|_| EnvFilter::new("info"));
    fmt().with_env_filter(filter).with_target(false).init();
}

fn router(state: Arc<AppState>) -> Router {
    let s = state.clone();

    // ── 网关面：调用方用 API Key ──
    // 路径**不带** /v1 前缀：整个 Router 会被 `nest("/v1", ...)` 挂上去。
    // 这里再写一遍前缀，实际路径就变成 /v1/v1/chat/completions，
    // 所有调用都会掉进 SPA 的 index.html。
    let gateway = Router::new()
        .route("/chat/completions", post(v1::chat_completions))
        .route("/responses", post(v1::responses))
        .route("/messages", post(v1::messages))
        .route("/models", get(v1::models))
        .route("/images/generations", post(images::generate))
        .route("/workflows", get(images::list_workflows))
        .route("/usage", get(v1::usage_info))
        .with_state(s.clone());

    // ── 管理面：管理员会话 ──
    // 公开面与受保护面**必须拆成两个 Router**：route_layer 作用于
    // 它之前的全部路由，混在一起会把 /auth/login 自己挡在门外。
    let admin_public = Router::new()
        .route("/auth/login", post(admin::login))
        .with_state(s.clone());

    let admin = Router::new()
        .route("/auth/logout", post(admin::logout))
        .route("/auth/me", get(admin::me))
        .route("/auth/password", put(admin::change_password))
        // 账号
        .route("/accounts", get(admin::list_accounts).post(admin::create_account))
        .route(
            "/accounts/{id}",
            put(admin::update_account).delete(admin::delete_account),
        )
        // 调用方的 key。和下面的「节点 key」是两回事，路径也分开：
        // 节点 key 是上游那把 LLM key，这里是调用方自己的 key。
        .route("/accounts/{id}/api-keys", post(admin::create_api_key))
        .route(
            "/api-keys/{id}",
            put(admin::update_api_key).delete(admin::delete_api_key),
        )
        .route("/api-keys/{id}/rotate", post(admin::rotate_api_key))
        .route("/api-keys/{id}/stats", get(admin::api_key_stats))
        .route("/accounts/{id}/topup", post(admin::topup))
        .route("/accounts/{id}/txns", get(admin::account_txns))
        // 节点
        .route("/nodes", get(admin::list_nodes).post(admin::create_node))
        .route("/nodes/{id}", put(admin::update_node).delete(admin::delete_node))
        .route("/nodes/{id}/keys", get(admin::list_keys).post(admin::create_key))
        .route("/keys/{id}", put(admin::update_key).delete(admin::delete_key))
        .route("/keys/{id}/clear-cooldown", post(admin::clear_key_cooldown))
        .route("/keys/{id}/events", get(admin::key_events))
        // 路由
        .route("/routes", get(admin::list_routes).post(admin::upsert_route))
        .route("/routes/{id}", delete(admin::delete_route))
        // 定价
        .route("/prices", get(admin::list_prices).post(admin::upsert_price))
        .route("/prices/{model}", delete(admin::delete_price))
        // ComfyUI
        .route(
            "/comfy-nodes",
            get(admin::list_comfy_nodes).post(admin::create_comfy_node),
        )
        .route(
            "/comfy-nodes/{id}",
            put(admin::update_comfy_node).delete(admin::delete_comfy_node),
        )
        // 工作流
        .route(
            "/workflows",
            get(admin::list_workflows).post(admin::create_workflow),
        )
        .route(
            "/workflows/{id}",
            get(admin::get_workflow)
                .put(admin::update_workflow)
                .delete(admin::delete_workflow),
        )
        .route("/workflows/{id}/dry-run", post(admin::dry_run_workflow))
        // 用量
        .route("/logs", get(admin::list_logs))
        .route("/usage/summary", get(admin::usage_summary))
        .route("/overview", get(admin::overview))
        .route("/runtime", get(admin::runtime))
        // 设置与备份
        .route("/settings", get(admin::get_settings).put(admin::put_settings))
        .route("/backups", get(admin::list_backups))
        .route("/backups/run", post(admin::backup_now))
        .route("/backups/purge", post(admin::backup_retention))
        .route_layer(axum::middleware::from_fn_with_state(
            s.clone(),
            require_admin,
        ))
        .with_state(s.clone())
        .merge(admin_public);

    let cors = build_cors(&state.cfg);

    let app = Router::new()
        .route("/api/health", get(health))
        .nest("/v1", gateway)
        .nest("/api/admin", admin)
        .fallback(hac::webui::serve)
        .layer(TraceLayer::new_for_http())
        .with_state(s);

    // 默认**不加** CORS 层 = 只允许同源。真要跨源就显式列出来。
    match cors {
        Some(c) => app.layer(c),
        None => app,
    }
}

/// 跨域白名单。没配就返回 `None`，即「什么都不加」。
///
/// 之前这里是 `CorsLayer::permissive()`：任意网页都能带着调用方 key
/// 跨域打 `/v1` 并**读到响应**。管理面因为 Cookie 是 `SameSite=Strict`
/// 且 permissive 不带 credentials，勉强挡住；网关面没有任何东西挡。
fn build_cors(cfg: &Config) -> Option<tower_http::cors::CorsLayer> {
    use axum::http::{header, HeaderName, Method};
    use tower_http::cors::{AllowOrigin, CorsLayer};

    let (origins, bad) = split_cors_origins(&cfg.cors_origins);
    // 配错的项必须**看得见**。静默丢掉一个拼错的 origin 属于「配了不生效」：
    // 跨域请求照样被浏览器拦，而用户只会看到一个 CORS 报错，
    // 根本猜不到是自己少写了 `https://`。
    for o in &bad {
        tracing::warn!(
            origin = %o,
            "HOME_AI_CORS_ORIGINS 里这一项不是合法的 Origin，已忽略；\
             必须带 scheme，例如 https://your-ui.example.org"
        );
    }
    if origins.is_empty() {
        return None;
    }
    Some(
        CorsLayer::new()
            .allow_origin(AllowOrigin::list(origins))
            .allow_methods([
                Method::GET,
                Method::POST,
                Method::PUT,
                Method::DELETE,
                Method::OPTIONS,
            ])
            // 只放行真正用到的头，不放行 `Any`：头名是攻击面，
            // 白名单比「全放」多一行字、少一个洞。
            .allow_headers([
                header::AUTHORIZATION,
                header::CONTENT_TYPE,
                HeaderName::from_static("x-api-key"),
                HeaderName::from_static("x-admin-token"),
                HeaderName::from_static("anthropic-version"),
            ]),
    )
}

/// 把 `HOME_AI_CORS_ORIGINS` 拆成「能用的」和「用不了的」两份。
///
/// 分两份就是为了让配错的项能被告警出来。返回 `None` 不行——那等于
/// 又变回「配了不生效」，正是这个函数要消灭的东西。
///
/// **光靠 `HeaderValue::from_str` 是不够的**：裸主机名
/// `your-ui.example.org` 是个合法的 header 值，于是会被「通过」，
/// 但它永远匹配不上浏览器发来的任何 `Origin`——配了等于没配。
/// 所以先按 Origin 的实际形态校验，再交给 `HeaderValue`。
fn split_cors_origins(raw: &[String]) -> (Vec<axum::http::HeaderValue>, Vec<&str>) {
    let mut ok = Vec::new();
    let mut bad = Vec::new();
    for o in raw {
        let lower = o.to_ascii_lowercase();
        // 浏览器发来的 Origin 只可能是 http(s)://host[:port]。
        // 沙箱 iframe 的 `null` 那种源本来就不该白名单放行。
        let shaped = (lower.starts_with("http://") || lower.starts_with("https://"))
            // 结尾斜杠是第二个经典配错：Origin 头里没有它，永远匹配不上
            && !o.ends_with('/');
        if !shaped {
            bad.push(o.as_str());
            continue;
        }
        match o.parse::<axum::http::HeaderValue>() {
            Ok(v) => ok.push(v),
            Err(_) => bad.push(o.as_str()),
        }
    }
    (ok, bad)
}

/// 管理面守卫。挂在 `route_layer` 上，`/auth/login` 因为在
/// 同一个 Router 的另一层而豁免。
async fn require_admin(
    State(s): State<Arc<AppState>>,
    req: axum::extract::Request,
    next: axum::middleware::Next,
) -> axum::response::Response {
    let (mut parts, body) = req.into_parts();
    match auth::AdminAuthed::from_request_parts(&mut parts, &s).await {
        Ok(_) => next.run(axum::extract::Request::from_parts(parts, body)).await,
        Err(e) => e.into_response(),
    }
}

/// `/api/health` 不需要鉴权：容器健康检查与前端启动探测都要能打进来。
async fn health(State(s): State<Arc<AppState>>) -> impl IntoResponse {
    let snap = s.snapshot();
    let db_ok = sqlx::query("SELECT 1")
        .fetch_one(&s.pool)
        .await
        .is_ok();
    let status = if db_ok {
        StatusCode::OK
    } else {
        StatusCode::SERVICE_UNAVAILABLE
    };
    (
        status,
        axum::Json(json!({
            "ok": db_ok,
            "version": env!("CARGO_PKG_VERSION"),
            "uptime_secs": uptime_secs(),
            "nodes": snap.nodes.len(),
            "comfy_nodes": snap.comfy_nodes.len(),
            "models": snap.models().len(),
            "workflows": snap.workflows.len(),
            "frontend": hac::webui::has_frontend(),
        })),
    )
}

static START: std::sync::OnceLock<std::time::Instant> = std::sync::OnceLock::new();

fn uptime_secs() -> u64 {
    START
        .get_or_init(std::time::Instant::now)
        .elapsed()
        .as_secs()
}

async fn shutdown_signal() {
    let ctrl_c = async {
        tokio::signal::ctrl_c()
            .await
            .expect("注册 Ctrl+C 失败");
    };
    #[cfg(unix)]
    let terminate = async {
        tokio::signal::unix::signal(tokio::signal::unix::SignalKind::terminate())
            .expect("注册 SIGTERM 失败")
            .recv()
            .await;
    };
    #[cfg(not(unix))]
    let terminate = std::future::pending::<()>();

    tokio::select! {
        _ = ctrl_c => tracing::info!("收到 Ctrl+C"),
        _ = terminate => tracing::info!("收到 SIGTERM"),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn v(items: &[&str]) -> Vec<String> {
        items.iter().map(|s| s.to_string()).collect()
    }

    /// 少写 `https://` 是最常见的配错。它必须能被单独挑出来告警，
    /// 而不是和没配过一样安静——否则用户只会看到一个 CORS 报错。
    #[test]
    fn a_broken_origin_is_reported_not_swallowed() {
        let raw = v(&["your-ui.example.org"]);
        let (ok, bad) = split_cors_origins(&raw);
        assert!(ok.is_empty(), "没 scheme 的不该通过");
        assert_eq!(bad, vec!["your-ui.example.org"]);
    }

    #[test]
    fn a_well_formed_origin_passes() {
        let raw = v(&["https://your-ui.example.org"]);
        let (ok, bad) = split_cors_origins(&raw);
        assert_eq!(ok.len(), 1);
        assert!(bad.is_empty());
    }

    /// 一好一坏时要各归各位，不能因为有一个坏的就整份丢弃。
    #[test]
    fn good_and_bad_are_separated() {
        let raw = v(&[
            "https://a.example.org",
            "not a origin",
            "https://b.example.org",
        ]);
        let (ok, bad) = split_cors_origins(&raw);
        assert_eq!(ok.len(), 2);
        assert_eq!(bad, vec!["not a origin"]);
    }

    /// 结尾斜杠同样匹配不上（Origin 头里没有），也必须被挑出来。
    #[test]
    fn a_trailing_slash_is_also_rejected() {
        let raw = v(&["https://your-ui.example.org/"]);
        let (ok, bad) = split_cors_origins(&raw);
        assert!(ok.is_empty());
        assert_eq!(bad, vec!["https://your-ui.example.org/"]);
    }

    /// 端口是 Origin 的合法一部分，别把它一起拒了。
    #[test]
    fn a_port_is_kept() {
        let raw = v(&["http://192.168.1.20:5173"]);
        let (ok, bad) = split_cors_origins(&raw);
        assert_eq!(ok.len(), 1, "带端口的源要能通过");
        assert!(bad.is_empty());
    }

    /// 默认（没配）时不能凭空造出任何白名单——那就是 permissive 复活。
    #[test]
    fn nothing_configured_means_nothing_allowed() {
        let (ok, bad) = split_cors_origins(&[]);
        assert!(ok.is_empty());
        assert!(bad.is_empty());
    }
}
