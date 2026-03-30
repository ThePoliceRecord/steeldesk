use crate::{
    common::CheckTestNatType,
    privacy_mode::PrivacyModeState,
    ui_interface::{get_local_option, set_local_option},
};
use bytes::Bytes;
use parity_tokio_ipc::{
    Connection as Conn, ConnectionClient as ConnClient, Endpoint, Incoming, SecurityAttributes,
};
use serde_derive::{Deserialize, Serialize};
use std::{
    collections::HashMap,
    sync::atomic::{AtomicBool, Ordering},
};
#[cfg(not(windows))]
use std::{fs::File, io::prelude::*};

/// Unix file mode for IPC sockets and related files.
/// Owner read+write only (0o600). This is a security invariant:
/// world-writable IPC sockets (0o777) allow any local user to connect
/// and modify remote-access passwords, configuration, etc. (CWE-732).
#[cfg(not(windows))]
const IPC_SOCKET_MODE: u32 = 0o0600;

#[cfg(all(feature = "flutter", feature = "plugin_framework"))]
#[cfg(not(any(target_os = "android", target_os = "ios")))]
use crate::plugin::ipc::Plugin;
#[cfg(not(any(target_os = "android", target_os = "ios")))]
pub use clipboard::ClipboardFile;
use hbb_common::{
    allow_err, bail, bytes,
    bytes_codec::BytesCodec,
    config::{
        self,
        keys::{self, OPTION_ALLOW_WEBSOCKET},
        Config, Config2,
    },
    futures::StreamExt as _,
    futures_util::sink::SinkExt,
    log, password_security as password, timeout,
    tokio::{
        self,
        io::{AsyncRead, AsyncWrite},
    },
    tokio_util::codec::Framed,
    ResultType,
};

use crate::{common::is_server, privacy_mode, rendezvous_mediator::RendezvousMediator};

// IPC actions here.
pub const IPC_ACTION_CLOSE: &str = "close";
pub static EXIT_RECV_CLOSE: AtomicBool = AtomicBool::new(true);

#[derive(Debug, Serialize, Deserialize, Clone)]
#[serde(tag = "t", content = "c")]
pub enum FS {
    ReadEmptyDirs {
        dir: String,
        include_hidden: bool,
    },
    ReadDir {
        dir: String,
        include_hidden: bool,
    },
    RemoveDir {
        path: String,
        id: i32,
        recursive: bool,
    },
    RemoveFile {
        path: String,
        id: i32,
        file_num: i32,
    },
    CreateDir {
        path: String,
        id: i32,
    },
    NewWrite {
        path: String,
        id: i32,
        file_num: i32,
        files: Vec<(String, u64)>,
        overwrite_detection: bool,
        total_size: u64,
        conn_id: i32,
    },
    CancelWrite {
        id: i32,
    },
    WriteBlock {
        id: i32,
        file_num: i32,
        data: Bytes,
        compressed: bool,
    },
    WriteDone {
        id: i32,
        file_num: i32,
    },
    WriteError {
        id: i32,
        file_num: i32,
        err: String,
    },
    WriteOffset {
        id: i32,
        file_num: i32,
        offset_blk: u32,
    },
    CheckDigest {
        id: i32,
        file_num: i32,
        file_size: u64,
        last_modified: u64,
        is_upload: bool,
        is_resume: bool,
    },
    SendConfirm(Vec<u8>),
    Rename {
        id: i32,
        path: String,
        new_name: String,
    },
    // CM-side file reading operations (Windows only)
    // These enable Connection Manager to read files and stream them back to Connection
    ReadFile {
        path: String,
        id: i32,
        file_num: i32,
        include_hidden: bool,
        conn_id: i32,
        overwrite_detection: bool,
    },
    CancelRead {
        id: i32,
        conn_id: i32,
    },
    SendConfirmForRead {
        id: i32,
        file_num: i32,
        skip: bool,
        offset_blk: u32,
        conn_id: i32,
    },
    ReadAllFiles {
        path: String,
        id: i32,
        include_hidden: bool,
        conn_id: i32,
    },
}

#[cfg(target_os = "windows")]
#[derive(Debug, Serialize, Deserialize, Clone)]
#[serde(tag = "t")]
pub struct ClipboardNonFile {
    pub compress: bool,
    pub content: bytes::Bytes,
    pub content_len: usize,
    pub next_raw: bool,
    pub width: i32,
    pub height: i32,
    // message.proto: ClipboardFormat
    pub format: i32,
    pub special_name: String,
}

#[cfg(not(any(target_os = "android", target_os = "ios")))]
#[derive(Debug, Serialize, Deserialize, Clone)]
#[serde(tag = "t", content = "c")]
pub enum DataKeyboard {
    Sequence(String),
    KeyDown(enigo::Key),
    KeyUp(enigo::Key),
    KeyClick(enigo::Key),
    GetKeyState(enigo::Key),
}

#[derive(Debug, Serialize, Deserialize, Clone)]
#[serde(tag = "t", content = "c")]
pub enum DataKeyboardResponse {
    GetKeyState(bool),
}

#[cfg(not(any(target_os = "android", target_os = "ios")))]
#[derive(Debug, Serialize, Deserialize, Clone)]
#[serde(tag = "t", content = "c")]
pub enum DataMouse {
    MoveTo(i32, i32),
    MoveRelative(i32, i32),
    Down(enigo::MouseButton),
    Up(enigo::MouseButton),
    Click(enigo::MouseButton),
    ScrollX(i32),
    ScrollY(i32),
    Refresh,
}

#[derive(Debug, Serialize, Deserialize, Clone)]
#[serde(tag = "t", content = "c")]
pub enum DataControl {
    Resolution {
        minx: i32,
        maxx: i32,
        miny: i32,
        maxy: i32,
    },
}

#[derive(Debug, Serialize, Deserialize, Clone)]
#[serde(tag = "t", content = "c")]
pub enum DataPortableService {
    Ping,
    Pong,
    ConnCount(Option<usize>),
    Mouse((Vec<u8>, i32, String, u32, bool, bool)),
    Pointer((Vec<u8>, i32)),
    Key(Vec<u8>),
    RequestStart,
    WillClose,
    CmShowElevation(bool),
}

#[derive(Debug, Serialize, Deserialize, Clone)]
#[serde(tag = "t", content = "c")]
pub enum Data {
    Login {
        id: i32,
        is_file_transfer: bool,
        is_view_camera: bool,
        is_terminal: bool,
        peer_id: String,
        name: String,
        avatar: String,
        authorized: bool,
        port_forward: String,
        keyboard: bool,
        clipboard: bool,
        audio: bool,
        file: bool,
        file_transfer_enabled: bool,
        restart: bool,
        recording: bool,
        block_input: bool,
        from_switch: bool,
    },
    ChatMessage {
        text: String,
    },
    SwitchPermission {
        name: String,
        enabled: bool,
    },
    SystemInfo(Option<String>),
    ClickTime(i64),
    #[cfg(not(any(target_os = "android", target_os = "ios")))]
    MouseMoveTime(i64),
    Authorize,
    Close,
    #[cfg(windows)]
    SAS,
    UserSid(Option<u32>),
    OnlineStatus(Option<(i64, bool)>),
    Config((String, Option<String>)),
    Options(Option<HashMap<String, String>>),
    NatType(Option<i32>),
    ConfirmedKey(Option<(Vec<u8>, Vec<u8>)>),
    RawMessage(Vec<u8>),
    Socks(Option<config::Socks5Server>),
    FS(FS),
    Test,
    SyncConfig(Option<Box<(Config, Config2)>>),
    #[cfg(target_os = "windows")]
    ClipboardFile(ClipboardFile),
    ClipboardFileEnabled(bool),
    #[cfg(target_os = "windows")]
    ClipboardNonFile(Option<(String, Vec<ClipboardNonFile>)>),
    PrivacyModeState((i32, PrivacyModeState, String)),
    TestRendezvousServer,
    #[cfg(not(any(target_os = "android", target_os = "ios")))]
    Keyboard(DataKeyboard),
    #[cfg(not(any(target_os = "android", target_os = "ios")))]
    KeyboardResponse(DataKeyboardResponse),
    #[cfg(not(any(target_os = "android", target_os = "ios")))]
    Mouse(DataMouse),
    Control(DataControl),
    Theme(String),
    Language(String),
    Empty,
    Disconnected,
    DataPortableService(DataPortableService),
    SwitchSidesRequest(String),
    SwitchSidesBack,
    UrlLink(String),
    VoiceCallIncoming,
    StartVoiceCall,
    VoiceCallResponse(bool),
    CloseVoiceCall(String),
    #[cfg(all(feature = "flutter", feature = "plugin_framework"))]
    #[cfg(not(any(target_os = "android", target_os = "ios")))]
    Plugin(Plugin),
    #[cfg(windows)]
    SyncWinCpuUsage(Option<f64>),
    FileTransferLog((String, String)),
    #[cfg(windows)]
    ControlledSessionCount(usize),
    CmErr(String),
    // CM-side file reading responses (Windows only)
    // These are sent from CM back to Connection when CM handles file reading
    /// Response to ReadFile: contains initial file list or error
    ReadJobInitResult {
        id: i32,
        file_num: i32,
        include_hidden: bool,
        conn_id: i32,
        /// Serialized protobuf bytes of FileDirectory, or error string
        result: Result<Vec<u8>, String>,
    },
    /// File data block read by CM.
    ///
    /// The actual data is sent separately via `send_raw()` after this message to avoid
    /// JSON encoding overhead for large binary data. This mirrors the `WriteBlock` pattern.
    ///
    /// **Protocol:**
    /// - Sender: `send(FileBlockFromCM{...})` then `send_raw(data)`
    /// - Receiver: `next()` returns `FileBlockFromCM`, then `next_raw()` returns data bytes
    ///
    /// **Note on empty data (e.g., empty files):**
    /// Empty data is supported. The IPC connection uses `BytesCodec` with `raw=false` (default),
    /// which prefixes each frame with a length header. So `send_raw(Bytes::new())` sends a
    /// 1-byte frame (length=0), and `next_raw()` correctly returns an empty `BytesMut`.
    /// See `libs/hbb_common/src/bytes_codec.rs` test `test_codec2` for verification.
    FileBlockFromCM {
        id: i32,
        file_num: i32,
        /// Data is sent separately via `send_raw()` to avoid JSON encoding overhead.
        /// This field is skipped during serialization; sender must call `send_raw()` after sending.
        /// Receiver must call `next_raw()` and populate this field manually.
        #[serde(skip)]
        data: bytes::Bytes,
        compressed: bool,
        conn_id: i32,
    },
    /// File read completed successfully
    FileReadDone {
        id: i32,
        file_num: i32,
        conn_id: i32,
    },
    /// File read failed with error
    FileReadError {
        id: i32,
        file_num: i32,
        err: String,
        conn_id: i32,
    },
    /// Digest info from CM for overwrite detection
    FileDigestFromCM {
        id: i32,
        file_num: i32,
        last_modified: u64,
        file_size: u64,
        is_resume: bool,
        conn_id: i32,
    },
    /// Response to ReadAllFiles: recursive directory listing
    AllFilesResult {
        id: i32,
        conn_id: i32,
        path: String,
        /// Serialized protobuf bytes of FileDirectory, or error string
        result: Result<Vec<u8>, String>,
    },
    CheckHwcodec,
    #[cfg(feature = "flutter")]
    VideoConnCount(Option<usize>),
    // Although the key is not necessary, it is used to avoid hardcoding the key.
    WaylandScreencastRestoreToken((String, String)),
    HwCodecConfig(Option<String>),
    RemoveTrustedDevices(Vec<Bytes>),
    ClearTrustedDevices,
    #[cfg(all(target_os = "windows", feature = "flutter"))]
    PrinterData(Vec<u8>),
    InstallOption(Option<(String, String)>),
    #[cfg(all(
        feature = "flutter",
        not(any(target_os = "android", target_os = "ios"))
    ))]
    ControllingSessionCount(usize),
    #[cfg(target_os = "linux")]
    TerminalSessionCount(usize),
    #[cfg(target_os = "windows")]
    PortForwardSessionCount(Option<usize>),
    SocksWs(Option<Box<(Option<config::Socks5Server>, String)>>),
    #[cfg(not(any(target_os = "android", target_os = "ios")))]
    Whiteboard((String, crate::whiteboard::CustomEvent)),
    ControlPermissionsRemoteModify(Option<bool>),
    #[cfg(target_os = "windows")]
    FileTransferEnabledState(Option<bool>),
}

#[tokio::main(flavor = "current_thread")]
pub async fn start(postfix: &str) -> ResultType<()> {
    let mut incoming = new_listener(postfix).await?;
    loop {
        if let Some(result) = incoming.next().await {
            match result {
                Ok(stream) => {
                    let mut stream = Connection::new(stream);
                    let postfix = postfix.to_owned();
                    tokio::spawn(async move {
                        loop {
                            match stream.next().await {
                                Err(err) => {
                                    log::trace!("ipc '{}' connection closed: {}", postfix, err);
                                    break;
                                }
                                Ok(Some(data)) => {
                                    handle(data, &mut stream).await;
                                }
                                _ => {}
                            }
                        }
                    });
                }
                Err(err) => {
                    log::error!("Couldn't get client: {:?}", err);
                }
            }
        }
    }
}

pub async fn new_listener(postfix: &str) -> ResultType<Incoming> {
    let path = Config::ipc_path(postfix);
    #[cfg(not(any(windows, target_os = "android", target_os = "ios")))]
    check_pid(postfix).await;
    let mut endpoint = Endpoint::new(path.clone());
    match SecurityAttributes::allow_everyone_create() {
        Ok(attr) => endpoint.set_security_attributes(attr),
        Err(err) => log::error!("Failed to set ipc{} security: {}", postfix, err),
    };
    match endpoint.incoming() {
        Ok(incoming) => {
            log::info!("Started ipc{} server at path: {}", postfix, &path);
            #[cfg(not(windows))]
            {
                use std::os::unix::fs::PermissionsExt;
                std::fs::set_permissions(&path, std::fs::Permissions::from_mode(IPC_SOCKET_MODE)).ok();
                write_pid(postfix);
            }
            Ok(incoming)
        }
        Err(err) => {
            log::error!(
                "Failed to start ipc{} server at path {}: {}",
                postfix,
                path,
                err
            );
            Err(err.into())
        }
    }
}

pub struct CheckIfRestart {
    stop_service: String,
    rendezvous_servers: Vec<String>,
    audio_input: String,
    voice_call_input: String,
    ws: String,
    disable_udp: String,
    allow_insecure_tls_fallback: String,
    api_server: String,
}

impl CheckIfRestart {
    pub fn new() -> CheckIfRestart {
        CheckIfRestart {
            stop_service: Config::get_option("stop-service"),
            rendezvous_servers: Config::get_rendezvous_servers(),
            audio_input: Config::get_option("audio-input"),
            voice_call_input: Config::get_option("voice-call-input"),
            ws: Config::get_option(OPTION_ALLOW_WEBSOCKET),
            disable_udp: Config::get_option(config::keys::OPTION_DISABLE_UDP),
            allow_insecure_tls_fallback: Config::get_option(
                config::keys::OPTION_ALLOW_INSECURE_TLS_FALLBACK,
            ),
            api_server: Config::get_option("api-server"),
        }
    }
}
impl Drop for CheckIfRestart {
    fn drop(&mut self) {
        // If https proxy is used, we need to restart rendezvous mediator.
        // No need to check if https proxy is used, because this option does not change frequently
        // and restarting mediator is safe even https proxy is not used.
        let allow_insecure_tls_fallback_changed = self.allow_insecure_tls_fallback
            != Config::get_option(config::keys::OPTION_ALLOW_INSECURE_TLS_FALLBACK);
        if allow_insecure_tls_fallback_changed
            || self.stop_service != Config::get_option("stop-service")
            || self.rendezvous_servers != Config::get_rendezvous_servers()
            || self.ws != Config::get_option(OPTION_ALLOW_WEBSOCKET)
            || self.disable_udp != Config::get_option(config::keys::OPTION_DISABLE_UDP)
            || self.api_server != Config::get_option("api-server")
        {
            if allow_insecure_tls_fallback_changed {
                hbb_common::tls::reset_tls_cache();
            }
            RendezvousMediator::restart();
        }
        if self.audio_input != Config::get_option("audio-input") {
            crate::audio_service::restart();
        }
        if self.voice_call_input != Config::get_option("voice-call-input") {
            crate::audio_service::set_voice_call_input_device(
                Some(Config::get_option("voice-call-input")),
                true,
            )
        }
    }
}

async fn handle(data: Data, stream: &mut Connection) {
    match data {
        Data::SystemInfo(_) => {
            let info = format!(
                "log_path: {}, config: {}, username: {}",
                Config::log_path().to_str().unwrap_or(""),
                Config::file().to_str().unwrap_or(""),
                crate::username(),
            );
            allow_err!(stream.send(&Data::SystemInfo(Some(info))).await);
        }
        Data::ClickTime(_) => {
            let t = crate::server::CLICK_TIME.load(Ordering::SeqCst);
            allow_err!(stream.send(&Data::ClickTime(t)).await);
        }
        #[cfg(not(any(target_os = "android", target_os = "ios")))]
        Data::MouseMoveTime(_) => {
            let t = crate::server::MOUSE_MOVE_TIME.load(Ordering::SeqCst);
            allow_err!(stream.send(&Data::MouseMoveTime(t)).await);
        }
        Data::Close => {
            log::info!("Receive close message");
            if EXIT_RECV_CLOSE.load(Ordering::SeqCst) {
                #[cfg(not(target_os = "android"))]
                crate::server::input_service::fix_key_down_timeout_at_exit();
                if is_server() {
                    let _ = privacy_mode::turn_off_privacy(0, Some(PrivacyModeState::OffByPeer));
                }
                #[cfg(any(target_os = "macos", target_os = "linux"))]
                if crate::is_main() {
                    // below part is for main windows can be reopen during rustdesk installation and installing service from UI
                    // this make new ipc server (domain socket) can be created.
                    std::fs::remove_file(&Config::ipc_path("")).ok();
                    #[cfg(target_os = "linux")]
                    {
                        hbb_common::sleep((crate::platform::SERVICE_INTERVAL * 2) as f32 / 1000.0)
                            .await;
                        // https://github.com/rustdesk/rustdesk/discussions/9254
                        crate::run_me::<&str>(vec!["--no-server"]).ok();
                    }
                    #[cfg(target_os = "macos")]
                    {
                        // our launchagent interval is 1 second
                        hbb_common::sleep(1.5).await;
                        std::process::Command::new("open")
                            .arg("-n")
                            .arg(&format!("/Applications/{}.app", crate::get_app_name()))
                            .spawn()
                            .ok();
                    }
                    // leave above open a little time
                    hbb_common::sleep(0.3).await;
                    // in case below exit failed
                    crate::platform::quit_gui();
                }
                std::process::exit(-1); // to make sure --server luauchagent process can restart because SuccessfulExit used
            }
        }
        Data::OnlineStatus(_) => {
            let x = config::get_online_state();
            let confirmed = Config::get_key_confirmed();
            allow_err!(stream.send(&Data::OnlineStatus(Some((x, confirmed)))).await);
        }
        Data::ConfirmedKey(None) => {
            let out = if Config::get_key_confirmed() {
                Some(Config::get_key_pair())
            } else {
                None
            };
            allow_err!(stream.send(&Data::ConfirmedKey(out)).await);
        }
        Data::Socks(s) => match s {
            None => {
                allow_err!(stream.send(&Data::Socks(Config::get_socks())).await);
            }
            Some(data) => {
                let _nat = CheckTestNatType::new();
                if data.proxy.is_empty() {
                    Config::set_socks(None);
                } else {
                    Config::set_socks(Some(data));
                }
                RendezvousMediator::restart();
                log::info!("socks updated");
            }
        },
        Data::SocksWs(s) => match s {
            None => {
                allow_err!(
                    stream
                        .send(&Data::SocksWs(Some(Box::new((
                            Config::get_socks(),
                            Config::get_option(OPTION_ALLOW_WEBSOCKET)
                        )))))
                        .await
                );
            }
            _ => {}
        },
        #[cfg(feature = "flutter")]
        Data::VideoConnCount(None) => {
            let n = crate::server::AUTHED_CONNS
                .lock()
                .unwrap()
                .iter()
                .filter(|x| x.conn_type == crate::server::AuthConnType::Remote)
                .count();
            allow_err!(stream.send(&Data::VideoConnCount(Some(n))).await);
        }
        Data::Config((name, value)) => match value {
            None => {
                let value;
                if name == "id" {
                    value = Some(Config::get_id());
                } else if name == "temporary-password" {
                    value = Some(password::temporary_password());
                } else if name == "permanent-password-storage-and-salt" {
                    let (storage, salt) = Config::get_local_permanent_password_storage_and_salt();
                    value = Some(storage + "\n" + &salt);
                } else if name == "permanent-password-set" {
                    value = Some(if Config::has_permanent_password() {
                        "Y".to_owned()
                    } else {
                        "N".to_owned()
                    });
                } else if name == "permanent-password-is-preset" {
                    let hard = config::HARD_SETTINGS
                        .read()
                        .unwrap()
                        .get("password")
                        .cloned()
                        .unwrap_or_default();
                    let is_preset =
                        !hard.is_empty() && Config::matches_permanent_password_plain(&hard);
                    value = Some(if is_preset {
                        "Y".to_owned()
                    } else {
                        "N".to_owned()
                    });
                } else if name == "salt" {
                    value = Some(Config::get_salt());
                } else if name == "rendezvous_server" {
                    value = Some(format!(
                        "{},{}",
                        Config::get_rendezvous_server(),
                        Config::get_rendezvous_servers().join(",")
                    ));
                } else if name == "rendezvous_servers" {
                    value = Some(Config::get_rendezvous_servers().join(","));
                } else if name == "fingerprint" {
                    value = if Config::get_key_confirmed() {
                        Some(crate::common::pk_to_fingerprint(Config::get_key_pair().1))
                    } else {
                        None
                    };
                } else if name == "hide_cm" {
                    value = if crate::hbbs_http::sync::is_pro() || crate::common::is_custom_client()
                    {
                        Some(hbb_common::password_security::hide_cm().to_string())
                    } else {
                        None
                    };
                } else if name == "voice-call-input" {
                    value = crate::audio_service::get_voice_call_input_device();
                } else if name == "unlock-pin" {
                    value = Some(Config::get_unlock_pin());
                } else if name == "trusted-devices" {
                    value = Some(Config::get_trusted_devices_json());
                } else {
                    value = None;
                }
                allow_err!(stream.send(&Data::Config((name, value))).await);
            }
            Some(value) => {
                let mut updated = true;
                if name == "id" {
                    Config::set_key_confirmed(false);
                    Config::set_id(&value);
                } else if name == "temporary-password" {
                    password::update_temporary_password();
                } else if name == "permanent-password" {
                    if Config::is_disable_change_permanent_password() {
                        log::warn!("Changing permanent password is disabled");
                        updated = false;
                    } else {
                        Config::set_permanent_password(&value);
                    }
                    // Explicitly ACK/NACK permanent-password writes. This allows UIs/FFI to
                    // distinguish "accepted by daemon" vs "IPC send succeeded" without
                    // reading back any secret.
                    let ack = if updated { "Y" } else { "N" }.to_owned();
                    allow_err!(stream.send(&Data::Config((name.clone(), Some(ack)))).await);
                } else if name == "salt" {
                    Config::set_salt(&value);
                } else if name == "voice-call-input" {
                    crate::audio_service::set_voice_call_input_device(Some(value), true);
                } else if name == "unlock-pin" {
                    Config::set_unlock_pin(&value);
                } else {
                    return;
                }
                if updated {
                    log::info!("{} updated", name);
                }
            }
        },
        Data::Options(value) => match value {
            None => {
                let v = Config::get_options();
                allow_err!(stream.send(&Data::Options(Some(v))).await);
            }
            Some(value) => {
                let _chk = CheckIfRestart::new();
                let _nat = CheckTestNatType::new();
                if let Some(v) = value.get("privacy-mode-impl-key") {
                    crate::privacy_mode::switch(v);
                }
                Config::set_options(value);
                allow_err!(stream.send(&Data::Options(None)).await);
            }
        },
        Data::NatType(_) => {
            let t = Config::get_nat_type();
            allow_err!(stream.send(&Data::NatType(Some(t))).await);
        }
        Data::SyncConfig(Some(configs)) => {
            let (config, config2) = *configs;
            let _chk = CheckIfRestart::new();
            Config::set(config);
            Config2::set(config2);
            allow_err!(stream.send(&Data::SyncConfig(None)).await);
        }
        Data::SyncConfig(None) => {
            allow_err!(
                stream
                    .send(&Data::SyncConfig(Some(
                        (Config::get(), Config2::get()).into()
                    )))
                    .await
            );
        }
        #[cfg(windows)]
        Data::SyncWinCpuUsage(None) => {
            allow_err!(
                stream
                    .send(&Data::SyncWinCpuUsage(
                        hbb_common::platform::windows::cpu_uage_one_minute()
                    ))
                    .await
            );
        }
        Data::TestRendezvousServer => {
            crate::test_rendezvous_server();
        }
        Data::SwitchSidesRequest(id) => {
            let uuid = uuid::Uuid::new_v4();
            crate::server::insert_switch_sides_uuid(id, uuid.clone());
            allow_err!(
                stream
                    .send(&Data::SwitchSidesRequest(uuid.to_string()))
                    .await
            );
        }
        #[cfg(all(feature = "flutter", feature = "plugin_framework"))]
        #[cfg(not(any(target_os = "android", target_os = "ios")))]
        Data::Plugin(plugin) => crate::plugin::ipc::handle_plugin(plugin, stream).await,
        #[cfg(windows)]
        Data::ControlledSessionCount(_) => {
            allow_err!(
                stream
                    .send(&Data::ControlledSessionCount(
                        crate::Connection::alive_conns().len()
                    ))
                    .await
            );
        }
        #[cfg(all(
            feature = "flutter",
            not(any(target_os = "android", target_os = "ios"))
        ))]
        Data::ControllingSessionCount(count) => {
            crate::updater::update_controlling_session_count(count);
        }
        #[cfg(target_os = "linux")]
        Data::TerminalSessionCount(_) => {
            let count = crate::terminal_service::get_terminal_session_count(true);
            allow_err!(stream.send(&Data::TerminalSessionCount(count)).await);
        }
        #[cfg(feature = "hwcodec")]
        #[cfg(not(any(target_os = "android", target_os = "ios")))]
        Data::CheckHwcodec => {
            scrap::hwcodec::start_check_process();
        }
        #[cfg(feature = "hwcodec")]
        #[cfg(not(any(target_os = "android", target_os = "ios")))]
        Data::HwCodecConfig(c) => {
            match c {
                None => {
                    let v = match scrap::hwcodec::HwCodecConfig::get_set_value() {
                        Some(v) => Some(serde_json::to_string(&v).unwrap_or_default()),
                        None => None,
                    };
                    allow_err!(stream.send(&Data::HwCodecConfig(v)).await);
                }
                Some(v) => {
                    // --server and portable
                    scrap::hwcodec::HwCodecConfig::set(v);
                }
            }
        }
        Data::WaylandScreencastRestoreToken((key, value)) => {
            let v = if value == "get" {
                let opt = get_local_option(key.clone());
                #[cfg(not(target_os = "linux"))]
                {
                    Some(opt)
                }
                #[cfg(target_os = "linux")]
                {
                    let v = if opt.is_empty() {
                        if scrap::wayland::pipewire::is_rdp_session_hold() {
                            "fake token".to_string()
                        } else {
                            "".to_owned()
                        }
                    } else {
                        opt
                    };
                    Some(v)
                }
            } else if value == "clear" {
                set_local_option(key.clone(), "".to_owned());
                #[cfg(target_os = "linux")]
                scrap::wayland::pipewire::close_session();
                Some("".to_owned())
            } else {
                None
            };
            if let Some(v) = v {
                allow_err!(
                    stream
                        .send(&Data::WaylandScreencastRestoreToken((key, v)))
                        .await
                );
            }
        }
        Data::RemoveTrustedDevices(v) => {
            Config::remove_trusted_devices(&v);
        }
        Data::ClearTrustedDevices => {
            Config::clear_trusted_devices();
        }
        Data::InstallOption(opt) => match opt {
            Some((_k, _v)) => {
                #[cfg(target_os = "windows")]
                if let Err(e) = crate::platform::windows::update_install_option(&_k, &_v) {
                    log::error!(
                        "Failed to update install option \"{}\" to \"{}\", error: {}",
                        &_k,
                        &_v,
                        e
                    );
                }
            }
            None => {
                // `None` is usually used to get values.
                // This branch is left blank for unification and further use.
            }
        },
        #[cfg(target_os = "windows")]
        Data::PortForwardSessionCount(c) => match c {
            None => {
                let count = crate::server::AUTHED_CONNS
                    .lock()
                    .unwrap()
                    .iter()
                    .filter(|c| c.conn_type == crate::server::AuthConnType::PortForward)
                    .count();
                allow_err!(
                    stream
                        .send(&Data::PortForwardSessionCount(Some(count)))
                        .await
                );
            }
            _ => {
                // Port forward session count is only a get value.
            }
        },
        Data::ControlPermissionsRemoteModify(_) => {
            use hbb_common::rendezvous_proto::control_permissions::Permission;
            let state =
                crate::server::get_control_permission_state(Permission::remote_modify, true);
            allow_err!(
                stream
                    .send(&Data::ControlPermissionsRemoteModify(state))
                    .await
            );
        }
        #[cfg(target_os = "windows")]
        Data::FileTransferEnabledState(_) => {
            use hbb_common::rendezvous_proto::control_permissions::Permission;
            let state = crate::server::get_control_permission_state(Permission::file, false);
            let enabled = state.unwrap_or_else(|| {
                crate::server::Connection::is_permission_enabled_locally(
                    config::keys::OPTION_ENABLE_FILE_TRANSFER,
                )
            });
            allow_err!(
                stream
                    .send(&Data::FileTransferEnabledState(Some(enabled)))
                    .await
            );
        }
        _ => {}
    }
}

pub async fn connect(ms_timeout: u64, postfix: &str) -> ResultType<ConnectionTmpl<ConnClient>> {
    let path = Config::ipc_path(postfix);
    let client = timeout(ms_timeout, Endpoint::connect(&path)).await??;
    Ok(ConnectionTmpl::new(client))
}

#[cfg(target_os = "linux")]
#[tokio::main(flavor = "current_thread")]
pub async fn start_pa() {
    use crate::audio_service::AUDIO_DATA_SIZE_U8;

    match new_listener("_pa").await {
        Ok(mut incoming) => {
            loop {
                if let Some(result) = incoming.next().await {
                    match result {
                        Ok(stream) => {
                            let mut stream = Connection::new(stream);
                            let mut device: String = "".to_owned();
                            if let Some(Ok(Some(Data::Config((_, Some(x)))))) =
                                stream.next_timeout2(1000).await
                            {
                                device = x;
                            }
                            if !device.is_empty() {
                                device = crate::platform::linux::get_pa_source_name(&device);
                            }
                            if device.is_empty() {
                                device = crate::platform::linux::get_pa_monitor();
                            }
                            if device.is_empty() {
                                continue;
                            }
                            let spec = pulse::sample::Spec {
                                format: pulse::sample::Format::F32le,
                                channels: 2,
                                rate: crate::platform::PA_SAMPLE_RATE,
                            };
                            log::info!("pa monitor: {:?}", device);
                            // systemctl --user status pulseaudio.service
                            let mut buf: Vec<u8> = vec![0; AUDIO_DATA_SIZE_U8];
                            match psimple::Simple::new(
                                None,                             // Use the default server
                                &crate::get_app_name(),           // Our application’s name
                                pulse::stream::Direction::Record, // We want a record stream
                                Some(&device),                    // Use the default device
                                "record",                         // Description of our stream
                                &spec,                            // Our sample format
                                None,                             // Use default channel map
                                None, // Use default buffering attributes
                            ) {
                                Ok(s) => loop {
                                    if let Ok(_) = s.read(&mut buf) {
                                        let out =
                                            if buf.iter().filter(|x| **x != 0).next().is_none() {
                                                vec![]
                                            } else {
                                                buf.clone()
                                            };
                                        if let Err(err) = stream.send_raw(out.into()).await {
                                            log::error!("Failed to send audio data:{}", err);
                                            break;
                                        }
                                    }
                                },
                                Err(err) => {
                                    log::error!("Could not create simple pulse: {}", err);
                                }
                            }
                        }
                        Err(err) => {
                            log::error!("Couldn't get pa client: {:?}", err);
                        }
                    }
                }
            }
        }
        Err(err) => {
            log::error!("Failed to start pa ipc server: {}", err);
        }
    }
}

#[inline]
#[cfg(not(windows))]
fn get_pid_file(postfix: &str) -> String {
    let path = Config::ipc_path(postfix);
    format!("{}.pid", path)
}

#[cfg(not(any(windows, target_os = "android", target_os = "ios")))]
async fn check_pid(postfix: &str) {
    let pid_file = get_pid_file(postfix);
    if let Ok(mut file) = File::open(&pid_file) {
        let mut content = String::new();
        file.read_to_string(&mut content).ok();
        let pid = content.parse::<usize>().unwrap_or(0);
        if pid > 0 {
            use hbb_common::sysinfo::System;
            let mut sys = System::new();
            sys.refresh_processes();
            if let Some(p) = sys.process(pid.into()) {
                if let Some(current) = sys.process((std::process::id() as usize).into()) {
                    if current.name() == p.name() {
                        // double check with connect
                        if connect(1000, postfix).await.is_ok() {
                            return;
                        }
                    }
                }
            }
        }
    }
    // if not remove old ipc file, the new ipc creation will fail
    // if we remove a ipc file, but the old ipc process is still running,
    // new connection to the ipc will connect to new ipc, old connection to old ipc still keep alive
    std::fs::remove_file(&Config::ipc_path(postfix)).ok();
}

#[inline]
#[cfg(not(windows))]
fn write_pid(postfix: &str) {
    let path = get_pid_file(postfix);
    if let Ok(mut file) = File::create(&path) {
        use std::os::unix::fs::PermissionsExt;
        std::fs::set_permissions(&path, std::fs::Permissions::from_mode(IPC_SOCKET_MODE)).ok();
        file.write_all(&std::process::id().to_string().into_bytes())
            .ok();
    }
}

pub struct ConnectionTmpl<T> {
    inner: Framed<T, BytesCodec>,
}

pub type Connection = ConnectionTmpl<Conn>;

impl<T> ConnectionTmpl<T>
where
    T: AsyncRead + AsyncWrite + std::marker::Unpin,
{
    pub fn new(conn: T) -> Self {
        Self {
            inner: Framed::new(conn, BytesCodec::new()),
        }
    }

    pub async fn send(&mut self, data: &Data) -> ResultType<()> {
        let v = serde_json::to_vec(data)?;
        self.inner.send(bytes::Bytes::from(v)).await?;
        Ok(())
    }

    async fn send_config(&mut self, name: &str, value: String) -> ResultType<()> {
        self.send(&Data::Config((name.to_owned(), Some(value))))
            .await
    }

    pub async fn next_timeout(&mut self, ms_timeout: u64) -> ResultType<Option<Data>> {
        Ok(timeout(ms_timeout, self.next()).await??)
    }

    pub async fn next_timeout2(&mut self, ms_timeout: u64) -> Option<ResultType<Option<Data>>> {
        if let Ok(x) = timeout(ms_timeout, self.next()).await {
            Some(x)
        } else {
            None
        }
    }

    pub async fn next(&mut self) -> ResultType<Option<Data>> {
        match self.inner.next().await {
            Some(res) => {
                let bytes = res?;
                if let Ok(s) = std::str::from_utf8(&bytes) {
                    if let Ok(data) = serde_json::from_str::<Data>(s) {
                        return Ok(Some(data));
                    }
                }
                return Ok(None);
            }
            _ => {
                bail!("reset by the peer");
            }
        }
    }

    pub async fn send_raw(&mut self, data: Bytes) -> ResultType<()> {
        self.inner.send(data).await?;
        Ok(())
    }

    pub async fn next_raw(&mut self) -> ResultType<bytes::BytesMut> {
        match self.inner.next().await {
            Some(Ok(res)) => Ok(res),
            _ => {
                bail!("reset by the peer");
            }
        }
    }
}

#[tokio::main(flavor = "current_thread")]
pub async fn get_config(name: &str) -> ResultType<Option<String>> {
    get_config_async(name, 1_000).await
}

async fn get_config_async(name: &str, ms_timeout: u64) -> ResultType<Option<String>> {
    let mut c = connect(ms_timeout, "").await?;
    c.send(&Data::Config((name.to_owned(), None))).await?;
    if let Some(Data::Config((name2, value))) = c.next_timeout(ms_timeout).await? {
        if name == name2 {
            return Ok(value);
        }
    }
    return Ok(None);
}

pub async fn set_config_async(name: &str, value: String) -> ResultType<()> {
    let mut c = connect(1000, "").await?;
    c.send_config(name, value).await?;
    Ok(())
}

#[tokio::main(flavor = "current_thread")]
pub async fn set_data(data: &Data) -> ResultType<()> {
    set_data_async(data).await
}

async fn set_data_async(data: &Data) -> ResultType<()> {
    let mut c = connect(1000, "").await?;
    c.send(data).await?;
    Ok(())
}

#[tokio::main(flavor = "current_thread")]
pub async fn set_config(name: &str, value: String) -> ResultType<()> {
    set_config_async(name, value).await
}

pub fn update_temporary_password() -> ResultType<()> {
    set_config("temporary-password", "".to_owned())
}

fn apply_permanent_password_storage_and_salt_payload(payload: Option<&str>) -> ResultType<()> {
    let Some(payload) = payload else {
        return Ok(());
    };
    let Some((storage, salt)) = payload.split_once('\n') else {
        bail!("Invalid permanent-password-storage-and-salt payload");
    };

    if storage.is_empty() {
        Config::set_permanent_password_storage_for_sync("", "")?;
        return Ok(());
    }

    Config::set_permanent_password_storage_for_sync(storage, salt)?;
    Ok(())
}

pub fn sync_permanent_password_storage_from_daemon() -> ResultType<()> {
    let v = get_config("permanent-password-storage-and-salt")?;
    apply_permanent_password_storage_and_salt_payload(v.as_deref())
}

async fn sync_permanent_password_storage_from_daemon_async() -> ResultType<()> {
    let ms_timeout = 1_000;
    let v = get_config_async("permanent-password-storage-and-salt", ms_timeout).await?;
    apply_permanent_password_storage_and_salt_payload(v.as_deref())
}

pub fn is_permanent_password_set() -> bool {
    match get_config("permanent-password-set") {
        Ok(Some(v)) => {
            let v = v.trim();
            return v == "Y";
        }
        Ok(None) => {
            // No response/value (timeout).
        }
        Err(_) => {
            // Connection error.
        }
    }
    log::warn!("Failed to query permanent password state from daemon");
    false
}

pub fn is_permanent_password_preset() -> bool {
    if let Ok(Some(v)) = get_config("permanent-password-is-preset") {
        let v = v.trim();
        return v == "Y";
    }
    false
}

pub fn get_fingerprint() -> String {
    get_config("fingerprint")
        .unwrap_or_default()
        .unwrap_or_default()
}

pub fn set_permanent_password(v: String) -> ResultType<()> {
    if Config::is_disable_change_permanent_password() {
        bail!("Changing permanent password is disabled");
    }
    if set_permanent_password_with_ack(v)? {
        Ok(())
    } else {
        bail!("Changing permanent password was rejected by daemon");
    }
}

#[tokio::main(flavor = "current_thread")]
pub async fn set_permanent_password_with_ack(v: String) -> ResultType<bool> {
    set_permanent_password_with_ack_async(v).await
}

async fn set_permanent_password_with_ack_async(v: String) -> ResultType<bool> {
    // The daemon ACK/NACK is expected quickly since it applies the config in-process.
    let ms_timeout = 1_000;
    let mut c = connect(ms_timeout, "").await?;
    c.send_config("permanent-password", v).await?;
    if let Some(Data::Config((name2, Some(v)))) = c.next_timeout(ms_timeout).await? {
        if name2 == "permanent-password" {
            let v = v.trim();
            let ok = v == "Y";
            if ok {
                // Ensure the hashed permanent password storage is written to the user config file.
                // This sync must not affect the daemon ACK outcome.
                if let Err(err) = sync_permanent_password_storage_from_daemon_async().await {
                    log::warn!("Failed to sync permanent password storage from daemon: {err}");
                }
            }
            return Ok(ok);
        }
    }
    Ok(false)
}

#[cfg(feature = "flutter")]
#[cfg(not(any(target_os = "android", target_os = "ios")))]
pub fn set_unlock_pin(v: String, translate: bool) -> ResultType<()> {
    let v = v.trim().to_owned();
    let min_len = 4;
    let max_len = crate::ui_interface::max_encrypt_len();
    let len = v.chars().count();
    if !v.is_empty() {
        if len < min_len {
            let err = if translate {
                crate::lang::translate(
                    "Requires at least {".to_string() + &format!("{min_len}") + "} characters",
                )
            } else {
                // Sometimes, translated can't show normally in command line
                format!("Requires at least {} characters", min_len)
            };
            bail!(err);
        }
        if len > max_len {
            bail!("No more than {max_len} characters");
        }
    }
    Config::set_unlock_pin(&v);
    set_config("unlock-pin", v)
}

#[cfg(feature = "flutter")]
#[cfg(not(any(target_os = "android", target_os = "ios")))]
pub fn get_unlock_pin() -> String {
    if let Ok(Some(v)) = get_config("unlock-pin") {
        Config::set_unlock_pin(&v);
        v
    } else {
        Config::get_unlock_pin()
    }
}

#[cfg(feature = "flutter")]
#[cfg(not(any(target_os = "android", target_os = "ios")))]
pub fn get_trusted_devices() -> String {
    if let Ok(Some(v)) = get_config("trusted-devices") {
        v
    } else {
        Config::get_trusted_devices_json()
    }
}

#[cfg(feature = "flutter")]
#[cfg(not(any(target_os = "android", target_os = "ios")))]
pub fn remove_trusted_devices(hwids: Vec<Bytes>) {
    Config::remove_trusted_devices(&hwids);
    allow_err!(set_data(&Data::RemoveTrustedDevices(hwids)));
}

#[cfg(feature = "flutter")]
#[cfg(not(any(target_os = "android", target_os = "ios")))]
pub fn clear_trusted_devices() {
    Config::clear_trusted_devices();
    allow_err!(set_data(&Data::ClearTrustedDevices));
}

pub fn get_id() -> String {
    if let Ok(Some(v)) = get_config("id") {
        // update salt also, so that next time reinstallation not causing first-time auto-login failure
        if let Ok(Some(v2)) = get_config("salt") {
            Config::set_salt(&v2);
        }
        if v != Config::get_id() {
            Config::set_key_confirmed(false);
            Config::set_id(&v);
        }
        v
    } else {
        Config::get_id()
    }
}

pub async fn get_rendezvous_server(ms_timeout: u64) -> (String, Vec<String>) {
    if let Ok(Some(v)) = get_config_async("rendezvous_server", ms_timeout).await {
        let mut urls = v.split(",");
        let a = urls.next().unwrap_or_default().to_owned();
        let b: Vec<String> = urls.map(|x| x.to_owned()).collect();
        (a, b)
    } else {
        (
            Config::get_rendezvous_server(),
            Config::get_rendezvous_servers(),
        )
    }
}

async fn get_options_(ms_timeout: u64) -> ResultType<HashMap<String, String>> {
    let mut c = connect(ms_timeout, "").await?;
    c.send(&Data::Options(None)).await?;
    if let Some(Data::Options(Some(value))) = c.next_timeout(ms_timeout).await? {
        Config::set_options(value.clone());
        Ok(value)
    } else {
        Ok(Config::get_options())
    }
}

pub async fn get_options_async() -> HashMap<String, String> {
    get_options_(1000).await.unwrap_or(Config::get_options())
}

#[tokio::main(flavor = "current_thread")]
pub async fn get_options() -> HashMap<String, String> {
    get_options_async().await
}

pub async fn get_option_async(key: &str) -> String {
    if let Some(v) = get_options_async().await.get(key) {
        v.clone()
    } else {
        "".to_owned()
    }
}

pub fn set_option(key: &str, value: &str) {
    let mut options = get_options();
    if value.is_empty() {
        options.remove(key);
    } else {
        options.insert(key.to_owned(), value.to_owned());
    }
    set_options(options).ok();
}

#[tokio::main(flavor = "current_thread")]
pub async fn set_options(value: HashMap<String, String>) -> ResultType<()> {
    let _nat = CheckTestNatType::new();
    if let Ok(mut c) = connect(1000, "").await {
        c.send(&Data::Options(Some(value.clone()))).await?;
        // do not put below before connect, because we need to check should_exit
        c.next_timeout(1000).await.ok();
    }
    Config::set_options(value);
    Ok(())
}

#[inline]
async fn get_nat_type_(ms_timeout: u64) -> ResultType<i32> {
    let mut c = connect(ms_timeout, "").await?;
    c.send(&Data::NatType(None)).await?;
    if let Some(Data::NatType(Some(value))) = c.next_timeout(ms_timeout).await? {
        Config::set_nat_type(value);
        Ok(value)
    } else {
        Ok(Config::get_nat_type())
    }
}

pub async fn get_nat_type(ms_timeout: u64) -> i32 {
    get_nat_type_(ms_timeout)
        .await
        .unwrap_or(Config::get_nat_type())
}

pub async fn get_rendezvous_servers(ms_timeout: u64) -> Vec<String> {
    if let Ok(Some(v)) = get_config_async("rendezvous_servers", ms_timeout).await {
        return v.split(',').map(|x| x.to_owned()).collect();
    }
    return Config::get_rendezvous_servers();
}

#[inline]
async fn get_socks_(ms_timeout: u64) -> ResultType<Option<config::Socks5Server>> {
    let mut c = connect(ms_timeout, "").await?;
    c.send(&Data::Socks(None)).await?;
    if let Some(Data::Socks(value)) = c.next_timeout(ms_timeout).await? {
        Config::set_socks(value.clone());
        Ok(value)
    } else {
        Ok(Config::get_socks())
    }
}

pub async fn get_socks_async(ms_timeout: u64) -> Option<config::Socks5Server> {
    get_socks_(ms_timeout).await.unwrap_or(Config::get_socks())
}

#[tokio::main(flavor = "current_thread")]
pub async fn get_socks() -> Option<config::Socks5Server> {
    get_socks_async(1_000).await
}

#[tokio::main(flavor = "current_thread")]
pub async fn set_socks(value: config::Socks5Server) -> ResultType<()> {
    let _nat = CheckTestNatType::new();
    Config::set_socks(if value.proxy.is_empty() {
        None
    } else {
        Some(value.clone())
    });
    connect(1_000, "")
        .await?
        .send(&Data::Socks(Some(value)))
        .await?;
    Ok(())
}

async fn get_socks_ws_(ms_timeout: u64) -> ResultType<(Option<config::Socks5Server>, String)> {
    let mut c = connect(ms_timeout, "").await?;
    c.send(&Data::SocksWs(None)).await?;
    if let Some(Data::SocksWs(Some(value))) = c.next_timeout(ms_timeout).await? {
        Config::set_socks(value.0.clone());
        Config::set_option(OPTION_ALLOW_WEBSOCKET.to_string(), value.1.clone());
        Ok(*value)
    } else {
        Ok((
            Config::get_socks(),
            Config::get_option(OPTION_ALLOW_WEBSOCKET),
        ))
    }
}

#[tokio::main(flavor = "current_thread")]
pub async fn get_socks_ws() -> (Option<config::Socks5Server>, String) {
    get_socks_ws_(1_000).await.unwrap_or((
        Config::get_socks(),
        Config::get_option(OPTION_ALLOW_WEBSOCKET),
    ))
}

pub fn get_proxy_status() -> bool {
    Config::get_socks().is_some()
}
#[tokio::main(flavor = "current_thread")]
pub async fn test_rendezvous_server() -> ResultType<()> {
    let mut c = connect(1000, "").await?;
    c.send(&Data::TestRendezvousServer).await?;
    Ok(())
}

#[tokio::main(flavor = "current_thread")]
pub async fn send_url_scheme(url: String) -> ResultType<()> {
    connect(1_000, "_url")
        .await?
        .send(&Data::UrlLink(url))
        .await?;
    Ok(())
}

// Emit `close` events to ipc.
pub fn close_all_instances() -> ResultType<bool> {
    match crate::ipc::send_url_scheme(IPC_ACTION_CLOSE.to_owned()) {
        Ok(_) => Ok(true),
        Err(err) => Err(err),
    }
}

#[tokio::main(flavor = "current_thread")]
pub async fn connect_to_user_session(usid: Option<u32>) -> ResultType<()> {
    let mut stream = crate::ipc::connect(1000, crate::POSTFIX_SERVICE).await?;
    timeout(1000, stream.send(&crate::ipc::Data::UserSid(usid))).await??;
    Ok(())
}

#[tokio::main(flavor = "current_thread")]
pub async fn notify_server_to_check_hwcodec() -> ResultType<()> {
    connect(1_000, "").await?.send(&&Data::CheckHwcodec).await?;
    Ok(())
}

#[cfg(target_os = "windows")]
pub async fn get_port_forward_session_count(ms_timeout: u64) -> ResultType<usize> {
    let mut c = connect(ms_timeout, "").await?;
    c.send(&Data::PortForwardSessionCount(None)).await?;
    if let Some(Data::PortForwardSessionCount(Some(count))) = c.next_timeout(ms_timeout).await? {
        return Ok(count);
    }
    bail!("Failed to get port forward session count");
}

#[cfg(feature = "hwcodec")]
#[cfg(not(any(target_os = "android", target_os = "ios")))]
#[tokio::main(flavor = "current_thread")]
pub async fn get_hwcodec_config_from_server() -> ResultType<()> {
    if !scrap::codec::enable_hwcodec_option() || scrap::hwcodec::HwCodecConfig::already_set() {
        return Ok(());
    }
    let mut c = connect(50, "").await?;
    c.send(&Data::HwCodecConfig(None)).await?;
    if let Some(Data::HwCodecConfig(v)) = c.next_timeout(50).await? {
        match v {
            Some(v) => {
                scrap::hwcodec::HwCodecConfig::set(v);
                return Ok(());
            }
            None => {
                bail!("hwcodec config is none");
            }
        }
    }
    bail!("failed to get hwcodec config");
}

#[cfg(feature = "hwcodec")]
#[cfg(not(any(target_os = "android", target_os = "ios")))]
pub fn client_get_hwcodec_config_thread(wait_sec: u64) {
    static ONCE: std::sync::Once = std::sync::Once::new();
    if !crate::platform::is_installed()
        || !scrap::codec::enable_hwcodec_option()
        || scrap::hwcodec::HwCodecConfig::already_set()
    {
        return;
    }
    ONCE.call_once(move || {
        std::thread::spawn(move || {
            std::thread::sleep(std::time::Duration::from_secs(1));
            let mut intervals: Vec<u64> = vec![wait_sec, 3, 3, 6, 9];
            for i in intervals.drain(..) {
                if i > 0 {
                    std::thread::sleep(std::time::Duration::from_secs(i));
                }
                if get_hwcodec_config_from_server().is_ok() {
                    break;
                }
            }
        });
    });
}

#[cfg(feature = "hwcodec")]
#[tokio::main(flavor = "current_thread")]
pub async fn hwcodec_process() {
    let s = scrap::hwcodec::check_available_hwcodec();
    for _ in 0..5 {
        match crate::ipc::connect(1000, "").await {
            Ok(mut conn) => {
                match conn
                    .send(&crate::ipc::Data::HwCodecConfig(Some(s.clone())))
                    .await
                {
                    Ok(()) => {
                        log::info!("send ok");
                        break;
                    }
                    Err(e) => {
                        log::error!("send failed: {e:?}");
                    }
                }
            }
            Err(e) => {
                log::error!("connect failed: {e:?}");
            }
        }
        std::thread::sleep(std::time::Duration::from_secs(1));
    }
}

#[tokio::main(flavor = "current_thread")]
pub async fn get_wayland_screencast_restore_token(key: String) -> ResultType<String> {
    let v = handle_wayland_screencast_restore_token(key, "get".to_owned()).await?;
    Ok(v.unwrap_or_default())
}

#[tokio::main(flavor = "current_thread")]
pub async fn clear_wayland_screencast_restore_token(key: String) -> ResultType<bool> {
    if let Some(v) = handle_wayland_screencast_restore_token(key, "clear".to_owned()).await? {
        return Ok(v.is_empty());
    }
    return Ok(false);
}

#[cfg(all(
    feature = "flutter",
    not(any(target_os = "android", target_os = "ios"))
))]
#[tokio::main(flavor = "current_thread")]
pub async fn update_controlling_session_count(count: usize) -> ResultType<()> {
    let mut c = connect(1000, "").await?;
    c.send(&Data::ControllingSessionCount(count)).await?;
    Ok(())
}

#[cfg(target_os = "linux")]
#[tokio::main(flavor = "current_thread")]
pub async fn get_terminal_session_count() -> ResultType<usize> {
    let ms_timeout = 1_000;
    let mut c = connect(ms_timeout, "").await?;
    c.send(&Data::TerminalSessionCount(0)).await?;
    if let Some(Data::TerminalSessionCount(c)) = c.next_timeout(ms_timeout).await? {
        return Ok(c);
    }
    Ok(0)
}

async fn handle_wayland_screencast_restore_token(
    key: String,
    value: String,
) -> ResultType<Option<String>> {
    let ms_timeout = 1_000;
    let mut c = connect(ms_timeout, "").await?;
    c.send(&Data::WaylandScreencastRestoreToken((key, value)))
        .await?;
    if let Some(Data::WaylandScreencastRestoreToken((_key, v))) = c.next_timeout(ms_timeout).await?
    {
        return Ok(Some(v));
    }
    return Ok(None);
}

#[tokio::main(flavor = "current_thread")]
pub async fn set_install_option(k: String, v: String) -> ResultType<()> {
    if let Ok(mut c) = connect(1000, "").await {
        c.send(&&Data::InstallOption(Some((k, v)))).await?;
        // do not put below before connect, because we need to check should_exit
        c.next_timeout(1000).await.ok();
    }
    Ok(())
}

#[cfg(test)]
mod test {
    use super::*;

    #[test]
    fn verify_ffi_enum_data_size() {
        println!("{}", std::mem::size_of::<Data>());
        assert!(std::mem::size_of::<Data>() <= 120);
    }

    // ---------------------------------------------------------------
    // Helper: serialize then deserialize, return the round-tripped value
    // ---------------------------------------------------------------
    fn round_trip(data: &Data) -> Data {
        let json = serde_json::to_string(data).expect("serialize failed");
        serde_json::from_str::<Data>(&json).expect("deserialize failed")
    }

    fn round_trip_fs(fs: &FS) -> FS {
        let json = serde_json::to_string(fs).expect("serialize failed");
        serde_json::from_str::<FS>(&json).expect("deserialize failed")
    }

    // ===============================================================
    // 1. Data enum serialization round-trips
    // ===============================================================

    #[test]
    fn data_login_round_trip() {
        let data = Data::Login {
            id: 42,
            is_file_transfer: true,
            is_view_camera: false,
            is_terminal: false,
            peer_id: "abc123".into(),
            name: "Alice".into(),
            avatar: "avatar.png".into(),
            authorized: true,
            port_forward: "".into(),
            keyboard: true,
            clipboard: true,
            audio: false,
            file: true,
            file_transfer_enabled: true,
            restart: false,
            recording: false,
            block_input: false,
            from_switch: false,
        };
        let rt = round_trip(&data);
        match rt {
            Data::Login {
                id,
                is_file_transfer,
                peer_id,
                name,
                authorized,
                keyboard,
                clipboard,
                file,
                ..
            } => {
                assert_eq!(id, 42);
                assert!(is_file_transfer);
                assert_eq!(peer_id, "abc123");
                assert_eq!(name, "Alice");
                assert!(authorized);
                assert!(keyboard);
                assert!(clipboard);
                assert!(file);
            }
            other => panic!("expected Data::Login, got {:?}", other),
        }
    }

    #[test]
    fn data_chat_message_round_trip() {
        let data = Data::ChatMessage {
            text: "hello world".into(),
        };
        let rt = round_trip(&data);
        match rt {
            Data::ChatMessage { text } => assert_eq!(text, "hello world"),
            other => panic!("expected ChatMessage, got {:?}", other),
        }
    }

    #[test]
    fn data_switch_permission_round_trip() {
        let data = Data::SwitchPermission {
            name: "clipboard".into(),
            enabled: true,
        };
        let rt = round_trip(&data);
        match rt {
            Data::SwitchPermission { name, enabled } => {
                assert_eq!(name, "clipboard");
                assert!(enabled);
            }
            other => panic!("expected SwitchPermission, got {:?}", other),
        }
    }

    #[test]
    fn data_system_info_round_trip() {
        let data = Data::SystemInfo(Some("test info".into()));
        match round_trip(&data) {
            Data::SystemInfo(Some(v)) => assert_eq!(v, "test info"),
            other => panic!("expected SystemInfo(Some), got {:?}", other),
        }

        let data_none = Data::SystemInfo(None);
        match round_trip(&data_none) {
            Data::SystemInfo(None) => {}
            other => panic!("expected SystemInfo(None), got {:?}", other),
        }
    }

    #[test]
    fn data_click_time_round_trip() {
        let data = Data::ClickTime(1234567890);
        match round_trip(&data) {
            Data::ClickTime(t) => assert_eq!(t, 1234567890),
            other => panic!("expected ClickTime, got {:?}", other),
        }
    }

    #[test]
    fn data_online_status_round_trip() {
        let data = Data::OnlineStatus(Some((99, true)));
        match round_trip(&data) {
            Data::OnlineStatus(Some((ts, confirmed))) => {
                assert_eq!(ts, 99);
                assert!(confirmed);
            }
            other => panic!("expected OnlineStatus, got {:?}", other),
        }

        let data_none = Data::OnlineStatus(None);
        match round_trip(&data_none) {
            Data::OnlineStatus(None) => {}
            other => panic!("expected OnlineStatus(None), got {:?}", other),
        }
    }

    #[test]
    fn data_config_round_trip() {
        // Config get request (value = None)
        let data = Data::Config(("id".into(), None));
        match round_trip(&data) {
            Data::Config((name, value)) => {
                assert_eq!(name, "id");
                assert!(value.is_none());
            }
            other => panic!("expected Config, got {:?}", other),
        }

        // Config set request (value = Some)
        let data = Data::Config(("id".into(), Some("my-id-123".into())));
        match round_trip(&data) {
            Data::Config((name, value)) => {
                assert_eq!(name, "id");
                assert_eq!(value.unwrap(), "my-id-123");
            }
            other => panic!("expected Config, got {:?}", other),
        }
    }

    #[test]
    fn data_options_round_trip() {
        let mut opts = HashMap::new();
        opts.insert("enable-tunnel".into(), "Y".into());
        opts.insert("enable-lan-discovery".into(), "N".into());
        let data = Data::Options(Some(opts.clone()));
        match round_trip(&data) {
            Data::Options(Some(v)) => {
                assert_eq!(v.get("enable-tunnel").map(|s| s.as_str()), Some("Y"));
                assert_eq!(
                    v.get("enable-lan-discovery").map(|s| s.as_str()),
                    Some("N")
                );
                assert_eq!(v.len(), 2);
            }
            other => panic!("expected Options(Some), got {:?}", other),
        }

        let data_none = Data::Options(None);
        match round_trip(&data_none) {
            Data::Options(None) => {}
            other => panic!("expected Options(None), got {:?}", other),
        }
    }

    #[test]
    fn data_nat_type_round_trip() {
        let data = Data::NatType(Some(2));
        match round_trip(&data) {
            Data::NatType(Some(v)) => assert_eq!(v, 2),
            other => panic!("expected NatType(Some(2)), got {:?}", other),
        }
    }

    #[test]
    fn data_confirmed_key_round_trip() {
        let sk = vec![1u8, 2, 3, 4];
        let pk = vec![5u8, 6, 7, 8];
        let data = Data::ConfirmedKey(Some((sk.clone(), pk.clone())));
        match round_trip(&data) {
            Data::ConfirmedKey(Some((s, p))) => {
                assert_eq!(s, sk);
                assert_eq!(p, pk);
            }
            other => panic!("expected ConfirmedKey, got {:?}", other),
        }

        let data_none = Data::ConfirmedKey(None);
        match round_trip(&data_none) {
            Data::ConfirmedKey(None) => {}
            other => panic!("expected ConfirmedKey(None), got {:?}", other),
        }
    }

    #[test]
    fn data_raw_message_round_trip() {
        let data = Data::RawMessage(vec![0xDE, 0xAD, 0xBE, 0xEF]);
        match round_trip(&data) {
            Data::RawMessage(v) => assert_eq!(v, vec![0xDE, 0xAD, 0xBE, 0xEF]),
            other => panic!("expected RawMessage, got {:?}", other),
        }
    }

    #[test]
    fn data_unit_variants_round_trip() {
        // Test all unit-like variants
        for data in [
            Data::Authorize,
            Data::Close,
            Data::Test,
            Data::TestRendezvousServer,
            Data::Empty,
            Data::Disconnected,
            Data::SwitchSidesBack,
            Data::VoiceCallIncoming,
            Data::StartVoiceCall,
            Data::CheckHwcodec,
            Data::ClearTrustedDevices,
        ] {
            let json = serde_json::to_string(&data).expect("serialize");
            let rt = serde_json::from_str::<Data>(&json).expect("deserialize");
            // Verify tag matches by re-serializing
            assert_eq!(
                serde_json::to_string(&rt).unwrap(),
                json,
                "round-trip mismatch for {:?}",
                data
            );
        }
    }

    #[test]
    fn data_socks_round_trip() {
        let socks = config::Socks5Server {
            proxy: "socks5://127.0.0.1:1080".into(),
            username: "user".into(),
            password: "pass".into(),
        };
        let data = Data::Socks(Some(socks));
        match round_trip(&data) {
            Data::Socks(Some(s)) => {
                assert_eq!(s.proxy, "socks5://127.0.0.1:1080");
                assert_eq!(s.username, "user");
                assert_eq!(s.password, "pass");
            }
            other => panic!("expected Socks(Some), got {:?}", other),
        }

        let data_none = Data::Socks(None);
        match round_trip(&data_none) {
            Data::Socks(None) => {}
            other => panic!("expected Socks(None), got {:?}", other),
        }
    }

    #[test]
    fn data_privacy_mode_state_round_trip() {
        let data = Data::PrivacyModeState((5, PrivacyModeState::OffByPeer, "mag".into()));
        match round_trip(&data) {
            Data::PrivacyModeState((conn_id, state, key)) => {
                assert_eq!(conn_id, 5);
                assert!(matches!(state, PrivacyModeState::OffByPeer));
                assert_eq!(key, "mag");
            }
            other => panic!("expected PrivacyModeState, got {:?}", other),
        }

        // Test all three PrivacyModeState variants
        for state in [
            PrivacyModeState::OffSucceeded,
            PrivacyModeState::OffByPeer,
            PrivacyModeState::OffUnknown,
        ] {
            let data = Data::PrivacyModeState((0, state.clone(), "test".into()));
            let json = serde_json::to_string(&data).unwrap();
            let rt = serde_json::from_str::<Data>(&json).unwrap();
            match rt {
                Data::PrivacyModeState((_, _, _)) => {} // just verify it round-trips
                _ => panic!("PrivacyModeState variant failed round-trip"),
            }
        }
    }

    #[test]
    fn data_theme_language_round_trip() {
        let data = Data::Theme("dark".into());
        match round_trip(&data) {
            Data::Theme(v) => assert_eq!(v, "dark"),
            other => panic!("expected Theme, got {:?}", other),
        }

        let data = Data::Language("en_US".into());
        match round_trip(&data) {
            Data::Language(v) => assert_eq!(v, "en_US"),
            other => panic!("expected Language, got {:?}", other),
        }
    }

    #[test]
    fn data_voice_call_round_trip() {
        let data = Data::VoiceCallResponse(true);
        match round_trip(&data) {
            Data::VoiceCallResponse(v) => assert!(v),
            other => panic!("expected VoiceCallResponse, got {:?}", other),
        }

        let data = Data::CloseVoiceCall("reason".into());
        match round_trip(&data) {
            Data::CloseVoiceCall(v) => assert_eq!(v, "reason"),
            other => panic!("expected CloseVoiceCall, got {:?}", other),
        }
    }

    #[test]
    fn data_url_link_round_trip() {
        let data = Data::UrlLink("rustdesk://connect/123456".into());
        match round_trip(&data) {
            Data::UrlLink(v) => assert_eq!(v, "rustdesk://connect/123456"),
            other => panic!("expected UrlLink, got {:?}", other),
        }
    }

    #[test]
    fn data_switch_sides_request_round_trip() {
        let data = Data::SwitchSidesRequest("some-uuid".into());
        match round_trip(&data) {
            Data::SwitchSidesRequest(v) => assert_eq!(v, "some-uuid"),
            other => panic!("expected SwitchSidesRequest, got {:?}", other),
        }
    }

    #[test]
    fn data_clipboard_file_enabled_round_trip() {
        let data = Data::ClipboardFileEnabled(true);
        match round_trip(&data) {
            Data::ClipboardFileEnabled(v) => assert!(v),
            other => panic!("expected ClipboardFileEnabled, got {:?}", other),
        }
    }

    #[test]
    fn data_file_transfer_log_round_trip() {
        let data = Data::FileTransferLog(("peer123".into(), "/tmp/file.txt".into()));
        match round_trip(&data) {
            Data::FileTransferLog((peer, path)) => {
                assert_eq!(peer, "peer123");
                assert_eq!(path, "/tmp/file.txt");
            }
            other => panic!("expected FileTransferLog, got {:?}", other),
        }
    }

    #[test]
    fn data_cm_err_round_trip() {
        let data = Data::CmErr("something went wrong".into());
        match round_trip(&data) {
            Data::CmErr(msg) => assert_eq!(msg, "something went wrong"),
            other => panic!("expected CmErr, got {:?}", other),
        }
    }

    #[test]
    fn data_user_sid_round_trip() {
        let data = Data::UserSid(Some(1001));
        match round_trip(&data) {
            Data::UserSid(Some(v)) => assert_eq!(v, 1001),
            other => panic!("expected UserSid(Some), got {:?}", other),
        }

        let data_none = Data::UserSid(None);
        match round_trip(&data_none) {
            Data::UserSid(None) => {}
            other => panic!("expected UserSid(None), got {:?}", other),
        }
    }

    #[test]
    fn data_wayland_screencast_restore_token_round_trip() {
        let data = Data::WaylandScreencastRestoreToken(("key1".into(), "token-val".into()));
        match round_trip(&data) {
            Data::WaylandScreencastRestoreToken((k, v)) => {
                assert_eq!(k, "key1");
                assert_eq!(v, "token-val");
            }
            other => panic!("expected WaylandScreencastRestoreToken, got {:?}", other),
        }
    }

    #[test]
    fn data_hwcodec_config_round_trip() {
        let data = Data::HwCodecConfig(Some("{\"encoders\":[]}".into()));
        match round_trip(&data) {
            Data::HwCodecConfig(Some(v)) => assert_eq!(v, "{\"encoders\":[]}"),
            other => panic!("expected HwCodecConfig(Some), got {:?}", other),
        }

        let data_none = Data::HwCodecConfig(None);
        match round_trip(&data_none) {
            Data::HwCodecConfig(None) => {}
            other => panic!("expected HwCodecConfig(None), got {:?}", other),
        }
    }

    #[test]
    fn data_remove_trusted_devices_round_trip() {
        let hwids = vec![Bytes::from(vec![1, 2, 3]), Bytes::from(vec![4, 5, 6])];
        let data = Data::RemoveTrustedDevices(hwids.clone());
        match round_trip(&data) {
            Data::RemoveTrustedDevices(v) => {
                assert_eq!(v.len(), 2);
                assert_eq!(v[0].as_ref(), &[1, 2, 3]);
                assert_eq!(v[1].as_ref(), &[4, 5, 6]);
            }
            other => panic!("expected RemoveTrustedDevices, got {:?}", other),
        }
    }

    #[test]
    fn data_install_option_round_trip() {
        let data = Data::InstallOption(Some(("key".into(), "value".into())));
        match round_trip(&data) {
            Data::InstallOption(Some((k, v))) => {
                assert_eq!(k, "key");
                assert_eq!(v, "value");
            }
            other => panic!("expected InstallOption(Some), got {:?}", other),
        }

        let data_none = Data::InstallOption(None);
        match round_trip(&data_none) {
            Data::InstallOption(None) => {}
            other => panic!("expected InstallOption(None), got {:?}", other),
        }
    }

    #[test]
    fn data_socks_ws_round_trip() {
        let socks = config::Socks5Server {
            proxy: "socks5://10.0.0.1:1080".into(),
            username: "".into(),
            password: "".into(),
        };
        let data = Data::SocksWs(Some(Box::new((Some(socks), "Y".into()))));
        match round_trip(&data) {
            Data::SocksWs(Some(inner)) => {
                let (s, ws) = *inner;
                assert_eq!(s.unwrap().proxy, "socks5://10.0.0.1:1080");
                assert_eq!(ws, "Y");
            }
            other => panic!("expected SocksWs(Some), got {:?}", other),
        }

        let data_none = Data::SocksWs(None);
        match round_trip(&data_none) {
            Data::SocksWs(None) => {}
            other => panic!("expected SocksWs(None), got {:?}", other),
        }
    }

    #[test]
    fn data_control_permissions_remote_modify_round_trip() {
        let data = Data::ControlPermissionsRemoteModify(Some(true));
        match round_trip(&data) {
            Data::ControlPermissionsRemoteModify(Some(v)) => assert!(v),
            other => panic!(
                "expected ControlPermissionsRemoteModify(Some(true)), got {:?}",
                other
            ),
        }
    }

    // ===============================================================
    // 2. FS enum serialization round-trips
    // ===============================================================

    #[test]
    fn fs_read_dir_round_trip() {
        let fs = FS::ReadDir {
            dir: "/home/user".into(),
            include_hidden: true,
        };
        match round_trip_fs(&fs) {
            FS::ReadDir {
                dir,
                include_hidden,
            } => {
                assert_eq!(dir, "/home/user");
                assert!(include_hidden);
            }
            other => panic!("expected ReadDir, got {:?}", other),
        }
    }

    #[test]
    fn fs_read_empty_dirs_round_trip() {
        let fs = FS::ReadEmptyDirs {
            dir: "/tmp".into(),
            include_hidden: false,
        };
        match round_trip_fs(&fs) {
            FS::ReadEmptyDirs {
                dir,
                include_hidden,
            } => {
                assert_eq!(dir, "/tmp");
                assert!(!include_hidden);
            }
            other => panic!("expected ReadEmptyDirs, got {:?}", other),
        }
    }

    #[test]
    fn fs_remove_dir_round_trip() {
        let fs = FS::RemoveDir {
            path: "/tmp/test".into(),
            id: 7,
            recursive: true,
        };
        match round_trip_fs(&fs) {
            FS::RemoveDir {
                path,
                id,
                recursive,
            } => {
                assert_eq!(path, "/tmp/test");
                assert_eq!(id, 7);
                assert!(recursive);
            }
            other => panic!("expected RemoveDir, got {:?}", other),
        }
    }

    #[test]
    fn fs_remove_file_round_trip() {
        let fs = FS::RemoveFile {
            path: "/tmp/file.txt".into(),
            id: 3,
            file_num: 1,
        };
        match round_trip_fs(&fs) {
            FS::RemoveFile {
                path,
                id,
                file_num,
            } => {
                assert_eq!(path, "/tmp/file.txt");
                assert_eq!(id, 3);
                assert_eq!(file_num, 1);
            }
            other => panic!("expected RemoveFile, got {:?}", other),
        }
    }

    #[test]
    fn fs_create_dir_round_trip() {
        let fs = FS::CreateDir {
            path: "/tmp/newdir".into(),
            id: 10,
        };
        match round_trip_fs(&fs) {
            FS::CreateDir { path, id } => {
                assert_eq!(path, "/tmp/newdir");
                assert_eq!(id, 10);
            }
            other => panic!("expected CreateDir, got {:?}", other),
        }
    }

    #[test]
    fn fs_new_write_round_trip() {
        let fs = FS::NewWrite {
            path: "/tmp/upload".into(),
            id: 1,
            file_num: 0,
            files: vec![("file1.txt".into(), 1024), ("file2.txt".into(), 2048)],
            overwrite_detection: true,
            total_size: 3072,
            conn_id: 42,
        };
        match round_trip_fs(&fs) {
            FS::NewWrite {
                path,
                id,
                file_num,
                files,
                overwrite_detection,
                total_size,
                conn_id,
            } => {
                assert_eq!(path, "/tmp/upload");
                assert_eq!(id, 1);
                assert_eq!(file_num, 0);
                assert_eq!(files.len(), 2);
                assert_eq!(files[0], ("file1.txt".into(), 1024));
                assert_eq!(files[1], ("file2.txt".into(), 2048));
                assert!(overwrite_detection);
                assert_eq!(total_size, 3072);
                assert_eq!(conn_id, 42);
            }
            other => panic!("expected NewWrite, got {:?}", other),
        }
    }

    #[test]
    fn fs_cancel_write_round_trip() {
        let fs = FS::CancelWrite { id: 99 };
        match round_trip_fs(&fs) {
            FS::CancelWrite { id } => assert_eq!(id, 99),
            other => panic!("expected CancelWrite, got {:?}", other),
        }
    }

    #[test]
    fn fs_write_block_round_trip() {
        let fs = FS::WriteBlock {
            id: 1,
            file_num: 0,
            data: Bytes::from(vec![0xAA, 0xBB, 0xCC]),
            compressed: true,
        };
        match round_trip_fs(&fs) {
            FS::WriteBlock {
                id,
                file_num,
                data,
                compressed,
            } => {
                assert_eq!(id, 1);
                assert_eq!(file_num, 0);
                assert_eq!(data.as_ref(), &[0xAA, 0xBB, 0xCC]);
                assert!(compressed);
            }
            other => panic!("expected WriteBlock, got {:?}", other),
        }
    }

    #[test]
    fn fs_write_done_round_trip() {
        let fs = FS::WriteDone {
            id: 5,
            file_num: 3,
        };
        match round_trip_fs(&fs) {
            FS::WriteDone { id, file_num } => {
                assert_eq!(id, 5);
                assert_eq!(file_num, 3);
            }
            other => panic!("expected WriteDone, got {:?}", other),
        }
    }

    #[test]
    fn fs_write_error_round_trip() {
        let fs = FS::WriteError {
            id: 2,
            file_num: 1,
            err: "disk full".into(),
        };
        match round_trip_fs(&fs) {
            FS::WriteError {
                id,
                file_num,
                err,
            } => {
                assert_eq!(id, 2);
                assert_eq!(file_num, 1);
                assert_eq!(err, "disk full");
            }
            other => panic!("expected WriteError, got {:?}", other),
        }
    }

    #[test]
    fn fs_write_offset_round_trip() {
        let fs = FS::WriteOffset {
            id: 1,
            file_num: 0,
            offset_blk: 512,
        };
        match round_trip_fs(&fs) {
            FS::WriteOffset {
                id,
                file_num,
                offset_blk,
            } => {
                assert_eq!(id, 1);
                assert_eq!(file_num, 0);
                assert_eq!(offset_blk, 512);
            }
            other => panic!("expected WriteOffset, got {:?}", other),
        }
    }

    #[test]
    fn fs_check_digest_round_trip() {
        let fs = FS::CheckDigest {
            id: 3,
            file_num: 2,
            file_size: 999999,
            last_modified: 1700000000,
            is_upload: true,
            is_resume: false,
        };
        match round_trip_fs(&fs) {
            FS::CheckDigest {
                id,
                file_num,
                file_size,
                last_modified,
                is_upload,
                is_resume,
            } => {
                assert_eq!(id, 3);
                assert_eq!(file_num, 2);
                assert_eq!(file_size, 999999);
                assert_eq!(last_modified, 1700000000);
                assert!(is_upload);
                assert!(!is_resume);
            }
            other => panic!("expected CheckDigest, got {:?}", other),
        }
    }

    #[test]
    fn fs_send_confirm_round_trip() {
        let fs = FS::SendConfirm(vec![1, 2, 3, 4, 5]);
        match round_trip_fs(&fs) {
            FS::SendConfirm(v) => assert_eq!(v, vec![1, 2, 3, 4, 5]),
            other => panic!("expected SendConfirm, got {:?}", other),
        }
    }

    #[test]
    fn fs_rename_round_trip() {
        let fs = FS::Rename {
            id: 1,
            path: "/tmp/old.txt".into(),
            new_name: "new.txt".into(),
        };
        match round_trip_fs(&fs) {
            FS::Rename { id, path, new_name } => {
                assert_eq!(id, 1);
                assert_eq!(path, "/tmp/old.txt");
                assert_eq!(new_name, "new.txt");
            }
            other => panic!("expected Rename, got {:?}", other),
        }
    }

    #[test]
    fn fs_read_file_round_trip() {
        let fs = FS::ReadFile {
            path: "/tmp/read.txt".into(),
            id: 10,
            file_num: 0,
            include_hidden: false,
            conn_id: 5,
            overwrite_detection: true,
        };
        match round_trip_fs(&fs) {
            FS::ReadFile {
                path,
                id,
                file_num,
                include_hidden,
                conn_id,
                overwrite_detection,
            } => {
                assert_eq!(path, "/tmp/read.txt");
                assert_eq!(id, 10);
                assert_eq!(file_num, 0);
                assert!(!include_hidden);
                assert_eq!(conn_id, 5);
                assert!(overwrite_detection);
            }
            other => panic!("expected ReadFile, got {:?}", other),
        }
    }

    #[test]
    fn fs_cancel_read_round_trip() {
        let fs = FS::CancelRead {
            id: 7,
            conn_id: 3,
        };
        match round_trip_fs(&fs) {
            FS::CancelRead { id, conn_id } => {
                assert_eq!(id, 7);
                assert_eq!(conn_id, 3);
            }
            other => panic!("expected CancelRead, got {:?}", other),
        }
    }

    #[test]
    fn fs_send_confirm_for_read_round_trip() {
        let fs = FS::SendConfirmForRead {
            id: 1,
            file_num: 2,
            skip: true,
            offset_blk: 100,
            conn_id: 9,
        };
        match round_trip_fs(&fs) {
            FS::SendConfirmForRead {
                id,
                file_num,
                skip,
                offset_blk,
                conn_id,
            } => {
                assert_eq!(id, 1);
                assert_eq!(file_num, 2);
                assert!(skip);
                assert_eq!(offset_blk, 100);
                assert_eq!(conn_id, 9);
            }
            other => panic!("expected SendConfirmForRead, got {:?}", other),
        }
    }

    #[test]
    fn fs_read_all_files_round_trip() {
        let fs = FS::ReadAllFiles {
            path: "/home".into(),
            id: 4,
            include_hidden: true,
            conn_id: 2,
        };
        match round_trip_fs(&fs) {
            FS::ReadAllFiles {
                path,
                id,
                include_hidden,
                conn_id,
            } => {
                assert_eq!(path, "/home");
                assert_eq!(id, 4);
                assert!(include_hidden);
                assert_eq!(conn_id, 2);
            }
            other => panic!("expected ReadAllFiles, got {:?}", other),
        }
    }

    // FS wrapped in Data::FS
    #[test]
    fn data_fs_round_trip() {
        let data = Data::FS(FS::ReadDir {
            dir: "/var".into(),
            include_hidden: false,
        });
        match round_trip(&data) {
            Data::FS(FS::ReadDir {
                dir,
                include_hidden,
            }) => {
                assert_eq!(dir, "/var");
                assert!(!include_hidden);
            }
            other => panic!("expected Data::FS(ReadDir), got {:?}", other),
        }
    }

    // ===============================================================
    // 3. DataKeyboard / DataKeyboardResponse / DataMouse / DataControl
    // ===============================================================

    #[test]
    fn data_keyboard_sequence_round_trip() {
        let dk = DataKeyboard::Sequence("hello".into());
        let json = serde_json::to_string(&dk).unwrap();
        let rt: DataKeyboard = serde_json::from_str(&json).unwrap();
        match rt {
            DataKeyboard::Sequence(s) => assert_eq!(s, "hello"),
            other => panic!("expected Sequence, got {:?}", other),
        }
    }

    #[test]
    fn data_keyboard_key_variants_round_trip() {
        // Test KeyDown, KeyUp, KeyClick with a representative key
        for (variant_name, dk) in [
            ("KeyDown", DataKeyboard::KeyDown(enigo::Key::Return)),
            ("KeyUp", DataKeyboard::KeyUp(enigo::Key::Alt)),
            ("KeyClick", DataKeyboard::KeyClick(enigo::Key::Tab)),
            (
                "GetKeyState",
                DataKeyboard::GetKeyState(enigo::Key::CapsLock),
            ),
        ] {
            let json = serde_json::to_string(&dk).unwrap();
            let rt: DataKeyboard = serde_json::from_str(&json).unwrap();
            let json_rt = serde_json::to_string(&rt).unwrap();
            assert_eq!(json, json_rt, "round-trip mismatch for {}", variant_name);
        }
    }

    #[test]
    fn data_keyboard_response_round_trip() {
        let resp = DataKeyboardResponse::GetKeyState(true);
        let json = serde_json::to_string(&resp).unwrap();
        let rt: DataKeyboardResponse = serde_json::from_str(&json).unwrap();
        match rt {
            DataKeyboardResponse::GetKeyState(v) => assert!(v),
        }

        let resp_false = DataKeyboardResponse::GetKeyState(false);
        let json = serde_json::to_string(&resp_false).unwrap();
        let rt: DataKeyboardResponse = serde_json::from_str(&json).unwrap();
        match rt {
            DataKeyboardResponse::GetKeyState(v) => assert!(!v),
        }
    }

    #[test]
    fn data_mouse_variants_round_trip() {
        let variants: Vec<DataMouse> = vec![
            DataMouse::MoveTo(100, 200),
            DataMouse::MoveRelative(-5, 10),
            DataMouse::Down(enigo::MouseButton::Left),
            DataMouse::Up(enigo::MouseButton::Right),
            DataMouse::Click(enigo::MouseButton::Middle),
            DataMouse::ScrollX(3),
            DataMouse::ScrollY(-5),
            DataMouse::Refresh,
        ];
        for dm in &variants {
            let json = serde_json::to_string(dm).unwrap();
            let rt: DataMouse = serde_json::from_str(&json).unwrap();
            let json_rt = serde_json::to_string(&rt).unwrap();
            assert_eq!(json, json_rt, "round-trip mismatch for {:?}", dm);
        }
    }

    #[test]
    fn data_control_resolution_round_trip() {
        let dc = DataControl::Resolution {
            minx: 0,
            maxx: 1920,
            miny: 0,
            maxy: 1080,
        };
        let json = serde_json::to_string(&dc).unwrap();
        let rt: DataControl = serde_json::from_str(&json).unwrap();
        match rt {
            DataControl::Resolution {
                minx,
                maxx,
                miny,
                maxy,
            } => {
                assert_eq!(minx, 0);
                assert_eq!(maxx, 1920);
                assert_eq!(miny, 0);
                assert_eq!(maxy, 1080);
            }
        }
    }

    // ===============================================================
    // 4. DataPortableService serialization round-trips
    // ===============================================================

    #[test]
    fn data_portable_service_round_trip() {
        let variants: Vec<DataPortableService> = vec![
            DataPortableService::Ping,
            DataPortableService::Pong,
            DataPortableService::ConnCount(Some(5)),
            DataPortableService::ConnCount(None),
            DataPortableService::RequestStart,
            DataPortableService::WillClose,
            DataPortableService::CmShowElevation(true),
            DataPortableService::CmShowElevation(false),
            DataPortableService::Key(vec![1, 2, 3]),
            DataPortableService::Pointer((vec![4, 5], 10)),
            DataPortableService::Mouse((vec![6, 7], 20, "test".into(), 3, true, false)),
        ];
        for dps in &variants {
            let json = serde_json::to_string(dps).unwrap();
            let rt: DataPortableService = serde_json::from_str(&json).unwrap();
            let json_rt = serde_json::to_string(&rt).unwrap();
            assert_eq!(json, json_rt, "round-trip mismatch for {:?}", dps);
        }
    }

    #[test]
    fn data_portable_service_wrapped_round_trip() {
        let data = Data::DataPortableService(DataPortableService::Ping);
        match round_trip(&data) {
            Data::DataPortableService(DataPortableService::Ping) => {}
            other => panic!(
                "expected Data::DataPortableService(Ping), got {:?}",
                other
            ),
        }
    }

    // ===============================================================
    // 5. CM-side file reading structs serialization
    // ===============================================================

    #[test]
    fn data_read_job_init_result_round_trip() {
        // Success case
        let data = Data::ReadJobInitResult {
            id: 1,
            file_num: 0,
            include_hidden: true,
            conn_id: 42,
            result: Ok(vec![10, 20, 30]),
        };
        match round_trip(&data) {
            Data::ReadJobInitResult {
                id,
                file_num,
                include_hidden,
                conn_id,
                result,
            } => {
                assert_eq!(id, 1);
                assert_eq!(file_num, 0);
                assert!(include_hidden);
                assert_eq!(conn_id, 42);
                assert_eq!(result.unwrap(), vec![10, 20, 30]);
            }
            other => panic!("expected ReadJobInitResult, got {:?}", other),
        }

        // Error case
        let data = Data::ReadJobInitResult {
            id: 2,
            file_num: 1,
            include_hidden: false,
            conn_id: 10,
            result: Err("file not found".into()),
        };
        match round_trip(&data) {
            Data::ReadJobInitResult { result, .. } => {
                assert_eq!(result.unwrap_err(), "file not found");
            }
            other => panic!("expected ReadJobInitResult, got {:?}", other),
        }
    }

    #[test]
    fn data_file_block_from_cm_serde_skip_data() {
        // FileBlockFromCM has #[serde(skip)] on the `data` field.
        // After round-trip, data should be empty (default Bytes).
        let data = Data::FileBlockFromCM {
            id: 5,
            file_num: 2,
            data: Bytes::from(vec![0xAA, 0xBB]),
            compressed: true,
            conn_id: 7,
        };
        let json = serde_json::to_string(&data).unwrap();
        // Verify the JSON does NOT contain the data bytes
        assert!(
            !json.contains("0xAA") && !json.contains("170"),
            "serde(skip) should omit data field from JSON, but got: {}",
            json
        );

        let rt: Data = serde_json::from_str(&json).unwrap();
        match rt {
            Data::FileBlockFromCM {
                id,
                file_num,
                data,
                compressed,
                conn_id,
            } => {
                assert_eq!(id, 5);
                assert_eq!(file_num, 2);
                assert!(data.is_empty(), "serde(skip) should produce empty Bytes");
                assert!(compressed);
                assert_eq!(conn_id, 7);
            }
            other => panic!("expected FileBlockFromCM, got {:?}", other),
        }
    }

    #[test]
    fn data_file_read_done_round_trip() {
        let data = Data::FileReadDone {
            id: 1,
            file_num: 3,
            conn_id: 10,
        };
        match round_trip(&data) {
            Data::FileReadDone {
                id,
                file_num,
                conn_id,
            } => {
                assert_eq!(id, 1);
                assert_eq!(file_num, 3);
                assert_eq!(conn_id, 10);
            }
            other => panic!("expected FileReadDone, got {:?}", other),
        }
    }

    #[test]
    fn data_file_read_error_round_trip() {
        let data = Data::FileReadError {
            id: 2,
            file_num: 1,
            err: "permission denied".into(),
            conn_id: 8,
        };
        match round_trip(&data) {
            Data::FileReadError {
                id,
                file_num,
                err,
                conn_id,
            } => {
                assert_eq!(id, 2);
                assert_eq!(file_num, 1);
                assert_eq!(err, "permission denied");
                assert_eq!(conn_id, 8);
            }
            other => panic!("expected FileReadError, got {:?}", other),
        }
    }

    #[test]
    fn data_file_digest_from_cm_round_trip() {
        let data = Data::FileDigestFromCM {
            id: 3,
            file_num: 2,
            last_modified: 1700000000,
            file_size: 65536,
            is_resume: true,
            conn_id: 11,
        };
        match round_trip(&data) {
            Data::FileDigestFromCM {
                id,
                file_num,
                last_modified,
                file_size,
                is_resume,
                conn_id,
            } => {
                assert_eq!(id, 3);
                assert_eq!(file_num, 2);
                assert_eq!(last_modified, 1700000000);
                assert_eq!(file_size, 65536);
                assert!(is_resume);
                assert_eq!(conn_id, 11);
            }
            other => panic!("expected FileDigestFromCM, got {:?}", other),
        }
    }

    #[test]
    fn data_all_files_result_round_trip() {
        let data = Data::AllFilesResult {
            id: 1,
            conn_id: 5,
            path: "/home/user/docs".into(),
            result: Ok(vec![99, 100]),
        };
        match round_trip(&data) {
            Data::AllFilesResult {
                id,
                conn_id,
                path,
                result,
            } => {
                assert_eq!(id, 1);
                assert_eq!(conn_id, 5);
                assert_eq!(path, "/home/user/docs");
                assert_eq!(result.unwrap(), vec![99, 100]);
            }
            other => panic!("expected AllFilesResult, got {:?}", other),
        }

        // Error case
        let data = Data::AllFilesResult {
            id: 2,
            conn_id: 6,
            path: "/nonexistent".into(),
            result: Err("not found".into()),
        };
        match round_trip(&data) {
            Data::AllFilesResult { result, .. } => {
                assert_eq!(result.unwrap_err(), "not found");
            }
            other => panic!("expected AllFilesResult, got {:?}", other),
        }
    }

    // ===============================================================
    // 6. JSON tag structure verification (serde(tag = "t", content = "c"))
    // ===============================================================

    #[test]
    fn data_json_uses_tag_t_and_content_c() {
        let data = Data::ChatMessage {
            text: "hi".into(),
        };
        let json = serde_json::to_string(&data).unwrap();
        let parsed: serde_json::Value = serde_json::from_str(&json).unwrap();
        // The adjacently-tagged repr should use "t" for the variant tag
        assert_eq!(parsed["t"], "ChatMessage");
        // and "c" for the content
        assert!(parsed["c"].is_object(), "content should be an object");
        assert_eq!(parsed["c"]["text"], "hi");
    }

    #[test]
    fn data_unit_variant_json_has_no_content() {
        let data = Data::Authorize;
        let json = serde_json::to_string(&data).unwrap();
        let parsed: serde_json::Value = serde_json::from_str(&json).unwrap();
        assert_eq!(parsed["t"], "Authorize");
        // Unit variants have no "c" field
        assert!(parsed.get("c").is_none());
    }

    #[test]
    fn fs_json_uses_tag_t_and_content_c() {
        let fs = FS::CreateDir {
            path: "/tmp".into(),
            id: 1,
        };
        let json = serde_json::to_string(&fs).unwrap();
        let parsed: serde_json::Value = serde_json::from_str(&json).unwrap();
        assert_eq!(parsed["t"], "CreateDir");
        assert_eq!(parsed["c"]["path"], "/tmp");
        assert_eq!(parsed["c"]["id"], 1);
    }

    // ===============================================================
    // 7. Cross-deserialization: Data from raw JSON strings
    // ===============================================================

    #[test]
    fn data_deserialize_from_handcrafted_json() {
        // Simulate what the IPC wire format looks like
        let json = r#"{"t":"Config","c":["my-key",null]}"#;
        let data: Data = serde_json::from_str(json).unwrap();
        match data {
            Data::Config((name, value)) => {
                assert_eq!(name, "my-key");
                assert!(value.is_none());
            }
            other => panic!("expected Config, got {:?}", other),
        }
    }

    #[test]
    fn data_deserialize_config_with_value_from_json() {
        let json = r#"{"t":"Config","c":["id","abc123"]}"#;
        let data: Data = serde_json::from_str(json).unwrap();
        match data {
            Data::Config((name, value)) => {
                assert_eq!(name, "id");
                assert_eq!(value.unwrap(), "abc123");
            }
            other => panic!("expected Config, got {:?}", other),
        }
    }

    #[test]
    fn data_deserialize_options_from_json() {
        let json = r#"{"t":"Options","c":{"enable-tunnel":"Y","stop-service":""}}"#;
        let data: Data = serde_json::from_str(json).unwrap();
        match data {
            Data::Options(Some(opts)) => {
                assert_eq!(opts.get("enable-tunnel").map(|s| s.as_str()), Some("Y"));
                assert_eq!(opts.get("stop-service").map(|s| s.as_str()), Some(""));
            }
            other => panic!("expected Options(Some), got {:?}", other),
        }
    }

    #[test]
    fn data_deserialize_nat_type_from_json() {
        let json = r#"{"t":"NatType","c":1}"#;
        let data: Data = serde_json::from_str(json).unwrap();
        match data {
            Data::NatType(Some(v)) => assert_eq!(v, 1),
            other => panic!("expected NatType(Some(1)), got {:?}", other),
        }
    }

    #[test]
    fn data_deserialize_close_from_json() {
        let json = r#"{"t":"Close"}"#;
        let data: Data = serde_json::from_str(json).unwrap();
        assert!(matches!(data, Data::Close));
    }

    // ===============================================================
    // 8. apply_permanent_password_storage_and_salt_payload
    // ===============================================================

    #[test]
    fn apply_permanent_password_payload_none_is_ok() {
        // None payload should succeed without error
        let result = apply_permanent_password_storage_and_salt_payload(None);
        assert!(result.is_ok());
    }

    #[test]
    fn apply_permanent_password_payload_missing_newline_is_err() {
        // Payload without a newline separator should fail
        let result = apply_permanent_password_storage_and_salt_payload(Some("no-newline-here"));
        assert!(result.is_err());
        let err_msg = result.unwrap_err().to_string();
        assert!(
            err_msg.contains("Invalid"),
            "error should mention invalid payload, got: {}",
            err_msg
        );
    }

    #[test]
    fn apply_permanent_password_payload_empty_storage_is_ok() {
        // Empty storage (before newline) should succeed -- this clears the password
        let result = apply_permanent_password_storage_and_salt_payload(Some("\nsome-salt"));
        assert!(result.is_ok());
    }

    // ===============================================================
    // 9. IPC path construction
    // ===============================================================

    #[test]
    fn ipc_path_contains_postfix() {
        let path = Config::ipc_path("");
        assert!(
            path.contains("ipc"),
            "ipc path should contain 'ipc': {}",
            path
        );

        let path_service = Config::ipc_path("_service");
        assert!(
            path_service.contains("_service"),
            "ipc path with postfix should contain postfix: {}",
            path_service
        );
    }

    #[test]
    fn ipc_path_different_postfixes_differ() {
        let p1 = Config::ipc_path("");
        let p2 = Config::ipc_path("_cm");
        let p3 = Config::ipc_path("_url");
        assert_ne!(p1, p2);
        assert_ne!(p2, p3);
        assert_ne!(p1, p3);
    }

    #[cfg(not(windows))]
    #[test]
    fn get_pid_file_path() {
        let pid_path = get_pid_file("");
        assert!(
            pid_path.ends_with(".pid"),
            "pid file should end with .pid: {}",
            pid_path
        );
        assert!(
            pid_path.contains("ipc"),
            "pid file path should contain 'ipc': {}",
            pid_path
        );

        let pid_path_svc = get_pid_file("_service");
        assert!(pid_path_svc.contains("_service"));
        assert!(pid_path_svc.ends_with(".pid"));
        assert_ne!(pid_path, pid_path_svc);
    }

    // ===============================================================
    // 10. IPC_ACTION_CLOSE constant
    // ===============================================================

    #[test]
    fn ipc_action_close_constant() {
        assert_eq!(IPC_ACTION_CLOSE, "close");
    }

    // ===============================================================
    // 11. EXIT_RECV_CLOSE default state
    // ===============================================================

    #[test]
    fn exit_recv_close_default_is_true() {
        // This is the static default; in tests it may have been mutated, so we
        // just verify it can be read without panic.  The declaration initializes
        // it to `true`.
        let _ = EXIT_RECV_CLOSE.load(Ordering::SeqCst);
    }

    // ===============================================================
    // 12. Whiteboard/Keyboard/Mouse nested in Data
    // ===============================================================

    #[test]
    fn data_keyboard_wrapped_in_data_round_trip() {
        let data = Data::Keyboard(DataKeyboard::Sequence("test input".into()));
        match round_trip(&data) {
            Data::Keyboard(DataKeyboard::Sequence(s)) => assert_eq!(s, "test input"),
            other => panic!("expected Data::Keyboard(Sequence), got {:?}", other),
        }
    }

    #[test]
    fn data_keyboard_response_wrapped_in_data_round_trip() {
        let data = Data::KeyboardResponse(DataKeyboardResponse::GetKeyState(false));
        match round_trip(&data) {
            Data::KeyboardResponse(DataKeyboardResponse::GetKeyState(v)) => assert!(!v),
            other => panic!("expected Data::KeyboardResponse, got {:?}", other),
        }
    }

    #[test]
    fn data_mouse_wrapped_in_data_round_trip() {
        let data = Data::Mouse(DataMouse::MoveTo(500, 300));
        match round_trip(&data) {
            Data::Mouse(DataMouse::MoveTo(x, y)) => {
                assert_eq!(x, 500);
                assert_eq!(y, 300);
            }
            other => panic!("expected Data::Mouse(MoveTo), got {:?}", other),
        }
    }

    #[test]
    fn data_control_wrapped_in_data_round_trip() {
        let data = Data::Control(DataControl::Resolution {
            minx: -100,
            maxx: 3840,
            miny: -50,
            maxy: 2160,
        });
        match round_trip(&data) {
            Data::Control(DataControl::Resolution {
                minx,
                maxx,
                miny,
                maxy,
            }) => {
                assert_eq!(minx, -100);
                assert_eq!(maxx, 3840);
                assert_eq!(miny, -50);
                assert_eq!(maxy, 2160);
            }
            other => panic!("expected Data::Control(Resolution), got {:?}", other),
        }
    }

    #[test]
    fn data_whiteboard_round_trip() {
        let data = Data::Whiteboard((
            "conn-123".into(),
            crate::whiteboard::CustomEvent::Clear,
        ));
        match round_trip(&data) {
            Data::Whiteboard((id, event)) => {
                assert_eq!(id, "conn-123");
                assert!(matches!(event, crate::whiteboard::CustomEvent::Clear));
            }
            other => panic!("expected Data::Whiteboard, got {:?}", other),
        }
    }

    // ===============================================================
    // 13. SyncConfig round-trip (boxed tuple)
    // ===============================================================

    #[test]
    fn data_sync_config_none_round_trip() {
        let data = Data::SyncConfig(None);
        match round_trip(&data) {
            Data::SyncConfig(None) => {}
            other => panic!("expected SyncConfig(None), got {:?}", other),
        }
    }

    #[test]
    fn data_sync_config_some_round_trip() {
        let config = Config::default();
        let config2 = Config2::default();
        let data = Data::SyncConfig(Some(Box::new((config, config2))));
        match round_trip(&data) {
            Data::SyncConfig(Some(_)) => {} // just verify it doesn't panic
            other => panic!("expected SyncConfig(Some), got {:?}", other),
        }
    }

    // ===============================================================
    // 14. Malformed/unknown JSON handling
    // ===============================================================

    #[test]
    fn data_deserialize_unknown_variant_fails() {
        let json = r#"{"t":"NonExistentVariant","c":null}"#;
        let result = serde_json::from_str::<Data>(json);
        assert!(
            result.is_err(),
            "unknown variant should fail deserialization"
        );
    }

    #[test]
    fn data_deserialize_empty_object_fails() {
        let json = r#"{}"#;
        let result = serde_json::from_str::<Data>(json);
        assert!(result.is_err(), "empty object should fail deserialization");
    }

    #[test]
    fn data_deserialize_invalid_json_fails() {
        let result = serde_json::from_str::<Data>("not json at all");
        assert!(result.is_err());
    }

    #[test]
    fn data_deserialize_wrong_content_type_fails() {
        // ChatMessage expects an object with "text" field, not a number
        let json = r#"{"t":"ChatMessage","c":42}"#;
        let result = serde_json::from_str::<Data>(json);
        assert!(
            result.is_err(),
            "wrong content type should fail deserialization"
        );
    }

    // ===============================================================
    // 15. Large/edge-case payloads
    // ===============================================================

    #[test]
    fn data_raw_message_empty_round_trip() {
        let data = Data::RawMessage(vec![]);
        match round_trip(&data) {
            Data::RawMessage(v) => assert!(v.is_empty()),
            other => panic!("expected empty RawMessage, got {:?}", other),
        }
    }

    #[test]
    fn data_options_empty_hashmap_round_trip() {
        let data = Data::Options(Some(HashMap::new()));
        match round_trip(&data) {
            Data::Options(Some(v)) => assert!(v.is_empty()),
            other => panic!("expected Options(Some(empty)), got {:?}", other),
        }
    }

    #[test]
    fn fs_new_write_empty_files_round_trip() {
        let fs = FS::NewWrite {
            path: "".into(),
            id: 0,
            file_num: 0,
            files: vec![],
            overwrite_detection: false,
            total_size: 0,
            conn_id: 0,
        };
        match round_trip_fs(&fs) {
            FS::NewWrite { files, .. } => assert!(files.is_empty()),
            other => panic!("expected NewWrite with empty files, got {:?}", other),
        }
    }

    #[test]
    fn data_login_unicode_fields_round_trip() {
        let data = Data::Login {
            id: 1,
            is_file_transfer: false,
            is_view_camera: false,
            is_terminal: false,
            peer_id: "peer-\u{1F600}".into(),
            name: "\u{4F60}\u{597D}".into(), // Chinese characters
            avatar: "".into(),
            authorized: false,
            port_forward: "".into(),
            keyboard: false,
            clipboard: false,
            audio: false,
            file: false,
            file_transfer_enabled: false,
            restart: false,
            recording: false,
            block_input: false,
            from_switch: false,
        };
        match round_trip(&data) {
            Data::Login { peer_id, name, .. } => {
                assert!(peer_id.contains('\u{1F600}'));
                assert_eq!(name, "\u{4F60}\u{597D}");
            }
            other => panic!("expected Login with unicode, got {:?}", other),
        }
    }

    // ===============================================================
    // 16. TerminalSessionCount (linux-only variant)
    // ===============================================================

    #[cfg(target_os = "linux")]
    #[test]
    fn data_terminal_session_count_round_trip() {
        let data = Data::TerminalSessionCount(7);
        match round_trip(&data) {
            Data::TerminalSessionCount(v) => assert_eq!(v, 7),
            other => panic!("expected TerminalSessionCount, got {:?}", other),
        }
    }

    // ===============================================================
    // Security invariant: IPC socket permissions (CWE-732)
    // ===============================================================

    /// The IPC socket must NOT be world-writable. A mode of 0o777 would let any
    /// local user connect and change remote-access passwords, configuration, etc.
    /// This test ensures IPC_SOCKET_MODE stays at 0o600 (owner read+write only).
    #[cfg(not(windows))]
    #[test]
    fn ipc_socket_mode_is_owner_only() {
        assert_eq!(
            IPC_SOCKET_MODE, 0o600,
            "IPC_SOCKET_MODE must be 0o600 (owner rw only) to prevent local privilege escalation"
        );
        // Verify no group or other bits are set
        assert_eq!(
            IPC_SOCKET_MODE & 0o077,
            0,
            "IPC_SOCKET_MODE must not grant any group or other permissions"
        );
    }
}
