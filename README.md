# coinbase-sweeper

A small service for Zcash mining pools. It moves the pool's transparent block rewards into
Ironwood, and keeps the wallet the pool pays miners from funded out of Ironwood. It is for
pools whose own software sweeps coinbase into Sapling and cannot be changed quickly. When a
network upgrade stops deposits into Sapling, such sweeps fail and payouts pause. This tool
replaces them.

It has two commands, which run on their own or together:

- `sweep` shields mature transparent coinbase into the pool's Orchard-only unified address,
  where it is held as Ironwood notes.
- `refill` moves funds from that address to the pool's payout address or payout wallet. With
  several payout addresses (`refill.destinations`), one refill pays one of them: in turn in
  `drain_above` mode, or the one with the lowest balance in `top_up` mode. The caps apply to
  all destinations together.

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

[Releases](https://github.com/valargroup/coinbase-sweeper/releases) provide static
Linux binaries for amd64 and arm64. Download your architecture's tarball and
`SHA256SUMS` into the same directory, then verify it with
`sha256sum --check --ignore-missing SHA256SUMS` before extracting it.

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

Everything below ran on regtest (a private test network). Transaction ids are from those test
networks and exist nowhere else; they are listed so the runs can be matched to their logs.

**Machine:** 8 vCPU (Intel, 2.0 GHz, cloud VM), 31 GB RAM, Linux 6.8; Rust 1.99.1. Regtest
nodes and wallets were pinned to 6 of the 8 cores and run at low priority next to other work.

**Software under test:** zakurad v1.8.8 (tag, commit 009b60bea); zecd with mining-pool support
(commit d73dd937); zcashd with Ironwood support (build 6404203f0; the newest commit of that
change, a8b5d25f7, only edits a Python test).

### How to run the tests

```sh
cargo fmt --check && cargo clippy --all-targets -- -D warnings && cargo test   # regtests skip
export ZAKURAD_BIN=/path/to/zakurad ZECD_BIN=/path/to/zecd
. scripts/regtest-env.sh     # ZECD_REGTEST_NODE=zakura, ZAKURAD_BIN, ZECD_BIN,
                             # ZECD_REGTEST_NU63_HEIGHT=8, ZECD_REGTEST_NU7_HEIGHT=12
cargo test --test regtest_sweep --test regtest_refill -- --test-threads=1 --nocapture
cargo test --release --test regtest_perf -- --ignored --nocapture
SOAK_MINUTES=30 cargo test --release --test regtest_soak -- --ignored --nocapture
```

Without `ZAKURAD_BIN` and `ZECD_BIN` the regtests print `SKIP` and pass. The zecd regtests use
the regtest harness from the zecd repository as a test dependency.

### Results with zecd (automated regtests)

| Test | Result | Transactions |
| --- | --- | --- |
| `sweep --once` with 72 mature rewards | 2 batches (50 + 23), each v6 with no transparent outputs and Ironwood actions; nothing mature left; a unified address with a Sapling receiver is refused at startup | `b9a750de…bdd66`, `08385fdc…5793a` |
| Tool killed right after submitting a batch, then restarted | The restarted run finds the killed run's operation (`outcome=ok_recovered`), waits for it, sweeps the rest; all 72 rewards spent exactly once on chain | recovered sweep, then `bb20153b…6d66e` |
| Wallet restarted mid-operation, then node hung for several cycles | Skipped cycles, one ERROR after 3 skips, recovery; every reward spent exactly once | |
| `refill top_up` | 25 ZEC to an empty wallet-owned t-address; then exactly the difference (21 ZEC); then nothing, because 46 + 25 would pass `max_per_day` = 50. Ironwood only, no Sapling, one output | `94c50916…f28b7b9`, `baf586b4…5bd2361` |
| External destination | Refused without `external_destination`; with it, `drain_above` sends exactly spendable − reserve, waits for the confirmation, and sends nothing more | `4c221edc…426e1e` |
| Tool killed between writing the state file and the send's result | Settled from the wallet (`ok_recovered`); exactly one payment on chain | `aab71db6…a0e548` |

### Results with zcashd (manual runs on the same setup, zakurad v1.8.8)

| Test | Result | Transactions |
| --- | --- | --- |
| `--check`, `--dry-run` | 140 mature rewards; 3 batches planned; 3 RPC calls | |
| `sweep --once`, 140 rewards | 3 batches (50, 50, 48); all 140 spent exactly once (checked on the node); every sweep v6, no transparent outputs, Ironwood actions | `e32dcf0d…6c4f8`, `1f063121…eedd5`, `77acb882…19c11` |
| Killed mid-batch, restarted | Recovered the operation, swept the rest; 123 of 123 spent once | `2ba15aa9…f4f0d` (recovered), `dab2f1cf…3ef2`, `fd628de1…dec6` |
| zcashd restarted mid-operation | Hold for one block, skipped cycles, recovery; 123 of 123 spent once | `7319e019…bdd`, `cecd0df2…599c` |
| `refill top_up` | Exactly 25 ZEC, v6, Ironwood only | `f55618a5…8b964` |
| External destination | Refused without the double entry | |
| `drain_above`, killed mid-send, restarted (twice) | Settled from the wallet, one payment each time | `4029b5c4…cbd3`, `b23c29fa…4a50` |

### Full transaction ids

- zecd: sweeps `b9a750de19f1c9c9e54917278754817c70f4455bc073d10a149ed156fd3bdd66`,
  `08385fdc1c095e133992fcc23dfbdbcc0bcd9484af5b18f0fad76fc39d25793a`,
  `bb20153b833a6e484fbcd4db44ffbee3d4f7427a9b9d896bf7eb0f976bd6d66e`; refills
  `94c509168143fe41510a6f227f895dfbe4e725234f3a4957c0cad3a16f28b7b9`,
  `baf586b4ed9474cc93d773b6aab854d16b9fa1384fe9310f24ace158b5bd2361`,
  `4c221edc8e83f7f62392e73542b8e2e2c615594bcbe64b40f38e6248ec426e1e`,
  `aab71db67e360818116b2f2cd30b49dcfcb43266c12af339da3bf1c4fba0e548`.
- zcashd: sweeps `e32dcf0d21e18201cc64ddd3efd0297304645c7833c5e8152032f7c05a36c4f8`,
  `1f0631211db53d8aa47fd4af6aad6054ed57af22e8a9341ab2a5d40f9f7eedd5`,
  `77acb88259f39ae63a3c9491c794e073700efd40e36cf86425d3a9800db19c11`,
  `2ba15aa9a3833c51bedd702018dfc933529ed6d55c090cfbe968320e279f4f0d`,
  `dab2f1cfc9ee6992e826fec4645f0d92241fdc7e40f11af802668a566c423ef2`,
  `fd628de19ca22879304c38db7f655cc421370f3cfd7573e2e0b8da54abdddec6`,
  `7319e01917375adc669093dfa4800ae986b257dde0bd5484b727b553538f7bdd`,
  `cecd0df258654fa3d565ff5650256614b57f9a67b6596a7c9246e6566874599c`; refills
  `f55618a52c1eef7d2f0f442e3d790e5c113cdebb0b5f59aae1002b562318b964`,
  `4029b5c4a5ea563c3882dfd14b47db7f3e428629adcce3867c723ab74864cbd3`,
  `b23c29fa9f146829e8a1a3023ec55bfc4032149cdd0fbd10a50375e1e1f74a50`.

### Performance (zecd, release build)

| Measurement | Result | Target |
| --- | --- | --- |
| One 50-input sweep, call to success | 0.62 s; wallet CPU 1.6 s for the whole run | well under 25 s; flag above 10 s: **met** |
| Backlog: 1,023 rewards, one block mined per batch | 21 batches in 44.4 s = 83,000 rewards per hour. On mainnet a batch waits for a block: 50 per 25 s block = 7,200 per hour | at least 1,440 per hour: **met** (5× at mainnet block times) |
| Idle cycle (nothing due) | 5 RPC calls, 90 ms; 0.3 s for a whole `--once` process | a few calls, under 1 s: **met** |
| The tool's footprint | Over a 30-minute soak: peak memory 4.3 MB per process; CPU 0.7 s (sweep) and 0.6 s (refill) in total | negligible next to the wallet: **met** |
| Scale | 50,000 `listunspent` entries parsed in 0.17 s; planning over 200,000 entries is linear (unit tests) | no quadratic work: **met** |

No target was missed.

### Soak (zecd, both commands as services)

Both commands ran as services against one zecd wallet for 30 minutes while a block was mined
every 4 seconds (450 blocks), with sweeps (`min_inputs = 10`) and refills (`drain_above` to an
external address) running alongside each other.

- No ERROR lines in either log (sweep: 869 cycles; refill: 864 cycles).
- 482 rewards swept in 46 sweeps; every one spent exactly once (checked on the node). The 109
  left unswept were immature, or fewer than `min_inputs` at the end.
- 51 refills paid 877.47 ZEC.
- The books balance from the chain: coinbase swept (916.67830982 ZEC) minus sweep fees
  (0.0287 ZEC) minus refill fees (0.00805 ZEC) equals the Ironwood balance (39.1666317 ZEC) plus
  everything refilled, to within 10,000 zatoshis. That remainder is two refill change notes of
  at most 5,000 zatoshis each. A note that small costs as much to spend as it holds, so the
  wallet leaves it out of its balance; nothing is lost.

### Documentation reader test

A reviewer given only these documents (no code) wrote a configuration for each of the four
setups in the pool guide. The tool accepted both configurations, and the two "no tool needed"
answers were right. The reviewer's questions led to these additions:

- a unified address with a Sapling receiver does not qualify;
- how setups 3 and 4 pay miners;
- the delay before the first refill;
- the meaning of "payable balance" and of `low_water`;
- how to size `alert_unswept_zec`, `max_per_transfer`, `max_per_day` and `min_transfer`;
- cookie files on remote wallets;
- address formats.

