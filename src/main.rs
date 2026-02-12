use forge::prelude::*;
use std::sync::Arc;

mod functions;
mod schema;
mod services;

#[cfg(feature = "embedded-frontend")]
mod embedded {
    use axum::{
        body::Body,
        http::{Request, StatusCode, header},
        response::{IntoResponse, Response},
    };
    use rust_embed::Embed;
    use std::future::Future;
    use std::pin::Pin;

    #[derive(Embed)]
    #[folder = "frontend/build"]
    pub struct Assets;

    async fn serve_frontend_inner(req: Request<Body>) -> Response {
        let path = req.uri().path().trim_start_matches('/');
        let path = if path.is_empty() { "index.html" } else { path };

        match Assets::get(path) {
            Some(content) => {
                let mime = mime_guess::from_path(path).first_or_octet_stream();
                ([(header::CONTENT_TYPE, mime.as_ref())], content.data).into_response()
            }
            None => match Assets::get("index.html") {
                Some(content) => {
                    ([(header::CONTENT_TYPE, "text/html")], content.data).into_response()
                }
                None => (StatusCode::NOT_FOUND, "not found").into_response(),
            },
        }
    }

    pub fn serve_frontend(req: Request<Body>) -> Pin<Box<dyn Future<Output = Response> + Send>> {
        Box::pin(serve_frontend_inner(req))
    }
}

static AI_SERVICE: tokio::sync::OnceCell<std::sync::Arc<dyn services::AiService>> =
    tokio::sync::OnceCell::const_new();

static MEDIA_PREPROCESSOR: tokio::sync::OnceCell<services::MediaPreprocessor> =
    tokio::sync::OnceCell::const_new();

fn init_ai_service() -> Arc<dyn services::AiService> {
    let embedding = Arc::new(
        services::EmbeddingService::new().expect("failed to initialize embedding model"),
    );

    Arc::new(
        services::RealAiService::new(embedding).expect("failed to create AI service"),
    )
}

pub fn get_ai_service() -> Arc<dyn services::AiService> {
    AI_SERVICE
        .get()
        .cloned()
        .expect("AI service not initialized")
}

pub fn get_media_preprocessor() -> Option<&'static services::MediaPreprocessor> {
    MEDIA_PREPROCESSOR.get()
}

#[tokio::main]
async fn main() -> Result<()> {
    dotenvy::dotenv().ok();
    tracing_subscriber::fmt()
        .with_env_filter(tracing_subscriber::EnvFilter::from_default_env())
        .init();

    let ai_service = init_ai_service();
    AI_SERVICE.set(ai_service).ok();

    MEDIA_PREPROCESSOR
        .set(services::MediaPreprocessor::from_env())
        .ok();
    tracing::info!("media preprocessor initialized");

    let config = ForgeConfig::from_file("forge.toml")?;
    let mut builder = Forge::builder();

    let fns = builder.function_registry_mut();
    fns.register_query::<functions::ListEventsQuery>();
    fns.register_query::<functions::ListJobsQuery>();
    fns.register_query::<functions::ListOutboxQuery>();
    fns.register_query::<functions::ListCronsQuery>();
    fns.register_query::<functions::ListMessagesQuery>();
    fns.register_query::<functions::GetTraceQuery>();
    fns.register_query::<functions::GetHealthQuery>();
    fns.register_mutation::<functions::CancelJobMutation>();
    fns.register_mutation::<functions::ToggleCronMutation>();

    let daemons = builder.daemon_registry_mut();
    daemons.register::<functions::GatewayDaemon>();
    daemons.register::<functions::TriageDaemon>();
    daemons.register::<functions::ContextLoopDaemon>();
    daemons.register::<functions::ClockDaemon>();
    daemons.register::<functions::RuntimeDaemon>();
    daemons.register::<functions::ReplyDaemon>();
    daemons.register::<functions::DeliveryDaemon>();
    daemons.register::<functions::AuditDaemon>();

    #[cfg(feature = "embedded-frontend")]
    builder.frontend_handler(embedded::serve_frontend);

    builder.config(config).build()?.run().await
}

