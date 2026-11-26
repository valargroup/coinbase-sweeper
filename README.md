# coinbase-sweeper

A small service for Zcash mining pools. It moves the pool's transparent block rewards into
Ironwood, and keeps the wallet the pool pays miners from funded out of Ironwood. It is for
pools whose own software sweeps coinbase into Sapling and cannot be changed quickly. When a
network upgrade stops deposits into Sapling, such sweeps fail and payouts pause. This tool
replaces them.

It has two commands, which run on their own or together:

- `sweep` shields mature transparent coinbase into the pool's Orchard-only unified address,
  where it is held as Ironwood notes.
- `refill` moves funds from that address to the pool's payout address or payout wallet.

What it is not: it does not pay miners (your pool software still does), it never holds keys
(the wallet signs everything), it never sets fees (the wallet pays the standard ZIP 317 fee),
and it does not touch funds already in Sapling.

**Start with the [pool guide](docs/POOL_GUIDE.md)**: what is changing, which setup you have,
and a step-by-step migration checklist. Every configuration key is in
[docs/CONFIGURATION.md](docs/CONFIGURATION.md).

## Wallets

It talks to the wallet over zcashd-style JSON-RPC. Supported:

- **zecd** with mining-pool support, with transparent receiving enabled (`[pools] transparent = true`);
- **zcashd** with Ironwood support.

The wallet needs a node that is past the Ironwood activation.

## Build

Requires Rust 1.89 or newer (the lock file uses the standard library's file locking, added
in 1.89).

```sh
cargo build --release --locked
# the binary: target/release/coinbase-sweeper
```

## Quick start

```sh
cp sweeper.example.toml sweeper.toml              # then fill it in: see docs/CONFIGURATION.md
export SWEEPER_RPC_PASSWORD=...                   # or set rpc.cookie_file
coinbase-sweeper sweep  --config sweeper.toml --check     # what the tool sees
coinbase-sweeper sweep  --config sweeper.toml --dry-run   # what one cycle would do
coinbase-sweeper sweep  --config sweeper.toml --once      # one real cycle
coinbase-sweeper refill --config sweeper.toml --once
```

To run as services, use the systemd units in [deploy/](deploy/); see the pool guide, steps 9
and 10. Exit codes for `--once`: 0 the cycle completed (including "nothing to do"), 1 startup
refused, 2 the cycle failed or was skipped.

## Documents

- [docs/POOL_GUIDE.md](docs/POOL_GUIDE.md): the operator's guide (setups, migration, rollback,
  monitoring, troubleshooting, security, FAQ).
- [docs/CONFIGURATION.md](docs/CONFIGURATION.md): every configuration key, with complete
  examples.
- [sweeper.example.toml](sweeper.example.toml): an annotated starting point.
- [deploy/](deploy/): systemd units, one per command.

## Validation

VALIDATION_PLACEHOLDER
