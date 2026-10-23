//! How the HTTP adapters treat what a backend answers: what an HTTP status
//! means as a port [`Error`], how long a `Retry-After` asks to wait, and
//! the retry made when a token is rejected.
//!
//! The helpers take plain values (a status code, a header's text), so this
//! crate needs no HTTP library.

use std::future::Future;
use std::time::Duration;

use crate::{AccessToken, Authenticator, Error, Result};

/// The seconds a `Retry-After` header (if there is one) asks to wait. A
/// value that isn't a number of seconds (an HTTP date) gives none.
pub fn retry_after(header: Option<&str>) -> Option<Duration> {
    header
        .and_then(|v| v.trim().parse::<u64>().ok())
        .map(Duration::from_secs)
}

/// The port error for an unsuccessful HTTP `status`: 429 is
/// [`Error::Throttled`] (with `retry_after`), 401 and 403 are
/// [`Error::Auth`], 408 and 5xx are [`Error::Network`] (`detail` says what
/// failed), and anything else is [`Error::Backend`].
///
/// 404 is `not_found` if the backend gives one (it says what was not
/// found); a backend that doesn't has it as any other status.
pub fn error_for_status(
    status: u16,
    retry_after: Option<Duration>,
    detail: String,
    not_found: Option<Error>,
) -> Error {
    match (status, not_found) {
        (429, _) => Error::Throttled { retry_after },
        (404, Some(not_found)) => not_found,
        (401 | 403, _) => Error::Auth(detail),
        (408 | 500..=599, _) => Error::Network(detail),
        _ => Error::Backend(detail),
    }
}

/// Makes a request with `auth`'s token and, if the service rejects it
/// (`rejected` says so of a response) though it hadn't expired (it may
/// have been revoked), once more with a new one. Returns the last response
/// and the token it was got with.
///
/// `attempt` makes the request with (a copy of) the token it is given; it is called
/// again only if `retryable` (a request whose body can't be sent twice
/// isn't) and the new token differs. A token rejected a second time is
/// reported to `auth` too, so the next request doesn't reuse it. Errors
/// from getting a token go through `before`.
pub async fn with_fresh_token<A, R, Fut>(
    auth: &A,
    before: fn(Error) -> Error,
    retryable: bool,
    rejected: impl Fn(&R) -> bool,
    mut attempt: impl FnMut(AccessToken) -> Fut,
) -> Result<(R, AccessToken)>
where
    A: Authenticator,
    Fut: Future<Output = Result<R>>,
{
    let token = auth.access_token().await.map_err(before)?;
    let response = attempt(token.clone()).await?;
    if !rejected(&response) {
        return Ok((response, token));
    }
    auth.token_rejected(&token).await;
    let fresh = auth.access_token().await.map_err(before)?;
    if !retryable || fresh == token {
        return Ok((response, token));
    }
    let response = attempt(fresh.clone()).await?;
    if rejected(&response) {
        auth.token_rejected(&fresh).await;
    }
    Ok((response, fresh))
}

#[cfg(test)]
mod tests {
    use std::sync::Mutex;
    use std::sync::atomic::Ordering;

    use super::*;
    use crate::mock::{CountingAuth, RotatingAuth};

    type Answer = (u16, String);

    fn unauthorized(answer: &Answer) -> bool {
        answer.0 == 401
    }

    #[test]
    fn reads_the_seconds_to_wait() {
        assert_eq!(retry_after(Some(" 7 ")), Some(Duration::from_secs(7)));
        assert_eq!(retry_after(Some("0")), Some(Duration::ZERO));
        assert_eq!(retry_after(Some("Wed, 21 Oct 2026 07:28:00 GMT")), None);
        assert_eq!(retry_after(Some("-3")), None);
        assert_eq!(retry_after(None), None);
    }

    #[test]
    fn maps_statuses_onto_errors() {
        let map =
            |status| error_for_status(status, Some(Duration::from_secs(2)), "why".into(), None);
        assert!(matches!(
            map(429),
            Error::Throttled { retry_after: Some(d) } if d == Duration::from_secs(2)
        ));
        for status in [401, 403] {
            assert!(matches!(map(status), Error::Auth(d) if d == "why"));
        }
        for status in [408, 500, 502, 599] {
            assert!(matches!(map(status), Error::Network(d) if d == "why"));
        }
        for status in [400, 404, 409, 410, 418, 499] {
            assert!(matches!(map(status), Error::Backend(d) if d == "why"));
        }
    }

    #[test]
    fn a_missing_object_is_the_error_given_for_it() {
        let missing = || Some(Error::not_found("message", "m1"));
        let found = error_for_status(404, None, "why".into(), missing());
        assert!(matches!(found, Error::NotFound { kind: "message", id } if id == "m1"));
        // Only 404 is a missing object.
        let found = error_for_status(403, None, "why".into(), missing());
        assert!(matches!(found, Error::Auth(_)));
        let found = error_for_status(429, None, "why".into(), missing());
        assert!(matches!(found, Error::Throttled { retry_after: None }));
    }

    /// An attempt that answers with each of `statuses` in turn, and the
    /// token it was made with.
    fn answers<const N: usize>(
        statuses: [u16; N],
    ) -> impl FnMut(AccessToken) -> std::future::Ready<Result<Answer>> {
        let statuses = Mutex::new(statuses.into_iter());
        move |token| {
            let status = statuses
                .lock()
                .unwrap()
                .next()
                .expect("one answer per attempt");
            std::future::ready(Ok((status, token.secret().to_owned())))
        }
    }

    #[tokio::test]
    async fn a_token_that_works_is_used_once() {
        let auth = CountingAuth::default();
        let (answer, _) = with_fresh_token(&auth, |e| e, true, unauthorized, answers([200]))
            .await
            .unwrap();
        assert_eq!(answer.0, 200);
        assert_eq!(auth.rejected.load(Ordering::SeqCst), 0);
    }

    #[tokio::test]
    async fn a_rejected_token_is_replaced_and_the_request_repeated() {
        let auth = RotatingAuth::default();
        let (answer, token) =
            with_fresh_token(&auth, |e| e, true, unauthorized, answers([401, 200]))
                .await
                .unwrap();
        assert_eq!(answer, (200, "new".to_owned()));
        assert_eq!(token.secret(), "new");
    }

    #[tokio::test]
    async fn the_same_token_again_is_reported_and_not_repeated() {
        let auth = CountingAuth::default();
        let (answer, token) = with_fresh_token(&auth, |e| e, true, unauthorized, answers([401]))
            .await
            .unwrap();
        assert_eq!((answer.0, token.secret()), (401, "tok"));
        assert_eq!(auth.rejected.load(Ordering::SeqCst), 1);
    }

    #[tokio::test]
    async fn a_request_that_cant_be_repeated_is_not() {
        let auth = RotatingAuth::default();
        let (answer, token) = with_fresh_token(&auth, |e| e, false, unauthorized, answers([401]))
            .await
            .unwrap();
        assert_eq!((answer.0, token.secret()), (401, "old"));
        assert!(auth.rejected.load(Ordering::SeqCst));
    }

    #[tokio::test]
    async fn failing_to_get_a_token_goes_through_before() {
        struct NoAuth;
        impl Authenticator for NoAuth {
            async fn access_token(&self) -> Result<AccessToken> {
                Err(Error::Auth("none".into()))
            }
        }
        let result = with_fresh_token(
            &NoAuth,
            |e| Error::NotSent(e.to_string()),
            true,
            |_: &()| false,
            |_| -> std::future::Ready<Result<()>> { unreachable!("there is no token") },
        )
        .await;
        assert!(matches!(result, Err(Error::NotSent(_))));
    }
}
