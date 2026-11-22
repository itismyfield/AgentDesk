//! vistool fixture: `core` and `services::store` are the lower crate, `app` and `services::web` the upper one.
mod app;
mod core;
mod services;

pub fn run() -> u32 {
    app::start()
}
