// Toy "SaaS" HTTP server — simulates an external API with auth.
//
// Used by the E2E integration test to verify credential injection.
// Listens on 127.0.0.1:9091, requires Authorization: Bearer toy-saas-secret-token-xyz.

use axum::{
    Json, Router,
    extract::Request,
    http::StatusCode,
    middleware::{self, Next},
    response::{IntoResponse, Response},
    routing::{get, post},
};
use serde::{Deserialize, Serialize};
use serde_json::json;
use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};

const VALID_TOKEN: &str = "toy-saas-secret-token-xyz";

/// Shared state tracking request counts for verification.
#[derive(Debug, Default)]
pub struct ToySaasState {
    pub authed_requests: AtomicU64,
    pub unauthed_requests: AtomicU64,
}

/// Auth middleware — rejects requests without the correct Bearer token.
async fn require_auth(req: Request, next: Next) -> Response {
    let auth_header = req
        .headers()
        .get("authorization")
        .and_then(|v| v.to_str().ok())
        .map(|s| s.to_string());

    let state = req.extensions().get::<Arc<ToySaasState>>().cloned();

    match auth_header.as_deref() {
        Some(h) if h == format!("Bearer {VALID_TOKEN}") => {
            if let Some(s) = &state {
                s.authed_requests.fetch_add(1, Ordering::Relaxed);
            }
            next.run(req).await
        }
        _ => {
            if let Some(s) = &state {
                s.unauthed_requests.fetch_add(1, Ordering::Relaxed);
            }
            (
                StatusCode::UNAUTHORIZED,
                Json(
                    json!({"error": "unauthorized", "message": "missing or invalid Bearer token"}),
                ),
            )
                .into_response()
        }
    }
}

async fn get_me() -> impl IntoResponse {
    Json(json!({
        "id": "user-42",
        "name": "Test User",
        "email": "test@example.com"
    }))
}

async fn get_projects() -> impl IntoResponse {
    Json(json!({
        "projects": [
            {"id": "p1", "name": "Alpha"},
            {"id": "p2", "name": "Beta"}
        ]
    }))
}

/// GET /api/deals — deterministic CRM-style pipeline. Fixed data so E2E
/// assertions remain stable across runs.
async fn get_deals() -> impl IntoResponse {
    Json(json!({"deals": deals_fixture()}))
}

/// GET /api/contacts — deterministic contact roster linked to deals.
async fn get_contacts() -> impl IntoResponse {
    Json(json!({"contacts": contacts_fixture()}))
}

/// GET /api/pipeline/summary — aggregate stats (total value, stage breakdown,
/// at-risk count). Computed from the deals fixture.
async fn get_pipeline_summary() -> impl IntoResponse {
    let deals = deals_fixture();
    let mut total_value: f64 = 0.0;
    let mut stage_counts: std::collections::BTreeMap<String, u64> =
        std::collections::BTreeMap::new();
    let mut at_risk = 0u64;

    for deal in &deals {
        let amount = deal["amount"].as_f64().unwrap_or(0.0);
        total_value += amount;
        let stage = deal["stage"].as_str().unwrap_or("unknown").to_string();
        *stage_counts.entry(stage).or_insert(0) += 1;
        if deal["at_risk"].as_bool().unwrap_or(false) {
            at_risk += 1;
        }
    }

    Json(json!({
        "total_value": total_value,
        "stage_counts": stage_counts,
        "at_risk_count": at_risk,
        "deal_count": deals.len(),
    }))
}

fn deals_fixture() -> Vec<serde_json::Value> {
    let owners = ["alice", "bob", "carol", "dan"];
    let stages = [
        "discovery",
        "proposal",
        "negotiation",
        "closed_won",
        "closed_lost",
    ];
    let mut deals = Vec::with_capacity(24);
    // 24 deterministic deals. Amount, stage, dates are derived from index so the
    // fixture is stable. 4 of them are flagged at_risk.
    for i in 0..24u32 {
        let stage = stages[(i as usize) % stages.len()];
        let amount = 1_000.0 + (i as f64) * 5_750.0;
        let owner = owners[(i as usize) % owners.len()];
        // Close dates spread across Q1–Q3 2026. Day-of-month capped at 28 to
        // avoid month-length edge cases.
        let month = 1 + (i % 9);
        let day = 1 + (i % 28);
        let close_date = format!("2026-{month:02}-{day:02}");
        let last_activity = format!("2026-{month:02}-{:02}", day.clamp(1, 28));
        let at_risk = matches!(i, 3 | 9 | 15 | 21);
        deals.push(json!({
            "id": format!("deal-{i:03}"),
            "name": format!("Deal with Customer {}", i + 1),
            "amount": amount,
            "stage": stage,
            "close_date": close_date,
            "owner": owner,
            "last_activity_date": last_activity,
            "at_risk": at_risk,
        }));
    }
    deals
}

fn contacts_fixture() -> Vec<serde_json::Value> {
    (0..12u32)
        .map(|i| {
            // Each contact is linked to two deals to give agents a join to reason
            // about.
            let deal_ids: Vec<String> =
                vec![format!("deal-{:03}", i), format!("deal-{:03}", i + 12)];
            json!({
                "id": format!("contact-{i:03}"),
                "name": format!("Contact {}", i + 1),
                "email": format!("contact{}@example.com", i + 1),
                "company": format!("Customer {} Inc", i + 1),
                "deal_ids": deal_ids,
            })
        })
        .collect()
}

#[derive(Debug, Deserialize, Serialize)]
struct CreateNoteRequest {
    text: String,
}

async fn post_notes(Json(body): Json<CreateNoteRequest>) -> impl IntoResponse {
    (
        StatusCode::CREATED,
        Json(json!({
            "id": "note-123",
            "created": true,
            "text": body.text
        })),
    )
}

/// Build the toy SaaS router with shared state for request tracking.
pub fn build_toy_saas_router(state: Arc<ToySaasState>) -> Router {
    let api_routes = Router::new()
        .route("/api/me", get(get_me))
        .route("/api/projects", get(get_projects))
        .route("/api/notes", post(post_notes))
        .route("/api/deals", get(get_deals))
        .route("/api/contacts", get(get_contacts))
        .route("/api/pipeline/summary", get(get_pipeline_summary))
        .layer(middleware::from_fn(require_auth));

    // Health endpoint (no auth)
    let health = Router::new().route(
        "/health",
        get(|| async { Json(json!({"ok": true, "service": "toy-saas"})) }),
    );

    Router::new()
        .merge(api_routes)
        .merge(health)
        .layer(axum::Extension(state))
}

#[tokio::main]
async fn main() {
    let state = Arc::new(ToySaasState::default());
    let router = build_toy_saas_router(state);

    println!("Toy SaaS server listening on http://127.0.0.1:9091");
    let listener = tokio::net::TcpListener::bind("127.0.0.1:9091")
        .await
        .expect("failed to bind 9091");
    axum::serve(listener, router).await.unwrap();
}
