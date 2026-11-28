pub(crate) mod identity;
pub mod source;

#[derive(Clone, Copy, Debug)]
pub struct OutboxMessage<'a> {
    pub target: &'a str,
    pub content: &'a str,
    pub bot: &'a str,
    pub source: &'a str,
    pub reason_code: Option<&'a str>,
    pub session_key: Option<&'a str>,
}
