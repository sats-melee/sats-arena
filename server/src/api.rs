use std::sync::{Arc, Mutex};

use axum::{
    Json, Router,
    extract::{Path, State},
    http::{HeaderMap, StatusCode, header},
    response::{IntoResponse, Response},
    routing::{get, post},
};
use sats_arena_protocol::{
    Account, Deposit, DepositRequest, ErrorResponse, MatchReport, MatchStatus, RegisterRequest, RegisterResponse,
    Withdrawal, WithdrawalRequest,
};

use crate::{
    config::Rules,
    ledger::{Ledger, LedgerError},
    reconcile,
    wallet::{PaymentState, Wallet},
};

pub struct AppState {
    pub ledger: Mutex<Ledger>,
    pub wallet: Box<dyn Wallet>,
    pub rules: Rules,
    pub invite_code: String,
}

pub type Shared = Arc<AppState>;

pub fn router(state: Shared) -> Router {
    Router::new()
        .route("/health", get(|| async { "ok" }))
        .route("/v1/register", post(register))
        .route("/v1/account", get(account))
        .route("/v1/deposits", post(create_deposit))
        .route("/v1/deposits/{id}", get(get_deposit))
        .route("/v1/withdrawals", post(create_withdrawal))
        .route("/v1/withdrawals/{id}", get(get_withdrawal))
        .route("/v1/matches/report", post(report_match))
        .with_state(state)
}

pub enum ApiError {
    Status(StatusCode, String),
    Internal(anyhow::Error),
}

impl From<LedgerError> for ApiError {
    fn from(error: LedgerError) -> Self {
        match error {
            LedgerError::Rejected(message) => Self::Status(StatusCode::BAD_REQUEST, message),
            LedgerError::Internal(error) => Self::Internal(error),
        }
    }
}

impl From<anyhow::Error> for ApiError {
    fn from(error: anyhow::Error) -> Self {
        Self::Internal(error)
    }
}

impl IntoResponse for ApiError {
    fn into_response(self) -> Response {
        let (status, error) = match self {
            Self::Status(status, error) => (status, error),
            Self::Internal(error) => {
                tracing::error!("{error:#}");
                (StatusCode::INTERNAL_SERVER_ERROR, "internal error".to_string())
            }
        };
        (status, Json(ErrorResponse { error })).into_response()
    }
}

type ApiResult<T> = Result<Json<T>, ApiError>;

fn bad_request(message: impl Into<String>) -> ApiError {
    ApiError::Status(StatusCode::BAD_REQUEST, message.into())
}

fn not_found() -> ApiError {
    ApiError::Status(StatusCode::NOT_FOUND, "not found".into())
}

fn authenticate(state: &AppState, headers: &HeaderMap) -> Result<String, ApiError> {
    let unauthorized = || ApiError::Status(StatusCode::UNAUTHORIZED, "missing or invalid token".into());
    let token = headers
        .get(header::AUTHORIZATION)
        .and_then(|v| v.to_str().ok())
        .and_then(|v| v.strip_prefix("Bearer "))
        .ok_or_else(unauthorized)?;
    state.ledger.lock().unwrap().authenticate(token)?.ok_or_else(unauthorized)
}

/// Covers routing fees on withdrawals; whatever isn't spent is refunded.
pub fn fee_reserve(amount_sats: u64) -> u64 {
    (amount_sats / 100).max(10)
}

async fn register(State(state): State<Shared>, Json(req): Json<RegisterRequest>) -> ApiResult<RegisterResponse> {
    if req.invite_code != state.invite_code {
        return Err(ApiError::Status(StatusCode::FORBIDDEN, "wrong invite code".into()));
    }
    let (player_id, token) = state.ledger.lock().unwrap().register(&req.name)?;
    Ok(Json(RegisterResponse { player_id, token }))
}

async fn account(State(state): State<Shared>, headers: HeaderMap) -> ApiResult<Account> {
    let player = authenticate(&state, &headers)?;
    Ok(Json(state.ledger.lock().unwrap().account(&player)?))
}

async fn create_deposit(
    State(state): State<Shared>,
    headers: HeaderMap,
    Json(req): Json<DepositRequest>,
) -> ApiResult<Deposit> {
    let player = authenticate(&state, &headers)?;
    if req.amount_sats == 0 || req.amount_sats > state.rules.max_deposit_sats {
        return Err(bad_request(format!("deposit must be 1-{} sats", state.rules.max_deposit_sats)));
    }
    let invoice = state
        .wallet
        .create_invoice(req.amount_sats, "Sats Arena deposit", state.rules.invoice_expiry_secs)
        .await?;
    let deposit =
        state.ledger.lock().unwrap().insert_deposit(&player, req.amount_sats, &invoice.bolt11, &invoice.payment_hash)?;
    Ok(Json(deposit))
}

async fn get_deposit(State(state): State<Shared>, headers: HeaderMap, Path(id): Path<String>) -> ApiResult<Deposit> {
    let player = authenticate(&state, &headers)?;
    let (_, payment_hash) = state.ledger.lock().unwrap().deposit(&player, &id)?.ok_or_else(not_found)?;
    // Check now instead of waiting for the background loop.
    reconcile::check_deposit(&state, &id, &payment_hash).await?;
    let (deposit, _) = state.ledger.lock().unwrap().deposit(&player, &id)?.ok_or_else(not_found)?;
    Ok(Json(deposit))
}

async fn create_withdrawal(
    State(state): State<Shared>,
    headers: HeaderMap,
    Json(req): Json<WithdrawalRequest>,
) -> ApiResult<Withdrawal> {
    let player = authenticate(&state, &headers)?;
    let invoice = state.wallet.decode_invoice(req.bolt11.trim()).map_err(|e| bad_request(format!("{e:#}")))?;
    match invoice.amount_sats {
        Some(amount) if amount == req.amount_sats && amount > 0 => {}
        Some(_) => return Err(bad_request("amount_sats doesn't match the invoice")),
        None => return Err(bad_request("invoices without an amount aren't supported")),
    }

    let id = state.ledger.lock().unwrap().reserve_withdrawal(
        &player,
        req.amount_sats,
        fee_reserve(req.amount_sats),
        req.bolt11.trim(),
        &invoice.payment_hash,
    )?;

    // The funds are reserved before paying. An unclear result stays pending
    // and the reconciler checks it later rather than paying again.
    match state.wallet.pay_invoice(req.bolt11.trim(), fee_reserve(req.amount_sats)).await {
        Ok(PaymentState::Succeeded { fee_sats }) => state.ledger.lock().unwrap().finish_withdrawal(&id, Some(fee_sats), None)?,
        Ok(PaymentState::Failed { reason }) => state.ledger.lock().unwrap().finish_withdrawal(&id, None, Some(&reason))?,
        Ok(PaymentState::Pending) => {}
        Err(error) => tracing::warn!("withdrawal {id} outcome unknown: {error:#}"),
    }
    Ok(Json(state.ledger.lock().unwrap().withdrawal(&player, &id)?.ok_or_else(not_found)?))
}

async fn get_withdrawal(
    State(state): State<Shared>,
    headers: HeaderMap,
    Path(id): Path<String>,
) -> ApiResult<Withdrawal> {
    let player = authenticate(&state, &headers)?;
    Ok(Json(state.ledger.lock().unwrap().withdrawal(&player, &id)?.ok_or_else(not_found)?))
}

async fn report_match(
    State(state): State<Shared>,
    headers: HeaderMap,
    Json(report): Json<MatchReport>,
) -> ApiResult<MatchStatus> {
    let player = authenticate(&state, &headers)?;
    let status = state.ledger.lock().unwrap().submit_report(&player, &report, &state.rules)?;
    if status == MatchStatus::Disputed {
        tracing::warn!("match {} is disputed and needs manual review", report.match_id);
    }
    Ok(Json(status))
}
