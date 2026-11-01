//! Read-only E2E receipt audit. No invoice creation or payment submission.
//! Paths arrive on stdin. Only the dedicated Sandbox fixture may be read.
use anyhow::{Context, ensure};
use nwc::{
    NostrWalletConnect,
    prelude::{
        ListTransactionsRequest, LookupInvoiceRequest, NostrWalletConnectUri, TransactionType,
    },
};
use sats_arena_core::wallet::{InvoiceState, NwcWallet, PaymentState, Wallet};
use serde::Deserialize;
use serde_json::{Value, json};
use std::{
    collections::BTreeMap,
    io::{self, Read},
    path::PathBuf,
};

#[derive(Deserialize)]
struct Input {
    fixture: PathBuf,
    state: PathBuf,
}

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    let mut input = String::new();
    io::stdin().take(32768).read_to_string(&mut input)?;
    let input: Input = serde_json::from_str(&input).context("Invalid audit input")?;
    ensure!(
        input
            .fixture
            .file_name()
            .is_some_and(|n| n == "wallets.json")
            && input
                .fixture
                .parent()
                .and_then(|p| p.file_name())
                .is_some_and(|n| n == "sandbox"),
        "Use the dedicated Sandbox wallet fixture"
    );
    ensure!(
        input
            .state
            .components()
            .any(|p| p.as_os_str() == "SatsArenaSandbox"),
        "Use a Sandbox match state"
    );
    let fixture: Value = serde_json::from_slice(&std::fs::read(&input.fixture)?)
        .context("Invalid Sandbox fixture")?;
    let wallets = fixture["wallets"]
        .as_array()
        .context("Missing Sandbox wallets")?;
    ensure!(
        fixture["version"] == 1 && wallets.len() == 5,
        "Incomplete Sandbox fixture"
    );
    let saved: Value = serde_json::from_slice(&std::fs::read(&input.state)?)?;
    ensure!(
        saved["simulated"] == false,
        "Simulated balances are not E2E receipts"
    );
    let uri = |i: usize| {
        wallets[i]["uri"]
            .as_str()
            .context("Missing Sandbox connection")
    };
    let host = NwcWallet::connect(uri(0)?).await?;
    ensure!(
        saved["wallet_identity"].as_str() == Some(host.identity().as_str()),
        "Fixture does not match the game wallet"
    );
    let pot = &saved["pot"];
    let entry = pot["entry_sats"].as_u64().context("Missing entry amount")?;
    let fee = (u128::from(entry) * u128::from(pot["surcharge_bps"].as_u64().unwrap_or(0)))
        .div_ceil(10_000) as u64;
    let mut incoming = Vec::new();
    for player in 0..4 {
        if pot["paid"][player] != true {
            continue;
        }
        let hash = saved["hashes"][player]
            .as_str()
            .context("Missing entry hash")?;
        ensure!(
            host.invoice_state(hash).await?
                == (InvoiceState::Paid {
                    amount_sats: entry + fee
                }),
            "Host did not confirm the expected incoming entry"
        );
        incoming.push(json!({"player_port": player + 1, "sats": entry + fee}));
    }
    let mut receivers = BTreeMap::new();
    let mut receiver_probes = BTreeMap::new();
    let mut confirmed = Vec::new();
    let mut unpaid_originals = Vec::new();
    let mut discarded = Vec::new();
    let mut outstanding = Vec::new();
    let payouts = pot["payouts"].as_array().context("Missing payouts")?;
    let probe = NostrWalletConnect::builder(
        NostrWalletConnectUri::parse(uri(0)?)
            .map_err(|_| anyhow::anyhow!("Invalid Sandbox connection"))?,
    )
    .timeout(std::time::Duration::from_secs(12))
    .build();
    // shortcut: inspect only the latest 200 outgoing records for diagnostics;
    // paginate if older failures need investigation. Absence proves no outcome.
    let history = if payouts.iter().any(|p| {
        !matches!(
            p["state"].as_str(),
            Some("Paid" | "Superseded" | "Discarded")
        )
    }) {
        probe
            .list_transactions(ListTransactionsRequest {
                transaction_type: Some(TransactionType::Outgoing),
                unpaid: Some(true),
                limit: Some(200),
                ..Default::default()
            })
            .await
            .ok()
    } else {
        None
    };
    let groups = saved["payout_groups"].as_object();
    let mut transfers = BTreeMap::new();
    for payout in payouts {
        let id = payout["id"].as_str().context("Missing payout ID")?;
        let state = payout["state"].as_str().context("Missing payout state")?;
        let player = payout["player"].as_u64().context("Missing payout player")? as usize;
        ensure!(player < 4, "Invalid payout player");
        let prepared = &saved["prepared"][id];
        if state == "Discarded" {
            discarded.push(id);
        }
        if prepared.is_null() && !matches!(state, "Paid" | "Superseded" | "Discarded") {
            outstanding.push(json!({"id": id, "local_state": state, "prepared": false}));
            continue;
        }
        if matches!(state, "Superseded" | "Discarded") && prepared.is_null() {
            continue;
        }
        let hash = prepared["payment_hash"]
            .as_str()
            .context("Missing bound payment hash")?;
        ensure!(
            prepared["address"] == wallets[player + 1]["address"],
            "Payout is bound to a different receiver"
        );
        if let std::collections::btree_map::Entry::Vacant(entry) = receivers.entry(player) {
            entry.insert(NwcWallet::connect(uri(player + 1)?).await?);
            receiver_probes.insert(
                player,
                NostrWalletConnect::builder(
                    NostrWalletConnectUri::parse(uri(player + 1)?)
                        .map_err(|_| anyhow::anyhow!("Invalid Sandbox connection"))?,
                )
                .timeout(std::time::Duration::from_secs(12))
                .build(),
            );
        }
        if !matches!(state, "Paid" | "Superseded" | "Discarded") {
            let lookup = match probe
                .lookup_invoice(LookupInvoiceRequest {
                    payment_hash: Some(hash.into()),
                    invoice: None,
                })
                .await
            {
                Ok(value) => json!({"type": format!("{:?}", value.transaction_type),
                                   "state": format!("{:?}", value.state)}),
                Err(error) => {
                    // The SDK flattens provider codes into text. Emit only an
                    // allowlisted suffix, never the provider's free-form message.
                    let message = error.to_string();
                    let code = [
                        "NotFound",
                        "RateLimited",
                        "InsufficientBalance",
                        "QuotaExceeded",
                        "Restricted",
                        "Unauthorized",
                        "PaymentFailed",
                        "NotImplemented",
                        "Internal",
                        "Other",
                    ]
                    .into_iter()
                    .find(|code| message.ends_with(&format!("[{code}]")));
                    json!({"error_kind": format!("{:?}", error.kind()), "code": code})
                }
            };
            outstanding.push(json!({
                "id": id, "local_state": state,
                "host_status": format!("{:?}", host.payment_state(hash).await),
                "receiver_status": format!("{:?}", receivers[&player].invoice_state(hash).await),
                "provider_lookup": lookup,
                "outgoing_history": history.as_ref().map(|rows| rows.iter()
                    .filter(|r| r.payment_hash == hash)
                    .map(|r| json!({"state": format!("{:?}", r.state),
                        "settled_at": r.settled_at, "amount_msat": r.amount}))
                    .collect::<Vec<_>>()),
            }));
            continue;
        }
        let received = receivers[&player].invoice_state(hash).await?;
        if matches!(state, "Superseded" | "Discarded") {
            ensure!(
                !matches!(received, InvoiceState::Paid { .. }),
                "An unsent consolidated or discarded invoice was paid"
            );
            if state == "Superseded" {
                unpaid_originals.push(id);
            }
            continue;
        }
        let sats = payout["sats"].as_u64().context("Missing payout amount")?;
        let group = groups.and_then(|groups| {
            groups.iter().find(|(_, members)| {
                members
                    .as_array()
                    .is_some_and(|members| members.iter().any(|m| m.as_str() == Some(id)))
            })
        });
        let (transfer_id, transfer_sats) = if let Some((leader, members)) = group {
            let members = members.as_array().context("Invalid combined transfer")?;
            ensure!(
                members.iter().any(|m| m.as_str() == Some(leader.as_str())),
                "Missing transfer leader"
            );
            let mut total = 0u64;
            for member in members {
                let member = member.as_str().context("Invalid transfer allocation")?;
                let part = payouts
                    .iter()
                    .find(|p| p["id"] == member)
                    .context("Missing transfer allocation")?;
                ensure!(
                    part["state"] == "Paid"
                        && saved["prepared"][member]["payment_hash"] == hash
                        && saved["prepared"][member]["invoice"] == prepared["invoice"]
                        && saved["prepared"][member]["address"] == prepared["address"],
                    "Inconsistent transfer allocation"
                );
                total = total
                    .checked_add(part["sats"].as_u64().context("Invalid allocation amount")?)
                    .context("Transfer overflow")?;
            }
            (leader.as_str(), total)
        } else {
            (id, sats)
        };
        ensure!(
            received
                == (InvoiceState::Paid {
                    amount_sats: transfer_sats
                }),
            "Receiver has not confirmed the exact payout amount"
        );
        ensure!(
            matches!(
                host.payment_state(hash).await?,
                PaymentState::Succeeded { .. }
            ),
            "Host has not confirmed the outgoing payout"
        );
        let context = receiver_probes[&player]
            .lookup_invoice(LookupInvoiceRequest {
                payment_hash: Some(hash.into()),
                invoice: None,
            })
            .await?;
        let physical = json!({"transfer_id": transfer_id, "payment_hash": hash, "sats": transfer_sats,
            "address": prepared["address"]});
        if let Some(previous) = transfers.insert(hash.to_owned(), physical.clone()) {
            ensure!(
                previous == physical,
                "A payment hash was reused outside its transfer"
            );
        }
        confirmed.push(json!({"id": id, "player_port": player + 1, "sats": sats,
            "transfer_id": transfer_id, "transfer_sats": transfer_sats, "payment_hash": hash,
            "memo": prepared["memo"], "wallet_description": context.description,
            "wallet_comment": context.metadata.as_ref().and_then(|m| m.get("comment")).and_then(Value::as_str)}));
    }
    for receiver in receivers.values() {
        receiver.shutdown().await;
    }
    for receiver in receiver_probes.values() {
        receiver.shutdown().await;
    }
    host.shutdown().await;
    probe.shutdown().await;
    println!(
        "{}",
        serde_json::to_string_pretty(&json!({
            "match_id": pot["id"], "incoming_entries": incoming,
            "receiver_confirmed_payouts": confirmed,
            "physical_transfers": transfers.into_values().collect::<Vec<_>>(),
            "unpaid_consolidated_originals": unpaid_originals,
            "discarded_payouts": discarded,
            "outstanding": outstanding,
            "all_payouts_confirmed": outstanding.is_empty() && discarded.is_empty(),
            "simulated": false,
        }))?
    );
    ensure!(
        outstanding.is_empty(),
        "The game still has unresolved payouts"
    );
    Ok(())
}
