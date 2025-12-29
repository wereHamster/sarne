-----------------------------------------------------------------------------
-- Graph

CREATE TABLE node (
  id serial PRIMARY KEY,
  pubkey bytea,

  -- The latest known node alias.
  alias text
);

CREATE UNIQUE INDEX node_pubkey_idx
  ON node (pubkey);



-----------------------------------------------------------------------------
-- History

CREATE TABLE edge_policy_history (
  node_id integer REFERENCES node (id),
  channel_id bigint,

  valid_from timestamp,
  valid_to timestamp,

  disabled boolean,

  time_lock_delta integer,

  min_htlc_msat bigint,
  max_htlc_msat bigint,

  fee_base_msat bigint,
  fee_rate_milli_msat bigint,

  inbound_fee_base_msat bigint,
  inbound_fee_rate_milli_msat bigint,

  PRIMARY KEY (node_id, channel_id, valid_from)
);



-----------------------------------------------------------------------------
-- Node

CREATE TABLE channel_liquidity_sample (
  node_id integer REFERENCES node (id),
  channel_id bigint,

  sampled_at timestamp,

  outgoing_liquidity_msat bigint,
  incoming_liquidity_msat bigint,

  PRIMARY KEY (node_id, channel_id, sampled_at)
);


CREATE TABLE forward (
  node_id integer REFERENCES node (id),
  incoming_channel_id bigint,
  outgoing_channel_id bigint,
  incoming_htlc_id bigint,
  outgoing_htlc_id bigint,

  created_at timestamp,
  finalized_at timestamp,

  incoming_amount_msat bigint,
  outgoing_amount_msat bigint,

  state smallint,

  PRIMARY KEY (node_id, incoming_channel_id, outgoing_channel_id, incoming_htlc_id, outgoing_htlc_id)
);

CREATE INDEX forward_idx ON forward (node_id, created_at);
CREATE INDEX forward_incoming_channel_id_idx ON forward (incoming_channel_id, created_at);
CREATE INDEX forward_outgoing_channel_id_idx ON forward (outgoing_channel_id, created_at);



-----------------------------------------------------------------------------
-- Payment Probes

CREATE TABLE payment_probe (
  id bigserial PRIMARY KEY,

  created_at timestamp,

  src_node_id integer REFERENCES node (id),
  outgoing_channel_id bigint,
  dst_node_id integer REFERENCES node (id),

  amount_msat bigint
);

CREATE TABLE payment_probe_attempt (
  payment_probe_id bigint REFERENCES payment_probe (id) ON DELETE CASCADE,
  attempt_seqno smallint,

  created_at timestamp,

  node_path_hash bytea,
  channel_path_hash bytea,

  fee_msat bigint,
  latency_ns bigint,

  PRIMARY KEY (payment_probe_id, attempt_seqno)
);

CREATE TABLE payment_probe_hop (
  payment_probe_id bigint REFERENCES payment_probe (id) ON DELETE CASCADE,
  attempt_seqno smallint,
  hop_seqno smallint,

  channel_id bigint,
  node_id integer REFERENCES node (id),
  fee_msat bigint,

  PRIMARY KEY (payment_probe_id, attempt_seqno, hop_seqno),

  FOREIGN KEY (payment_probe_id, attempt_seqno)
    REFERENCES payment_probe_attempt (payment_probe_id, attempt_seqno)
    ON DELETE CASCADE
);

CREATE TABLE payment_probe_failure (
  payment_probe_id bigint REFERENCES payment_probe (id) ON DELETE CASCADE,
  attempt_seqno smallint,
  hop_seqno smallint,

  failure_code integer,

  PRIMARY KEY (payment_probe_id, attempt_seqno),

  FOREIGN KEY (payment_probe_id, attempt_seqno)
    REFERENCES payment_probe_attempt (payment_probe_id, attempt_seqno)
    ON DELETE CASCADE,

  FOREIGN KEY (payment_probe_id, attempt_seqno, hop_seqno)
    REFERENCES payment_probe_hop (payment_probe_id, attempt_seqno, hop_seqno)
    ON DELETE CASCADE
);
