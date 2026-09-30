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
        account_gates: Default::default(),
        comfy_gates: Default::default(),
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
        .route("/accounts/{id}/key", post(admin::rotate_account_key))
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

    Router::new()
        .route("/api/health", get(health))
        .nest("/v1", gateway)
        .nest("/api/admin", admin)
        .fallback(hac::webui::serve)
        .layer(TraceLayer::new_for_http())
        .layer(tower_http::cors::CorsLayer::permissive())
        .with_state(s)
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
