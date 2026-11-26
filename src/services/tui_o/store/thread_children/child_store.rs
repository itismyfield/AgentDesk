//! The thread GC's view of O's thread child stores.

use futures::future::BoxFuture;

/// O's thread child stores as thread GC reaches them: pinned before a row delete and asked to
/// retire only after that exact delete. The owner decides; GC grants it nothing else.
pub trait ChildStore: Sync {
    type Pinned: Send;
    fn pin(&self, thread: u64) -> Option<Self::Pinned>;
    fn retire<'a>(
        &'a self,
        pinned: Self::Pinned,
        successor: BoxFuture<'a, Result<bool, String>>,
    ) -> BoxFuture<'a, Result<(), String>>;
}

/// No child store to retire.
impl ChildStore for () {
    type Pinned = std::convert::Infallible;
    fn pin(&self, _thread: u64) -> Option<Self::Pinned> {
        None
    }
    fn retire<'a>(
        &'a self,
        pinned: Self::Pinned,
        _successor: BoxFuture<'a, Result<bool, String>>,
    ) -> BoxFuture<'a, Result<(), String>> {
        match pinned {}
    }
}
