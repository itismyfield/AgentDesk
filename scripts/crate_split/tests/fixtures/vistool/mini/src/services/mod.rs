pub(crate) mod store;
mod web;

pub(crate) fn web_total() -> u32 {
    web::total()
}
