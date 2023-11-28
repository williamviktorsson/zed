mod update_notification;

use anyhow::{anyhow, Context, Result};
use client::{Client, TelemetrySettings, ZED_APP_PATH, ZED_APP_VERSION, ZED_SECRET_CLIENT_TOKEN};
use db::kvp::KEY_VALUE_STORE;
use db::RELEASE_CHANNEL;
use gpui::{
    actions, AppContext, AsyncAppContext, Context as _, Model, ModelContext, SemanticVersion, Task,
    ViewContext, VisualContext,
};
use isahc::AsyncBody;
use serde::Deserialize;
use serde_derive::Serialize;
use smol::io::AsyncReadExt;

use settings::{Settings, SettingsStore};
use smol::{fs::File, process::Command};
use std::{ffi::OsString, sync::Arc, time::Duration};
use update_notification::UpdateNotification;
use util::channel::{AppCommitSha, ReleaseChannel};
use util::http::HttpClient;
use workspace::Workspace;

const SHOULD_SHOW_UPDATE_NOTIFICATION_KEY: &str = "auto-updater-should-show-updated-notification";
const POLL_INTERVAL: Duration = Duration::from_secs(60 * 60);

//todo!(remove CheckThatAutoUpdaterWorks)
actions!(
    Check,
    DismissErrorMessage,
    ViewReleaseNotes,
    CheckThatAutoUpdaterWorks
);

#[derive(Serialize)]
struct UpdateRequestBody {
    installation_id: Option<Arc<str>>,
    release_channel: Option<&'static str>,
    telemetry: bool,
}

#[derive(Clone, Copy, PartialEq, Eq)]
pub enum AutoUpdateStatus {
    Idle,
    Checking,
    Downloading,
    Installing,
    Updated,
    Errored,
}

pub struct AutoUpdater {
    status: AutoUpdateStatus,
    current_version: SemanticVersion,
    http_client: Arc<dyn HttpClient>,
    pending_poll: Option<Task<Option<()>>>,
    server_url: String,
}

#[derive(Deserialize)]
struct JsonRelease {
    version: String,
    url: String,
}

struct AutoUpdateSetting(bool);

impl Settings for AutoUpdateSetting {
    const KEY: Option<&'static str> = Some("auto_update");

    type FileContent = Option<bool>;

    fn load(
        default_value: &Option<bool>,
        user_values: &[&Option<bool>],
        _: &mut AppContext,
    ) -> Result<Self> {
        Ok(Self(
            Self::json_merge(default_value, user_values)?.ok_or_else(Self::missing_default)?,
        ))
    }
}

pub fn init(http_client: Arc<dyn HttpClient>, server_url: String, cx: &mut AppContext) {
    AutoUpdateSetting::register(cx);

    cx.observe_new_views(|workspace: &mut Workspace, _cx| {
        workspace
            .register_action(|_, action: &Check, cx| check(action, cx))
            .register_action(|_, _action: &CheckThatAutoUpdaterWorks, cx| {
                let prompt = cx.prompt(gpui::PromptLevel::Info, "It does!", &["Ok"]);
                cx.spawn(|_, _cx| async move {
                    prompt.await.ok();
                })
                .detach();
            });

        // @nate - code to trigger update notification on launch
        // workspace.show_notification(0, _cx, |cx| {
        //     cx.build_view(|_| UpdateNotification::new(SemanticVersion::from_str("1.1.1").unwrap()))
        // });
    })
    .detach();

    if let Some(version) = *ZED_APP_VERSION {
        let auto_updater = cx.build_model(|cx| {
            let updater = AutoUpdater::new(version, http_client, server_url);

            let mut update_subscription = AutoUpdateSetting::get_global(cx)
                .0
                .then(|| updater.start_polling(cx));

            cx.observe_global::<SettingsStore>(move |updater, cx| {
                if AutoUpdateSetting::get_global(cx).0 {
                    if update_subscription.is_none() {
                        update_subscription = Some(updater.start_polling(cx))
                    }
                } else {
                    update_subscription.take();
                }
            })
            .detach();

            updater
        });
        cx.set_global(Some(auto_updater));
        //todo!(action)
        // cx.add_global_action(view_release_notes);
        // cx.add_action(UpdateNotification::dismiss);
    }
}

pub fn check(_: &Check, cx: &mut AppContext) {
    if let Some(updater) = AutoUpdater::get(cx) {
        updater.update(cx, |updater, cx| updater.poll(cx));
    }
}

pub fn view_release_notes(_: &ViewReleaseNotes, cx: &mut AppContext) {
    if let Some(auto_updater) = AutoUpdater::get(cx) {
        let auto_updater = auto_updater.read(cx);
        let server_url = &auto_updater.server_url;
        let current_version = auto_updater.current_version;
        if cx.has_global::<ReleaseChannel>() {
            match cx.global::<ReleaseChannel>() {
                ReleaseChannel::Dev => {}
                ReleaseChannel::Nightly => {}
                ReleaseChannel::Preview => {
                    cx.open_url(&format!("{server_url}/releases/preview/{current_version}"))
                }
                ReleaseChannel::Stable => {
                    cx.open_url(&format!("{server_url}/releases/stable/{current_version}"))
                }
            }
        }
    }
}

pub fn notify_of_any_new_update(cx: &mut ViewContext<Workspace>) -> Option<()> {
    let updater = AutoUpdater::get(cx)?;
    let version = updater.read(cx).current_version;
    let should_show_notification = updater.read(cx).should_show_update_notification(cx);

    cx.spawn(|workspace, mut cx| async move {
        let should_show_notification = should_show_notification.await?;
        if should_show_notification {
            workspace.update(&mut cx, |workspace, cx| {
                workspace.show_notification(0, cx, |cx| {
                    cx.build_view(|_| UpdateNotification::new(version))
                });
                updater
                    .read(cx)
                    .set_should_show_update_notification(false, cx)
                    .detach_and_log_err(cx);
            })?;
        }
        anyhow::Ok(())
    })
    .detach();

    None
}

impl AutoUpdater {
    pub fn get(cx: &mut AppContext) -> Option<Model<Self>> {
        cx.default_global::<Option<Model<Self>>>().clone()
    }

    fn new(
        current_version: SemanticVersion,
        http_client: Arc<dyn HttpClient>,
        server_url: String,
    ) -> Self {
        Self {
            status: AutoUpdateStatus::Idle,
            current_version,
            http_client,
            server_url,
            pending_poll: None,
        }
    }

    pub fn start_polling(&self, cx: &mut ModelContext<Self>) -> Task<Result<()>> {
        cx.spawn(|this, mut cx| async move {
            loop {
                this.update(&mut cx, |this, cx| this.poll(cx))?;
                cx.background_executor().timer(POLL_INTERVAL).await;
            }
        })
    }

    pub fn poll(&mut self, cx: &mut ModelContext<Self>) {
        if self.pending_poll.is_some() || self.status == AutoUpdateStatus::Updated {
            return;
        }

        self.status = AutoUpdateStatus::Checking;
        cx.notify();

        self.pending_poll = Some(cx.spawn(|this, mut cx| async move {
            let result = Self::update(this.upgrade()?, cx.clone()).await;
            this.update(&mut cx, |this, cx| {
                this.pending_poll = None;
                if let Err(error) = result {
                    log::error!("auto-update failed: error:{:?}", error);
                    this.status = AutoUpdateStatus::Errored;
                    cx.notify();
                }
            })
            .ok()
        }));
    }

    pub fn status(&self) -> AutoUpdateStatus {
        self.status
    }

    pub fn dismiss_error(&mut self, cx: &mut ModelContext<Self>) {
        self.status = AutoUpdateStatus::Idle;
        cx.notify();
    }

    async fn update(this: Model<Self>, mut cx: AsyncAppContext) -> Result<()> {
        let (client, server_url, current_version) = this.read_with(&cx, |this, _| {
            (
                this.http_client.clone(),
                this.server_url.clone(),
                this.current_version,
            )
        })?;

        let mut url_string = format!(
            "{server_url}/api/releases/latest?token={ZED_SECRET_CLIENT_TOKEN}&asset=Zed.dmg"
        );
        cx.update(|cx| {
            if cx.has_global::<ReleaseChannel>() {
                if let Some(param) = cx.global::<ReleaseChannel>().release_query_param() {
                    url_string += "&";
                    url_string += param;
                }
            }
        })?;

        let mut response = client.get(&url_string, Default::default(), true).await?;

        let mut body = Vec::new();
        response
            .body_mut()
            .read_to_end(&mut body)
            .await
            .context("error reading release")?;
        let release: JsonRelease =
            serde_json::from_slice(body.as_slice()).context("error deserializing release")?;

        let should_download = match *RELEASE_CHANNEL {
            ReleaseChannel::Nightly => cx
                .try_read_global::<AppCommitSha, _>(|sha, _| release.version != sha.0)
                .unwrap_or(true),
            _ => release.version.parse::<SemanticVersion>()? <= current_version,
        };

        if !should_download {
            this.update(&mut cx, |this, cx| {
                this.status = AutoUpdateStatus::Idle;
                cx.notify();
            })?;
            return Ok(());
        }

        this.update(&mut cx, |this, cx| {
            this.status = AutoUpdateStatus::Downloading;
            cx.notify();
        })?;

        let temp_dir = tempdir::TempDir::new("zed-auto-update")?;
        let dmg_path = temp_dir.path().join("Zed.dmg");
        let mount_path = temp_dir.path().join("Zed");
        let running_app_path = ZED_APP_PATH
            .clone()
            .map_or_else(|| cx.update(|cx| cx.app_path())?, Ok)?;
        let running_app_filename = running_app_path
            .file_name()
            .ok_or_else(|| anyhow!("invalid running app path"))?;
        let mut mounted_app_path: OsString = mount_path.join(running_app_filename).into();
        mounted_app_path.push("/");

        let mut dmg_file = File::create(&dmg_path).await?;

        let (installation_id, release_channel, telemetry) = cx.update(|cx| {
            let installation_id = cx.global::<Arc<Client>>().telemetry().installation_id();
            let release_channel = cx
                .has_global::<ReleaseChannel>()
                .then(|| cx.global::<ReleaseChannel>().display_name());
            let telemetry = TelemetrySettings::get_global(cx).metrics;

            (installation_id, release_channel, telemetry)
        })?;

        let request_body = AsyncBody::from(serde_json::to_string(&UpdateRequestBody {
            installation_id,
            release_channel,
            telemetry,
        })?);

        let mut response = client.get(&release.url, request_body, true).await?;
        smol::io::copy(response.body_mut(), &mut dmg_file).await?;
        log::info!("downloaded update. path:{:?}", dmg_path);

        this.update(&mut cx, |this, cx| {
            this.status = AutoUpdateStatus::Installing;
            cx.notify();
        })?;

        let output = Command::new("hdiutil")
            .args(&["attach", "-nobrowse"])
            .arg(&dmg_path)
            .arg("-mountroot")
            .arg(&temp_dir.path())
            .output()
            .await?;
        if !output.status.success() {
            Err(anyhow!(
                "failed to mount: {:?}",
                String::from_utf8_lossy(&output.stderr)
            ))?;
        }

        let output = Command::new("rsync")
            .args(&["-av", "--delete"])
            .arg(&mounted_app_path)
            .arg(&running_app_path)
            .output()
            .await?;
        if !output.status.success() {
            Err(anyhow!(
                "failed to copy app: {:?}",
                String::from_utf8_lossy(&output.stderr)
            ))?;
        }

        let output = Command::new("hdiutil")
            .args(&["detach"])
            .arg(&mount_path)
            .output()
            .await?;
        if !output.status.success() {
            Err(anyhow!(
                "failed to unmount: {:?}",
                String::from_utf8_lossy(&output.stderr)
            ))?;
        }

        this.update(&mut cx, |this, cx| {
            this.set_should_show_update_notification(true, cx)
                .detach_and_log_err(cx);
            this.status = AutoUpdateStatus::Updated;
            cx.notify();
        })?;
        Ok(())
    }

    fn set_should_show_update_notification(
        &self,
        should_show: bool,
        cx: &AppContext,
    ) -> Task<Result<()>> {
        cx.background_executor().spawn(async move {
            if should_show {
                KEY_VALUE_STORE
                    .write_kvp(
                        SHOULD_SHOW_UPDATE_NOTIFICATION_KEY.to_string(),
                        "".to_string(),
                    )
                    .await?;
            } else {
                KEY_VALUE_STORE
                    .delete_kvp(SHOULD_SHOW_UPDATE_NOTIFICATION_KEY.to_string())
                    .await?;
            }
            Ok(())
        })
    }

    fn should_show_update_notification(&self, cx: &AppContext) -> Task<Result<bool>> {
        cx.background_executor().spawn(async move {
            Ok(KEY_VALUE_STORE
                .read_kvp(SHOULD_SHOW_UPDATE_NOTIFICATION_KEY)?
                .is_some())
        })
    }
}