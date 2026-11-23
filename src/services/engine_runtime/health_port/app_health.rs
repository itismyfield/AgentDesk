//! `AppHealthRegistry`: the name KE carries the runtime health registry by; the Discord health
//! module binds it to its registry, so KE never names the type where it lives.

/// The binding's anchor: the health side implements [`AppHealthBinding`] for it once.
pub struct AppHealth;

/// What the health side binds as the application's runtime registry.
pub trait AppHealthBinding {
    type Registry: Send + Sync + 'static;
}

/// The runtime health registry the application carries.
pub type AppHealthRegistry = <AppHealth as AppHealthBinding>::Registry;
