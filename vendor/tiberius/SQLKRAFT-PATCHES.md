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

## Going back to the official crate

The goal is to drop this directory once upstream fixes the cancel bug, so we don't drift from
the maintainers. Upstream tracking (repository moved to `tiberius-rs/tiberius`):

- [#459](https://github.com/tiberius-rs/tiberius/issues/459): the cancel acknowledgement
  arriving in a later message (our patch 1). Open PRs: #460 (drains token by token) and #469
  (drains whole packets, and says it also covers a cancel that lands part-way through a token,
  which would make patch 3 unnecessary).
- [#79](https://github.com/tiberius-rs/tiberius/issues/79): futures dropped mid-response
  leaving the connection out of step (what patches 2 and 3 work around).

`.github/workflows/tiberius-watch.yml` checks weekly for a newer tiberius release and for #459
being closed, and opens an issue here when either happens. To switch:

1. See which patches the new upstream code makes unnecessary.
2. Remove the `[patch.crates-io]` entry from the top-level `Cargo.toml` and point `tiberius` at
   the new release (or an upstream git commit, if the fix is merged but not released), then
   delete `vendor/tiberius`.
3. If patches 2 and 3 are gone, change `run_one` in `src/db.rs` back to `simple_query` and
   rely on upstream's cancel draining.
4. Run the full suite against a live server, including `cancel_keeps_the_session` and
   `cancel_mid_row_many_times`, several times under load. Keep any patch whose test fails
   without it.
