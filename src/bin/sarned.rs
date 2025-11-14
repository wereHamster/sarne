use anyhow::Result;
use clap::Parser;
use hyper_tls::{native_tls, HttpsConnector};
use hyper_util::client::legacy::connect::HttpConnector;
use std::fs;
use std::sync::Arc;
use tokio::signal::unix::{signal, SignalKind};
use tokio_postgres::NoTls;
use tokio_stream::StreamExt;
use tokio_util::sync::CancellationToken;
use tonic::metadata::MetadataValue;
use tonic::transport::Endpoint;
use tonic::Request;
use tower::service_fn;
use tracing::{debug, error, info, warn};
use tracing_subscriber::prelude::*;

use sarne::config::Config;
use sarne::{lnrpc, routerrpc, to_channel_id, upsert_node};

#[derive(Parser, Debug)]
#[command(name = "sarned")]
struct Args {
    /// Path to the configuration file
    #[arg(short, long, default_value = "./sarne.toml")]
    config: String,
}

fn init_tracing_subscriber() {
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

async fn connect_to_database(config: &Config) -> Result<Arc<tokio_postgres::Client>> {
    info!("Connecting to PostgreSQL at {}", config.database.url);

    let (db_client, connection) = tokio_postgres::connect(&config.database.url, NoTls).await?;

    tokio::spawn(async move {
        if let Err(e) = connection.await {
            error!("Database connection error: {}", e);
        }
    });

    Ok(Arc::new(db_client))
}

async fn connect_to_lnd(config: &Config) -> Result<(tonic::transport::Channel, String)> {
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

async fn create_lightning_clients(
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

async fn get_node_id(
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

#[tokio::main]
async fn main() -> Result<()> {
    init_tracing_subscriber();

    let args = Args::parse();

    info!("Loading config from {}", &args.config);

    let config = Config::from_file(&args.config)?;

    let (db_client, (lightning_client, router_client)) = tokio::try_join!(
        // PostgreSQL
        connect_to_database(&config),
        // LND
        async {
            let (channel, macaroon) = connect_to_lnd(&config).await?;
            create_lightning_clients(channel, macaroon).await
        },
    )?;

    let node_id = get_node_id(lightning_client.clone(), &db_client).await?;

    let cancellation_token = CancellationToken::new();

    let db_client_htlc = Arc::clone(&db_client);
    let cancel_htlc = cancellation_token.clone();
    let node_id_htlc = node_id;
    let mut htlc_task = tokio::spawn(async move {
        process_htlc_stream(router_client, db_client_htlc, node_id_htlc, cancel_htlc).await
    });

    let db_client_graph = Arc::clone(&db_client);
    let lightning_client_graph = lightning_client.clone();
    let cancel_graph = cancellation_token.clone();
    let mut graph_task = tokio::spawn(async move {
        process_channel_graph_stream(lightning_client_graph, db_client_graph, cancel_graph).await
    });

    let db_client_liquidity = Arc::clone(&db_client);
    let cancel_liquidity = cancellation_token.clone();
    let mut liquidity_task = tokio::spawn(async move {
        sample_channel_liquidity_loop(
            lightning_client,
            db_client_liquidity,
            node_id,
            cancel_liquidity,
        )
        .await
    });

    let mut sigterm = signal(SignalKind::terminate())?;
    let mut sigint = signal(SignalKind::interrupt())?;

    let result = tokio::select! {
        r = &mut htlc_task => {
            warn!("HTLC task exited, cancelling other tasks");
            Some(r)
        }
        r = &mut graph_task => {
            warn!("Channel graph task exited, cancelling other tasks");
            Some(r)
        }
        r = &mut liquidity_task => {
            warn!("Channel liquidity task exited, cancelling other tasks");
            Some(r)
        }
        _ = sigterm.recv() => {
            info!("Received SIGTERM, shutting down gracefully");
            None
        }
        _ = sigint.recv() => {
            info!("Received SIGINT, shutting down gracefully");
            None
        }
    };

    cancellation_token.cancel();

    let _ = tokio::join!(htlc_task, graph_task, liquidity_task);

    match result {
        Some(Ok(Ok(()))) => {
            warn!("A task completed unexpectedly");
            Ok(())
        }
        Some(Ok(Err(e))) => {
            error!(error = %e, "Task failed");
            Err(e)
        }
        Some(Err(e)) => {
            error!(error = %e, "Task join error");
            Err(anyhow::anyhow!("Task join error: {}", e))
        }
        None => Ok(()),
    }
}

async fn process_htlc_stream(
    mut client: routerrpc::router_client::RouterClient<
        tonic::service::interceptor::InterceptedService<
            tonic::transport::Channel,
            impl Fn(Request<()>) -> Result<Request<()>, tonic::Status> + Clone,
        >,
    >,
    db_client: Arc<tokio_postgres::Client>,
    node_id: i32,
    cancellation_token: CancellationToken,
) -> Result<()> {
    let request = Request::new(routerrpc::SubscribeHtlcEventsRequest {});
    let mut stream = client.subscribe_htlc_events(request).await?.into_inner();

    info!("Subscribed to HTLC events");

    while let Some(event) = stream.next().await {
        if cancellation_token.is_cancelled() {
            info!("HTLC stream cancelled, exiting gracefully");
            break;
        }

        match event {
            Ok(htlc_event) => {
                if let Err(e) = process_htlc_event(&db_client, node_id, &htlc_event).await {
                    error!(error = %e, "Error processing HTLC event");
                }
            }
            Err(err) => {
                error!(error = %err, "Error receiving HTLC event");
                return Err(err.into());
            }
        }
    }

    info!("HTLC stream ended");

    Ok(())
}

async fn process_htlc_event(
    db_client: &tokio_postgres::Client,
    node_id: i32,
    event: &routerrpc::HtlcEvent,
) -> Result<()> {
    let event_type = routerrpc::htlc_event::EventType::try_from(event.event_type)
        .unwrap_or(routerrpc::htlc_event::EventType::Unknown);

    if event_type != routerrpc::htlc_event::EventType::Forward {
        debug!(
            event_type = ?event_type,
            incoming_channel_id = event.incoming_channel_id,
            outgoing_channel_id = event.outgoing_channel_id,
            incoming_htlc_id = event.incoming_htlc_id,
            outgoing_htlc_id = event.outgoing_htlc_id,
            timestamp_ns = event.timestamp_ns,
            "Skipping non-FORWARD event type"
        );
        return Ok(());
    }

    let incoming_channel_id = to_channel_id(event.incoming_channel_id);
    let outgoing_channel_id = to_channel_id(event.outgoing_channel_id);
    let incoming_htlc_id = event.incoming_htlc_id as i64;
    let outgoing_htlc_id = event.outgoing_htlc_id as i64;

    let timestamp_ms = event.timestamp_ns / 1_000_000;
    let created_at = chrono::DateTime::from_timestamp_millis(timestamp_ms as i64)
        .ok_or_else(|| anyhow::anyhow!("Invalid timestamp"))?
        .naive_utc();

    if let Some(ref event_detail) = event.event {
        match event_detail {
            routerrpc::htlc_event::Event::ForwardEvent(forward) => {
                if let Some(ref info) = forward.info {
                    debug!(
                        incoming_channel_id = event.incoming_channel_id,
                        outgoing_channel_id = event.outgoing_channel_id,
                        incoming_htlc_id = event.incoming_htlc_id,
                        outgoing_htlc_id = event.outgoing_htlc_id,
                        incoming_amt_msat = info.incoming_amt_msat,
                        outgoing_amt_msat = info.outgoing_amt_msat,
                        "Tracing new forward event"
                    );

                    db_client
                        .execute(
                            "INSERT INTO forward
                             (node_id, incoming_channel_id, outgoing_channel_id,
                              incoming_htlc_id, outgoing_htlc_id, created_at,
                              incoming_amount_msat, outgoing_amount_msat, state)
                             VALUES ($1, $2, $3, $4, $5, $6, $7, $8, $9)",
                            &[
                                &node_id,
                                &incoming_channel_id,
                                &outgoing_channel_id,
                                &incoming_htlc_id,
                                &outgoing_htlc_id,
                                &created_at,
                                &(info.incoming_amt_msat as i64),
                                &(info.outgoing_amt_msat as i64),
                                &0i16,
                            ],
                        )
                        .await?;
                }
            }
            routerrpc::htlc_event::Event::LinkFailEvent(link_fail) => {
                if let Some(ref info) = link_fail.info {
                    debug!(
                        incoming_channel_id = event.incoming_channel_id,
                        outgoing_channel_id = event.outgoing_channel_id,
                        incoming_htlc_id = event.incoming_htlc_id,
                        outgoing_htlc_id = event.outgoing_htlc_id,
                        incoming_amt_msat = info.incoming_amt_msat,
                        outgoing_amt_msat = info.outgoing_amt_msat,
                        failure_string = %link_fail.failure_string,
                        "Forward failed on local link"
                    );

                    db_client
                        .execute(
                            "INSERT INTO forward
                             (node_id, incoming_channel_id, outgoing_channel_id,
                              incoming_htlc_id, outgoing_htlc_id, created_at, finalized_at,
                              incoming_amount_msat, outgoing_amount_msat, state)
                             VALUES ($1, $2, $3, $4, $5, $6, $7, $8, $9, $10)",
                            &[
                                &node_id,
                                &incoming_channel_id,
                                &outgoing_channel_id,
                                &incoming_htlc_id,
                                &outgoing_htlc_id,
                                &created_at,
                                &created_at,
                                &(info.incoming_amt_msat as i64),
                                &(info.outgoing_amt_msat as i64),
                                &-1i16,
                            ],
                        )
                        .await?;
                }
            }
            routerrpc::htlc_event::Event::ForwardFailEvent(_) => {
                debug!(
                    incoming_channel_id = event.incoming_channel_id,
                    outgoing_channel_id = event.outgoing_channel_id,
                    incoming_htlc_id = event.incoming_htlc_id,
                    outgoing_htlc_id = event.outgoing_htlc_id,
                    "Forward failed on peer or beyond"
                );

                db_client
                    .execute(
                        "UPDATE forward
                         SET finalized_at = $1, state = $2
                         WHERE node_id = $3
                           AND incoming_channel_id = $4
                           AND outgoing_channel_id = $5
                           AND incoming_htlc_id = $6
                           AND outgoing_htlc_id = $7",
                        &[
                            &created_at,
                            &-2i16,
                            &node_id,
                            &incoming_channel_id,
                            &outgoing_channel_id,
                            &incoming_htlc_id,
                            &outgoing_htlc_id,
                        ],
                    )
                    .await?;
            }
            routerrpc::htlc_event::Event::SettleEvent(_) => {
                debug!(
                    incoming_channel_id = event.incoming_channel_id,
                    outgoing_channel_id = event.outgoing_channel_id,
                    incoming_htlc_id = event.incoming_htlc_id,
                    outgoing_htlc_id = event.outgoing_htlc_id,
                    "Forward settled"
                );

                db_client
                    .execute(
                        "UPDATE forward
                         SET finalized_at = $1, state = $2
                         WHERE node_id = $3
                           AND incoming_channel_id = $4
                           AND outgoing_channel_id = $5
                           AND incoming_htlc_id = $6
                           AND outgoing_htlc_id = $7",
                        &[
                            &created_at,
                            &1i16,
                            &node_id,
                            &incoming_channel_id,
                            &outgoing_channel_id,
                            &incoming_htlc_id,
                            &outgoing_htlc_id,
                        ],
                    )
                    .await?;
            }
            routerrpc::htlc_event::Event::SubscribedEvent(_) => {}
            routerrpc::htlc_event::Event::FinalHtlcEvent(_) => {}
        }
    }

    Ok(())
}

async fn process_channel_graph_stream(
    mut client: lnrpc::lightning_client::LightningClient<
        tonic::service::interceptor::InterceptedService<
            tonic::transport::Channel,
            impl Fn(Request<()>) -> Result<Request<()>, tonic::Status> + Clone,
        >,
    >,
    db_client: Arc<tokio_postgres::Client>,
    cancellation_token: CancellationToken,
) -> Result<()> {
    let request = Request::new(lnrpc::GraphTopologySubscription {});
    let mut stream = client.subscribe_channel_graph(request).await?.into_inner();

    info!("Subscribed to channel graph updates");

    while let Some(update) = stream.next().await {
        if cancellation_token.is_cancelled() {
            info!("Channel graph stream cancelled, exiting gracefully");
            break;
        }

        match update {
            Ok(graph_update) => {
                if let Err(e) = process_channel_graph_update(&db_client, &graph_update).await {
                    error!(error = %e, "Error processing channel graph update");
                }
            }
            Err(err) => {
                error!(error = %err, "Error receiving channel graph update");
                return Err(err.into());
            }
        }
    }

    info!("Channel graph stream ended");

    Ok(())
}

async fn process_channel_graph_update(
    db_client: &tokio_postgres::Client,
    update: &lnrpc::GraphTopologyUpdate,
) -> Result<()> {
    if !update.channel_updates.is_empty() {
        let now = chrono::Utc::now().naive_utc();

        for channel_update in &update.channel_updates {
            if let Some(ref routing_policy) = channel_update.routing_policy {
                let node_pubkey = hex::decode(&channel_update.advertising_node)?;
                let channel_id = to_channel_id(channel_update.chan_id);

                let node_id = upsert_node(db_client, &node_pubkey).await?;

                debug!(
                    node_pubkey_hex = &channel_update.advertising_node,
                    channel_id = channel_update.chan_id,
                    "Update edge policy"
                );

                db_client
                    .execute(
                        "UPDATE edge_policy_history
                         SET valid_to = $1
                         WHERE node_id = $2 AND channel_id = $3 AND valid_to IS NULL",
                        &[&now, &node_id, &channel_id],
                    )
                    .await?;

                db_client
                    .execute(
                        "INSERT INTO edge_policy_history
                         (node_id, channel_id, valid_from, disabled,
                          time_lock_delta, min_htlc_msat, max_htlc_msat,
                          fee_base_msat, fee_rate_milli_msat,
                          inbound_fee_base_msat, inbound_fee_rate_milli_msat)
                         VALUES ($1, $2, $3, $4, $5, $6, $7, $8, $9, $10, $11)",
                        &[
                            &node_id,
                            &channel_id,
                            &now,
                            &routing_policy.disabled,
                            &(routing_policy.time_lock_delta as i32),
                            &(routing_policy.min_htlc * 1000),
                            &(routing_policy.max_htlc_msat as i64),
                            &routing_policy.fee_base_msat,
                            &routing_policy.fee_rate_milli_msat,
                            &(routing_policy.inbound_fee_base_msat as i64),
                            &(routing_policy.inbound_fee_rate_milli_msat as i64),
                        ],
                    )
                    .await?;
            }
        }
    }

    Ok(())
}

async fn sample_channel_liquidity_loop(
    mut client: lnrpc::lightning_client::LightningClient<
        tonic::service::interceptor::InterceptedService<
            tonic::transport::Channel,
            impl Fn(Request<()>) -> Result<Request<()>, tonic::Status> + Clone,
        >,
    >,
    db_client: Arc<tokio_postgres::Client>,
    node_id: i32,
    cancellation_token: CancellationToken,
) -> Result<()> {
    info!("Starting channel liquidity sampling (every 5 minutes)");

    if let Err(e) = sample_channel_liquidity(&mut client, &db_client, node_id).await {
        error!(error = %e, "Error sampling channel liquidity");
    }

    loop {
        tokio::select! {
            _ = tokio::time::sleep(tokio::time::Duration::from_secs(5 * 60)) => {
            }
            _ = cancellation_token.cancelled() => {
                info!("Channel liquidity sampling cancelled, exiting gracefully");
                break;
            }
        }

        if let Err(e) = sample_channel_liquidity(&mut client, &db_client, node_id).await {
            error!(error = %e, "Error sampling channel liquidity");
        }
    }

    Ok(())
}

async fn sample_channel_liquidity(
    client: &mut lnrpc::lightning_client::LightningClient<
        tonic::service::interceptor::InterceptedService<
            tonic::transport::Channel,
            impl Fn(Request<()>) -> Result<Request<()>, tonic::Status> + Clone,
        >,
    >,
    db_client: &tokio_postgres::Client,
    node_id: i32,
) -> Result<()> {
    let request = Request::new(lnrpc::ListChannelsRequest {
        active_only: false,
        inactive_only: false,
        public_only: false,
        private_only: false,
        peer: vec![],
        peer_alias_lookup: false,
    });

    let response = client.list_channels(request).await?;
    let channels = response.into_inner().channels;
    let channel_count = channels.len();

    let sampled_at = chrono::Utc::now().naive_utc();

    debug!(channel_count = channel_count, "Sampling channel liquidity");

    for channel in channels {
        let channel_id = to_channel_id(channel.chan_id);

        let outgoing_liquidity_msat = channel.local_balance * 1000;
        let incoming_liquidity_msat = channel.remote_balance * 1000;

        db_client
            .execute(
                "INSERT INTO channel_liquidity_sample
                 (node_id, channel_id, sampled_at,
                  outgoing_liquidity_msat, incoming_liquidity_msat)
                 VALUES ($1, $2, $3, $4, $5)",
                &[
                    &node_id,
                    &channel_id,
                    &sampled_at,
                    &outgoing_liquidity_msat,
                    &incoming_liquidity_msat,
                ],
            )
            .await?;
    }

    Ok(())
}
