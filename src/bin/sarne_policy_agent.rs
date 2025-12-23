use anyhow::Result;
use chrono::{DateTime, Duration, NaiveDateTime, Utc};
use clap::Parser;
use futures::future::join_all;
use tonic::Request;
use tracing::{debug, info};

use sarne::config::Config;
use sarne::{
    connect_to_lnd, count_forwards, create_lightning_clients, create_postgres_connection_pool,
    get_chan_info, get_channel_balance_info, init_tracing_subscriber, lnrpc,
};

#[derive(Parser, Debug)]
#[command(name = "sarne-policy-agent")]
struct Args {
    /// Path to the configuration file
    #[arg(short, long, default_value = "./sarne.toml")]
    config: String,

    /// Print what would be done without actually updating policies
    #[arg(short, long, default_value_t = false)]
    dry_run: bool,

    /// Apply all policy changes instead of just the first one
    #[arg(short, long, default_value_t = false)]
    all: bool,
}

/// Time between policy changes on a particular channel (and direction).
///
/// We avoid changing policy too frequently, to give the network time to
/// propagate the changes and get an accurate signal of its effects.
const CHANNEL_POLICY_CHANGE_COOLDOWN: Duration = Duration::hours(4);

/// The forwarding history in this window will be used to determine the
/// channel policy action.
///
/// The duration is chosen to be long enough to give a meaningful picture
/// but short enough to not be too stale.
const FORWARDING_HISTORY_WINDOW: Duration = Duration::hours(48);

#[derive(Debug, Clone)]
struct ChannelPolicy {
    chan_point: ChanPoint,
    base_fee_msat: i64,
    fee_rate_ppm: i64,
    time_lock_delta: u32,
    max_htlc_msat: u64,
    inbound_fee_base_msat: i32,
    inbound_fee_rate_ppm: i32,
}

#[derive(Debug, Clone)]
struct ChanPoint {
    funding_txid_str: String,
    output_index: u32,
}

#[derive(Debug)]
struct ChannelPolicyChange {
    apply_at: DateTime<Utc>,
    channel: lnrpc::Channel,
    action: String,
    reason: String,

    old_policy: ChannelPolicy,
    new_policy: ChannelPolicy,
}

#[tokio::main]
async fn main() -> Result<()> {
    init_tracing_subscriber();

    let args = Args::parse();

    info!("Loading config from {}", &args.config);

    let config = Config::from_file(&args.config)?;

    let (pgp, (lightning_client, _router_client)) = tokio::try_join!(
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

    let channels = list_channels(lightning_client.clone()).await?;
    info!("Found {} channels", channels.len());

    let channel_policy_changes =
        collect_channel_policy_changes(&pgp, lightning_client.clone(), &info, channels).await?;

    let now = Utc::now();
    let mut ready_to_apply_channel_policy_changes: Vec<_> = channel_policy_changes
        .iter()
        .filter(|pc| pc.apply_at < now)
        .collect();

    if ready_to_apply_channel_policy_changes.is_empty() {
        if let Some(policy_change) = channel_policy_changes.first() {
            info!(
                "No policy changes to apply (next at {})",
                policy_change.apply_at.to_rfc3339()
            );
        } else {
            info!("No policy changes needed");
        }

        return Ok(());
    }

    ready_to_apply_channel_policy_changes.sort_by(|a, b| a.apply_at.cmp(&b.apply_at));
    let to_apply_channel_policy_changes = if args.all {
        ready_to_apply_channel_policy_changes
    } else {
        ready_to_apply_channel_policy_changes
            .into_iter()
            .take(1)
            .collect()
    };

    info!(
        "Applying {} policy change{}",
        to_apply_channel_policy_changes.len(),
        if to_apply_channel_policy_changes.len() > 1 {
            "s"
        } else {
            ""
        }
    );

    for channel_policy_change in to_apply_channel_policy_changes {
        apply_channel_policy_change(&args, lightning_client.clone(), channel_policy_change).await?;
    }

    Ok(())
}

async fn apply_channel_policy_change(
    args: &Args,
    client: lnrpc::lightning_client::LightningClient<
        tonic::service::interceptor::InterceptedService<
            tonic::transport::Channel,
            impl Fn(Request<()>) -> Result<Request<()>, tonic::Status> + Clone,
        >,
    >,
    channel_policy_change: &ChannelPolicyChange,
) -> Result<()> {
    let alias = get_node_alias(client.clone(), &channel_policy_change.channel.remote_pubkey).await;

    let channel = &channel_policy_change.channel;

    let old_policy = &channel_policy_change.old_policy;
    let new_policy = &channel_policy_change.new_policy;

    info!(
        "Applying channel policy change on {} ({})",
        channel.chan_id, alias
    );
    info!("Action: {}", channel_policy_change.action);
    info!("Reason: {}", channel_policy_change.reason);
    info!("Policy Changes:");

    let outgoing_fee_rate_diff = new_policy.fee_rate_ppm - old_policy.fee_rate_ppm;
    info!(
        "    Outgoing Fee Rate: {:>5} -> {:>5} ppm   ({:+})",
        old_policy.fee_rate_ppm, new_policy.fee_rate_ppm, outgoing_fee_rate_diff
    );

    let incoming_fee_rate_diff = new_policy.inbound_fee_rate_ppm - old_policy.inbound_fee_rate_ppm;
    info!(
        "    Incoming Fee Rate: {:>5} -> {:>5} ppm   ({:+})",
        old_policy.inbound_fee_rate_ppm, new_policy.inbound_fee_rate_ppm, incoming_fee_rate_diff
    );

    if !args.dry_run {
        update_channel_policy(client.clone(), new_policy).await?;
    }

    Ok(())
}

async fn get_node_info(
    mut client: lnrpc::lightning_client::LightningClient<
        tonic::service::interceptor::InterceptedService<
            tonic::transport::Channel,
            impl Fn(Request<()>) -> Result<Request<()>, tonic::Status> + Clone,
        >,
    >,
) -> Result<lnrpc::GetInfoResponse> {
    let request = Request::new(lnrpc::GetInfoRequest {});
    let response = client.get_info(request).await?;
    Ok(response.into_inner())
}

async fn list_channels(
    mut client: lnrpc::lightning_client::LightningClient<
        tonic::service::interceptor::InterceptedService<
            tonic::transport::Channel,
            impl Fn(Request<()>) -> Result<Request<()>, tonic::Status> + Clone,
        >,
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

// Helper function to convert channel ID to bigint (i64)
// The channel ID is not expected to be larger than signed 64bit can hold for the next 300+ years
pub fn to_channel_id(channel_id: u64) -> i64 {
    channel_id as i64
}

async fn collect_channel_policy_changes(
    pgp: &deadpool_postgres::Pool,
    client: lnrpc::lightning_client::LightningClient<
        tonic::service::interceptor::InterceptedService<
            tonic::transport::Channel,
            impl Fn(Request<()>) -> Result<Request<()>, tonic::Status> + Clone,
        >,
    >,
    info: &lnrpc::GetInfoResponse,
    channels: Vec<lnrpc::Channel>,
) -> Result<Vec<ChannelPolicyChange>> {
    let tasks: Vec<_> = channels
        .into_iter()
        .map(|channel| process_channel(pgp, client.clone(), info, channel))
        .collect();

    let channel_policy_change_results = join_all(tasks).await;
    let channel_policy_changes: Vec<ChannelPolicyChange> = channel_policy_change_results
        .into_iter()
        .filter_map(|result| result.ok())
        .flatten()
        .collect();

    Ok(channel_policy_changes)
}

fn adjust_outgoing_fee_rate(fee_rate: i64, change: f64) -> i64 {
    let jitter = ((rand::random::<f64>() * 5.0) + 5.0) * change.signum();
    let new_fee_rate = ((fee_rate as f64 * (1.0 + (change / 100.0))) + jitter).floor() as i64;

    std::cmp::max(0, new_fee_rate)
}

fn adjust_incoming_fee_rate(fee_rate: i32, change: f64) -> i32 {
    let jitter = ((rand::random::<f64>() * 5.0) + 5.0) * -change.signum();
    let new_fee_rate = ((fee_rate as f64 * (1.0 + (change / 100.0))) + jitter).ceil() as i32;

    std::cmp::min(0, new_fee_rate)
}

async fn process_channel(
    pgp: &deadpool_postgres::Pool,
    client: lnrpc::lightning_client::LightningClient<
        tonic::service::interceptor::InterceptedService<
            tonic::transport::Channel,
            impl Fn(Request<()>) -> Result<Request<()>, tonic::Status> + Clone,
        >,
    >,
    info: &lnrpc::GetInfoResponse,
    channel: lnrpc::Channel,
) -> Result<Vec<ChannelPolicyChange>> {
    let db_client = pgp.get().await?;

    let chan_info = match get_chan_info(client.clone(), channel.chan_id).await {
        Ok(info) => info,
        Err(e) => {
            debug!("Failed to get channel info for {}: {}", channel.chan_id, e);
            return Ok(Vec::new());
        }
    };

    let local_chan_policy = if chan_info.node1_pub == info.identity_pubkey {
        chan_info.node1_policy.as_ref()
    } else if chan_info.node2_pub == info.identity_pubkey {
        chan_info.node2_policy.as_ref()
    } else {
        return Ok(Vec::new());
    };

    let local_policy = match local_chan_policy {
        Some(p) => p,
        None => return Ok(Vec::new()),
    };

    let mut channel_policy_changes = Vec::new();
    let channel_id = to_channel_id(channel.chan_id);

    let now = Utc::now();
    let after = now - FORWARDING_HISTORY_WINDOW;

    let incoming_forwards_count = count_forwards(&db_client, Some(channel_id), None, after).await?;
    let outgoing_forwards_count = count_forwards(&db_client, None, Some(channel_id), after).await?;

    let channel_balance_info =
        get_channel_balance_info(client.clone(), &db_client, info, &channel, &chan_info).await?;

    let chan_point_parts: Vec<&str> = chan_info.chan_point.split(':').collect();
    let policy = ChannelPolicy {
        chan_point: ChanPoint {
            funding_txid_str: chan_point_parts[0].to_string(),
            output_index: chan_point_parts[1].parse().unwrap_or(0),
        },
        base_fee_msat: local_policy.fee_base_msat,
        fee_rate_ppm: local_policy.fee_rate_milli_msat,
        time_lock_delta: local_policy.time_lock_delta,
        max_htlc_msat: local_policy.max_htlc_msat,
        inbound_fee_base_msat: local_policy.inbound_fee_base_msat,
        inbound_fee_rate_ppm: local_policy.inbound_fee_rate_milli_msat,
    };

    let local_balance = channel.local_balance;
    let remote_balance = channel.remote_balance;
    let total_balance = local_balance + remote_balance;

    let has_outbound_liquidity = has_outbound_liquidity(local_balance, total_balance);
    let has_inbound_liquidity = has_inbound_liquidity(remote_balance, total_balance);

    let last_outbound_policy = get_last_edge_policy_change(
        &db_client,
        &info.identity_pubkey,
        channel_id,
        "fee_rate_milli_msat",
    )
    .await?;

    let last_inbound_policy = get_last_edge_policy_change(
        &db_client,
        &info.identity_pubkey,
        channel_id,
        "inbound_fee_rate_milli_msat",
    )
    .await?;

    // Outgoing Fee Rate
    {
        if !has_outbound_liquidity {
            let outgoing_forwards_count = outgoing_forwards_count.total_forwards_count;

            let outgoing_forwards_lo_water_mark = 24;
            let outgoing_forwards_hi_water_mark = 30;

            if outgoing_forwards_count > outgoing_forwards_hi_water_mark {
                let new_fee_rate = adjust_outgoing_fee_rate(policy.fee_rate_ppm, 1.0);
                if new_fee_rate != policy.fee_rate_ppm {
                    channel_policy_changes.push(ChannelPolicyChange {
                        apply_at: add_time_to_policy_change(
                            last_outbound_policy.as_ref(),
                            CHANNEL_POLICY_CHANGE_COOLDOWN,
                        ),
                        channel: channel.clone(),
                        action: "Increase Outgoing Fee Rate by 1%".to_string(),
                        reason: format!(
                            "{}; Outgoing Forward Count {} > {} (window: {})",
                            channel_balance_info.channel_balance_category,
                            outgoing_forwards_count,
                            outgoing_forwards_hi_water_mark,
                            CHANNEL_POLICY_CHANGE_COOLDOWN
                        ),
                        old_policy: policy.clone(),
                        new_policy: ChannelPolicy {
                            fee_rate_ppm: new_fee_rate,
                            ..policy.clone()
                        },
                    });
                }
            } else if outgoing_forwards_count < outgoing_forwards_lo_water_mark {
                let new_fee_rate = adjust_outgoing_fee_rate(policy.fee_rate_ppm, -1.0);
                if new_fee_rate != policy.fee_rate_ppm {
                    let new_inbound_fee_rate = std::cmp::max(
                        (-(new_fee_rate as f64 * 0.95)).ceil() as i32,
                        policy.inbound_fee_rate_ppm,
                    );

                    channel_policy_changes.push(ChannelPolicyChange {
                        apply_at: add_time_to_policy_change(
                            last_outbound_policy.as_ref(),
                            CHANNEL_POLICY_CHANGE_COOLDOWN,
                        ),
                        channel: channel.clone(),
                        action: "Decrease Outgoing Fee Rate by 1%".to_string(),
                        reason: format!(
                            "{}; Outgoing Forward Count {} < {} (window: {})",
                            channel_balance_info.channel_balance_category,
                            outgoing_forwards_count,
                            outgoing_forwards_lo_water_mark,
                            CHANNEL_POLICY_CHANGE_COOLDOWN
                        ),
                        old_policy: policy.clone(),
                        new_policy: ChannelPolicy {
                            fee_rate_ppm: new_fee_rate,
                            inbound_fee_rate_ppm: new_inbound_fee_rate,
                            ..policy.clone()
                        },
                    });
                }
            }
        } else {
            let successful_outgoing_forward_count =
                outgoing_forwards_count.successful_forwards_count;

            let outgoing_forwards_lo_water_mark = 2;
            let outgoing_forwards_hi_water_mark = 20;

            if successful_outgoing_forward_count > outgoing_forwards_hi_water_mark {
                let new_fee_rate = adjust_outgoing_fee_rate(policy.fee_rate_ppm, 2.0);
                if new_fee_rate != policy.fee_rate_ppm {
                    channel_policy_changes.push(ChannelPolicyChange {
                        apply_at: add_time_to_policy_change(
                            last_outbound_policy.as_ref(),
                            CHANNEL_POLICY_CHANGE_COOLDOWN,
                        ),
                        channel: channel.clone(),
                        action: "Increase Outgoing Fee Rate by 2%".to_string(),
                        reason: format!(
                            "{}; Outgoing Successful Forward Count {} > {} (window: {})",
                            channel_balance_info.channel_balance_category,
                            successful_outgoing_forward_count,
                            outgoing_forwards_hi_water_mark,
                            FORWARDING_HISTORY_WINDOW
                        ),
                        old_policy: policy.clone(),
                        new_policy: ChannelPolicy {
                            fee_rate_ppm: new_fee_rate,
                            ..policy.clone()
                        },
                    });
                }
            } else if successful_outgoing_forward_count < outgoing_forwards_lo_water_mark {
                let new_fee_rate = adjust_outgoing_fee_rate(policy.fee_rate_ppm, -1.0);
                if new_fee_rate != policy.fee_rate_ppm {
                    let new_inbound_fee_rate = std::cmp::max(
                        (-(new_fee_rate as f64 * 0.95)).ceil() as i32,
                        policy.inbound_fee_rate_ppm,
                    );

                    channel_policy_changes.push(ChannelPolicyChange {
                        apply_at: add_time_to_policy_change(
                            last_outbound_policy.as_ref(),
                            CHANNEL_POLICY_CHANGE_COOLDOWN,
                        ),
                        channel: channel.clone(),
                        action: "Decrease Outgoing Fee Rate by 1%".to_string(),
                        reason: format!(
                            "{}; Outgoing Successful Forward Count {} < {} (window: {})",
                            channel_balance_info.channel_balance_category,
                            successful_outgoing_forward_count,
                            outgoing_forwards_lo_water_mark,
                            FORWARDING_HISTORY_WINDOW
                        ),
                        old_policy: policy.clone(),
                        new_policy: ChannelPolicy {
                            fee_rate_ppm: new_fee_rate,
                            inbound_fee_rate_ppm: new_inbound_fee_rate,
                            ..policy.clone()
                        },
                    });
                }
            }
        }
    }

    // Incoming Fee Rate
    {
        if !has_outbound_liquidity {
            let new_inbound_fee_rate = std::cmp::max(
                (-(policy.fee_rate_ppm as f64 * 0.95)).ceil() as i32,
                adjust_incoming_fee_rate(policy.inbound_fee_rate_ppm, 5.0),
            );
            if new_inbound_fee_rate != policy.inbound_fee_rate_ppm {
                channel_policy_changes.push(ChannelPolicyChange {
                    apply_at: add_time_to_policy_change(
                        last_inbound_policy.as_ref(),
                        CHANNEL_POLICY_CHANGE_COOLDOWN,
                    ),
                    channel: channel.clone(),
                    action: "Increase Incoming Fee Rate Discount by 5%".to_string(),
                    reason: format!("{}; ", channel_balance_info.channel_balance_category),
                    old_policy: policy.clone(),
                    new_policy: ChannelPolicy {
                        inbound_fee_rate_ppm: new_inbound_fee_rate,
                        ..policy.clone()
                    },
                });
            }
        } else {
            let successful_incoming_forward_count =
                incoming_forwards_count.successful_forwards_count;

            let incoming_forwards_lo_water_mark = 2;
            let incoming_forwards_hi_water_mark = 20;

            if successful_incoming_forward_count > incoming_forwards_hi_water_mark {
                let new_inbound_fee_rate =
                    adjust_incoming_fee_rate(policy.inbound_fee_rate_ppm, -15.0);
                if new_inbound_fee_rate != policy.inbound_fee_rate_ppm {
                    channel_policy_changes.push(ChannelPolicyChange {
                        apply_at: add_time_to_policy_change(
                            last_inbound_policy.as_ref(),
                            CHANNEL_POLICY_CHANGE_COOLDOWN,
                        ),
                        channel: channel.clone(),
                        action: "Decrease Incoming Fee Rate Discount by 15%".to_string(),
                        reason: format!(
                            "{}; Incoming Successful Forward Count {} > {} (window: {})",
                            channel_balance_info.channel_balance_category,
                            successful_incoming_forward_count,
                            incoming_forwards_hi_water_mark,
                            FORWARDING_HISTORY_WINDOW
                        ),
                        old_policy: policy.clone(),
                        new_policy: ChannelPolicy {
                            inbound_fee_rate_ppm: new_inbound_fee_rate,
                            ..policy.clone()
                        },
                    });
                }
            } else if successful_incoming_forward_count < incoming_forwards_lo_water_mark {
                if !has_inbound_liquidity {
                    if policy.inbound_fee_rate_ppm != 0 {
                        channel_policy_changes.push(ChannelPolicyChange {
                            apply_at: add_time_to_policy_change(
                                last_inbound_policy.as_ref(),
                                CHANNEL_POLICY_CHANGE_COOLDOWN,
                            ),
                            channel: channel.clone(),
                            action: "Set Incoming Fee Rate Discount to 0".to_string(),
                            reason: format!("{}", channel_balance_info.channel_balance_category),
                            old_policy: policy.clone(),
                            new_policy: ChannelPolicy {
                                inbound_fee_rate_ppm: 0,
                                ..policy.clone()
                            },
                        });
                    }
                } else {
                    let incoming_forward_count = incoming_forwards_count.total_forwards_count;

                    if incoming_forward_count > incoming_forwards_hi_water_mark {
                        let new_inbound_fee_rate = std::cmp::min(
                            0,
                            ((policy.inbound_fee_rate_ppm as f64 * 0.9)
                                + (rand::random::<f64>() * 10.0)
                                + 2.0)
                                .ceil() as i32,
                        );
                        if new_inbound_fee_rate != policy.inbound_fee_rate_ppm {
                            channel_policy_changes.push(ChannelPolicyChange {
                                apply_at: add_time_to_policy_change(
                                    last_inbound_policy.as_ref(),
                                    CHANNEL_POLICY_CHANGE_COOLDOWN,
                                ),
                                channel: channel.clone(),
                                action: "Decrease Incoming Fee Rate Discount by 10%".to_string(),
                                reason: format!(
                                    "Incoming Forward Count {} > {} (window: {})",
                                    incoming_forward_count,
                                    incoming_forwards_hi_water_mark,
                                    FORWARDING_HISTORY_WINDOW
                                ),
                                old_policy: policy.clone(),
                                new_policy: ChannelPolicy {
                                    inbound_fee_rate_ppm: new_inbound_fee_rate,
                                    ..policy.clone()
                                },
                            });
                        }
                    } else if incoming_forward_count < incoming_forwards_lo_water_mark {
                        let new_inbound_fee_rate = std::cmp::max(
                            (-(policy.fee_rate_ppm as f64 * 0.95)).ceil() as i32,
                            ((policy.inbound_fee_rate_ppm as f64 * 1.05)
                                - (rand::random::<f64>() * 10.0)
                                - 2.0)
                                .floor() as i32,
                        );
                        if new_inbound_fee_rate != policy.inbound_fee_rate_ppm {
                            channel_policy_changes.push(ChannelPolicyChange {
                                apply_at: add_time_to_policy_change(
                                    last_inbound_policy.as_ref(),
                                    CHANNEL_POLICY_CHANGE_COOLDOWN,
                                ),
                                channel: channel.clone(),
                                action: "Increase Incoming Fee Rate Discount by 5%".to_string(),
                                reason: format!(
                                    "Incoming Forward Count {} < {} (window: {})",
                                    incoming_forward_count,
                                    incoming_forwards_lo_water_mark,
                                    FORWARDING_HISTORY_WINDOW
                                ),
                                old_policy: policy.clone(),
                                new_policy: ChannelPolicy {
                                    inbound_fee_rate_ppm: new_inbound_fee_rate,
                                    ..policy.clone()
                                },
                            });
                        }
                    }
                }
            }
        }
    }

    Ok(channel_policy_changes)
}

#[derive(Debug, Clone)]
#[allow(dead_code)]
struct EdgePolicyHistoryRecord {
    valid_from: NaiveDateTime,
    fee_rate_milli_msat: Option<i64>,
    inbound_fee_rate_milli_msat: Option<i64>,
}

async fn get_last_edge_policy_change(
    db_client: &tokio_postgres::Client,
    node_pubkey: &str,
    channel_id: i64,
    column: &str,
) -> Result<Option<EdgePolicyHistoryRecord>> {
    let node_pubkey_bytes = hex::decode(node_pubkey)?;

    let query = format!(
        "WITH ordered AS (
            SELECT eph.valid_from, eph.fee_rate_milli_msat, eph.inbound_fee_rate_milli_msat,
                   LAG({}) OVER (
                     PARTITION BY node_id, channel_id
                     ORDER BY valid_from
                   ) AS previous_value
            FROM edge_policy_history AS eph
            WHERE node_id = (SELECT id FROM node WHERE pubkey = $1)
              AND channel_id = $2
        )
        SELECT valid_from, fee_rate_milli_msat, inbound_fee_rate_milli_msat
        FROM ordered
        WHERE {} IS DISTINCT FROM previous_value
        ORDER BY valid_from DESC
        LIMIT 1",
        column, column
    );

    let rows = db_client
        .query(&query, &[&node_pubkey_bytes, &channel_id])
        .await?;

    if let Some(row) = rows.first() {
        Ok(Some(EdgePolicyHistoryRecord {
            valid_from: row.get(0),
            fee_rate_milli_msat: row.get(1),
            inbound_fee_rate_milli_msat: row.get(2),
        }))
    } else {
        Ok(None)
    }
}

fn has_outbound_liquidity(local_balance: i64, total_balance: i64) -> bool {
    !(total_balance <= 800_000 && local_balance < 100_000
        || total_balance > 800_000 && local_balance < 300_000)
}

fn has_inbound_liquidity(remote_balance: i64, total_balance: i64) -> bool {
    !(total_balance <= 800_000 && remote_balance < 100_000
        || total_balance > 800_000 && remote_balance < 300_000)
}

fn add_time_to_policy_change(
    last_policy: Option<&EdgePolicyHistoryRecord>,
    cooldown: Duration,
) -> DateTime<Utc> {
    if let Some(policy) = last_policy {
        DateTime::from_naive_utc_and_offset(policy.valid_from + cooldown, Utc)
    } else {
        Utc::now()
    }
}

async fn get_node_alias(
    mut client: lnrpc::lightning_client::LightningClient<
        tonic::service::interceptor::InterceptedService<
            tonic::transport::Channel,
            impl Fn(Request<()>) -> Result<Request<()>, tonic::Status> + Clone,
        >,
    >,
    pub_key: &str,
) -> String {
    let request = Request::new(lnrpc::NodeInfoRequest {
        pub_key: pub_key.to_string(),
        include_channels: false,
    });

    match client.get_node_info(request).await {
        Ok(response) => response
            .into_inner()
            .node
            .map(|n| n.alias)
            .filter(|a| !a.is_empty())
            .unwrap_or_else(|| "-".to_string()),
        Err(_) => "-".to_string(),
    }
}

async fn update_channel_policy(
    mut client: lnrpc::lightning_client::LightningClient<
        tonic::service::interceptor::InterceptedService<
            tonic::transport::Channel,
            impl Fn(Request<()>) -> Result<Request<()>, tonic::Status> + Clone,
        >,
    >,
    policy: &ChannelPolicy,
) -> Result<()> {
    let request = Request::new(lnrpc::PolicyUpdateRequest {
        scope: Some(lnrpc::policy_update_request::Scope::ChanPoint(
            lnrpc::ChannelPoint {
                funding_txid: Some(lnrpc::channel_point::FundingTxid::FundingTxidStr(
                    policy.chan_point.funding_txid_str.clone(),
                )),
                output_index: policy.chan_point.output_index,
            },
        )),
        base_fee_msat: policy.base_fee_msat,
        fee_rate: 0.0,
        fee_rate_ppm: policy.fee_rate_ppm as u32,
        time_lock_delta: policy.time_lock_delta,
        max_htlc_msat: policy.max_htlc_msat,
        min_htlc_msat: 0,
        min_htlc_msat_specified: false,
        inbound_fee: Some(lnrpc::InboundFee {
            base_fee_msat: policy.inbound_fee_base_msat,
            fee_rate_ppm: policy.inbound_fee_rate_ppm,
        }),
        create_missing_edge: false,
    });

    client.update_channel_policy(request).await?;
    Ok(())
}
