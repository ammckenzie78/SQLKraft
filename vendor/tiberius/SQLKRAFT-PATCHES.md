# Local patches to tiberius 0.13.0

This directory is tiberius 0.13.0 from crates.io (MIT/Apache-2.0, see the licence files),
used through `[patch.crates-io]` in the top-level `Cargo.toml`. Only `src/`, `Cargo.toml`,
the licences and the README are kept.

## 1. Cancellation: wait for the Attention acknowledgement across messages

`src/client/connection.rs` (`cancel_request`) and `src/tds/stream/token.rs`
(`find_done_attention`).

`Client::cancel_query` sends an Attention packet and then reads the token stream for the
`DONE` token with the `DONE_ATTN` bit. Upstream reads only until the end of the current
message. When the cancelled request's own response ends first, for example a `WAITFOR`
cancelled before it returned anything, SQL Server sends the acknowledgement in the *next*
message. Upstream then fails with "Never got a DONE token acknowledging the Attention
signal", and the connection has to be thrown away, losing temp tables, `USE`, `SET`
options and open transactions.

The patch keeps reading messages, up to 64, until the acknowledgement arrives. Server
errors raised by the cancelled request are skipped while draining.

Drop this patch once upstream tiberius handles it.

## 2. `Client::send_batch` / `Client::batch_results`

`src/client.rs`. `simple_query` split into its two halves: sending the batch, then reading the
response. SQLKraft awaits the send without racing it against a cancel, because a send dropped
half-way leaves the connection poisoned, and races only the response. Purely additive;
`simple_query` is unchanged.

## 3. Mid-token flag (`Client::mid_token_flag`)

`src/client/connection.rs`, `src/tds/stream/token.rs`, `src/client.rs`. The connection keeps a
shared flag that the token stream sets after reading a token's type byte and clears once the
token is fully decoded. `Client::mid_token_flag()` hands out the flag. When SQLKraft
abandons a read to cancel a query, it checks the flag. If it is set, the read has consumed
part of a token (for example a row split across packets), so it is allowed to finish first;
dropping it would lose those bytes and leave the stream out of step, so the Attention
acknowledgement could never be found. If the flag is clear, dropping the read is safe and
immediate.
