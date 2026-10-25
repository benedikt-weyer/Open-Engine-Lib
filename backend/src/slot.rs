//! Which of the two open libraries a request is about.
//!
//! An Engine library and a folder of music can be open at the same time, one of each. The UI
//! says which one a request is about in the metadata `x-open-engine-library`; this wraps a
//! service so the handlers can ask for it (`current`) without every one of them looking
//! at the request. The value is only there while the handler runs, so a handler that
//! starts work on another task has to ask first.

use std::convert::Infallible;
use std::future::Future;
use std::pin::Pin;
use std::task::{Context, Poll};

use open_engine_api::{LIBRARY_SLOT, SLOT_ENGINE, SLOT_FOLDER};
use tonic::codegen::Service;
use tonic::codegen::http::Request;
use tonic::server::NamedService;

/// Where a library is kept.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Slot {
    Engine,
    Folder,
}

tokio::task_local! {
    static REQUESTED: Option<Slot>;
}

/// The slot the request that is being handled names, if it names one.
pub fn current() -> Option<Slot> {
    REQUESTED.try_with(|slot| *slot).ok().flatten()
}

fn named<B>(request: &Request<B>) -> Option<Slot> {
    match request.headers().get(LIBRARY_SLOT)?.to_str().ok()? {
        SLOT_ENGINE => Some(Slot::Engine),
        SLOT_FOLDER => Some(Slot::Folder),
        _ => None,
    }
}

/// A service that notes the slot a request names while the request is handled.
#[derive(Clone)]
pub struct Routed<S>(pub S);

impl<S: NamedService> NamedService for Routed<S> {
    const NAME: &'static str = S::NAME;
}

impl<S, B> Service<Request<B>> for Routed<S>
where
    S: Service<Request<B>, Error = Infallible> + Send + 'static,
    S::Future: Send + 'static,
    S::Response: Send + 'static,
{
    type Response = S::Response;
    type Error = S::Error;
    type Future = Pin<Box<dyn Future<Output = Result<S::Response, S::Error>> + Send>>;

    fn poll_ready(&mut self, context: &mut Context<'_>) -> Poll<Result<(), Self::Error>> {
        self.0.poll_ready(context)
    }

    fn call(&mut self, request: Request<B>) -> Self::Future {
        let slot = named(&request);
        Box::pin(REQUESTED.scope(slot, self.0.call(request)))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn the_slot_is_only_there_while_a_request_is_handled() {
        assert_eq!(current(), None);
        let request = Request::builder()
            .header(LIBRARY_SLOT, SLOT_FOLDER)
            .body(())
            .unwrap();
        assert_eq!(named(&request), Some(Slot::Folder));
        let seen = REQUESTED.scope(named(&request), async { current() }).await;
        assert_eq!(seen, Some(Slot::Folder));
        let nothing = Request::builder()
            .header(LIBRARY_SLOT, "elsewhere")
            .body(())
            .unwrap();
        assert_eq!(named(&nothing), None);
    }
}
