use crate::config::Config;

use anyhow::Result;
use chrono::{DateTime, Duration, NaiveDateTime, Utc};
use core::fmt;
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

pub async fn upsert_node_tx(
    transaction: &tokio_postgres::Transaction<'_>,
    pubkey: &[u8],
) -> Result<i32, tokio_postgres::Error> {
    let row = transaction
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
    )
    // Increase the decoding message size. The DescribeGraph request in particular
    // can return a large response (larger than the default).
    .max_decoding_message_size(64 * 1024 * 1024);

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

#[derive(Debug, Clone, PartialEq)]
pub enum ChannelBalanceCategory {
    DepletedOutgoingLiquidity,
    MinimalOutgoingLiquidity,
    ModerateOutgoingLiquidity,
    BalancedLiquidity,
    ModerateIncomingLiquidity,
    MinimalIncomingLiquidity,
    DepletedIncomingLiquidity,
}

impl fmt::Display for ChannelBalanceCategory {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            ChannelBalanceCategory::DepletedOutgoingLiquidity => {
                write!(f, "DepletedOutgoingLiquidity")
            }
            ChannelBalanceCategory::MinimalOutgoingLiquidity => {
                write!(f, "MinimalOutgoingLiquidity")
            }
            ChannelBalanceCategory::ModerateOutgoingLiquidity => {
                write!(f, "ModerateOutgoingLiquidity")
            }
            ChannelBalanceCategory::BalancedLiquidity => write!(f, "BalancedLiquidity"),
            ChannelBalanceCategory::ModerateIncomingLiquidity => {
                write!(f, "ModerateIncomingLiquidity")
            }
            ChannelBalanceCategory::MinimalIncomingLiquidity => {
                write!(f, "MinimalIncomingLiquidity")
            }
            ChannelBalanceCategory::DepletedIncomingLiquidity => {
                write!(f, "DepletedIncomingLiquidity")
            }
        }
    }
}

#[derive(Debug, Clone)]
pub struct ChannelBalanceInfo {
    pub channel: lnrpc::Channel,
    pub channel_balance_category: ChannelBalanceCategory,
}

pub async fn get_chan_info(
    mut client: lnrpc::lightning_client::LightningClient<
        tonic::service::interceptor::InterceptedService<
            tonic::transport::Channel,
            impl Fn(Request<()>) -> Result<Request<()>, tonic::Status> + Clone,
        >,
    >,
    chan_id: u64,
) -> Result<lnrpc::ChannelEdge> {
    let request = Request::new(lnrpc::ChanInfoRequest {
        chan_id,
        chan_point: String::new(),
    });
    let response = client.get_chan_info(request).await?;
    Ok(response.into_inner())
}

pub async fn get_channel_balance_info(
    lightning_client: lnrpc::lightning_client::LightningClient<
        tonic::service::interceptor::InterceptedService<
            tonic::transport::Channel,
            impl Fn(Request<()>) -> Result<Request<()>, tonic::Status> + Clone,
        >,
    >,
    db_client: &tokio_postgres::Client,
    _info: &lnrpc::GetInfoResponse,
    channel: &lnrpc::Channel,
    _channel_info: &lnrpc::ChannelEdge,
) -> Result<ChannelBalanceInfo> {
    let channel_id = to_channel_id(channel.chan_id);

    let now = Utc::now();
    let after = now - Duration::hours(48);

    let local_balance = channel.local_balance;
    let remote_balance = channel.remote_balance;

    #[allow(deprecated)]
    let local_chan_reserve_sat = channel.local_chan_reserve_sat;
    if local_balance < local_chan_reserve_sat {
        let channel_balance_category = ChannelBalanceInfo {
            channel: channel.clone(),
            channel_balance_category: ChannelBalanceCategory::DepletedOutgoingLiquidity,
        };

        return Ok(channel_balance_category);
    }

    #[allow(deprecated)]
    let remote_chan_reserve_sat = channel.remote_chan_reserve_sat;
    if remote_balance < remote_chan_reserve_sat {
        let channel_balance_category = ChannelBalanceInfo {
            channel: channel.clone(),
            channel_balance_category: ChannelBalanceCategory::DepletedIncomingLiquidity,
        };

        return Ok(channel_balance_category);
    }

    let outgoing_forwards_count = count_forwards(db_client, None, Some(channel_id), after).await?;
    let avg_outgoing_forward_size_msat = outgoing_forwards_count.avg_incoming_amount_msat;

    if local_balance < local_chan_reserve_sat + 2 * 1000 * avg_outgoing_forward_size_msat {
        let channel_balance_category = ChannelBalanceInfo {
            channel: channel.clone(),
            channel_balance_category: ChannelBalanceCategory::MinimalOutgoingLiquidity,
        };

        return Ok(channel_balance_category);
    }

    let incoming_forwards_count = count_forwards(db_client, Some(channel_id), None, after).await?;
    let avg_incoming_forward_size_msat = incoming_forwards_count.avg_incoming_amount_msat;

    if remote_balance < remote_chan_reserve_sat + 2 * 1000 * avg_incoming_forward_size_msat {
        let channel_balance_category = ChannelBalanceInfo {
            channel: channel.clone(),
            channel_balance_category: ChannelBalanceCategory::MinimalIncomingLiquidity,
        };

        return Ok(channel_balance_category);
    }

    let chan_info = get_chan_info(lightning_client.clone(), channel.chan_id).await?;
    let moderate_liquidity_threshold = (0.2 * chan_info.capacity as f64) as i64;

    if local_balance < moderate_liquidity_threshold {
        let channel_balance_category = ChannelBalanceInfo {
            channel: channel.clone(),
            channel_balance_category: ChannelBalanceCategory::ModerateOutgoingLiquidity,
        };

        return Ok(channel_balance_category);
    }

    if remote_balance < moderate_liquidity_threshold {
        let channel_balance_category = ChannelBalanceInfo {
            channel: channel.clone(),
            channel_balance_category: ChannelBalanceCategory::ModerateIncomingLiquidity,
        };

        return Ok(channel_balance_category);
    }

    let channel_balance_category = ChannelBalanceInfo {
        channel: channel.clone(),
        channel_balance_category: ChannelBalanceCategory::BalancedLiquidity,
    };

    Ok(channel_balance_category)
}

#[derive(Debug, Clone)]
#[allow(dead_code)]
pub struct ForwardRecord {
    pub incoming_channel_id: i64,
    pub outgoing_channel_id: i64,
    pub incoming_htlc_id: i64,
    pub outgoing_htlc_id: i64,
    pub created_at: NaiveDateTime,
    pub finalized_at: Option<NaiveDateTime>,
    pub incoming_amount_msat: i64,
    pub outgoing_amount_msat: i64,
    pub state: i16,
}

pub async fn list_forwards(
    db_client: &tokio_postgres::Client,
    incoming_channel_id: Option<i64>,
    outgoing_channel_id: Option<i64>,
    after: DateTime<Utc>,
) -> Result<Vec<ForwardRecord>> {
    let after_naive = after.naive_utc();

    let rows = if let Some(incoming_id) = incoming_channel_id {
        db_client
            .query(
                "SELECT incoming_channel_id, outgoing_channel_id, incoming_htlc_id,
                        outgoing_htlc_id, created_at, finalized_at, incoming_amount_msat,
                        outgoing_amount_msat, state
                 FROM forward
                 WHERE incoming_channel_id = $1 AND created_at > $2
                 ORDER BY created_at DESC",
                &[&incoming_id, &after_naive],
            )
            .await?
    } else if let Some(outgoing_id) = outgoing_channel_id {
        db_client
            .query(
                "SELECT incoming_channel_id, outgoing_channel_id, incoming_htlc_id,
                        outgoing_htlc_id, created_at, finalized_at, incoming_amount_msat,
                        outgoing_amount_msat, state
                 FROM forward
                 WHERE outgoing_channel_id = $1 AND created_at > $2
                 ORDER BY created_at DESC",
                &[&outgoing_id, &after_naive],
            )
            .await?
    } else {
        return Ok(vec![]);
    };

    let forwards = rows
        .iter()
        .map(|row| ForwardRecord {
            incoming_channel_id: row.get(0),
            outgoing_channel_id: row.get(1),
            incoming_htlc_id: row.get(2),
            outgoing_htlc_id: row.get(3),
            created_at: row.get(4),
            finalized_at: row.get(5),
            incoming_amount_msat: row.get(6),
            outgoing_amount_msat: row.get(7),
            state: row.get(8),
        })
        .collect();

    Ok(forwards)
}

#[derive(Debug, Clone)]
pub struct ForwardCount {
    pub total_forwards_count: i64,
    pub successful_forwards_count: i64,
    pub avg_incoming_amount_msat: i64,
}

pub async fn count_forwards(
    db_client: &tokio_postgres::Client,
    incoming_channel_id: Option<i64>,
    outgoing_channel_id: Option<i64>,
    after: DateTime<Utc>,
) -> Result<ForwardCount> {
    let after_naive = after.naive_utc();

    let row = if let Some(incoming_id) = incoming_channel_id {
        db_client
            .query_one(
                "SELECT
                    COUNT(*)                           AS total_forwards_count,
                    COUNT(*) FILTER (WHERE state = 1)  AS successful_forwards_count,
                    COALESCE(AVG(incoming_amount_msat)::bigint, 0) AS avg_incoming_amount_msat
                 FROM forward
                 WHERE incoming_channel_id = $1 AND created_at > $2",
                &[&incoming_id, &after_naive],
            )
            .await?
    } else if let Some(outgoing_id) = outgoing_channel_id {
        db_client
            .query_one(
                "SELECT
                    COUNT(*)                           AS total_forwards_count,
                    COUNT(*) FILTER (WHERE state = 1)  AS successful_forwards_count,
                    COALESCE(AVG(incoming_amount_msat)::bigint, 0) AS avg_incoming_amount_msat
                 FROM forward
                 WHERE outgoing_channel_id = $1 AND created_at > $2",
                &[&outgoing_id, &after_naive],
            )
            .await?
    } else {
        return Ok(ForwardCount {
            total_forwards_count: 0,
            successful_forwards_count: 0,
            avg_incoming_amount_msat: 0,
        });
    };

    Ok(ForwardCount {
        total_forwards_count: row.get(0),
        successful_forwards_count: row.get(1),
        avg_incoming_amount_msat: row.get(2),
    })
}
