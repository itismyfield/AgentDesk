#[cfg(unix)]
#[path = "herdr_terminate/http_force_tests.rs"]
mod http_force_tests;

#[cfg(unix)]
#[path = "herdr_terminate_exit_tests.rs"]
mod exit_tests;

#[cfg(unix)]
#[path = "herdr_terminate_route_tests.rs"]
mod route_tests;
