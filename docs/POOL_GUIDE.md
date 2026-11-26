# Pool guide: moving coinbase into Ironwood

This guide is for mining pool operators. It explains what is changing, which setup you have,
and how to move your pool's coinbase handling to Ironwood with coinbase-sweeper, step by step.

Three terms, defined once:

- **Sapling** is the older shielded pool most pools move their coinbase into today.
- **Ironwood** is the newer shielded pool. It is already live on mainnet. A unified address
  with an Orchard receiver receives Ironwood notes.
- **Coinbase maturity**: a block reward can only be spent once its block has 100 confirmations.
  A reward with fewer is "immature"; with 100 or more it is "mature".

## 1. What is changing and why it matters to a pool

A network upgrade may stop all new deposits into Sapling, possibly at short notice and with no
grace period. Many pools pay the block reward to a transparent address and then move ("sweep")
it into Sapling with `z_shieldcoinbase` before paying miners. Once deposits into Sapling stop,
those sweeps fail.

What keeps working:

- **Your coinbase is never lost.** Rewards stay at your transparent address, unspent, until you
  move them into Ironwood. You can do that at any time, before or after the upgrade.
- Funds already in Sapling stay spendable through your normal payout path.

What stops:

- **Payouts that depend on sweeping into Sapling pause** until your coinbase reaches Ironwood.

The fix is to sweep into Ironwood instead of Sapling, before the upgrade. If your pool software
can change its sweep destination, that is all you need (see setup 4 below). coinbase-sweeper is
for pools that cannot change their pool software quickly: it takes over the sweep, and keeps the
wallet your payouts come from funded.

What the tool is not:

- It does not pay miners. Your pool software keeps doing that, unchanged.
- It does not hold keys. Your wallet (zecd, or zcashd with Ironwood support) holds the keys and
  signs every transaction; the tool only asks it to over the wallet's RPC interface.
- It does not set fees. The wallet pays the standard network fee (ZIP 317).
- It does not move funds already in Sapling. Pay those out through your normal path.

## 2. Which setup you have

| Your pool today | What to run |
| --- | --- |
| **Setup 1.** Coinbase goes to a transparent address, and miners are paid from that same transparent address (it is refilled from shielded funds). | `sweep`, plus `refill` in `top_up` mode with the destination set to that address. |
| **Setup 2.** Coinbase goes to a transparent address, and miners are paid from a separate wallet. | `sweep`, plus `refill` in `drain_above` mode with the destination set to the payout wallet's address. |
| **Setup 3.** Coinbase is already paid straight into Ironwood. | Nothing. This tool is not needed. |
| **Setup 4.** Your pool software already sweeps coinbase into a unified address. | Change that destination to an Orchard-only unified address (section 3, step 2). A unified address that also has a Sapling receiver does not qualify: create a new Orchard-only one. This tool is not needed. |

`sweep` and `refill` are separate commands. Each runs on its own, and you can run either or
both. [CONFIGURATION.md](CONFIGURATION.md) has a complete example for setups 1 and 2.

In setups 3 and 4 your pool pays miners from the Orchard-only address directly, with
`z_sendmany` from that address (privacy policy `AllowRevealedRecipients` for transparent
miner addresses). Funds still at an old address with a Sapling receiver stay spendable from it;
pay them out through your normal path.

**Before the first refill (setups 1 and 2).** The pool's Orchard-only address starts empty. The
first refill can only happen after the first sweep, and after its notes have `refill.minconf`
confirmations (10 by default, about 4 minutes). Rewards already mature are swept on the first
cycle, so in practice this is minutes, not days. Keep paying miners from existing funds
(including Sapling funds, through your normal path) until then.

## 3. Migration checklist

The commands use `zcash-cli`; with zecd, send the same RPC calls with your usual client. Run the
tool on the machine that can reach the wallet's RPC port.

1. **Check your wallet.** You need zecd, or a zcashd build with Ironwood support. The tool
   refuses to start if the wallet cannot do what it needs.

2. **Create the pool's Orchard-only unified address**, in the wallet that holds the coinbase
   address:

   ```sh
   zcash-cli z_getaddressforaccount 0 '["orchard"]'
   ```

   Use the `address` field of the answer. On zcashd, create a **dedicated account** for the
   pool's Ironwood funds with `zcash-cli z_getnewaccount` and use its number in place of `0`:
   a refill spends from the whole account the address belongs to, not only from that address.
   zecd has one account per wallet, so there use `0`, and keep no other Orchard or Ironwood
   funds in that wallet: refills may spend any of them. The address must have an
   Orchard receiver and no Sapling receiver; the tool checks this at every start.

3. **Build and install the tool** (needs Rust 1.89 or newer):

   ```sh
   cargo build --release --locked
   sudo install -m 0755 target/release/coinbase-sweeper /usr/local/bin/
   ```

4. **Write the configuration.** Copy `sweeper.example.toml` to
   `/etc/coinbase-sweeper/sweeper.toml` and fill it in, using the example for your setup in
   [CONFIGURATION.md](CONFIGURATION.md). Put the RPC password in
   `/etc/coinbase-sweeper/env` as `SWEEPER_RPC_PASSWORD=...` (mode 0600), or set
   `rpc.cookie_file`.

5. **Check what the tool sees.** Nothing is sent:

   ```sh
   export SWEEPER_RPC_PASSWORD=...
   coinbase-sweeper sweep --config /etc/coinbase-sweeper/sweeper.toml --check
   coinbase-sweeper refill --config /etc/coinbase-sweeper/sweeper.toml --check
   ```

   You should see your mature, unswept coinbase per address, any pending wallet operations,
   your Ironwood balance, and today's refill total against its cap.

6. **See what one cycle would do.** Still nothing is sent:

   ```sh
   coinbase-sweeper sweep --config /etc/coinbase-sweeper/sweeper.toml --dry-run
   coinbase-sweeper refill --config /etc/coinbase-sweeper/sweeper.toml --dry-run
   ```

   Look for `event=dry_run ... would_shield_inputs=` and `event=dry_run ... would_send=`.

7. **Stop your pool software's Sapling sweep.** Turn off the step that calls
   `z_shieldcoinbase` into your Sapling address (for example, by removing its shielding
   interval or its shielded address setting). Two sweepers on the same coinbase do no harm to
   funds, but they compete for the same rewards and fail each other's transactions. Leave the
   step that pays out existing Sapling funds in place while Sapling is open.

8. **Run one cycle for real**, and watch the log:

   ```sh
   coinbase-sweeper sweep --config /etc/coinbase-sweeper/sweeper.toml --once
   coinbase-sweeper refill --config /etc/coinbase-sweeper/sweeper.toml --once
   ```

   Exit code 0 means the cycle completed (including "nothing to do"); 1 means it refused to
   start; 2 means the cycle failed or was skipped. A cycle with several sweeps waits for each to
   be mined before the next, so it can take a few block times.

9. **Enable the sweep service:**

   ```sh
   sudo cp deploy/coinbase-sweeper-sweep.service /etc/systemd/system/
   sudo systemctl daemon-reload
   sudo systemctl enable --now coinbase-sweeper-sweep
   ```

10. **Enable the refill service** (setups 1 and 2):

    ```sh
    sudo cp deploy/coinbase-sweeper-refill.service /etc/systemd/system/
    sudo systemctl daemon-reload
    sudo systemctl enable --now coinbase-sweeper-refill
    ```

11. **Confirm on chain.** Take a txid from an `event=swept` or `event=refilled` log line:

    ```sh
    zcash-cli getrawtransaction <txid> 1
    ```

    A sweep shows `"version": 6`, an empty `vout`, empty `vShieldedOutput`, and entries under
    `ironwood.actions`. A refill to a transparent address shows one `vout` paying your
    destination and nothing under `vShieldedSpend` or `vShieldedOutput`. The tool checks the
    same things itself and stops if they do not hold.

## 4. Rollback

Stop the services:

```sh
sudo systemctl disable --now coinbase-sweeper-sweep coinbase-sweeper-refill
```

Funds already in Ironwood stay in your wallet, at the pool's unified address, and stay
spendable: send them with `z_sendmany` from that address. Rewards not yet swept stay at your
transparent address. Before the upgrade you can turn your pool software's own sweep back on;
after it, sweeping into Sapling no longer works.

## 5. Monitoring

Every action is one log line of `key=value` pairs, with the command (`cmd=`), the address, the
number of inputs, the amount, the txid and the outcome. With systemd:
`journalctl -u coinbase-sweeper-sweep -f`. Watch for lines at ERROR level.

| Log line | Level | Meaning |
| --- | --- | --- |
| `event=swept ... txid=... outcome=ok` | INFO | A sweep went through and passed its check. `outcome=ok_recovered` or `ok_after_unknown`: one that an interrupted run started, found and checked now. |
| `event=refilled ... amount=... txid=...` | INFO | A refill went through. `sent_today=` is the day's total. |
| `event=nothing_due`, `event=refill_skip` | INFO | Nothing to do this cycle; `reason=` says why. |
| `event=wait_confirmation` | INFO | Waiting for a transaction to be mined before the next one. |
| `event=cycle_done ... rpc_calls=... millis=...` | INFO | A cycle finished. |
| `event=cycle_skipped` | WARN, then ERROR | The wallet is unreachable, still syncing, behind the node, or not connected to the network. The tool waits and retries; after `skipped_cycles_before_error` in a row it logs one ERROR and keeps retrying with longer pauses. `event=recovered` follows when it is back. |
| `event=unswept_alert` | ERROR | An address holds more unswept, mature coinbase than `alert_unswept_zec`. Sweeps are not keeping up, or are failing: read the lines before it. |
| `event=cycle_failed` | ERROR | A cycle failed; `err=` says why. The tool retries next cycle with nothing resent. |
| `event=node_rejected` | ERROR | The node refused a transaction (for example, right at an upgrade). The command pauses for `rejection_pause_secs`, then tries again. |
| `event=submit_unknown` | ERROR | A send got no usable answer. The tool never resends it; it settles what happened from the wallet first. |
| `event=refill_unsettled` | ERROR | An interrupted refill cannot be settled from the wallet. Refills stop until you resolve it (see troubleshooting). |
| `event=halted` | ERROR | A transaction did not pass its check (or could not be fetched to check it). Both commands stop sending until you investigate and clear the halt (see troubleshooting). |
| `event=refused` | ERROR | Startup refused; `reason=` says why. The service stays stopped (exit code 1). |

For a snapshot at any time, run `--check` (step 5).

## 6. Troubleshooting

| Error (in `reason=` or `err=`) | Likely cause | Fix |
| --- | --- | --- |
| `invalid config file ...` | A typo, an unknown key, or a value out of range. | The message names the key; see CONFIGURATION.md. |
| `the wallet refused the RPC credentials (HTTP 401)` | Wrong user or password, or an old cookie. | Check `rpc.user` and `SWEEPER_RPC_PASSWORD`, or `rpc.cookie_file`. |
| `SWEEPER_RPC_PASSWORD is not set` | No password given. | Set it in `/etc/coinbase-sweeper/env`, or use `rpc.cookie_file`. |
| `shielded_address ... has a Sapling receiver` | The pool address includes Sapling. | Create an Orchard-only address (step 2). |
| `shielded_address ... has no Orchard receiver` / `is not a unified address` | Wrong address in the config. | Use the address from step 2. |
| `shielded_address ... does not belong to this wallet` | The address was created in another wallet. | Create it in the wallet that holds the coinbase address. |
| `the wallet cannot sign for <address>` | A coinbase address the wallet does not own. | Fix `sweep.coinbase_addresses`, or import the key into the wallet. |
| `is not a transparent address on this network` | A shielded or wrong-network address in `coinbase_addresses`. | List only the transparent addresses your blocks pay. |
| `another ... instance is already running` | A second copy of the same command. | Stop the other copy (often the service while you run `--once`). |
| `reason=halted` | An earlier transaction failed its check, or could not be checked. | See "Clearing a halt" below. |
| `refill.destination ... does not belong to this wallet` | An external destination without the confirmation line. | If the address is right, add `external_destination` with the same address. |
| `external_destination ... does not match` | The two copies of the address differ. | Fix the typo. |
| `top_up mode needs a transparent destination this wallet owns` | `top_up` with a shielded or external destination. | Use `drain_above` for those. |
| `has no Orchard receiver, so a transfer to it could not stay in Ironwood` | A shielded destination without an Orchard receiver. | Use a unified address with an Orchard receiver, or a transparent address. |
| `the wallet has no connection to the node or network` | The wallet lost its node or peers. | Check the node; the tool resumes on its own. |
| `the wallet is N blocks behind the node` / `still syncing` | The wallet is catching up. | Wait; the tool resumes on its own. |
| `wallet RPC unreachable` / `no answer` / `busy or restarting` | The wallet is down or overloaded. | Check the wallet; the tool resumes on its own. |
| `waiting for block N after a sweep whose outcome was not seen` | The wallet restarted during a sweep. | Nothing: the tool waits one block, then continues. |
| `did not reach 1 confirmation(s)` | Blocks are slow, or the transaction is stuck. | Usually resolves next cycle. If it persists, check the txid in the wallet. |
| `sweep operation ... failed` / `refill operation ... failed` | The wallet could not build the transaction. | Read the wallet's message in the line. |
| `insufficient_funds` (`event=refill_skip`) | The source cannot cover the transfer plus the fee yet. | Wait for more notes to confirm, or lower `target`. |
| `would pass max_per_day` | The day's cap is reached. | Nothing, or raise `max_per_day` if the cap is too low. |
| `an interrupted refill ... is not settled yet` | A refill was interrupted recently. The tool waits the larger of 10 minutes and twice `rpc.submit_timeout_secs` before it will conclude that nothing was sent. | Nothing: the tool settles it on its own. |
| `an interrupted refill ... cannot be settled` | The tool cannot tell from the wallet whether an interrupted refill happened. | Check the destination's history (`listtransactions`, or the payout wallet). Stop the refill service, run `coinbase-sweeper refill --config ... --resolve-interrupted sent` or `not-sent`, then start the service again. The tool refuses a verdict the wallet contradicts. |
| `failed its check` (`event=halted`) | The wallet built a transaction that is not what was asked for. | Inspect the transaction (`getrawtransaction <txid> 1`); contact the wallet's maintainers. Clear the halt only once you understand it. |
| `cannot be fetched to check it` | The transaction cannot be found to verify. | Look the txid up by hand (in the wallet, or a block explorer). Once it is verified, clear the halt. |
| `state_write_failed` | The state directory is not writable or the disk is full. | Fix permissions or free space. The tool does not send while it cannot record. |

**Clearing a halt.** After a halt, both services stop (exit code 1) and stay stopped. Once you
have investigated the transaction named in the `event=halted` line:

```sh
sudo systemctl stop coinbase-sweeper-sweep coinbase-sweeper-refill
coinbase-sweeper sweep --config /etc/coinbase-sweeper/sweeper.toml --clear-halt    # if sweep halted
coinbase-sweeper refill --config /etc/coinbase-sweeper/sweeper.toml --clear-halt   # if refill halted
sudo systemctl start coinbase-sweeper-sweep coinbase-sweeper-refill
```

`--clear-halt` accepts the transaction that caused the halt (it is not checked again) and
removes that command's halt file; deleting the file by hand is not enough, because the same
check would halt the command again. A halt from either command stops both. Likewise, after
`--resolve-interrupted`, start the refill service again.

## 7. Security

- **Keys never leave the wallet.** The tool asks the wallet to build and sign each transaction
  over RPC. It never reads, stores or logs keys or seed phrases.
- **Scope the RPC credentials.** Keep the wallet's RPC port on localhost or a private network
  (the tool speaks plain HTTP). Give the tool its own RPC user where the wallet supports one
  (zcashd's `rpcauth`), and keep the password out of the config file: use
  `SWEEPER_RPC_PASSWORD` in a root-only environment file, or the cookie file. The wallet RPC
  can move all of the wallet's funds, so treat these credentials like a hot-wallet key.
- **Caps.** `refill.max_per_transfer` and `refill.max_per_day` bound what refill can move. The
  day's total is recorded before each transfer is sent, so a crash can only under-send. A
  transfer that would pass `max_per_day` is not sent at all (not even in part); the next UTC day
  starts a new total. Set `max_per_day` a little above what your pool pays out in a day, plus
  the first fill of the payout address on the day you start. Your own figures take precedence
  over this rule of thumb.
- **The destination is written twice** when the wallet does not own it (`destination` and
  `external_destination`). A typo in either stops the tool rather than sending money to the
  wrong place.
- **Checks on every transaction.** Each sweep must have no transparent outputs, no Sapling
  output, and at least one Ironwood action; when the wallet reports the amount shielded, the
  fee must also be at most 0.01 ZEC. Each refill must spend no
  Sapling and no transparent funds, pay only your destination (for a transparent destination,
  exactly the amount sent), and pay a fee within `fee_headroom`. For a shielded destination the
  tool cannot see the recipient inside the shielded output; it checks the rest. A failed check
  halts both commands.
- **One copy per command**, enforced by a lock file in `state_dir`.

## 8. FAQ

**What does it cost in fees?** The wallet pays the standard ZIP 317 fee; the tool never sets
one. On our test network a 50-input sweep paid 0.0026 ZEC and a refill 0.00015 to 0.0002 ZEC.

**What do the transactions reveal?** A sweep spends transparent outputs, so their addresses and
amounts are public, as with any sweep into Sapling today; what happens in Ironwood afterwards is
private. A refill to a transparent address reveals the amount and the recipient (it uses the
`AllowRevealedRecipients` policy); a refill to a shielded address uses `FullPrivacy`.

**What happens at the upgrade height?** Nothing changes for the tool: it never sends to Sapling.
A transaction built right at the boundary may be refused by the node; the tool logs
`event=node_rejected`, pauses for `rejection_pause_secs`, and tries again. Nothing is lost.

**zcashd or zecd?** Both work, with the same configuration. Differences you may notice:

- zecd sends the change of a payout from your transparent payout address to a new address of
  the wallet. In setup 1 the payout address's balance therefore drops by the full amount each
  payout spends, and refills top it up by that much; the change collects at the wallet's other
  addresses. Size `max_per_day` with that in mind.

- zecd has one account per wallet, so use account `0` in step 2.
- zcashd writes a new cookie file each time it starts; the tool reads it again automatically.
- zcashd passes transactions to the network over its peer connections, so one may take a few
  seconds longer to reach the miners than with zecd.

**Can I run only one of the commands?** Yes. Each command has its own lock, state and service.

**What if the wallet or the tool restarts in the middle of a transfer?** The tool never sends
twice. On restart it first finds out from the wallet what happened to anything it had started.
A sweep is found from the wallet's operation list, or waited out for one block. A refill is
settled from the operation list, the wallet history, and whether the notes it would spend are
still unspent. If a refill cannot be settled, it stops and asks you (see troubleshooting).

**Do rewards that matured while the tool was stopped get swept?** Yes, on the next cycle, up to
`max_batches_per_cycle` sweeps per cycle.
