/// Stored choice; `None` inherits the agent primary provider and its channel model setting.
#[derive(Debug, Clone, Default, PartialEq, Eq, sqlx::FromRow)]
pub struct ProviderChoice {
    pub provider: Option<String>,
    pub model: Option<String>,
}
