# Configuration reference

coinbase-sweeper reads one TOML file, given with `--config <file>`. Both commands, `sweep` and
`refill`, can share the same file: each reads the top-level key, `[rpc]`, `[general]` and its
own section, and ignores the other command's section. Unknown keys are an error, so a typo stops
the tool instead of being ignored.

Amounts are in ZEC and may be written as numbers (`0.001`) or strings (`"0.001"`). They are read
exactly, with up to 8 decimal places. Durations are in seconds.

The RPC password is never read from this file. Give it in the `SWEEPER_RPC_PASSWORD`
environment variable, or point `rpc.cookie_file` at the wallet's cookie file.

## Top level

| Key | Type | Default | Valid values | Meaning |
| --- | --- | --- | --- | --- |
| `shielded_address` | string | required | a unified address (`u1...` on mainnet) with an Orchard receiver and no Sapling receiver, owned by the wallet | The pool's Ironwood address. `sweep` sends coinbase here and `refill` spends from here. Create it with `z_getaddressforaccount <account> '["orchard"]'`. |

## `[rpc]`

| Key | Type | Default | Valid values | Meaning |
| --- | --- | --- | --- | --- |
| `url` | string | required | starts with `http://` | The wallet's JSON-RPC endpoint, for example `http://127.0.0.1:8232/`. TLS is not supported: keep the wallet RPC on localhost or a private network. |
| `user` | string | none | any | The RPC user. Required unless `cookie_file` is set. The password comes from `SWEEPER_RPC_PASSWORD`. |
| `cookie_file` | path | none | a file holding `user:password`, readable on the machine running the tool | The wallet's cookie file. When set, it is used instead of `user` and `SWEEPER_RPC_PASSWORD`, and it is read again if the wallet rejects the credentials (a wallet may write a new cookie each time it starts). If the wallet runs on another machine, use `user` and `SWEEPER_RPC_PASSWORD` instead, or make the cookie file available locally. |
| `timeout_secs` | integer | `30` | 1 to 3600 | Time limit for each read-only call. |
| `submit_timeout_secs` | integer | `120` | 1 to 3600 | Time limit for a call that moves funds (`z_shieldcoinbase`, `z_sendmany`). Such a call is never repeated: if it times out, the tool finds out from the wallet what happened. |
| `read_retries` | integer | `3` | 0 to 10 | How many times a read-only call is retried when the wallet does not answer, with growing pauses (0.25 s, 0.5 s, 1 s...). |

## `[general]`

| Key | Type | Default | Valid values | Meaning |
| --- | --- | --- | --- | --- |
| `state_dir` | path | `/var/lib/coinbase-sweeper` | a writable directory | Lock files, the halt and pause markers, and each command's state file. Back it up with the rest of the pool's state; never share one between two pools. |
| `interval_secs` | integer | `60` | 1 to 86400 | Time between cycles when running as a service. |
| `max_tip_lag_blocks` | integer | `2` | 0 to 100 | A cycle is skipped while the wallet is more than this many blocks behind the node. |
| `operation_timeout_secs` | integer | `600` | 10 to 86400 | How long to wait for one wallet operation (building and proving a transaction) to finish. |
| `confirmation_timeout_secs` | integer | `1800` | 10 to 86400 | How long to wait for a transaction to be mined before the cycle fails (it is waited for again next cycle; nothing is resent). |
| `skipped_cycles_before_error` | integer | `5` | at least 1 | After this many skipped cycles in a row, one ERROR line is logged. Skipping continues, with growing pauses. |
| `max_backoff_secs` | integer | `600` | `interval_secs` to 86400 | The longest pause between cycles while cycles are skipped or failing. |
| `rejection_pause_secs` | integer | `3600` | 60 to 604800 | After the node rejects a transaction, the command pauses this long before trying again. |

## `[sweep]`

| Key | Type | Default | Valid values | Meaning |
| --- | --- | --- | --- | --- |
| `coinbase_addresses` | list of strings | required | transparent addresses (`t1...` or `t3...` on mainnet) the wallet can sign for, no duplicates | The addresses the pool's blocks pay their reward to. The tool checks every address with the wallet at startup. |
| `min_inputs` | integer | `10` | 1 to `max_inputs` | An address is swept once it holds at least this many mature rewards... |
| `max_wait_blocks` | integer | `100` | 0 or more | ...or once its oldest mature reward has waited this many blocks since it matured. With `0`, any mature reward is swept at once. |
| `max_inputs` | integer | `50` | 1 to 50 | Rewards per sweep transaction. |
| `max_batches_per_cycle` | integer | `10` | 1 to 100 | Sweep transactions per cycle, across all addresses. Batches run one at a time, each waiting for the previous one to be mined. |
| `alert_unswept_zec` | amount | required | 0 or more | Log an ERROR when one address holds more mature, unswept coinbase than this. A good value is about one day of your pool's rewards at that address: normally sweeps keep it far lower. |

## `[refill]`

| Key | Type | Default | Valid values | Meaning |
| --- | --- | --- | --- | --- |
| `mode` | string | required | `"top_up"` or `"drain_above"` | `top_up` keeps a wallet-owned transparent address funded. `drain_above` sends everything above `reserve` to the destination. |
| `destination` | string | required | a transparent address, or a unified address with an Orchard receiver; not `shielded_address` | Where refills go. `top_up` requires a transparent address the wallet owns. Its *payable balance* is its unspent transparent outputs, unconfirmed ones included, except coinbase (rewards, mature or not, cannot pay miners until they are shielded). |
| `external_destination` | string | none | exactly the same text as `destination` | Required when the wallet does not own `destination` (an external payout wallet). Writing the address twice is deliberate: a typo stops the tool instead of sending money. |
| `low_water` | amount | none | less than `target` | `top_up` only, required there. A refill happens when the destination's payable balance falls below this. It is a trigger, checked once per cycle, not a floor: a payout larger than the balance above `low_water` can take the balance lower before the next cycle. Set it above your largest single payout run. |
| `target` | amount | none | more than `low_water` | `top_up` only, required there. A refill sends `target` minus the current payable balance (within the caps). |
| `reserve` | amount | none | 0 or more | `drain_above` only, required there. The amount left in the pool's shielded address. |
| `minconf` | integer | `10` | 1 to 1000 | Confirmations a shielded note needs before a refill spends it. |
| `fee_headroom` | amount | `0.001` | 0.0001 to 1 | Left unspent in every refill to pay the network fee, which the wallet sets (0.001 ZEC covers up to 20 actions). A refill transaction paying a higher fee stops the tool. |
| `min_transfer` | amount | `0.01` | more than 0, up to `max_per_transfer` | Smaller refills are skipped. In `drain_above` mode every sweep adds funds, so raise this (for example to 1 ZEC) to avoid a small, public transfer every cycle. |
| `max_per_transfer` | amount | required | more than 0 | The most one refill sends. A larger need is met over several cycles. In `top_up` mode, set it to at least `target` so one refill can fill an empty address. |
| `max_per_day` | amount | required | at least `max_per_transfer` | The most sent per UTC day. A refill that would pass it is not sent at all, not even in part. The first fill of the destination counts too. |
| `wait_confirmations` | integer | `1` | 1 to 100 | Confirmations a refill needs before the next refill. |

## Complete examples

The examples follow the setups in [the pool guide](POOL_GUIDE.md#2-which-setup-you-have).

### Setup 1: coinbase to a t-address, payouts from that same t-address

Run `sweep` and `refill` in `top_up` mode. The refill destination is the coinbase address
itself: coinbase outputs there are never counted as payable balance, because they cannot pay
miners until they are shielded.

```toml
# The pool's Orchard-only unified address (z_getaddressforaccount 0 '["orchard"]').
shielded_address = "u1exampleorchardonlyaddressxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxx"

[rpc]
url = "http://127.0.0.1:8232/"
user = "pool"                       # password from SWEEPER_RPC_PASSWORD

[general]
state_dir = "/var/lib/coinbase-sweeper"

[sweep]
coinbase_addresses = ["t1examplecoinbaseaddressxxxxxxxxxx"]
min_inputs = 10                     # sweep once 10 rewards are mature...
max_wait_blocks = 100               # ...or the oldest has waited 100 blocks
alert_unswept_zec = 500

[refill]
mode = "top_up"
destination = "t1examplecoinbaseaddressxxxxxxxxxx"   # the same address pays miners
low_water = 50                      # refill when payable balance drops below 50 ZEC
target = 200                        # back up to 200 ZEC
max_per_transfer = 250
max_per_day = 1000
```

### Setup 2: coinbase to a t-address, payouts from a separate wallet

Run `sweep` and `refill` in `drain_above` mode. The payout wallet is not this wallet, so its
address is written twice.

```toml
shielded_address = "u1exampleorchardonlyaddressxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxx"

[rpc]
url = "http://127.0.0.1:8232/"
cookie_file = "/var/lib/zcashd/.cookie"

[general]
state_dir = "/var/lib/coinbase-sweeper"

[sweep]
coinbase_addresses = ["t1examplecoinbaseaddressxxxxxxxxxx"]
alert_unswept_zec = 500

[refill]
mode = "drain_above"
destination = "t1examplehotwalletaddressxxxxxxxxx"
external_destination = "t1examplehotwalletaddressxxxxxxxxx"   # the same address, again
reserve = 5                         # keep 5 ZEC in the shielded address
max_per_transfer = 500
max_per_day = 2000
```

### Setups 3 and 4

No configuration: the tool is not needed. See the pool guide.
