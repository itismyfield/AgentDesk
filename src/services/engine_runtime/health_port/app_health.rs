//! The one name KE gives the runtime health registry it carries (`AppState`, the policy engine's
//! global handle, the routine executors): `AppHealthRegistry`. The Discord health module binds it
//! to its registry, so KE holds and passes the registry without naming the type it lives in.

/// The binding's anchor: the health side implements [`AppHealthBinding`] for it once.
pub struct AppHealth;

/// What the health side binds as the application's runtime registry.
pub trait AppHealthBinding {
    type Registry: Send + Sync + 'static;
}

/// The runtime health registry the application carries.
pub type AppHealthRegistry = <AppHealth as AppHealthBinding>::Registry;
