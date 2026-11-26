# Changelog

## [Unreleased]

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
