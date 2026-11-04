//! The selector against the resolutions measured on herdr 0.9.5 in an isolated HOME (spikes S4,
//! S4c, S4d) and the upstream session precedence.

use super::*;

const HOME: &str = "/h";
const DEFAULT: &str = "/h/.config/herdr/herdr.sock";

fn named(name: &str) -> String {
    format!("/h/.config/herdr/sessions/{name}/herdr.sock")
}

fn run(argv: &[&str], env: &[(&str, &str)]) -> Selection {
    let argv: Vec<String> = std::iter::once("/opt/homebrew/bin/herdr")
        .chain(argv.iter().copied())
        .map(String::from)
        .collect();
    let environ: Vec<String> = std::iter::once(("HOME", HOME))
        .chain(env.iter().copied())
        .map(|(key, value)| format!("{key}={value}"))
        .collect();
    select(&argv, &environ)
}

fn server(socket: &str) -> Selection {
    Selection {
        role: Role::Server,
        socket: Some(socket.into()),
    }
}

/// A measured case: its label, herdr's argv after the binary, its environment and what it bound.
type Case = (
    &'static str,
    Vec<&'static str>,
    Vec<(&'static str, &'static str)>,
    Selection,
);

#[test]
fn measured_resolutions_select_the_socket_herdr_bound() {
    let cases: Vec<Case> = vec![
        ("S4 A default", vec!["server"], vec![], server(DEFAULT)),
        (
            "S4 B env session",
            vec!["server"],
            vec![("HERDR_SESSION", "work")],
            server(&named("work")),
        ),
        (
            "S4 C argv session",
            vec!["--session", "work", "server"],
            vec![],
            server(&named("work")),
        ),
        (
            "S4 D socket env before env session",
            vec!["server"],
            vec![
                ("HERDR_SOCKET_PATH", "/r/alt.sock"),
                ("HERDR_SESSION", "work"),
            ],
            server("/r/alt.sock"),
        ),
        (
            "S4 E env default",
            vec!["server"],
            vec![("HERDR_SESSION", "default")],
            server(DEFAULT),
        ),
        (
            "S4c G xdg",
            vec!["server"],
            vec![("XDG_CONFIG_HOME", "/r/xdg")],
            server("/r/xdg/herdr/herdr.sock"),
        ),
        (
            "S4c H argv before env session",
            vec!["--session", "a", "server"],
            vec![("HERDR_SESSION", "b")],
            server(&named("a")),
        ),
        (
            "S4c I argv default before env session",
            vec!["--session", "default", "server"],
            vec![("HERDR_SESSION", "b")],
            server(DEFAULT),
        ),
        (
            "S4d J explicit session before socket env",
            vec!["--session", "work", "server"],
            vec![("HERDR_SOCKET_PATH", DEFAULT)],
            server(&named("work")),
        ),
        (
            "S4d K explicit default before socket env",
            vec!["--session", "default", "server"],
            vec![("HERDR_SOCKET_PATH", "/r/alt.sock")],
            server(DEFAULT),
        ),
        (
            "S4d L option after subcommand",
            vec!["server", "--session=work"],
            vec![],
            server(&named("work")),
        ),
        (
            "S4d M equals form",
            vec!["--session=work", "server"],
            vec![],
            server(&named("work")),
        ),
        (
            "S4d N last session wins",
            vec!["--session", "a", "--session", "b", "server"],
            vec![],
            server(&named("b")),
        ),
    ];
    for (name, argv, env, expected) in cases {
        assert_eq!(run(&argv, &env), expected, "{name}");
    }
}

#[test]
fn launchers_and_other_commands_are_told_apart() {
    assert_eq!(
        run(
            &["session", "attach", "work"],
            &[("HERDR_SOCKET_PATH", DEFAULT)]
        ),
        Selection {
            role: Role::Launcher,
            socket: Some(named("work").into()),
        },
        "session attach names its session explicitly, before the socket env"
    );
    assert_eq!(
        run(&[], &[]),
        Selection {
            role: Role::Launcher,
            socket: Some(DEFAULT.into()),
        }
    );
    for argv in [
        vec!["server", "stop"],
        vec!["server", "reload-config"],
        vec!["--remote", "mac-mini"],
        vec!["--machine", "m1", "pane", "list"],
        vec!["--remote=mac-mini"],
        vec!["--machine=m1", "pane", "list"],
        vec!["api", "ping"],
        vec!["--version"],
        vec!["config", "check"],
    ] {
        assert_eq!(run(&argv, &[]).role, Role::Other, "{argv:?}");
    }
    assert_eq!(run(&["--frobnicate", "server"], &[]).role, Role::Unknown);
}

#[test]
fn an_unresolvable_environment_selects_no_socket() {
    let no_home = select(
        &["herdr".into(), "server".into()],
        &["PATH=/usr/bin".into()],
    );
    assert_eq!(no_home.socket, None);
    assert_eq!(
        run(&["server"], &[("HERDR_SOCKET_PATH", "rel.sock")]).socket,
        None
    );
    assert_eq!(run(&["server"], &[("HERDR_SESSION", "")]).socket, None);
    let twice = select(
        &["herdr".into(), "server".into()],
        &["HOME=/h".into(), "HOME=/other".into()],
    );
    assert_eq!(twice.socket, None, "two HOME entries are ambiguous");
}
