use std::net::{IpAddr, Ipv4Addr, SocketAddr};

use axum::Json;
use axum::extract::{Query, State};
use axum::http::StatusCode;
use axum::response::{IntoResponse, Response};
use serde::{Deserialize, Serialize};

/// The daemon listens here. No flag, variable or file can change it.
pub const ADDR: SocketAddr = SocketAddr::new(IpAddr::V4(Ipv4Addr::LOCALHOST), 7268);

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct SearchResponse {
    pub query: String,
    pub results: Vec<ResultItem>,
    pub engines: Vec<EngineStatus>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ResultItem {
    pub url: String,
    pub title: String,
    pub content: String,
    pub engine: String,
    pub position: u32,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum EngineState {
    Ok,
    Empty,
    Error,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct EngineStatus {
    pub name: String,
    pub state: EngineState,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub error: Option<String>,
}

#[derive(Debug, Deserialize)]
struct SearchParams {
    q: Option<String>,
}

#[derive(Debug, Serialize)]
struct ErrorBody {
    error: &'static str,
}

pub fn router(client: wreq::Client) -> axum::Router {
    axum::Router::new()
        .route("/search", axum::routing::get(get_search))
        .with_state(client)
}

async fn get_search(
    State(client): State<wreq::Client>,
    Query(params): Query<SearchParams>,
) -> Response {
    let Some(query) = params.q.as_deref().map(str::trim).filter(|q| !q.is_empty()) else {
        return (
            StatusCode::BAD_REQUEST,
            Json(ErrorBody {
                error: "the query parameter q is missing",
            }),
        )
            .into_response();
    };
    let body = crate::search::search(&client, query).await;
    (StatusCode::OK, Json(body)).into_response()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn daemon_listens_on_ipv4_localhost_only() {
        assert_eq!(ADDR.ip(), IpAddr::V4(Ipv4Addr::new(127, 0, 0, 1)));
        assert!(ADDR.ip().is_loopback());
        assert_eq!(ADDR.port(), 7268);
    }
}
