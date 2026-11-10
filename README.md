# Ukemi contract

Assurance crowdfunding on NEAR. Outcome rule: only `raised` versus `target`, decided once after the deadline.

| Method | Who | What |
|---|---|---|
| `create_campaign(title, description, target, duration_minutes)` payable | creator | Attached deposit is the Gold Cushion. Minutes, so demos can end in minutes. |
| `back(campaign_id)` payable | backer | Attached deposit is the pledge. Allowed until the deadline. |
| `finalize(campaign_id)` | anyone | After the deadline: `Funded` if raised >= target, else `Missed`. |
| `claim_refund(campaign_id)` | backer | Missed only: principal + pledge / raised * cushion. |
| `withdraw(campaign_id)` | creator | Funded: raised + cushion. Missed with no backers: cushion back. |

Views: `get_campaign_count`, `get_campaign`, `get_campaigns(from_index, limit)`, `get_pledge`, `get_claimable`.

Amounts are yoctoNEAR strings (1 NEAR = 10^24). Bonus uses floor division, so a few yocto of dust can stay in the contract.

Known limits (prototype): a refund or withdraw to an account that no longer exists is not rolled back; no pause or upgrade logic.

## Test and build
```
cargo test
cargo near build non-reproducible-wasm   # needs cargo-near
```

## Deployed contract

Live on NEAR testnet as `ukemi.testnet`.

## Frontend

`frontend/index.html` is a single-file web app (no build step) for exploring campaigns, simulating payouts, launching campaigns, and backing, finalizing, claiming and withdrawing.

It reads campaigns directly from `ukemi.testnet` over public testnet RPC endpoints, so browsing works without a wallet. Sending transactions needs a NEAR testnet wallet, connected through `@hot-labs/near-connect`.

Run it locally:

```
cd frontend
python3 -m http.server 8000
```

Then open http://localhost:8000. It needs internet access, because Tailwind, the wallet library and the fonts load from CDNs.

A creator cannot back their own campaign, so to try the full flow, create a campaign with one testnet account and back it with another.

## Screenshots

| | |
|---|---|
| **Landing** | ![Landing](docs/screenshots/01-landing.png) |
| **Outcome Simulator, funded (108%)** | ![Simulator funded](docs/screenshots/02-simulator-funded.png) |
| **Outcome Simulator, missed (65%)** | ![Simulator missed](docs/screenshots/03-simulator-missed.png) |
| **Explore, before pledge** | ![Explore before](docs/screenshots/04a-explore-before.png) |
| **Explore, after pledge** | ![Explore after](docs/screenshots/04-explore.png) |
| **Launch Campaign** | ![Launch](docs/screenshots/05-launch.png) |
| **My Backings, after claim** | ![My Backings](docs/screenshots/06-my-backings.png) |

## Proof on testnet

The full missed-target flow, run end to end against `ukemi.testnet` (campaign #10, "Solar Weather Stations for Rural Schools"). The backer pledged 2 NEAR, the target was missed, and the backer claimed 2.50 NEAR (100% principal plus the 0.50 NEAR Gold Cushion bonus, +25%).

| Time (UTC) | Step | Method | Transaction |
|---|---|---|---|
| 2026-11-10 19:03:19 | Back campaign (2 NEAR) | `back` | [view](https://testnet.nearblocks.io/txns/3PNoHPSmnyBiThGhkJbbCRDxaNXDnS3WiK5oauzcKfTz) |
| 2026-11-10 19:12:33 | Finalize after deadline | `finalize` | [view](https://testnet.nearblocks.io/txns/6hyBjwVoBgnpdXUBHtfjbBhBaC8E8U8xaJFaqc1SeUhX) |
| 2026-11-10 19:15:12 | Claim principal + bonus | `claim_refund` | [view](https://testnet.nearblocks.io/txns/HRR2fS6Pw4sKV7onc1ddcRfJsmWfrb8Dres12wWg2Dg6) |

Transaction screenshots: [pledge](docs/screenshots/07-tx-pledge.png), [finalize](docs/screenshots/08-tx-finalize.png), [claim](docs/screenshots/09-tx-claim.png).

The funded path was also run on testnet: campaign #1 ("Demo Funded") reached its target and the creator withdrew.
