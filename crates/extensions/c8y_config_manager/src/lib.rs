mod actor;
mod child_device;
mod config;
mod download;
mod error;
mod plugin_config;
mod upload;

#[cfg(test)]
mod tests;

use self::child_device::ChildConfigOperationKey;
use actor::*;
use c8y_http_proxy::handle::C8YHttpProxy;
use c8y_http_proxy::messages::C8YRestRequest;
use c8y_http_proxy::messages::C8YRestResult;
pub use config::*;
use error::ConfigManagementError;
use std::path::Path;
use std::path::PathBuf;
use tedge_actors::futures::channel::mpsc;
use tedge_actors::Builder;
use tedge_actors::DynSender;
use tedge_actors::LinkError;
use tedge_actors::LoggingReceiver;
use tedge_actors::LoggingSender;
use tedge_actors::MessageSource;
use tedge_actors::NoConfig;
use tedge_actors::RuntimeRequest;
use tedge_actors::RuntimeRequestSink;
use tedge_actors::ServiceProvider;
use tedge_file_system_ext::FsWatchEvent;
use tedge_mqtt_ext::*;
use tedge_timer_ext::SetTimeout;
use tedge_utils::file::create_directory_with_user_group;
use tedge_utils::file::create_file_with_user_group;

pub fn init(config_dir: &Path) -> Result<(), ConfigManagementError> {
    create_directory_with_user_group(
        format!("{}/c8y", config_dir.display()),
        "root",
        "root",
        0o1777,
    )?;
    let example_config = r#"# Add the configurations to be managed by c8y-configuration-plugin

files = [
#    { path = '/etc/tedge/tedge.toml' },
#    { path = '/etc/tedge/mosquitto-conf/c8y-bridge.conf', type = 'c8y-bridge.conf' },
#    { path = '/etc/tedge/mosquitto-conf/tedge-mosquitto.conf', type = 'tedge-mosquitto.conf' },
#    { path = '/etc/mosquitto/mosquitto.conf', type = 'mosquitto.conf' },
#    { path = '/etc/tedge/c8y/example.txt', type = 'example', user = 'tedge', group = 'tedge', mode = 0o444 }
]"#;

    create_file_with_user_group(
        format!("{}/c8y/c8y-configuration-plugin.toml", config_dir.display()),
        "root",
        "root",
        0o644,
        Some(example_config),
    )?;

    create_directory_with_user_group(
        format!("{}/operations/c8y", config_dir.display()),
        "tedge",
        "tedge",
        0o775,
    )?;
    create_file_with_user_group(
        format!(
            "{}/operations/c8y/c8y_UploadConfigFile",
            config_dir.display()
        ),
        "tedge",
        "tedge",
        0o644,
        None,
    )?;
    create_file_with_user_group(
        format!(
            "{}/operations/c8y/c8y_DownloadConfigFile",
            config_dir.display()
        ),
        "tedge",
        "tedge",
        0o644,
        None,
    )?;
    Ok(())
}

/// An instance of the config manager
///
/// This is an actor builder.
pub struct ConfigManagerBuilder {
    config: ConfigManagerConfig,
    receiver: LoggingReceiver<ConfigInput>,
    mqtt_publisher: DynSender<MqttMessage>,
    c8y_http_proxy: C8YHttpProxy,
    timer_sender: DynSender<SetTimeout<ChildConfigOperationKey>>,
    signal_sender: mpsc::Sender<RuntimeRequest>,
}

impl ConfigManagerConfig {
    pub fn subscriptions(&self) -> TopicFilter {
        vec![
            "c8y/s/ds",
            "tedge/+/commands/res/config_snapshot",
            "tedge/+/commands/res/config_update",
        ]
        .try_into()
        .unwrap()
    }

    pub fn config_directory(&self) -> PathBuf {
        self.config_dir.clone().join(DEFAULT_OPERATION_DIR_NAME)
    }
}

impl ConfigManagerBuilder {
    pub fn new(
        config: ConfigManagerConfig,
        mqtt: &mut impl ServiceProvider<MqttMessage, MqttMessage, TopicFilter>,
        http: &mut impl ServiceProvider<C8YRestRequest, C8YRestResult, NoConfig>,
        timer: &mut impl ServiceProvider<OperationTimer, OperationTimeout, NoConfig>,
        fs_notify: &mut impl MessageSource<FsWatchEvent, PathBuf>,
    ) -> ConfigManagerBuilder {
        let (events_sender, events_receiver) = mpsc::channel(10);
        let (signal_sender, signal_receiver) = mpsc::channel(10);
        let receiver = LoggingReceiver::new(
            "C8Y-Config-Manager".into(),
            events_receiver,
            signal_receiver,
        );

        let mqtt_publisher =
            mqtt.connect_consumer(config.subscriptions(), events_sender.clone().into());
        let c8y_http_proxy = C8YHttpProxy::new("ConfigManager => C8Y", http);
        let timer_sender = timer.connect_consumer(NoConfig, events_sender.clone().into());
        fs_notify.register_peer(config.config_directory(), events_sender.into());

        ConfigManagerBuilder {
            config,
            receiver,
            mqtt_publisher,
            c8y_http_proxy,
            timer_sender,
            signal_sender,
        }
    }
}

impl RuntimeRequestSink for ConfigManagerBuilder {
    fn get_signal_sender(&self) -> DynSender<RuntimeRequest> {
        Box::new(self.signal_sender.clone())
    }
}

impl Builder<ConfigManagerActor> for ConfigManagerBuilder {
    type Error = LinkError;

    fn try_build(self) -> Result<ConfigManagerActor, Self::Error> {
        let mqtt_publisher =
            LoggingSender::new("ConfigManager MQTT publisher".into(), self.mqtt_publisher);
        let timer_sender = LoggingSender::new("ConfigManager timer".into(), self.timer_sender);

        let peers = ConfigManagerMessageBox::new(
            self.receiver,
            mqtt_publisher,
            self.c8y_http_proxy,
            timer_sender,
        );

        Ok(ConfigManagerActor::new(self.config, peers))
    }
}
