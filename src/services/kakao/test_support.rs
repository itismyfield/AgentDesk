use super::*;
pub(crate) fn client(origin: &str, account: &str) -> KakaoClient {
    KakaoClient {
        http: reqwest::Client::builder()
            .redirect(reqwest::redirect::Policy::none())
            .timeout(Duration::from_secs(2))
            .build()
            .unwrap(),
        environment: KakaoEnvironment {
            account_id: account.into(),
            landing_url: "https://example.com/".into(),
            env_prefix: account_env_prefix(account),
        },
        rest_api_key: Some("test-app-key".into()),
        client_secret: None,
        tokens: Mutex::new(TokenState {
            access_token: Some("old-test-access".into()),
            refresh_token: Some("test-refresh".into()),
            access_expires_at: None,
            refreshed_once: true,
            generation: 0,
            persistence_failed: false,
        }),
        store: None,
        test_origin: Some(origin.into()),
    }
}
pub(crate) async fn server(router: axum::Router) -> (String, tokio::task::JoinHandle<()>) {
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let address = listener.local_addr().unwrap();
    let task = tokio::spawn(async move {
        axum::serve(listener, router).await.unwrap();
    });
    (format!("http://{address}"), task)
}
