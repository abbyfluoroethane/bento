//! The viewer's account: identity, SSH keys, theme, sign-out.

use axum::extract::{Path as AxumPath, State};
use axum::http::StatusCode;
use axum::response::Response;
use axum::{Extension, Form};
use bento_types::User;
use russh::keys::{HashAlg, PublicKey};
use serde::{Deserialize, Serialize};

use super::{Page, Params, Viewer, done, failed, relative, render, shell, urlencode};
use crate::{AppState, error_parts};

#[derive(Debug, Serialize)]
struct KeyRow {
    id: i64,
    fingerprint: String,
    comment: String,
    added: String,
}

#[derive(Debug, Serialize)]
struct AccountData {
    title: &'static str,
    tab: &'static str,
    keys: Vec<KeyRow>,
    ssh_hint: String,
    error: Option<String>,
}

async fn render_account(
    state: &AppState,
    user: &User,
    params: &std::collections::HashMap<String, String>,
    status: StatusCode,
    error: Option<String>,
) -> Response {
    let path = "/settings/account";
    let shell = match shell(state, user, path, params).await {
        Ok(shell) => shell,
        Err(error) => return failed(state, user, path, error).await,
    };
    let keys = match state.0.store.ssh_keys_for_user(user.id).await {
        Ok(keys) => keys
            .into_iter()
            .map(|key| KeyRow {
                id: key.id,
                fingerprint: key.fingerprint,
                comment: key.comment,
                added: relative(Some(key.created_at)),
            })
            .collect(),
        Err(error) => return failed(state, user, path, error).await,
    };
    render(
        status,
        "account.html",
        &Page {
            data: AccountData {
                title: "Settings",
                tab: "account",
                keys,
                ssh_hint: format!("ssh {} ls", shell.base_domain),
                error,
            },
            shell,
        },
    )
}

pub(crate) async fn account(
    State(state): State<AppState>,
    Extension(user): Viewer,
    axum::extract::Query(params): Params,
) -> Response {
    render_account(&state, &user, &params, StatusCode::OK, None).await
}

#[derive(Debug, Deserialize)]
pub(crate) struct KeyForm {
    #[serde(default)]
    public_key: String,
    #[serde(default)]
    comment: String,
}

pub(crate) async fn add_key(
    State(state): State<AppState>,
    Extension(user): Viewer,
    Form(form): Form<KeyForm>,
) -> Response {
    let text = form.public_key.trim();
    let public_key = match PublicKey::from_openssh(text) {
        Ok(public_key) => public_key,
        Err(_) => {
            return render_account(
                &state,
                &user,
                &Default::default(),
                StatusCode::BAD_REQUEST,
                Some("not a valid SSH public key".to_string()),
            )
            .await;
        }
    };
    let comment = if form.comment.trim().is_empty() {
        public_key.comment().to_string()
    } else {
        form.comment.trim().to_string()
    };
    let fingerprint = public_key.fingerprint(HashAlg::Sha256).to_string();
    match state
        .0
        .store
        .add_ssh_key(user.id, text, &fingerprint, &comment)
        .await
    {
        Ok(_) => done("/settings/account", "Key added"),
        Err(error) => {
            let (status, message) = error_parts(&error);
            render_account(&state, &user, &Default::default(), status, Some(message)).await
        }
    }
}

pub(crate) async fn remove_key(
    State(state): State<AppState>,
    Extension(user): Viewer,
    AxumPath(id): AxumPath<String>,
) -> Response {
    use axum::response::IntoResponse;
    let Ok(id) = id.parse::<i64>() else {
        return axum::response::Redirect::to("/settings/account?warn=bad+key+id").into_response();
    };
    match state.0.store.delete_ssh_key(user.id, id).await {
        Ok(()) => done("/settings/account", "Key removed"),
        Err(error) => {
            let (_, message) = error_parts(&error);
            axum::response::Redirect::to(&format!("/settings/account?warn={}", urlencode(&message)))
                .into_response()
        }
    }
}
