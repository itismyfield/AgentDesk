//! Pure PostgreSQL fixture-target parsing and comparison.
//!
//! SQLx 0.8.6 chooses a physical peer from `socket`, `host`, and `port`, then
//! uses `host` plus `ssl_mode` while negotiating TLS. Fixture authorization
//! therefore compares both the physical peer and those TLS routing inputs.
//! URL authority host and port must be explicit so `PGHOSTADDR`, `PGHOST`,
//! `PGPORT`, and SQLx's filesystem-based default-host probe cannot select the
//! peer.

use std::fmt;
use std::str::FromStr;

use sqlx::postgres::{PgConnectOptions, PgSslMode};
use url::Url;

#[derive(Clone, Debug)]
pub(super) struct ParsedFixtureUrl {
    pub(super) options: PgConnectOptions,
    pub(super) password: Option<String>,
    pub(super) url: Url,
}

#[derive(Clone, Debug, PartialEq, Eq)]
enum FixtureTransport {
    Socket(String),
    Tcp,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum TlsMode {
    Disable,
    Allow,
    Prefer,
    Require,
    VerifyCa,
    VerifyFull,
}

#[derive(Clone, Debug, PartialEq, Eq)]
struct FixtureIdentity {
    transport: FixtureTransport,
    port: u16,
    routing_host: String,
    tls_mode: TlsMode,
}

impl fmt::Display for FixtureIdentity {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(
            formatter,
            "{:?}:{};route={};tls={:?}",
            self.transport, self.port, self.routing_host, self.tls_mode
        )
    }
}

fn tls_mode(options: &PgConnectOptions) -> TlsMode {
    match options.get_ssl_mode() {
        PgSslMode::Disable => TlsMode::Disable,
        PgSslMode::Allow => TlsMode::Allow,
        PgSslMode::Prefer => TlsMode::Prefer,
        PgSslMode::Require => TlsMode::Require,
        PgSslMode::VerifyCa => TlsMode::VerifyCa,
        PgSslMode::VerifyFull => TlsMode::VerifyFull,
    }
}

fn fixture_identity(options: &PgConnectOptions) -> FixtureIdentity {
    let raw_host = options.get_host().to_string();
    let transport = if let Some(socket) = options.get_socket() {
        FixtureTransport::Socket(socket.to_string_lossy().into_owned())
    } else if options.get_host().starts_with('/') {
        FixtureTransport::Socket(options.get_host().to_string())
    } else {
        FixtureTransport::Tcp
    };
    FixtureIdentity {
        transport,
        port: options.get_port(),
        routing_host: raw_host,
        tls_mode: tls_mode(options),
    }
}

pub(super) fn server_identity(options: &PgConnectOptions) -> String {
    fixture_identity(options).to_string()
}

fn authority_decodes_to_socket(host: &str) -> bool {
    host.starts_with('/')
        || host
            .as_bytes()
            .get(..3)
            .is_some_and(|prefix| prefix.eq_ignore_ascii_case(b"%2f"))
}

fn decode_percent_encoded(input: &str) -> Result<String, String> {
    let bytes = input.as_bytes();
    let mut decoded = Vec::with_capacity(bytes.len());
    let mut index = 0;
    while index < bytes.len() {
        if bytes[index] == b'%' {
            let encoded = bytes
                .get(index + 1..index + 3)
                .ok_or_else(|| "truncated percent escape in fixture credential".to_string())?;
            let encoded = std::str::from_utf8(encoded).map_err(|error| {
                format!("invalid percent escape in fixture credential: {error}")
            })?;
            decoded.push(u8::from_str_radix(encoded, 16).map_err(|error| {
                format!("invalid percent escape in fixture credential: {error}")
            })?);
            index += 3;
        } else {
            decoded.push(bytes[index]);
            index += 1;
        }
    }
    String::from_utf8(decoded)
        .map_err(|error| format!("fixture credential is not valid UTF-8: {error}"))
}

pub(super) fn parse_fixture_url(
    database_url: &str,
    label: &str,
) -> Result<ParsedFixtureUrl, String> {
    let url = Url::parse(database_url).map_err(|error| format!("{label} parse URL: {error}"))?;
    let host = url
        .host_str()
        .ok_or_else(|| format!("{label} URL authority must explicitly name a host"))?;
    if url.port().is_none() {
        return Err(format!("{label} URL authority must explicitly name a port"));
    }
    if authority_decodes_to_socket(host) {
        return Err(format!(
            "{label} URL must express a Unix socket as ?host=/path over an explicit hostname authority"
        ));
    }

    let mut password = url.password().map(decode_percent_encoded).transpose()?;
    for (key, value) in url.query_pairs() {
        if key == "password" {
            password = Some(value.into_owned());
        }
    }
    let options = PgConnectOptions::from_str(database_url)
        .map_err(|error| format!("{label} parse postgres URL: {error}"))?;
    Ok(ParsedFixtureUrl {
        options,
        password,
        url,
    })
}

pub(super) fn options_match(
    base_options: &PgConnectOptions,
    candidate_options: &PgConnectOptions,
) -> Result<(), String> {
    let base = fixture_identity(base_options);
    let candidate = fixture_identity(candidate_options);
    if candidate == base {
        return Ok(());
    }
    Err(format!(
        "fixture connection target {candidate} does not match configured fixture base {base}"
    ))
}

pub(super) fn config_base_is_representable(parsed_base: &ParsedFixtureUrl) -> Result<(), String> {
    if parsed_base.options.get_socket().is_some() {
        return Err("Config fixture base uses a Unix socket".to_string());
    }
    let candidate = PgConnectOptions::new()
        .host(parsed_base.options.get_host())
        .port(parsed_base.options.get_port());
    options_match(&parsed_base.options, &candidate)
}

pub(super) fn enforce_configured_target(
    configured_base: Option<&str>,
    candidate_options: &PgConnectOptions,
) -> Result<(), String> {
    let base = configured_base
        .ok_or_else(|| "fixture base unconfigured (POSTGRES_TEST_DATABASE_URL_BASE)".to_string())?;
    let parsed_base = parse_fixture_url(base, "configured PostgreSQL fixture base")?;
    options_match(&parsed_base.options, candidate_options)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn parsed(url: &str) -> ParsedFixtureUrl {
        parse_fixture_url(url, "matrix input").expect("parse explicit matrix URL")
    }

    fn accepted(base: &str, candidate: &str) -> bool {
        let base = parse_fixture_url(base, "matrix base");
        let candidate = parse_fixture_url(candidate, "matrix candidate");
        matches!((base, candidate), (Ok(base), Ok(candidate)) if options_match(&base.options, &candidate.options).is_ok())
    }

    #[test]
    fn endpoint_enumeration_matrix_has_no_ambient_authority() {
        let base = "postgresql://user@db.example:15432/root?sslmode=disable";
        let cases = [
            (
                "authority-host",
                "postgresql://user@DB.EXAMPLE:15432/db?sslmode=disable",
                false,
            ),
            (
                "socket-authority",
                "postgresql://user@%2Ftmp%2Fsafe:15432/db?sslmode=disable",
                false,
            ),
            (
                "authority-port",
                "postgresql://user@db.example:15432/db?sslmode=disable",
                true,
            ),
            (
                "query-host",
                "postgresql://user@db.example:15432/db?sslmode=disable&host=other.example",
                false,
            ),
            (
                "query-socket",
                "postgresql://user@db.example:15432/db?sslmode=disable&host=%2Ftmp%2Fsafe",
                false,
            ),
            (
                "query-address",
                "postgresql://user@db.example:15432/db?sslmode=disable&hostaddr=192.0.2.9",
                false,
            ),
            (
                "query-port",
                "postgresql://user@db.example:15432/db?sslmode=disable&port=15433",
                false,
            ),
            ("missing-host", "postgresql:///db?sslmode=disable", false),
            (
                "missing-port",
                "postgresql://user@db.example/db?sslmode=disable",
                false,
            ),
        ];
        for (name, candidate, expected) in cases {
            assert_eq!(accepted(base, candidate), expected, "matrix case {name}");
        }

        let direct = parsed(base).options;
        let changed_host = PgConnectOptions::new()
            .host("other.example")
            .port(15432)
            .ssl_mode(PgSslMode::Disable);
        let changed_port = PgConnectOptions::new()
            .host("db.example")
            .port(15433)
            .ssl_mode(PgSslMode::Disable);
        let changed_socket = PgConnectOptions::new()
            .host("db.example")
            .port(15432)
            .socket("/tmp/safe")
            .ssl_mode(PgSslMode::Disable);
        assert!(
            options_match(&direct, &changed_host).is_err(),
            "builder host"
        );
        assert!(
            options_match(&direct, &changed_port).is_err(),
            "builder port"
        );
        assert!(
            options_match(&direct, &changed_socket).is_err(),
            "builder socket"
        );
    }

    #[test]
    fn explicit_authority_fields_are_required() {
        let missing_port = parse_fixture_url(
            "postgresql://user@db.example/db?sslmode=disable",
            "missing port",
        )
        .expect_err("an omitted authority port must not inherit ambient input");
        assert!(missing_port.contains("explicitly name a port"));

        let socket_authority = parse_fixture_url(
            "postgresql://user@%2Ftmp%2Fsafe:15432/db?sslmode=disable",
            "socket authority",
        )
        .expect_err("socket authority must not leave an ambient TLS hostname");
        assert!(socket_authority.contains("express a Unix socket as ?host=/path"));
    }

    #[test]
    fn socket_transport_precedes_host_transport() {
        let base = parsed(
            "postgresql://route.example:secret@db.example:15432/root?sslmode=disable&host=%2Ftmp%2Fsafe",
        );
        let same = parsed(
            "postgresql://route.example:secret@db.example:15432/db?sslmode=disable&host=%2Ftmp%2Fsafe",
        );
        let other_socket = parsed(
            "postgresql://route.example:secret@db.example:15432/db?sslmode=disable&host=%2Ftmp%2Fother",
        );
        let other_route = parsed(
            "postgresql://route.example:secret@other.example:15432/db?sslmode=disable&host=%2Ftmp%2Fsafe",
        );
        assert!(options_match(&base.options, &same.options).is_ok());
        assert!(options_match(&base.options, &other_socket.options).is_err());
        assert!(options_match(&base.options, &other_route.options).is_err());
    }

    #[test]
    fn tls_routing_is_part_of_identity() {
        let base = parsed("postgresql://user@db.example:15432/root?sslmode=disable");
        for mode in ["allow", "prefer", "require", "verify-ca", "verify-full"] {
            let candidate = parsed(&format!(
                "postgresql://user@db.example:15432/db?sslmode={mode}"
            ));
            assert!(
                options_match(&base.options, &candidate.options).is_err(),
                "TLS mode {mode} must discriminate"
            );
        }
    }

    #[test]
    fn config_representation_rejects_socket_transport() {
        let socket = parsed("postgresql://user@db.example:15432/root?host=%2Ftmp%2Fsafe");
        assert!(config_base_is_representable(&socket).is_err());
    }

    #[test]
    fn raw_tcp_hosts_discriminate() {
        let ipv4 = parsed("postgresql://user@127.0.0.1:15432/root?sslmode=disable");
        let ipv6 = parsed("postgresql://user@[::1]:15432/db?sslmode=disable");
        assert!(options_match(&ipv4.options, &ipv6.options).is_err());
    }

    #[test]
    fn configured_target_rejects_changed_host() {
        let candidate = parsed("postgresql://user@other.example:15432/db?sslmode=disable");
        let result = enforce_configured_target(
            Some("postgresql://user@db.example:15432/root?sslmode=disable"),
            &candidate.options,
        );
        assert!(result.is_err(), "a changed host must be rejected");

        let socket_candidate =
            parsed("postgresql://user@other.example:15432/db?sslmode=disable&host=%2Ftmp%2Fsafe");
        let socket_result = enforce_configured_target(
            Some("postgresql://user@db.example:15432/root?sslmode=disable&host=%2Ftmp%2Fsafe"),
            &socket_candidate.options,
        );
        assert!(
            socket_result.is_err(),
            "a changed TLS routing host over the same socket must be rejected"
        );
    }

    #[test]
    fn configured_target_rejects_changed_port() {
        let candidate = parsed("postgresql://user@db.example:15433/db?sslmode=disable");
        let result = enforce_configured_target(
            Some("postgresql://user@db.example:15432/root?sslmode=disable"),
            &candidate.options,
        );
        assert!(result.is_err(), "a changed port must be rejected");
    }

    #[test]
    fn unconfigured_target_is_rejected_without_panicking() {
        let candidate = parsed("postgresql://user@127.0.0.1:5432/db?sslmode=disable");
        let result = enforce_configured_target(None, &candidate.options);
        assert!(result.is_err(), "an ambient target must be rejected");
    }
}
