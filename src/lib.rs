use crate::config::Config;

use anyhow::Result;
use hyper_tls::{native_tls, HttpsConnector};
use hyper_util::client::legacy::connect::HttpConnector;
use std::fs;
use std::sync::Arc;
use tokio_postgres::NoTls;
use tonic::metadata::MetadataValue;
use tonic::transport::Endpoint;
use tonic::Request;
use tower::service_fn;
use tracing::{error, info};
use tracing_subscriber::prelude::*;

pub mod config;

pub mod lnrpc {
    tonic::include_proto!("lnrpc");
}

pub mod routerrpc {
    tonic::include_proto!("routerrpc");
}

pub fn to_channel_id(channel_id: u64) -> i64 {
    channel_id as i64
}

pub async fn upsert_node(
    db_client: &tokio_postgres::Client,
    pubkey: &[u8],
) -> Result<i32, tokio_postgres::Error> {
    let row = db_client
        .query_one(
            "INSERT INTO node (pubkey) VALUES ($1)
             ON CONFLICT (pubkey) DO UPDATE SET pubkey = EXCLUDED.pubkey
             RETURNING id",
            &[&pubkey],
        )
        .await?;
    Ok(row.get(0))
}

pub fn init_tracing_subscriber() {
    let env_filter = tracing_subscriber::EnvFilter::try_from_default_env()
        .unwrap_or_else(|_| tracing_subscriber::EnvFilter::new("info"));

    // If we detect that stdout/stderr is connected to journald, use the
    // journald-specific layer.
    //
    // If connecting to journald fails, fall through to the fmt subscriber.
    if std::env::var("JOURNAL_STREAM").is_ok() {
        if let Ok(journald_layer) = tracing_journald::layer() {
            tracing_subscriber::registry()
                .with(env_filter)
                .with(journald_layer)
                .init();
            return;
        }
    }

    tracing_subscriber::fmt().with_env_filter(env_filter).init();
}

pub async fn connect_to_database(config: &Config) -> Result<Arc<tokio_postgres::Client>> {
    info!("Connecting to PostgreSQL at {}", config.database.url);

    let (db_client, connection) = tokio_postgres::connect(&config.database.url, NoTls).await?;

    tokio::spawn(async move {
        if let Err(e) = connection.await {
            error!("Database connection error: {}", e);
        }
    });

    Ok(Arc::new(db_client))
}

pub async fn connect_to_lnd(config: &Config) -> Result<(tonic::transport::Channel, String)> {
    info!("Connecting to LND at {}", config.lnd.endpoint);

    let macaroon_bytes = fs::read(&config.lnd.macaroon)?;
    let macaroon_hex = hex::encode(&macaroon_bytes);

    let mut http = HttpConnector::new();
    http.enforce_http(false);

    let tls_connector = native_tls::TlsConnector::builder()
        .danger_accept_invalid_certs(true)
        .danger_accept_invalid_hostnames(true)
        .build()?
        .into();

    let https = HttpsConnector::from((http, tls_connector));

    let endpoint = Endpoint::from_shared(config.lnd.endpoint.clone())?;
    let channel = endpoint
        .connect_with_connector(service_fn(move |uri: tonic::transport::Uri| {
            let https = https.clone();
            async move {
                let conn = tower::ServiceExt::<tonic::transport::Uri>::oneshot(https, uri)
                    .await
                    .map_err(std::io::Error::other)?;
                Ok::<_, std::io::Error>(conn)
            }
        }))
        .await?;

    Ok((channel, macaroon_hex))
}

pub async fn create_lightning_clients(
    channel: tonic::transport::Channel,
    macaroon: String,
) -> Result<(
    lnrpc::lightning_client::LightningClient<
        tonic::service::interceptor::InterceptedService<
            tonic::transport::Channel,
            impl Fn(Request<()>) -> Result<Request<()>, tonic::Status> + Clone,
        >,
    >,
    routerrpc::router_client::RouterClient<
        tonic::service::interceptor::InterceptedService<
            tonic::transport::Channel,
            impl Fn(Request<()>) -> Result<Request<()>, tonic::Status> + Clone,
        >,
    >,
)> {
    let interceptor = move |mut req: Request<()>| -> Result<Request<()>, tonic::Status> {
        let macaroon_value = MetadataValue::try_from(&macaroon).unwrap();
        req.metadata_mut().insert("macaroon", macaroon_value);
        Ok(req)
    };

    let lightning_client = lnrpc::lightning_client::LightningClient::with_interceptor(
        channel.clone(),
        interceptor.clone(),
    );

    let router_client = routerrpc::router_client::RouterClient::with_interceptor(
        channel.clone(),
        interceptor.clone(),
    );

    Ok((lightning_client, router_client))
}

pub async fn get_node_id(
    mut lightning_client: lnrpc::lightning_client::LightningClient<
        tonic::service::interceptor::InterceptedService<
            tonic::transport::Channel,
            impl Fn(Request<()>) -> Result<Request<()>, tonic::Status> + Clone,
        >,
    >,
    db_client: &tokio_postgres::Client,
) -> Result<i32> {
    let get_info_request = Request::new(lnrpc::GetInfoRequest {});
    let get_info_response = lightning_client.get_info(get_info_request).await?;
    let node_pubkey = get_info_response.into_inner().identity_pubkey;
    let node_pubkey_bytes = hex::decode(&node_pubkey)?;

    let node_id = upsert_node(db_client, &node_pubkey_bytes).await?;

    Ok(node_id)
}
