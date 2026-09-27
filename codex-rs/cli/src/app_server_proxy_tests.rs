use super::*;
use pretty_assertions::assert_eq;

#[test]
fn managed_proxy_startup_is_explicit() {
    let parsed = MultitoolCli::try_parse_from(["codex", "app-server", "proxy", "--start-daemon"]);
    assert!(parsed.is_ok(), "managed proxy should parse");
}

#[test]
fn managed_proxy_rejects_custom_socket() {
    assert!(
        MultitoolCli::try_parse_from([
            "codex",
            "app-server",
            "proxy",
            "--start-daemon",
            "--sock",
            "custom.sock",
        ])
        .is_err()
    );
}

#[test]
fn managed_proxy_rejects_profile() -> anyhow::Result<()> {
    let cli = MultitoolCli::try_parse_from([
        "codex",
        "--profile",
        "work",
        "app-server",
        "proxy",
        "--start-daemon",
    ])?;
    let error = profile_v2_for_subcommand(
        &cli.interactive,
        cli.subcommand.as_ref().expect("subcommand"),
    )
    .expect_err("profiles cannot configure a shared daemon");
    assert!(error.to_string().contains("--profile only applies"));
    Ok(())
}

#[test]
fn plain_app_server_still_uses_stdio() {
    let server = app_server_from_args(&["codex", "app-server"]);
    assert!(server.subcommand.is_none());
    assert_eq!(server.listen, codex_app_server::AppServerTransport::Stdio);
}

#[test]
fn managed_proxy_rejects_server_launch_settings() {
    for options in [
        vec!["--listen", "unix://"],
        vec!["--stdio"],
        vec!["--remote-control"],
        vec!["--managed-daemon"],
        vec!["--analytics-default-enabled"],
        vec!["--code-mode-host", "http://localhost:9000"],
        vec!["--ws-auth", "capability-token"],
        vec!["--ws-token-file", "token"],
        vec!["--ws-token-sha256", "digest"],
        vec!["--ws-shared-secret-file", "secret"],
        vec!["--ws-issuer", "issuer"],
        vec!["--ws-audience", "audience"],
        vec!["--ws-max-clock-skew-seconds", "30"],
    ] {
        let mut args = vec!["codex", "app-server"];
        args.extend(options);
        args.extend(["proxy", "--start-daemon"]);
        let server = app_server_from_args(&args);
        let error = app_server_proxy::validate(
            &server,
            &CliConfigOverrides::default(),
            &TuiCli::parse_from(["codex"]),
        )
        .expect_err("server settings must not be silently ignored");
        assert!(
            error
                .to_string()
                .contains("does not accept server launch settings")
        );
    }
}

#[test]
fn managed_proxy_validation_does_not_change_existing_entry_points() -> anyhow::Result<()> {
    for args in [
        vec!["codex", "app-server", "--listen", "unix://"],
        vec!["codex", "app-server", "proxy"],
    ] {
        let server = app_server_from_args(&args);
        app_server_proxy::validate(
            &server,
            &CliConfigOverrides {
                raw_overrides: vec!["model=other".into()],
            },
            &TuiCli::parse_from(["codex"]),
        )?;
    }
    Ok(())
}

fn app_server_from_args(args: &[&str]) -> AppServerCommand {
    let cli = MultitoolCli::try_parse_from(args).expect("CLI parses");
    let Some(Subcommand::AppServer(server)) = cli.subcommand else {
        panic!("expected app-server");
    };
    server
}
