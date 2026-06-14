use askama::Template;
use axum::{
    extract::State,
    http::StatusCode,
    response::{Html, IntoResponse, Response},
    Form,
};
use serde::Deserialize;
use tracing::error;

use crate::AppState;

struct UserOption {
    id: String,
    name: String,
}

#[derive(Template)]
#[template(path = "admin/homepage_layout.html")]
pub struct HomepageLayoutPageTemplate {
    pub ui_route: String,
}

#[derive(Template)]
#[template(path = "admin/homepage_layout_form.html")]
pub struct HomepageLayoutFormTemplate {
    ui_route: String,
    users: Vec<UserOption>,
    has_default: bool,
    message: String,
}

fn render<T: Template>(template: T) -> Response {
    match template.render() {
        Ok(html) => Html(html).into_response(),
        Err(e) => {
            error!("Failed to render homepage layout template: {}", e);
            (StatusCode::INTERNAL_SERVER_ERROR, "Template error").into_response()
        }
    }
}

pub async fn homepage_layout_page(State(state): State<AppState>) -> impl IntoResponse {
    render(HomepageLayoutPageTemplate {
        ui_route: state.get_ui_route().await,
    })
}

async fn build_form(state: &AppState, message: String) -> HomepageLayoutFormTemplate {
    let users = match state.user_authorization.list_users().await {
        Ok(list) => list
            .into_iter()
            .map(|u| UserOption {
                id: u.id,
                name: u.original_username,
            })
            .collect(),
        Err(e) => {
            error!("Failed to list users for homepage layout: {}", e);
            Vec::new()
        }
    };
    let has_default = state
        .display_preferences
        .has_default()
        .await
        .unwrap_or(false);
    HomepageLayoutFormTemplate {
        ui_route: state.get_ui_route().await,
        users,
        has_default,
        message,
    }
}

pub async fn homepage_layout_form(State(state): State<AppState>) -> impl IntoResponse {
    render(build_form(&state, String::new()).await)
}

#[derive(Deserialize)]
pub struct PromoteForm {
    pub user_id: String,
}

pub async fn promote_default(
    State(state): State<AppState>,
    Form(form): Form<PromoteForm>,
) -> impl IntoResponse {
    let message = if form.user_id.trim().is_empty() {
        "Select a user first.".to_string()
    } else {
        match state
            .display_preferences
            .promote_user_to_default(form.user_id.trim())
            .await
        {
            Ok(true) => "Default layout updated from the selected user.".to_string(),
            Ok(false) => {
                "That user hasn't customized their Home yet — set it up in the client first."
                    .to_string()
            }
            Err(e) => {
                error!("Failed to promote user to default: {}", e);
                "Failed to save default layout.".to_string()
            }
        }
    };
    render(build_form(&state, message).await)
}

pub async fn clear_default(State(state): State<AppState>) -> impl IntoResponse {
    let message = match state.display_preferences.clear_default().await {
        Ok(()) => "Default layout cleared.".to_string(),
        Err(e) => {
            error!("Failed to clear default layout: {}", e);
            "Failed to clear default layout.".to_string()
        }
    };
    render(build_form(&state, message).await)
}
