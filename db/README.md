# Database

Sarne persists data in a PostgreSQL database.

## Overview

The tables in the database can be categorized into three groups:

- **Graph**: The current state of the Lightning Network graph.
  This is a copy of data that's stored in the Lightning Network Node.
  We only keep what's relevant for data consistency (e.g. other tables reference these tables via foreign keys).

- **History**: Historized view on the graph.
  Some graph data (e.g. edge policies) change frequently.
  We want to use this historic data for policy decisions.

- **Node**: Information about what's happening on our own node.
  This is augmenting data that is stored in the Lightning Network Node.
  For example, the LND API does not provide information about failed forwards.
  However, we want to use this data when making policy decisions.

## Conventions

### Column Name Suffix

- Columns with suffix `at` store timestamps (without timezone).

- Columns with suffix `node_id` reference a node.
  The column references the `node` table where we compress the node pubkey into our own database identifier).

- Columns with suffix `channel_id` store the Short Channel ID as `bigint` (signed 64 bit number).
  This is safe because even though Short Channel ID is u64, it is not expected to overflow for the next hundred years.

- Columns with suffix `msat` are values in milli-satoshi.

- Columns with suffix `milli_msat` are values in milli-msat (used for fee rate).

## Tables

### Graph

- `node`

### History

- `edge_policy_history`

### Node

- `channel_liquidity_sample`
- `forward`
