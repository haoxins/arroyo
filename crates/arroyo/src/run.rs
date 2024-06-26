use crate::{db_source, RunArgs};
use anyhow::bail;
use arroyo_openapi::types::{Pipeline, PipelinePatch, PipelinePost, StopType, ValidateQueryPost};
use arroyo_openapi::Client;
use arroyo_rpc::config::{config, DatabaseType, DefaultSink, Scheduler};
use arroyo_rpc::{config, retry};
use arroyo_server_common::log_event;
use arroyo_server_common::shutdown::{Shutdown, ShutdownHandler, SignalBehavior};
use async_trait::async_trait;
use rand::random;
use serde_json::json;
use std::env;
use std::env::set_var;
use std::path::PathBuf;
use std::process::exit;
use std::str::FromStr;
use std::sync::{Arc, Mutex};
use std::time::Duration;
use tokio::time::timeout;
use tracing::level_filters::LevelFilter;
use tracing::{error, info, warn};
use tracing_subscriber::EnvFilter;

async fn get_state(client: &Client, pipeline_id: &str) -> String {
    let jobs = retry!(
        client.get_pipeline_jobs().id(pipeline_id).send().await,
        10,
        Duration::from_millis(100),
        Duration::from_secs(2),
        |e| { warn!("Failed to get job state from API: {}", e) }
    )
    .unwrap()
    .into_inner();

    jobs.data.into_iter().next().unwrap().state
}

async fn wait_for_state(
    client: &Client,
    pipeline_id: &str,
    expected_states: &[&str],
) -> anyhow::Result<()> {
    let mut last_state: String = get_state(client, pipeline_id).await;
    while !expected_states.contains(&last_state.as_str()) {
        let state = get_state(client, pipeline_id).await;
        if last_state != state {
            info!("Job transitioned to {}", state);
            last_state = state;
        }

        if last_state == "Failed" {
            bail!("Job transitioned to failed");
        }

        tokio::time::sleep(Duration::from_millis(100)).await;
    }

    Ok(())
}

async fn wait_for_connect(client: &Client) -> anyhow::Result<()> {
    for _ in 0..50 {
        if client.ping().send().await.is_ok() {
            return Ok(());
        }
        tokio::time::sleep(Duration::from_millis(10)).await;
    }

    bail!("API server did not start up successfully; see logs for more details");
}

#[derive(Clone)]
struct PipelineShutdownHandler {
    client: Arc<Client>,
    pipeline_id: Arc<Mutex<Option<String>>>,
}

#[async_trait]
impl ShutdownHandler for PipelineShutdownHandler {
    async fn shutdown(&self) {
        let Some(pipeline_id) = (*self.pipeline_id.lock().unwrap()).clone() else {
            return;
        };

        info!("Stopping pipeline with a final checkpoint...");
        if let Err(e) = self
            .client
            .patch_pipeline()
            .id(&pipeline_id)
            .body(PipelinePatch::builder().stop(StopType::Checkpoint))
            .send()
            .await
        {
            warn!("Unable to stop pipeline with a final checkpoint: {}", e);
            return;
        }

        if let Err(_) = timeout(
            Duration::from_secs(120),
            wait_for_state(&self.client, &pipeline_id, &["Stopped", "Failed"]),
        )
        .await
        {
            error!(
                "Pipeline did not complete checkpoint within timeout; shutting down immediately"
            );
        }
    }
}

async fn get_pipelines(client: &Client) -> anyhow::Result<Vec<Pipeline>> {
    let mut starting_after = "".to_string();
    let mut result = vec![];
    loop {
        let pipelines = client
            .get_pipelines()
            .starting_after(&starting_after)
            .send()
            .await?
            .into_inner();

        if let Some(next) = pipelines.data.last().map(|p| p.id.to_string()) {
            starting_after = next;
        }

        result.extend(pipelines.data.into_iter());

        if !pipelines.has_more {
            break;
        }
    }

    Ok(result)
}

async fn run_pipeline(
    client: Arc<Client>,
    name: Option<String>,
    query: String,
    parallelism: u32,
    http_port: u16,
    shutdown_handler: PipelineShutdownHandler,
) -> anyhow::Result<()> {
    // wait until server is available
    wait_for_connect(&client).await.unwrap();

    // validate the pipeline
    let errors = client
        .validate_query()
        .body(ValidateQueryPost::builder().query(&query))
        .send()
        .await?
        .into_inner();

    if !errors.errors.is_empty() {
        eprintln!("There were some issues with the provided query");
        for error in errors.errors {
            eprintln!("  * {error}");
        }
        exit(1);
    }

    // see if our current pipeline is in the existing pipelines
    let id = match get_pipelines(&client)
        .await?
        .into_iter()
        .find(|p| p.query == query)
    {
        Some(p) => {
            info!("Pipeline already exists in database as {}", p.id);
            client
                .patch_pipeline()
                .id(&p.id)
                .body(PipelinePatch::builder().stop(StopType::None))
                .send()
                .await?;
            p.id
        }
        None => {
            // or create it
            client
                .create_pipeline()
                .body(
                    PipelinePost::builder()
                        .name(name.unwrap_or_else(|| "query".to_string()))
                        .parallelism(parallelism)
                        .query(&query),
                )
                .send()
                .await?
                .into_inner()
                .id
        }
    };

    {
        *shutdown_handler.pipeline_id.lock().unwrap() = Some(id.clone());
    }

    wait_for_state(&client, &id, &["Running"]).await?;

    info!("Pipeline running... dashboard at http://localhost:{http_port}/pipelines/{id}");

    Ok(())
}

pub async fn run(args: RunArgs) {
    let _guard = arroyo_server_common::init_logging_with_filter(
        "pipeline",
        if !env::var("RUST_LOG").is_ok() {
            set_var("RUST_LOG", "WARN");
            EnvFilter::builder()
                .with_default_directive(LevelFilter::WARN.into())
                .from_env_lossy()
                .add_directive("arroyo::run=info".parse().unwrap())
        } else {
            EnvFilter::builder()
                .with_default_directive(LevelFilter::INFO.into())
                .from_env_lossy()
        },
    );

    let query = match config().query.clone() {
        Some(query) => query,
        None => std::io::read_to_string(args.query).unwrap(),
    };

    let mut shutdown = Shutdown::new("pipeline", SignalBehavior::Handle);

    let db_path = args.database.clone().unwrap_or_else(|| {
        PathBuf::from_str(&format!("/tmp/arroyo/{}.arroyo", random::<u32>())).unwrap()
    });

    config::update(|c| {
        c.database.r#type = DatabaseType::Sqlite;
        c.database.sqlite.path = db_path.clone();

        if let Some(port) = c.api.run_http_port {
            c.api.http_port = port;
        } else {
            c.api.http_port = 0;
        }
        c.controller.rpc_port = 0;
        c.controller.scheduler = Scheduler::Process;

        c.pipeline.default_sink = DefaultSink::Stdout;
    });

    let db = db_source().await;

    log_event("pipeline_cluster_start", json!({}));

    let controller_port = arroyo_controller::ControllerServer::new(db.clone())
        .await
        .start(shutdown.guard("controller"))
        .await
        .expect("could not start system");

    config::update(|c| c.controller.rpc_port = controller_port);

    let http_port = arroyo_api::start_server(db.clone(), shutdown.guard("api")).unwrap();

    let client = Arc::new(Client::new_with_client(
        &format!("http://localhost:{http_port}/api",),
        reqwest::ClientBuilder::new()
            .timeout(Duration::from_secs(60))
            .build()
            .unwrap(),
    ));

    let shutdown_handler = PipelineShutdownHandler {
        client: client.clone(),
        pipeline_id: Arc::new(Mutex::new(None)),
    };

    shutdown.set_handler(Box::new(shutdown_handler.clone()));

    shutdown.spawn_temporary(async move {
        run_pipeline(
            client,
            args.name,
            query,
            args.parallelism,
            http_port,
            shutdown_handler,
        )
        .await
    });

    Shutdown::handle_shutdown(shutdown.wait_for_shutdown(Duration::from_secs(60)).await);
}
