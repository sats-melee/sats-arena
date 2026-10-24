use std::sync::Arc;

use axum::{
    Json, Router,
    extract::{Path, State},
    http::{HeaderMap, StatusCode, header},
    response::{IntoResponse, Response},
    routing::{get, post},
};
use sats_arena_protocol::{
    Account, Deposit, DepositRequest, ErrorResponse, MatchReport, MatchStatus, RegisterRequest,
    RegisterResponse, Withdrawal, WithdrawalRequest,
};

use sats_arena_core::{Engine, Error};
use sats_arena_protocol::{Lobby, LobbyRequest};

pub struct AppState {
    pub engine: Engine,
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
        .route("/v1/lobbies", post(create_lobby))
        .route("/v1/lobbies/{id}", get(get_lobby))
        .route("/v1/lobbies/{id}/ready", post(ready_lobby))
        .route("/v1/lobbies/{id}/start", post(start_lobby))
        .route("/v1/lobbies/{id}/cancel", post(cancel_lobby))
        .with_state(state)
}

pub enum ApiError {
    Status(StatusCode, String),
    Internal(anyhow::Error),
}

impl From<Error> for ApiError {
    fn from(error: Error) -> Self {
        match error {
            Error::Rejected(message) => Self::Status(StatusCode::BAD_REQUEST, message),
            Error::Internal(error) => Self::Internal(error),
            Error::Unauthorized => Self::Status(StatusCode::UNAUTHORIZED, error.to_string()),
            Error::NotFound => Self::Status(StatusCode::NOT_FOUND, error.to_string()),
            Error::Closed | Error::WalletUnavailable => {
                Self::Status(StatusCode::SERVICE_UNAVAILABLE, error.to_string())
            }
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
                (
                    StatusCode::INTERNAL_SERVER_ERROR,
                    "internal error".to_string(),
                )
            }
        };
        (status, Json(ErrorResponse { error })).into_response()
    }
}

type ApiResult<T> = Result<Json<T>, ApiError>;

async fn create_lobby(
    State(state): State<Shared>,
    headers: HeaderMap,
    Json(request): Json<LobbyRequest>,
) -> ApiResult<Lobby> {
    Ok(Json(
        state.engine.create_lobby(token(&headers)?, request).await?,
    ))
}
async fn get_lobby(
    State(state): State<Shared>,
    headers: HeaderMap,
    Path(id): Path<String>,
) -> ApiResult<Lobby> {
    Ok(Json(state.engine.lobby(token(&headers)?, &id).await?))
}
async fn ready_lobby(
    State(state): State<Shared>,
    headers: HeaderMap,
    Path(id): Path<String>,
) -> ApiResult<Lobby> {
    Ok(Json(state.engine.ready_lobby(token(&headers)?, &id).await?))
}
async fn start_lobby(
    State(state): State<Shared>,
    headers: HeaderMap,
    Path(id): Path<String>,
) -> ApiResult<Lobby> {
    Ok(Json(state.engine.start_lobby(token(&headers)?, &id).await?))
}
async fn cancel_lobby(
    State(state): State<Shared>,
    headers: HeaderMap,
    Path(id): Path<String>,
) -> ApiResult<Lobby> {
    Ok(Json(
        state.engine.cancel_lobby(token(&headers)?, &id).await?,
    ))
}

fn token(headers: &HeaderMap) -> Result<&str, ApiError> {
    let unauthorized =
        || ApiError::Status(StatusCode::UNAUTHORIZED, "missing or invalid token".into());
    let token = headers
        .get(header::AUTHORIZATION)
        .and_then(|v| v.to_str().ok())
        .and_then(|v| v.strip_prefix("Bearer "))
        .ok_or_else(unauthorized)?;
    Ok(token)
}

async fn register(
    State(state): State<Shared>,
    Json(req): Json<RegisterRequest>,
) -> ApiResult<RegisterResponse> {
    if req.invite_code != state.invite_code {
        return Err(ApiError::Status(
            StatusCode::FORBIDDEN,
            "wrong invite code".into(),
        ));
    }
    Ok(Json(state.engine.register(&req.name).await?))
}

async fn account(State(state): State<Shared>, headers: HeaderMap) -> ApiResult<Account> {
    Ok(Json(state.engine.account(token(&headers)?).await?))
}

async fn create_deposit(
    State(state): State<Shared>,
    headers: HeaderMap,
    Json(req): Json<DepositRequest>,
) -> ApiResult<Deposit> {
    Ok(Json(
        state
            .engine
            .create_deposit(token(&headers)?, req.amount_sats)
            .await?,
    ))
}

async fn get_deposit(
    State(state): State<Shared>,
    headers: HeaderMap,
    Path(id): Path<String>,
) -> ApiResult<Deposit> {
    Ok(Json(state.engine.deposit(token(&headers)?, &id).await?))
}

async fn create_withdrawal(
    State(state): State<Shared>,
    headers: HeaderMap,
    Json(req): Json<WithdrawalRequest>,
) -> ApiResult<Withdrawal> {
    Ok(Json(state.engine.withdraw(token(&headers)?, req).await?))
}

async fn get_withdrawal(
    State(state): State<Shared>,
    headers: HeaderMap,
    Path(id): Path<String>,
) -> ApiResult<Withdrawal> {
    Ok(Json(state.engine.withdrawal(token(&headers)?, &id).await?))
}

async fn report_match(
    State(state): State<Shared>,
    headers: HeaderMap,
    Json(report): Json<MatchReport>,
) -> ApiResult<MatchStatus> {
    let match_id = report.match_id.clone();
    let status = state.engine.report_match(token(&headers)?, report).await?;
    if status == MatchStatus::Disputed {
        tracing::warn!("match {match_id} is disputed and needs manual review");
    }
    Ok(Json(status))
}

#[cfg(test)]
mod tests {
    use super::*;
    use axum::{
        body::{Body, to_bytes},
        http::Request,
    };
    use sats_arena_core::{Rules, wallet::FakeWallet};
    use serde_json::{Value, json};
    use tower::ServiceExt;

    async fn request(
        app: &Router,
        method: &str,
        path: &str,
        token: Option<&str>,
        body: Value,
    ) -> (StatusCode, Value) {
        let mut builder = Request::builder()
            .method(method)
            .uri(path)
            .header("content-type", "application/json");
        if let Some(token) = token {
            builder = builder.header("authorization", format!("Bearer {token}"));
        }
        let response = app
            .clone()
            .oneshot(builder.body(Body::from(body.to_string())).unwrap())
            .await
            .unwrap();
        let status = response.status();
        let data = to_bytes(response.into_body(), 1024 * 1024).await.unwrap();
        (status, serde_json::from_slice(&data).unwrap())
    }

    #[tokio::test]
    async fn http_is_only_a_transport_for_the_shared_engine() {
        let wallet = Arc::new(FakeWallet::auto_pay());
        let engine = Engine::open(
            std::path::Path::new(":memory:"),
            Rules::default(),
            wallet.clone(),
        )
        .unwrap();
        let app = router(Arc::new(AppState {
            engine: engine.clone(),
            invite_code: "friends".into(),
        }));
        let (status, _) = request(
            &app,
            "POST",
            "/v1/register",
            None,
            json!({"name":"Alice", "invite_code":"wrong"}),
        )
        .await;
        assert_eq!(status, StatusCode::FORBIDDEN);
        let (status, player) = request(
            &app,
            "POST",
            "/v1/register",
            None,
            json!({"name":"Alice", "invite_code":"friends"}),
        )
        .await;
        assert_eq!(status, StatusCode::OK);
        let token = player["token"].as_str().unwrap();
        assert_eq!(engine.account(token).await.unwrap().balance_sats, 0);

        let (status, deposit) = request(
            &app,
            "POST",
            "/v1/deposits",
            Some(token),
            json!({"amount_sats":1000}),
        )
        .await;
        assert_eq!(status, StatusCode::OK);
        assert_eq!(deposit["status"], "pending");
        assert_eq!(engine.account(token).await.unwrap().balance_sats, 0);
        let id = deposit["id"].as_str().unwrap();
        let (status, deposit) = request(
            &app,
            "GET",
            &format!("/v1/deposits/{id}"),
            Some(token),
            json!(null),
        )
        .await;
        assert_eq!(status, StatusCode::OK);
        assert_eq!(deposit["status"], "credited");

        let bob = engine.register("Bob").await.unwrap();
        let lobby = json!({"id":"http-lobby", "mode":"netplay", "budget_sats":100,
            "players":[{"port":0,"player_id":player["player_id"]},{"port":1,"player_id":bob.player_id}]});
        assert_eq!(
            request(&app, "POST", "/v1/lobbies", Some(token), lobby)
                .await
                .0,
            StatusCode::OK
        );
        assert_eq!(
            request(
                &app,
                "POST",
                "/v1/lobbies/http-lobby/start",
                Some(token),
                json!(null)
            )
            .await
            .0,
            StatusCode::BAD_REQUEST
        );
        assert_eq!(
            request(
                &app,
                "POST",
                "/v1/lobbies/http-lobby/ready",
                Some(token),
                json!(null)
            )
            .await
            .0,
            StatusCode::OK
        );
        assert_eq!(engine.account(token).await.unwrap().balance_sats, 900);
        assert_eq!(
            request(
                &app,
                "POST",
                "/v1/lobbies/http-lobby/cancel",
                Some(token),
                json!(null)
            )
            .await
            .0,
            StatusCode::OK
        );
        assert_eq!(engine.account(token).await.unwrap().balance_sats, 1000);

        let body = json!({"amount_sats":500,"bolt11":"lnfake:500:once-over-http"});
        let (status, payout) =
            request(&app, "POST", "/v1/withdrawals", Some(token), body.clone()).await;
        assert_eq!(status, StatusCode::OK);
        assert_eq!(payout["status"], "completed");
        let (_, retry) = request(&app, "POST", "/v1/withdrawals", Some(token), body).await;
        assert_eq!(retry, payout);
        assert_eq!(wallet.payment_attempts(), 1);
        let (status, account) = request(&app, "GET", "/v1/account", Some(token), json!(null)).await;
        assert_eq!(status, StatusCode::OK);
        assert_eq!(
            account["balance_sats"],
            engine.account(token).await.unwrap().balance_sats
        );
        assert_eq!(account["balance_sats"], 500);
        assert_eq!(
            request(&app, "GET", "/v1/account", None, json!(null))
                .await
                .0,
            StatusCode::UNAUTHORIZED
        );
        assert_eq!(
            request(&app, "GET", "/v1/account", Some("bad-token"), json!(null))
                .await
                .0,
            StatusCode::UNAUTHORIZED
        );
        engine.shutdown().await.unwrap();
    }
}
