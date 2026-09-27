use anyhow::Context;
use anyhow::Result;
use anyhow::ensure;
use clap::Args;
use codex_app_server::AppServerTransport;
use codex_app_server_daemon::LifecycleCommand;
use codex_utils_absolute_path::AbsolutePathBuf;
use codex_utils_cli::CliConfigOverrides;

use crate::AppServerCommand;
use crate::AppServerSubcommand;
use crate::parse_socket_path;

#[derive(Debug, Args)]
pub(crate) struct AppServerProxyCommand {
    /// Path to the app-server Unix domain socket to connect to.
    #[arg(long = "sock", value_name = "SOCKET_PATH", value_parser = parse_socket_path)]
    pub(super) socket_path: Option<AbsolutePathBuf>,

    /// Start or reuse the shared daemon before proxying WebSocket bytes over stdio.
    #[arg(long, conflicts_with = "socket_path")]
    pub(super) start_daemon: bool,
}

pub(crate) fn validate(
    server: &AppServerCommand,
    overrides: &CliConfigOverrides,
    interactive: &codex_tui::Cli,
) -> Result<()> {
    if !matches!(
        server.subcommand,
        Some(AppServerSubcommand::Proxy(AppServerProxyCommand {
            start_daemon: true,
            ..
        }))
    ) {
        return Ok(());
    }
    // Attaching must not silently ignore settings, even when startup is a no-op.
    ensure!(
        overrides.raw_overrides.is_empty()
            && interactive.model.is_none()
            && !interactive.oss
            && interactive.oss_provider.is_none()
            && interactive.sandbox_mode.is_none()
            && interactive.approval_policy.is_none()
            && !interactive.dangerously_bypass_approvals_and_sandbox
            && !interactive.bypass_hook_trust
            && !interactive.web_search
            && interactive.cwd.is_none()
            && interactive.add_dir.is_empty()
            && interactive.images.is_empty()
            && interactive.prompt.is_none()
            && !interactive.no_daemon
            && !interactive.no_alt_screen,
        "app-server proxy --start-daemon does not accept configuration overrides or interactive options; configure the daemon or use per-thread RPC settings"
    );
    ensure!(
        server.listen == AppServerTransport::Stdio
            && !server.stdio
            && !server.remote_control
            && !server.managed_daemon
            && !server.analytics_default_enabled
            && server.code_mode_host == Default::default()
            && server.auth == Default::default(),
        "app-server proxy --start-daemon does not accept server launch settings; the shared daemon retains its own settings"
    );
    ensure!(
        std::env::var_os("CODEX_EXEC_SERVER_URL").is_none()
            && !codex_login::is_workload_identity_selected(),
        "app-server proxy --start-daemon cannot apply process-specific executor or workload identity settings to a shared daemon"
    );
    Ok(())
}

pub(crate) async fn run(args: AppServerProxyCommand) -> Result<()> {
    let socket_path = if args.start_daemon {
        // Lifecycle startup already serializes concurrent callers and waits for readiness.
        // Never print its JSON result: stdout belongs exclusively to the byte proxy.
        codex_app_server_daemon::run(LifecycleCommand::Start)
            .await
            .context("failed to start shared app-server daemon")?
            .socket_path
    } else {
        match args.socket_path {
            Some(socket_path) => socket_path.into_path_buf(),
            None => {
                let codex_home = codex_core::config::find_codex_home()?;
                codex_app_server::app_server_control_socket_path(&codex_home)?.into_path_buf()
            }
        }
    };
    // The proxy owns only this connection, not the daemon's lifetime.
    codex_stdio_to_uds::run(&socket_path).await
}
