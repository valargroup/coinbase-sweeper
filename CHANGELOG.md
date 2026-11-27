# Changelog

## [Unreleased]

### Added

- `refill.destinations` lists several payout addresses. One refill pays one destination,
  never several in one transaction. In `drain_above` mode the destinations take turns in the
  order listed, and the last destination paid is kept in the state file so the order survives
  restarts. In `top_up` mode each cycle refills the destination with the lowest balance below
  `low_water`. `max_per_transfer`, `max_per_day`, `min_transfer` and `fee_headroom` apply to all
  destinations together. External destinations are confirmed by listing them again in
  `refill.external_destinations`.
- A single `refill.destination` (with `external_destination`) works as before.

## [0.1.0] - 2026-11-26

First release of coinbase-sweeper for Zcash mining pools using zecd with mining-pool
support or zcashd with Ironwood support. Downloads include static Linux binaries
for amd64 and arm64, an example config, systemd service units, and SHA-256 checksums.

### Added

- `sweep` shields mature transparent mining rewards into Ironwood.
- `refill` funds the payout wallet from Ironwood with configurable reserves and
  transfer caps. It checks the prior operation after a restart before sending again.
- Both commands support configuration checks, dry runs, and one-cycle or service
  operation. The wallet holds the keys, signs transactions, and pays ZIP 317 fees.

See the [pool guide](https://github.com/valargroup/coinbase-sweeper/blob/v0.1.0/docs/POOL_GUIDE.md)
for setup and migration.
