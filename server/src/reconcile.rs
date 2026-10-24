//! Background checks for deposits and withdrawals whose final state the
//! server hasn't seen yet, such as after a restart or a lost notification.

use std::time::Duration;

use crate::{
    api::{AppState, Shared},
    ledger,
    wallet::{InvoiceState, PaymentState},
};

pub async fn run(state: Shared) {
    let mut interval = tokio::time::interval(Duration::from_secs(15));
    loop {
        interval.tick().await;
        if let Err(error) = pass(&state).await {
            tracing::warn!("reconcile pass failed: {error:?}");
        }
    }
}

async fn pass(state: &AppState) -> ledger::Result<()> {
    let deposits = state.ledger.lock().unwrap().pending_deposits()?;
    for (id, payment_hash) in deposits {
        check_deposit(state, &id, &payment_hash).await?;
    }
    let withdrawals = state.ledger.lock().unwrap().pending_withdrawals()?;
    for w in withdrawals {
        match state.wallet.payment_state(&w.payment_hash).await {
            Ok(PaymentState::Succeeded { fee_sats }) => {
                state.ledger.lock().unwrap().finish_withdrawal(&w.id, Some(fee_sats), None)?
            }
            Ok(PaymentState::Failed { reason }) => {
                state.ledger.lock().unwrap().finish_withdrawal(&w.id, None, Some(&reason))?
            }
            Ok(PaymentState::Pending) => {}
            Err(error) => tracing::warn!("couldn't check withdrawal {}: {error:#}", w.id),
        }
    }
    Ok(())
}

pub async fn check_deposit(state: &AppState, id: &str, payment_hash: &str) -> ledger::Result<()> {
    match state.wallet.invoice_state(payment_hash).await {
        Ok(InvoiceState::Paid) => state.ledger.lock().unwrap().credit_deposit(id),
        Ok(InvoiceState::Expired) => state.ledger.lock().unwrap().expire_deposit(id),
        Ok(InvoiceState::Pending) => Ok(()),
        Err(error) => {
            tracing::warn!("couldn't check deposit {id}: {error:#}");
            Ok(())
        }
    }
}
