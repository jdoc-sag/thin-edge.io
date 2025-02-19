use anyhow::Context;
use anyhow::Result;
use c8y_http_proxy::credentials::C8YJwtRetriever;
use c8y_http_proxy::C8YHttpProxyBuilder;
use c8y_log_manager::LogManagerBuilder;
use c8y_log_manager::LogManagerConfig;
use clap::Parser;
use std::path::Path;
use std::path::PathBuf;
use tedge_actors::Runtime;
use tedge_config::mqtt_config::MqttConfigBuildError;
use tedge_config::system_services::get_log_level;
use tedge_config::system_services::set_log_level;
use tedge_config::ConfigRepository;
use tedge_config::ConfigSettingAccessor;
use tedge_config::LogPathSetting;
use tedge_config::TEdgeConfig;
use tedge_config::DEFAULT_TEDGE_CONFIG_PATH;
use tedge_file_system_ext::FsWatchActorBuilder;
use tedge_health_ext::HealthMonitorBuilder;
use tedge_http_ext::HttpActor;
use tedge_mqtt_ext::MqttActorBuilder;
use tedge_mqtt_ext::MqttConfig;
use tedge_signal_ext::SignalActor;
use tedge_utils::file::create_directory_with_user_group;
use tedge_utils::file::create_file_with_user_group;
use tracing::info;

const DEFAULT_PLUGIN_CONFIG_FILE: &str = "c8y/c8y-log-plugin.toml";
const AFTER_HELP_TEXT: &str = r#"On start, `c8y-log-plugin` notifies the cloud tenant of the log types listed in the `CONFIG_FILE`, sending this list with a `118` on `c8y/s/us`.
`c8y-log-plugin` subscribes then to `c8y/s/ds` listening for logfile operation requests (`522`) notifying the Cumulocity tenant of their progress (messages `501`, `502` and `503`).

The thin-edge `CONFIG_DIR` is used to store:
  * c8y-log-plugin.toml - the configuration file that specifies which logs to be retrieved"#;

const C8Y_LOG_PLUGIN: &str = "c8y-log-plugin";

#[derive(Debug, clap::Parser, Clone)]
#[clap(
name = clap::crate_name!(),
version = clap::crate_version!(),
about = clap::crate_description!(),
after_help = AFTER_HELP_TEXT
)]
pub struct LogfileRequestPluginOpt {
    /// Turn-on the debug log level.
    ///
    /// If off only reports ERROR, WARN, and INFO
    /// If on also reports DEBUG and TRACE
    #[clap(long)]
    pub debug: bool,

    /// Create supported operation files
    #[clap(short, long)]
    pub init: bool,

    #[clap(long = "config-dir", default_value = DEFAULT_TEDGE_CONFIG_PATH)]
    pub config_dir: PathBuf,
}

// FIXME:
// - subscribing also to c8y bridge health topic to know when the bridge is up
//   topics.add(C8Y_BRIDGE_HEALTH_TOPIC)?;
// - use the health check actor

#[tokio::main]
async fn main() -> Result<(), anyhow::Error> {
    let config_plugin_opt = LogfileRequestPluginOpt::parse();
    let config_dir = config_plugin_opt.config_dir;

    // Load tedge config from the provided location
    let tedge_config_location = tedge_config::TEdgeConfigLocation::from_custom_root(&config_dir);
    let log_level = if config_plugin_opt.debug {
        tracing::Level::TRACE
    } else {
        get_log_level(
            "c8y-log-plugin",
            &tedge_config_location.tedge_config_root_path,
        )?
    };

    set_log_level(log_level);

    let config_repository = tedge_config::TEdgeConfigRepository::new(tedge_config_location.clone());
    let tedge_config = config_repository.load()?;

    if config_plugin_opt.init {
        let logs_dir = tedge_config.query(LogPathSetting)?;
        init(&config_dir, logs_dir.as_std_path()).with_context(|| {
            format!(
                "Failed to initialize {}. You have to run the command with sudo.",
                C8Y_LOG_PLUGIN
            )
        })
    } else {
        run(config_dir, tedge_config).await
    }
}

async fn run(config_dir: impl AsRef<Path>, tedge_config: TEdgeConfig) -> Result<(), anyhow::Error> {
    let runtime_events_logger = None;
    let mut runtime = Runtime::try_new(runtime_events_logger).await?;

    let base_mqtt_config = mqtt_config(&tedge_config)?;
    let mqtt_config = base_mqtt_config.clone().with_session_name(C8Y_LOG_PLUGIN);

    let c8y_http_config = (&tedge_config).try_into()?;

    let mut mqtt_actor = MqttActorBuilder::new(mqtt_config);
    let health_actor = HealthMonitorBuilder::new(C8Y_LOG_PLUGIN, &mut mqtt_actor);
    let mut jwt_actor = C8YJwtRetriever::builder(base_mqtt_config);
    let mut http_actor = HttpActor::new().builder();
    let mut c8y_http_proxy_actor =
        C8YHttpProxyBuilder::new(c8y_http_config, &mut http_actor, &mut jwt_actor);
    let mut fs_watch_actor = FsWatchActorBuilder::new();

    // Instantiate log manager actor
    let log_manager_config = LogManagerConfig::from_tedge_config(config_dir, &tedge_config)?;
    let log_actor = LogManagerBuilder::new(
        log_manager_config,
        &mut mqtt_actor,
        &mut c8y_http_proxy_actor,
        &mut fs_watch_actor,
    );

    // Shutdown on SIGINT
    let signal_actor = SignalActor::builder(&runtime.get_handle());

    // Run the actors
    runtime.spawn(mqtt_actor).await?;
    runtime.spawn(jwt_actor).await?;
    runtime.spawn(http_actor).await?;
    runtime.spawn(c8y_http_proxy_actor).await?;
    runtime.spawn(fs_watch_actor).await?;
    runtime.spawn(log_actor).await?;
    runtime.spawn(signal_actor).await?;
    runtime.spawn(health_actor).await?;

    info!("Ready to serve log requests");
    runtime.run_to_completion().await?;
    Ok(())
}

fn init(config_dir: &Path, logs_dir: &Path) -> Result<(), anyhow::Error> {
    info!("Creating supported operation files");
    create_init_logs_directories_and_files(config_dir, logs_dir)?;
    Ok(())
}

/// for the log plugin to work the following directories and files are needed:
///
/// Directories:
/// - LOGS_DIR/tedge/agent
/// - CONFIG_DIR/operations/c8y
/// - CONFIG_DIR/c8y
///
/// Files:
/// - CONFIG_DIR/operations/c8y/c8y_LogfileRequest
/// - CONFIG_DIR/c8y/c8y-log-plugin.toml
fn create_init_logs_directories_and_files(
    config_dir: &Path,
    logs_dir: &Path,
) -> Result<(), anyhow::Error> {
    // creating logs_dir
    create_directory_with_user_group(
        format!("{}/tedge", logs_dir.display()),
        "tedge",
        "tedge",
        0o755,
    )?;
    create_directory_with_user_group(
        format!("{}/tedge/agent", logs_dir.display()),
        "tedge",
        "tedge",
        0o755,
    )?;
    // creating /operations/c8y directories
    create_directory_with_user_group(
        format!("{}/operations", config_dir.display()),
        "tedge",
        "tedge",
        0o755,
    )?;
    create_directory_with_user_group(
        format!("{}/operations/c8y", config_dir.display()),
        "tedge",
        "tedge",
        0o755,
    )?;
    // creating c8y_LogfileRequest operation file
    create_file_with_user_group(
        format!("{}/operations/c8y/c8y_LogfileRequest", config_dir.display()),
        "tedge",
        "tedge",
        0o644,
        None,
    )?;
    // creating c8y directory
    create_directory_with_user_group(
        format!("{}/c8y", config_dir.display()),
        "root",
        "root",
        0o1777,
    )?;

    // creating c8y-log-plugin.toml
    let logs_path = format!("{}/tedge/agent/software-*", logs_dir.display());
    let data = format!(
        r#"files = [
    {{ type = "software-management", path = "{logs_path}" }},
]"#
    );

    create_file_with_user_group(
        format!("{}/{DEFAULT_PLUGIN_CONFIG_FILE}", config_dir.display()),
        "root",
        "root",
        0o644,
        Some(&data),
    )?;

    Ok(())
}

fn mqtt_config(tedge_config: &TEdgeConfig) -> Result<MqttConfig, MqttConfigBuildError> {
    tedge_config.mqtt_config()
}
