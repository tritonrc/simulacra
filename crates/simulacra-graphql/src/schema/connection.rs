use base64::{Engine, engine::general_purpose::STANDARD_NO_PAD as B64};

#[derive(async_graphql::SimpleObject, Clone, Debug, PartialEq, Eq)]
pub struct PageInfoExt {
    pub has_next_page: bool,
    pub has_previous_page: bool,
    pub start_cursor: Option<String>,
    pub end_cursor: Option<String>,
}

#[derive(async_graphql::InputObject, Clone, Debug, Default)]
pub struct PageInput {
    pub first: Option<i32>,
    pub after: Option<String>,
    pub last: Option<i32>,
    pub before: Option<String>,
}

pub fn encode_cursor(created_at: chrono::DateTime<chrono::Utc>, id: &str) -> String {
    B64.encode(format!("{}|{id}", created_at.to_rfc3339()))
}
