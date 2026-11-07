use axum::extract::{FromRequest, Request};
use hyper::StatusCode;
use tracing::error;

use crate::{
    request_preprocessing::{
        preprocess_request, resolve_request_identity_from_headers_uri, NoAuthorizationSessions,
        PreprocessedRequest,
    },
    user_authorization_service::{AuthorizationSession, User},
    AppState,
};

pub struct RequirePrimaryUser(pub User);

impl FromRequest<AppState> for RequirePrimaryUser {
    type Rejection = StatusCode;

    async fn from_request(req: Request, state: &AppState) -> Result<Self, Self::Rejection> {
        let identity = resolve_request_identity_from_headers_uri(req.headers(), req.uri(), state)
            .await
            .map_err(|error| {
                error!("Failed to resolve API key caller: {error}");
                StatusCode::INTERNAL_SERVER_ERROR
            })?;
        let user = identity.user.ok_or(StatusCode::UNAUTHORIZED)?;
        let token = identity
            .auth
            .as_ref()
            .and_then(|auth| auth.token_ref())
            .ok_or(StatusCode::UNAUTHORIZED)?;

        if token != user.virtual_key {
            return Err(StatusCode::FORBIDDEN);
        }

        Ok(Self(user))
    }
}

/// The status a request-preprocessing failure should be reported with.
///
/// Credentials that resolve to no session are an **authentication** failure, and reporting them as
/// `400` is not a cosmetic inaccuracy - it changes what clients do. A client cannot distinguish
/// "your token is no good, authenticate and try again" from "this request was malformed, never
/// send it again", so a `400` here tells it to give up on a request that would have succeeded a
/// moment later.
///
/// That is not hypothetical. Media Bar boots from credentials in local storage before Jellyfin's
/// `ApiClient` is ready, and handles the rejection it expects:
///
/// ```text
/// if (response.status === 401 || response.status === 403) {
///   STATE.slideshow.credentialsRejected = true;   // waits for ApiClient, then re-bootstraps
///   return [];
/// }
/// console.error(`Failed to fetch items: ${response.status}`);
/// return [];                                      // gives up for the lifetime of the page
/// ```
///
/// Answering `400` routed it down the second branch, so the bar rendered its container and stayed
/// empty for ever, with the only clue a single line in the proxy log. Returning `401` lets the
/// plugin's own retry do its job.
///
/// Anything genuinely malformed still gets `400`.
pub fn preprocess_failure_status(error: &anyhow::Error) -> StatusCode {
    if error.downcast_ref::<NoAuthorizationSessions>().is_some() {
        StatusCode::UNAUTHORIZED
    } else {
        StatusCode::BAD_REQUEST
    }
}

pub struct Preprocessed(pub PreprocessedRequest);

impl FromRequest<AppState> for Preprocessed {
    type Rejection = StatusCode;

    async fn from_request(req: Request, state: &AppState) -> Result<Self, Self::Rejection> {
        preprocess_request(req, state).await.map(Self).map_err(|e| {
            error!("Failed to preprocess request: {}", e);
            preprocess_failure_status(&e)
        })
    }
}

pub struct RequireUser {
    pub preprocessed: PreprocessedRequest,
    pub user: User,
}

impl FromRequest<AppState> for RequireUser {
    type Rejection = StatusCode;

    async fn from_request(req: Request, state: &AppState) -> Result<Self, Self::Rejection> {
        let Preprocessed(preprocessed) = Preprocessed::from_request(req, state).await?;
        let user = preprocessed.user.clone().ok_or_else(|| {
            error!("User not found in request preprocessing");
            StatusCode::UNAUTHORIZED
        })?;

        Ok(Self { preprocessed, user })
    }
}

pub struct RequireSession {
    pub preprocessed: PreprocessedRequest,
    pub session: AuthorizationSession,
}

impl FromRequest<AppState> for RequireSession {
    type Rejection = StatusCode;

    async fn from_request(req: Request, state: &AppState) -> Result<Self, Self::Rejection> {
        let Preprocessed(preprocessed) = Preprocessed::from_request(req, state).await?;
        let session = preprocessed.session.clone().ok_or_else(|| {
            error!("Session not found in request preprocessing");
            StatusCode::UNAUTHORIZED
        })?;

        Ok(Self {
            preprocessed,
            session,
        })
    }
}

pub struct RequireUserSession {
    pub preprocessed: PreprocessedRequest,
    pub user: User,
    pub session: AuthorizationSession,
}

impl FromRequest<AppState> for RequireUserSession {
    type Rejection = StatusCode;

    async fn from_request(req: Request, state: &AppState) -> Result<Self, Self::Rejection> {
        let RequireUser { preprocessed, user } = RequireUser::from_request(req, state).await?;
        let session = preprocessed.session.clone().ok_or_else(|| {
            error!("Session not found in request preprocessing");
            StatusCode::UNAUTHORIZED
        })?;

        Ok(Self {
            preprocessed,
            user,
            session,
        })
    }
}

#[cfg(test)]
mod preprocess_failure_status_tests {
    use super::*;
    use anyhow::anyhow;

    /// The defect this mapper exists for. Reporting bad credentials as `400` tells a client the
    /// request itself was wrong, so a well-written client stops retrying something that would
    /// have succeeded once authentication settled - which is exactly how Media Bar ended up
    /// rendering an empty bar for the life of the page.
    #[test]
    fn credentials_that_resolve_to_no_session_are_unauthorized_not_a_bad_request() {
        let error = anyhow::Error::new(NoAuthorizationSessions);

        assert_eq!(
            preprocess_failure_status(&error),
            StatusCode::UNAUTHORIZED,
            "a token that resolves to no session is an authentication failure"
        );
    }

    /// The load-bearing half. `preprocess_request` is a deep call stack and the error travels up
    /// through it, so anything that attaches context on the way must not hide the type - a
    /// `downcast_ref` that stopped at the outermost error would silently send this back to `400`
    /// and restore the original defect without failing the test above.
    #[test]
    fn context_added_on_the_way_up_does_not_hide_the_cause() {
        let error = anyhow::Error::new(NoAuthorizationSessions)
            .context("resolving the server for the request")
            .context("preprocessing GET /Items");

        assert_eq!(
            preprocess_failure_status(&error),
            StatusCode::UNAUTHORIZED,
            "the cause must still be visible through the context chain"
        );
    }

    /// A genuinely malformed request must keep its `400`; the point is to tell the two apart, not
    /// to relabel every failure as an authentication problem.
    #[test]
    fn a_malformed_request_is_still_a_bad_request() {
        for error in [
            anyhow!("mixed-server playlists are unsupported"),
            anyhow!("playback report requires an item"),
            anyhow!("something nobody has thought of yet"),
        ] {
            assert_eq!(
                preprocess_failure_status(&error),
                StatusCode::BAD_REQUEST,
                "{error} is not an authentication failure"
            );
        }
    }

    /// Matching on the message instead of the type would be satisfied by any error that happens to
    /// quote it, so make sure the type is what decides.
    #[test]
    fn the_type_decides_rather_than_the_message() {
        let impostor = anyhow!("no authorization sessions available");

        assert_eq!(
            preprocess_failure_status(&impostor),
            StatusCode::BAD_REQUEST,
            "only the real error type may map to 401"
        );
        assert_eq!(
            NoAuthorizationSessions.to_string(),
            "no authorization sessions available",
            "the message is still what operators see in the log"
        );
    }
}
