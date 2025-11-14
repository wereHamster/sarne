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
