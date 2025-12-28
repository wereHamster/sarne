use anyhow::Result;
use chrono::{DateTime, NaiveDateTime, Utc};
use clap::Parser;
use rand::seq::IndexedRandom;
use rand::{rng, Rng};
use sha2::Digest;
use std::collections::{HashMap, HashSet};
use std::process::exit;
use std::sync::Arc;
use tokio::signal::unix::{signal, SignalKind};
use tokio_util::sync::CancellationToken;
use tonic::Request;
use tracing::{error, info, warn};

use sarne::config::Config;
use sarne::{
    connect_to_lnd, create_lightning_clients, create_postgres_connection_pool,
    init_tracing_subscriber, lnrpc, routerrpc, upsert_node_tx, LndInterceptor,
};

#[derive(Parser, Debug)]
#[command(name = "sarne-radar")]
struct Args {
    /// Path to the configuration file
    #[arg(short, long, default_value = "./sarne.toml")]
    config: String,

    /// When non-zero, the radar will sleep that many seconds between probes.
    #[arg(short, long, default_value = "0")]
    delay: i32,
}

struct App {
    args: Args,

    postgres_connection_pool: deadpool_postgres::Pool,

    cancellation_token: CancellationToken,

    info: lnrpc::GetInfoResponse,

    nodes: Option<(DateTime<Utc>, Arc<Vec<lnrpc::LightningNode>>)>,
    channels: Option<(DateTime<Utc>, Arc<Vec<lnrpc::Channel>>)>,
}

#[tokio::main]
async fn main() -> Result<()> {
    init_tracing_subscriber();

    let args = Args::parse();

    info!("Loading config from {}", &args.config);

    let config = Config::from_file(&args.config)?;

    let (postgres_connection_pool, (lightning_client, router_client)) = tokio::try_join!(
        // PostgreSQL
        create_postgres_connection_pool(&config),
        // LND
        async {
            let (channel, macaroon) = connect_to_lnd(&config).await?;
            create_lightning_clients(channel, macaroon).await
        },
    )?;

    let info = get_node_info(lightning_client.clone()).await?;
    info!("Connected to LND node: {}", info.identity_pubkey);

    let cancellation_token = CancellationToken::new();

    let mut app = App {
        args,

        postgres_connection_pool,

        cancellation_token: cancellation_token.clone(),

        info: info.clone(),

        nodes: None,
        channels: None,
    };

    let mut thread =
        tokio::spawn(async move { radar_thread(&mut app, lightning_client, router_client).await });

    let mut sigterm = signal(SignalKind::terminate())?;
    let mut sigint = signal(SignalKind::interrupt())?;

    let _ = tokio::select! {
        r = &mut thread => {
            error!("Thread exited unexpectedly");

            match &r {
                Ok(inner) => {
                    match inner {
                        Ok(_) => {
                            error!("Ok");
                        }
                        Err(e) => {
                            error!("Err {}", e);
                        }
                    };
                }
                Err(e) => {
                    error!("Err {}", e);
                }
            };

            Some(r)
        }
        _ = sigterm.recv() => {
            info!("Received SIGTERM, shutting down gracefully");

            cancellation_token.cancel();

            let _ = tokio::join!(thread);

            None
        }
        _ = sigint.recv() => {
            info!("Received SIGINT, shutting down gracefully");

            cancellation_token.cancel();

            let _ = tokio::join!(thread);

            None
        }
    };

    Ok(())
}

async fn get_node_info(
    mut client: lnrpc::lightning_client::LightningClient<
        tonic::service::interceptor::InterceptedService<tonic::transport::Channel, LndInterceptor>,
    >,
) -> Result<lnrpc::GetInfoResponse> {
    let request = Request::new(lnrpc::GetInfoRequest {});
    let response = client.get_info(request).await?;
    Ok(response.into_inner())
}

async fn list_channels(
    mut client: lnrpc::lightning_client::LightningClient<
        tonic::service::interceptor::InterceptedService<tonic::transport::Channel, LndInterceptor>,
    >,
) -> Result<Vec<lnrpc::Channel>> {
    let request = Request::new(lnrpc::ListChannelsRequest {
        active_only: false,
        inactive_only: false,
        public_only: false,
        private_only: false,
        peer: vec![],
        peer_alias_lookup: false,
    });
    let response = client.list_channels(request).await?;
    Ok(response.into_inner().channels)
}

async fn get_channels(
    app: &mut App,
    lightning_client: lnrpc::lightning_client::LightningClient<
        tonic::service::interceptor::InterceptedService<tonic::transport::Channel, LndInterceptor>,
    >,
) -> Result<Arc<Vec<lnrpc::Channel>>> {
    match app.channels.clone() {
        None => {
            let channels = Arc::new(list_channels(lightning_client).await?);
            app.channels = Some((Utc::now(), channels.clone()));
            info!("Got {} channels", channels.len());
            Ok(channels.clone())
        }
        Some((updated_at, channels)) => {
            if Utc::now() - updated_at > chrono::Duration::hours(24) {
                let channels = Arc::new(list_channels(lightning_client).await?);
                app.channels = Some((Utc::now(), channels.clone()));
                info!("Got {} channels", channels.len());
                Ok(channels.clone())
            } else {
                Ok(channels.clone())
            }
        }
    }
}

async fn list_nodes(
    mut client: lnrpc::lightning_client::LightningClient<
        tonic::service::interceptor::InterceptedService<tonic::transport::Channel, LndInterceptor>,
    >,
) -> Result<Vec<lnrpc::LightningNode>> {
    let request = Request::new(lnrpc::ChannelGraphRequest {
        include_unannounced: false,
    });
    let channel_graph_response = client.describe_graph(request).await?;
    let channel_graph = channel_graph_response.into_inner();

    struct Stats {
        peers: HashSet<String>,
        channel_count: i32,
        last_update_time: DateTime<Utc>,
    }

    let mut stats: HashMap<String, Stats> = HashMap::new();

    for node in &channel_graph.nodes {
        stats.insert(
            node.pub_key.clone(),
            Stats {
                peers: HashSet::new(),
                channel_count: 0,
                last_update_time: DateTime::from_timestamp_nanos(0),
            },
        );
    }

    for edge in channel_graph.edges {
        if let Some(node1_stats) = stats.get_mut(&edge.node1_pub) {
            node1_stats.peers.insert(edge.node2_pub.clone());
            node1_stats.channel_count += 1;

            if let Some(policy) = edge.node1_policy {
                node1_stats.last_update_time =
                    DateTime::from_timestamp(policy.last_update as i64, 0).unwrap();
            }
        }

        if let Some(node2_stats) = stats.get_mut(&edge.node2_pub) {
            node2_stats.peers.insert(edge.node1_pub.clone());
            node2_stats.channel_count += 1;

            if let Some(policy) = edge.node2_policy {
                node2_stats.last_update_time =
                    DateTime::from_timestamp(policy.last_update as i64, 0).unwrap();
            }
        }
    }

    let now = Utc::now();
    let nodes: Vec<lnrpc::LightningNode> = channel_graph
        .nodes
        .into_iter()
        .filter(|node| {
            let stats = stats.get(&node.pub_key);
            match stats {
                None => false,
                Some(stats) => {
                    stats.channel_count > 3
                        && now - stats.last_update_time < chrono::Duration::days(7)
                }
            }
        })
        .collect();

    Ok(nodes)
}

async fn get_nodes(
    app: &mut App,
    lightning_client: lnrpc::lightning_client::LightningClient<
        tonic::service::interceptor::InterceptedService<tonic::transport::Channel, LndInterceptor>,
    >,
) -> Result<Arc<Vec<lnrpc::LightningNode>>> {
    match app.nodes.clone() {
        None => {
            let nodes = Arc::new(list_nodes(lightning_client).await?);
            app.nodes = Some((Utc::now(), nodes.clone()));
            info!("Got {} nodes", nodes.len());
            Ok(nodes.clone())
        }
        Some((updated_at, nodes)) => {
            if Utc::now() - updated_at > chrono::Duration::hours(24) {
                let nodes = Arc::new(list_nodes(lightning_client).await?);
                app.nodes = Some((Utc::now(), nodes.clone()));
                info!("Got {} nodes", nodes.len());
                Ok(nodes.clone())
            } else {
                Ok(nodes.clone())
            }
        }
    }
}

struct PaymentProbe {
    created_at: NaiveDateTime,

    src_node_pubkey: String,
    channel: Option<lnrpc::Channel>,
    dst_node_pubkey: String,

    amount_msat: u64,

    attempts: Vec<PaymentProbeAttempt>,
}

struct PaymentProbeAttempt {
    seqno: i16,

    created_at: NaiveDateTime,

    route: lnrpc::Route,

    latency_ns: i64,

    failure: Option<lnrpc::Failure>,
}

async fn radar_thread(
    app: &mut App,
    lightning_client: lnrpc::lightning_client::LightningClient<
        tonic::service::interceptor::InterceptedService<tonic::transport::Channel, LndInterceptor>,
    >,
    router_client: routerrpc::router_client::RouterClient<
        tonic::service::interceptor::InterceptedService<tonic::transport::Channel, LndInterceptor>,
    >,
) -> Result<()> {
    let source_pub_key = app.info.identity_pubkey.clone();

    loop {
        if app.cancellation_token.is_cancelled() {
            info!("Thread cancelled, exiting gracefully");
            break;
        }

        let nodes = get_nodes(app, lightning_client.clone()).await?;
        let random_node = match nodes.choose(&mut rng()) {
            Some(node) => node,
            None => {
                info!("No nodes in channel graph");
                exit(0);
            }
        };

        // If we picked our own node, skip.
        let destination_pub_key = random_node.pub_key.clone();
        if destination_pub_key == source_pub_key {
            continue;
        }

        // For each destination, we run two probes:
        //
        //  - Probe A: We let LND pick the best route.
        //  - Probe B: We pick a random outgoing channel to route through.
        //
        // The first probe gives us the best (lowest fee) route to the target. The second probe
        // forces LND to pick an alternative route, giving us a more diverse view on the network.
        //
        // There is a chance that the random channel we select for probe B will be the same as
        // the one used by probe A. We don't bother to avoid that. Given that the two probes
        // use different amounts, it's possible that probe B is forced to use a different route.
        //
        // Probes always skip direct channels with the destination. The fee would always be zero
        // (since the local node doesn't charge for outgoing payments). The only useful signal we
        // would get from these probes is latency.

        execute_payment_probe(
            app,
            lightning_client.clone(),
            router_client.clone(),
            nodes.clone(),
            random_node,
            PaymentProbe {
                created_at: Utc::now().naive_utc(),

                src_node_pubkey: source_pub_key.clone(),
                channel: None,
                dst_node_pubkey: destination_pub_key.clone(),

                amount_msat: randomize_amount(200_000, 0.35) * 1000,

                attempts: Vec::new(),
            },
        )
        .await?;

        let channels = get_channels(app, lightning_client.clone()).await?;
        let random_channel = match channels.choose(&mut rng()) {
            Some(channel) => channel,
            None => {
                info!("No channels.");
                exit(0);
            }
        };

        if random_channel.remote_pubkey == destination_pub_key {
            continue;
        }

        execute_payment_probe(
            app,
            lightning_client.clone(),
            router_client.clone(),
            nodes.clone(),
            random_node,
            PaymentProbe {
                created_at: Utc::now().naive_utc(),

                src_node_pubkey: source_pub_key.clone(),
                channel: Some(random_channel.clone()),
                dst_node_pubkey: destination_pub_key.clone(),

                amount_msat: randomize_amount(200_000, 0.35) * 1000,

                attempts: Vec::new(),
            },
        )
        .await?;
    }

    Ok(())
}

async fn execute_payment_probe(
    app: &mut App,
    lightning_client: lnrpc::lightning_client::LightningClient<
        tonic::service::interceptor::InterceptedService<tonic::transport::Channel, LndInterceptor>,
    >,
    router_client: routerrpc::router_client::RouterClient<
        tonic::service::interceptor::InterceptedService<tonic::transport::Channel, LndInterceptor>,
    >,
    nodes: Arc<Vec<lnrpc::LightningNode>>,
    random_node: &lnrpc::LightningNode,
    payment_probe: PaymentProbe,
) -> Result<()> {
    let mut payment_probe = payment_probe;

    if app.cancellation_token.is_cancelled() {
        return Ok(());
    }

    let result = run_payment_probe(
        app,
        lightning_client.clone(),
        router_client.clone(),
        &mut payment_probe,
    )
    .await;

    match result {
        Ok(()) => {
            if !payment_probe.attempts.is_empty() {
                let payment_probe_id = save_payment_probe(app, &payment_probe).await;

                match payment_probe_id {
                    Ok(payment_probe_id) => {
                        print_payment_probe(nodes, &payment_probe, random_node, payment_probe_id);
                    }
                    Err(err) => {
                        warn!("Failed to save payment probe: {}", err);
                    }
                }

                if app.args.delay > 0 {
                    info!("Delay for {} seconds", app.args.delay);

                    tokio::select! {
                        _ = tokio::time::sleep(std::time::Duration::from_secs(app.args.delay as u64)) => (),
                        _ = app.cancellation_token.cancelled() => (),
                    };
                }
            }
        }
        Err(e) => {
            info!("run_payment_probe failed {}", e);
        }
    };

    Ok(())
}

async fn run_payment_probe(
    app: &mut App,
    mut lightning_client: lnrpc::lightning_client::LightningClient<
        tonic::service::interceptor::InterceptedService<tonic::transport::Channel, LndInterceptor>,
    >,
    mut router_client: routerrpc::router_client::RouterClient<
        tonic::service::interceptor::InterceptedService<tonic::transport::Channel, LndInterceptor>,
    >,
    payment_probe: &mut PaymentProbe,
) -> Result<()> {
    let src_node_bytes = hex::decode(&payment_probe.src_node_pubkey)?;
    let dst_node_bytes = hex::decode(&payment_probe.dst_node_pubkey)?;

    for attempt in 0..3 {
        if app.cancellation_token.is_cancelled() {
            info!("Thread cancelled, exiting gracefully");
            break;
        }

        let query_routes_req = Request::new(lnrpc::QueryRoutesRequest {
            source_pub_key: payment_probe.src_node_pubkey.clone(),
            pub_key: payment_probe.dst_node_pubkey.clone(),
            amt: 0,
            amt_msat: payment_probe.amount_msat as i64,
            ignored_nodes: [].to_vec(),
            #[allow(deprecated)]
            ignored_edges: [].to_vec(),

            // Skip direct channels between source and destination.
            ignored_pairs: [lnrpc::NodePair {
                from: src_node_bytes.clone(),
                to: dst_node_bytes.clone(),
            }]
            .to_vec(),

            cltv_limit: 0,
            dest_custom_records: HashMap::new(),
            outgoing_chan_id: payment_probe.channel.as_ref().map_or(0, |c| c.chan_id),
            last_hop_pubkey: [].to_vec(),
            route_hints: [].to_vec(),
            blinded_payment_paths: [].to_vec(),
            dest_features: [].to_vec(),
            time_pref: 0.0,
            final_cltv_delta: 144,
            fee_limit: Some(lnrpc::FeeLimit {
                limit: Some(lnrpc::fee_limit::Limit::Fixed(100_000)),
            }),
            use_mission_control: true,
        });
        let query_routes_res_result = lightning_client.query_routes(query_routes_req).await;
        let query_routes_res = match query_routes_res_result {
            Ok(res) => res,
            Err(_) => {
                return Ok(());
            }
        };

        let query_routes = query_routes_res.into_inner();
        let route = match query_routes.routes.first() {
            Some(route) => route,
            None => {
                warn!("QueryRoutes returned empty response");
                return Ok(());
            }
        };

        let preimage: [u8; 32] = rng().random();
        let payment_hash = sha2::Sha256::digest(preimage).to_vec();

        if app.cancellation_token.is_cancelled() {
            info!("Thread cancelled, exiting gracefully");
            break;
        }

        let send_to_route_req = Request::new(routerrpc::SendToRouteRequest {
            payment_hash,
            route: Some(route.clone()),
            skip_temp_err: false,
            first_hop_custom_records: HashMap::new(),
        });

        let send_to_route_res = match tokio::time::timeout(
            std::time::Duration::from_secs(60),
            router_client.send_to_route_v2(send_to_route_req),
        )
        .await
        {
            Ok(res) => res?,
            Err(_) => return Ok(()),
        };

        let res = send_to_route_res.into_inner();

        let latency_ns = res.resolve_time_ns - res.attempt_time_ns;

        match res.failure {
            None => {
                error!("Payment Probe unexpectedly succeeded");
                exit(0);
            }
            Some(failure) => match failure.code() {
                lnrpc::failure::FailureCode::IncorrectOrUnknownPaymentDetails => {
                    let payment_probe_attempt = PaymentProbeAttempt {
                        seqno: attempt,

                        created_at: Utc::now().naive_utc(),

                        route: route.clone(),

                        latency_ns,
                        failure: None,
                    };

                    payment_probe.attempts.push(payment_probe_attempt);

                    return Ok(());
                }
                _ => {
                    let payment_probe_attempt = PaymentProbeAttempt {
                        seqno: attempt,

                        created_at: Utc::now().naive_utc(),

                        route: route.clone(),

                        latency_ns,
                        failure: Some(failure),
                    };

                    payment_probe.attempts.push(payment_probe_attempt);

                    continue;
                }
            },
        }
    }

    Ok(())
}

fn randomize_amount(base_amount_sat: u64, variation: f64) -> u64 {
    let mut rng = rand::rng();

    let base_amount_f64 = base_amount_sat as f64;
    let min = base_amount_f64 * (1.0 - variation);
    let max = base_amount_f64 * (1.0 + variation);

    rng.random_range(min..max).round() as u64
}

async fn save_payment_probe(app: &App, payment_probe: &PaymentProbe) -> Result<i64> {
    let mut db_client = app.postgres_connection_pool.get().await?;
    let transaction = db_client.transaction().await?;

    let payment_probe_id = create_payment_probe(&transaction, payment_probe).await?;

    for attempt in &payment_probe.attempts {
        create_payment_probe_attempt(&transaction, payment_probe_id, attempt).await?;
    }

    transaction.commit().await?;

    Ok(payment_probe_id)
}

async fn create_payment_probe(
    transaction: &tokio_postgres::Transaction<'_>,
    payment_probe: &PaymentProbe,
) -> Result<i64> {
    let src_node_bytes = hex::decode(&payment_probe.src_node_pubkey)?;
    let dst_node_bytes = hex::decode(&payment_probe.dst_node_pubkey)?;

    let src_node_id = upsert_node_tx(transaction, &src_node_bytes).await?;
    let dst_node_id = upsert_node_tx(transaction, &dst_node_bytes).await?;

    let outgoing_chan_id_i64: Option<i64> =
        payment_probe.channel.as_ref().map(|c| c.chan_id as i64);
    let amount_msat = payment_probe.amount_msat as i64;

    let row = transaction
        .query_one(
            "
            INSERT INTO payment_probe
                ( created_at
                , src_node_id
                , outgoing_short_channel_id
                , dst_node_id
                , amount_msat
                )
            VALUES
                ( $1
                , $2
                , $3
                , $4
                , $5
                )
            RETURNING id;
            ",
            &[
                &payment_probe.created_at,
                &src_node_id,
                &outgoing_chan_id_i64,
                &dst_node_id,
                &amount_msat,
            ],
        )
        .await?;

    let payment_probe_id: i64 = row.get(0);
    Ok(payment_probe_id)
}

async fn create_payment_probe_attempt(
    transaction: &tokio_postgres::Transaction<'_>,
    payment_probe_id: i64,
    attempt: &PaymentProbeAttempt,
) -> Result<()> {
    let node_path_hash = {
        let mut hasher = sha2::Sha256::new();
        for hop in &attempt.route.hops {
            hasher.update(hex::decode(&hop.pub_key)?);
        }
        hasher.finalize().to_vec()
    };

    let channel_path_hash = {
        let mut hasher = sha2::Sha256::new();
        for hop in &attempt.route.hops {
            hasher.update(hop.chan_id.to_be_bytes());
        }
        hasher.finalize().to_vec()
    };

    transaction
        .execute(
            "
            INSERT INTO payment_probe_attempt
                ( payment_probe_id
                , attempt_seqno
                , created_at
                , node_path_hash
                , channel_path_hash
                , fee_msat
                , latency_ns
                )
            VALUES
                ( $1, $2, $3, $4, $5, $6, $7 )
            ",
            &[
                &payment_probe_id,
                &attempt.seqno,
                &attempt.created_at,
                &node_path_hash,
                &channel_path_hash,
                &attempt.route.total_fees_msat,
                &attempt.latency_ns,
            ],
        )
        .await?;

    for (seqno, hop) in attempt.route.hops.iter().enumerate() {
        let node_bytes = hex::decode(&hop.pub_key)?;
        let node_id = upsert_node_tx(transaction, &node_bytes).await?;

        transaction
            .execute(
                "
                INSERT INTO payment_probe_hop
                    ( payment_probe_id
                    , attempt_seqno
                    , hop_seqno
                    , short_channel_id
                    , node_id
                    , fee_msat
                    )
                VALUES
                    ( $1, $2, $3, $4, $5, $6 )
                ",
                &[
                    &payment_probe_id,
                    &attempt.seqno,
                    &(seqno as i16),
                    &(hop.chan_id as i64),
                    &node_id,
                    &hop.fee_msat,
                ],
            )
            .await?;
    }

    if let Some(failure) = &attempt.failure {
        transaction
            .execute(
                "
                INSERT INTO payment_probe_failure
                    ( payment_probe_id
                    , attempt_seqno
                    , hop_seqno
                    , failure_code
                    )
                VALUES
                    ( $1, $2, $3, $4 )
                ",
                &[
                    &payment_probe_id,
                    &attempt.seqno,
                    &(failure.failure_source_index as i16),
                    &failure.code,
                ],
            )
            .await?;
    }

    Ok(())
}

fn print_payment_probe(
    nodes: Arc<Vec<lnrpc::LightningNode>>,
    payment_probe: &PaymentProbe,
    dst_node: &lnrpc::LightningNode,
    payment_probe_id: i64,
) {
    info!("PROBE {}", payment_probe_id);
    info!(
        "    Destination Peer:    {} ({})",
        dst_node.alias, dst_node.pub_key
    );

    if let Some(channel) = &payment_probe.channel {
        let outgoing_channel_peer_alias = nodes
            .iter()
            .find(|n| n.pub_key == channel.remote_pubkey)
            .map(|n| n.alias.as_str())
            .unwrap_or(&channel.remote_pubkey);

        info!(
            "    Outgoing Channel:    {} [{}]",
            channel.chan_id, outgoing_channel_peer_alias
        );
    }

    info!(
        "    Amount:              {} sat",
        payment_probe.amount_msat / 1000
    );

    let payment_probe_attempts: Vec<String> = payment_probe
        .attempts
        .iter()
        .map(|attempt| {
            let time = (attempt.latency_ns as f64) / 1_000_000_000.0;

            match &attempt.failure {
                None => {
                    format!("[{:.3}s]", time)
                }
                Some(failure) => {
                    format!(
                        "[{:.3}s; code:{}; hop:{}/{}]",
                        time,
                        failure.code,
                        failure.failure_source_index,
                        attempt.route.hops.len(),
                    )
                }
            }
        })
        .collect();

    info!(
        "    Result:              {}",
        if payment_probe
            .attempts
            .last()
            .map_or(false, |attempt| attempt.failure.is_none())
        {
            String::from("SUCCESS")
        } else {
            String::from("FAILURE")
        },
    );

    info!(
        "    Attempts:            {} {}",
        payment_probe.attempts.len(),
        payment_probe_attempts.join(" ")
    );
}
