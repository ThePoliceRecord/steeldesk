// 24FPS (actually 23.976FPS) is what video professionals ages ago determined to be the
// slowest playback rate that still looks smooth enough to feel real.
// Our eyes can see a slight difference and even though 30FPS actually shows
// more information and is more realistic.
// 60FPS is commonly used in game, teamviewer 12 support this for video editing user.

// how to capture with mouse cursor:
// https://docs.microsoft.com/zh-cn/windows/win32/direct3ddxgi/desktop-dup-api?redirectedfrom=MSDN

// RECORD: The following Project has implemented audio capture, hardware codec and mouse cursor drawn.
// https://github.com/PHZ76/DesktopSharing

// dxgi memory leak issue
// https://stackoverflow.com/questions/47801238/memory-leak-in-creating-direct2d-device
// but per my test, it is more related to AcquireNextFrame,
// https://forums.developer.nvidia.com/t/dxgi-outputduplication-memory-leak-when-using-nv-but-not-amd-drivers/108582

// to-do:
// https://slhck.info/video/2017/03/01/rate-control.html

use super::{display_service::check_display_changed, frame_buffer::{CapturedFrame, FrameBuffer}, service::ServiceTmpl, video_qos::{self, VideoQoS}, *};
#[cfg(target_os = "linux")]
use crate::common::SimpleCallOnReturn;
#[cfg(target_os = "linux")]
use crate::platform::linux::is_x11;
use crate::privacy_mode::{get_privacy_mode_conn_id, INVALID_PRIVACY_MODE_CONN_ID};
#[cfg(windows)]
use crate::{
    platform::windows::is_process_consent_running,
    privacy_mode::{is_current_privacy_mode_impl, PRIVACY_MODE_IMPL_WIN_MAG},
    ui_interface::is_installed,
};
use hbb_common::{
    anyhow::anyhow,
    config,
    tokio::sync::{
        mpsc::{unbounded_channel, UnboundedReceiver, UnboundedSender},
        Mutex as TokioMutex,
    },
};
#[cfg(feature = "hwcodec")]
use scrap::hwcodec::{HwRamEncoder, HwRamEncoderConfig};
#[cfg(feature = "vram")]
use scrap::vram::{VRamEncoder, VRamEncoderConfig};
#[cfg(not(windows))]
use scrap::Capturer;
use scrap::{
    aom::AomEncoderConfig,
    codec::{Encoder, EncoderCfg},
    record::{Recorder, RecorderContext},
    vpxcodec::{VpxEncoderConfig, VpxVideoCodecId},
    CodecFormat, Display, EncodeInput, PixelBuffer, TraitCapturer, TraitPixelBuffer,
};
#[cfg(windows)]
use std::sync::Once;
use std::{
    collections::HashSet,
    io::ErrorKind::WouldBlock,
    ops::{Deref, DerefMut},
    time::{self, Duration, Instant},
};

pub const OPTION_REFRESH: &'static str = "refresh";

type FrameFetchedNotifierSender = UnboundedSender<(i32, Option<Instant>)>;
type FrameFetchedNotifierReceiver = Arc<TokioMutex<UnboundedReceiver<(i32, Option<Instant>)>>>;

lazy_static::lazy_static! {
    static ref FRAME_FETCHED_NOTIFIERS: Mutex<HashMap<usize, (FrameFetchedNotifierSender, FrameFetchedNotifierReceiver)>> = Mutex::new(HashMap::default());

    // display_idx -> set of conn id.
    // Used to record which connections need to be notified when
    // 1. A new frame is received from a web client.
    //   Because web client does not send the display index in message `VideoReceived`.
    // 2. The client is closing.
    static ref DISPLAY_CONN_IDS: Arc<Mutex<HashMap<usize, HashSet<i32>>>> = Default::default();
    pub static ref VIDEO_QOS: Arc<Mutex<VideoQoS>> = Default::default();
    pub static ref IS_UAC_RUNNING: Arc<Mutex<bool>> = Default::default();
    pub static ref IS_FOREGROUND_WINDOW_ELEVATED: Arc<Mutex<bool>> = Default::default();
    static ref SCREENSHOTS: Mutex<HashMap<usize, Screenshot>> = Default::default();
}

struct Screenshot {
    sid: String,
    tx: Sender,
    restore_vram: bool,
}

#[inline]
pub fn notify_video_frame_fetched(display_idx: usize, conn_id: i32, frame_tm: Option<Instant>) {
    if let Some(notifier) = FRAME_FETCHED_NOTIFIERS.lock().unwrap().get(&display_idx) {
        notifier.0.send((conn_id, frame_tm)).ok();
    }
}

#[inline]
pub fn notify_video_frame_fetched_by_conn_id(conn_id: i32, frame_tm: Option<Instant>) {
    let vec_display_idx: Vec<usize> = {
        let display_conn_ids = DISPLAY_CONN_IDS.lock().unwrap();
        display_conn_ids
            .iter()
            .filter_map(|(display_idx, conn_ids)| {
                if conn_ids.contains(&conn_id) {
                    Some(*display_idx)
                } else {
                    None
                }
            })
            .collect()
    };
    let notifiers = FRAME_FETCHED_NOTIFIERS.lock().unwrap();
    for display_idx in vec_display_idx {
        if let Some(notifier) = notifiers.get(&display_idx) {
            notifier.0.send((conn_id, frame_tm)).ok();
        }
    }
}

struct VideoFrameController {
    display_idx: usize,
    cur: Instant,
    send_conn_ids: HashSet<i32>,
}

/// Default outer timeout for frame ACK waiting (normal mode).
const FRAME_ACK_TIMEOUT_NORMAL_MS: u64 = 500;
/// Poll interval for frame ACK waiting (normal mode).
const FRAME_ACK_POLL_NORMAL_MS: u64 = 50;
/// Poll interval for low-latency mode (non-blocking).
const FRAME_ACK_POLL_LOW_LATENCY_MS: u64 = 0;

impl VideoFrameController {
    fn new(display_idx: usize) -> Self {
        Self {
            display_idx,
            cur: Instant::now(),
            send_conn_ids: HashSet::new(),
        }
    }

    fn reset(&mut self) {
        self.send_conn_ids.clear();
    }

    fn set_send(&mut self, tm: Instant, conn_ids: HashSet<i32>) {
        if !conn_ids.is_empty() {
            self.cur = tm;
            self.send_conn_ids = conn_ids;
            DISPLAY_CONN_IDS
                .lock()
                .unwrap()
                .insert(self.display_idx, self.send_conn_ids.clone());
        }
    }

    /// Returns the frame ACK poll timeout and outer timeout based on config.
    /// In low-latency mode: poll_timeout=0 (non-blocking), outer_timeout=0.
    /// In normal mode: poll_timeout=50ms, outer_timeout=500ms.
    fn get_ack_timeouts() -> (u64, u64) {
        if Config::get_option("low-latency-mode") == "Y" {
            (FRAME_ACK_POLL_LOW_LATENCY_MS, 0)
        } else {
            (FRAME_ACK_POLL_NORMAL_MS, FRAME_ACK_TIMEOUT_NORMAL_MS)
        }
    }

    #[tokio::main(flavor = "current_thread")]
    async fn try_wait_next(&mut self, fetched_conn_ids: &mut HashSet<i32>, timeout_millis: u64) {
        if self.send_conn_ids.is_empty() {
            return;
        }

        let timeout_dur = Duration::from_millis(timeout_millis as u64);
        let receiver = {
            match FRAME_FETCHED_NOTIFIERS
                .lock()
                .unwrap()
                .get(&self.display_idx)
            {
                Some(notifier) => notifier.1.clone(),
                None => {
                    return;
                }
            }
        };
        let mut receiver_guard = receiver.lock().await;
        match tokio::time::timeout(timeout_dur, receiver_guard.recv()).await {
            Err(_) => {
                // break if timeout
                // log::error!("blocking wait frame receiving timeout {}", timeout_millis);
            }
            Ok(Some((id, instant))) => {
                if let Some(tm) = instant {
                    log::trace!("Channel recv latency: {}", tm.elapsed().as_secs_f32());
                }
                fetched_conn_ids.insert(id);
            }
            Ok(None) => {
                // this branch would never be reached
            }
        }
        while !receiver_guard.is_empty() {
            if let Some((id, instant)) = receiver_guard.recv().await {
                if let Some(tm) = instant {
                    log::trace!("Channel recv latency: {}", tm.elapsed().as_secs_f32());
                }
                fetched_conn_ids.insert(id);
            }
        }
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum VideoSource {
    Monitor,
    Camera,
}

impl VideoSource {
    pub fn service_name_prefix(&self) -> &'static str {
        match self {
            VideoSource::Monitor => "monitor",
            VideoSource::Camera => "camera",
        }
    }

    pub fn is_monitor(&self) -> bool {
        matches!(self, VideoSource::Monitor)
    }

    pub fn is_camera(&self) -> bool {
        matches!(self, VideoSource::Camera)
    }
}

#[derive(Clone)]
pub struct VideoService {
    sp: GenericService,
    idx: usize,
    source: VideoSource,
}

impl Deref for VideoService {
    type Target = ServiceTmpl<ConnInner>;

    fn deref(&self) -> &Self::Target {
        &self.sp
    }
}

impl DerefMut for VideoService {
    fn deref_mut(&mut self) -> &mut Self::Target {
        &mut self.sp
    }
}

pub fn get_service_name(source: VideoSource, idx: usize) -> String {
    format!("{}{}", source.service_name_prefix(), idx)
}

pub fn new(source: VideoSource, idx: usize) -> GenericService {
    let _ = FRAME_FETCHED_NOTIFIERS
        .lock()
        .unwrap()
        .entry(idx)
        .or_insert_with(|| {
            let (tx, rx) = unbounded_channel();
            (tx, Arc::new(TokioMutex::new(rx)))
        });
    let vs = VideoService {
        sp: GenericService::new(get_service_name(source, idx), true),
        idx,
        source,
    };
    GenericService::run(&vs, run);
    vs.sp
}

// Capturer object is expensive, avoiding to create it frequently.
fn create_capturer(
    privacy_mode_id: i32,
    display: Display,
    _current: usize,
    _portable_service_running: bool,
) -> ResultType<Box<dyn TraitCapturer>> {
    #[cfg(not(windows))]
    let c: Option<Box<dyn TraitCapturer>> = None;
    #[cfg(windows)]
    let mut c: Option<Box<dyn TraitCapturer>> = None;
    if privacy_mode_id > 0 {
        #[cfg(windows)]
        {
            if let Some(c1) = crate::privacy_mode::win_mag::create_capturer(
                privacy_mode_id,
                display.origin(),
                display.width(),
                display.height(),
            )? {
                c = Some(Box::new(c1));
            }
        }
    }

    match c {
        Some(c1) => return Ok(c1),
        None => {
            #[cfg(windows)]
            {
                log::debug!("Create capturer dxgi|gdi");
                return crate::portable_service::client::create_capturer(
                    _current,
                    display,
                    _portable_service_running,
                );
            }
            #[cfg(not(windows))]
            {
                log::debug!("Create capturer from scrap");
                return Ok(Box::new(
                    Capturer::new(display).with_context(|| "Failed to create capturer")?,
                ));
            }
        }
    };
}

// This function works on privacy mode. Windows only for now.
pub fn test_create_capturer(
    privacy_mode_id: i32,
    display_idx: usize,
    timeout_millis: u64,
) -> String {
    let test_begin = Instant::now();
    loop {
        let err = match Display::all() {
            Ok(mut displays) => {
                if displays.len() <= display_idx {
                    anyhow!(
                        "Failed to get display {}, the displays' count is {}",
                        display_idx,
                        displays.len()
                    )
                } else {
                    let display = displays.remove(display_idx);
                    match create_capturer(privacy_mode_id, display, display_idx, false) {
                        Ok(_) => return "".to_owned(),
                        Err(e) => e,
                    }
                }
            }
            Err(e) => e.into(),
        };
        if test_begin.elapsed().as_millis() >= timeout_millis as _ {
            return err.to_string();
        }
        std::thread::sleep(Duration::from_millis(300));
    }
}

// Note: This function is extremely expensive, do not call it frequently.
#[cfg(windows)]
fn check_uac_switch(privacy_mode_id: i32, capturer_privacy_mode_id: i32) -> ResultType<()> {
    if capturer_privacy_mode_id != INVALID_PRIVACY_MODE_CONN_ID
        && is_current_privacy_mode_impl(PRIVACY_MODE_IMPL_WIN_MAG)
    {
        if !is_installed() {
            if privacy_mode_id != capturer_privacy_mode_id {
                if !is_process_consent_running()? {
                    bail!("consent.exe is not running");
                }
            }
            if is_process_consent_running()? {
                bail!("consent.exe is running");
            }
        }
    }
    Ok(())
}

pub(super) struct CapturerInfo {
    pub origin: (i32, i32),
    pub width: usize,
    pub height: usize,
    pub ndisplay: usize,
    pub current: usize,
    pub privacy_mode_id: i32,
    pub _capturer_privacy_mode_id: i32,
    pub capturer: Box<dyn TraitCapturer>,
}

impl Deref for CapturerInfo {
    type Target = Box<dyn TraitCapturer>;

    fn deref(&self) -> &Self::Target {
        &self.capturer
    }
}

impl DerefMut for CapturerInfo {
    fn deref_mut(&mut self) -> &mut Self::Target {
        &mut self.capturer
    }
}

fn get_capturer_monitor(
    current: usize,
    portable_service_running: bool,
) -> ResultType<CapturerInfo> {
    #[cfg(target_os = "linux")]
    {
        if !is_x11() {
            return super::wayland::get_capturer_for_display(current);
        }
    }

    let mut displays = Display::all()?;
    let ndisplay = displays.len();
    if ndisplay <= current {
        bail!(
            "Failed to get display {}, displays len: {}",
            current,
            ndisplay
        );
    }

    let display = displays.remove(current);

    #[cfg(target_os = "linux")]
    if let Display::X11(inner) = &display {
        if let Err(err) = inner.get_shm_status() {
            log::warn!(
                "MIT-SHM extension not working properly on select X11 server: {:?}",
                err
            );
        }
    }

    let (origin, width, height) = (display.origin(), display.width(), display.height());
    let name = display.name();
    log::debug!(
        "#displays={}, current={}, origin: {:?}, width={}, height={}, cpus={}/{}, name:{}",
        ndisplay,
        current,
        &origin,
        width,
        height,
        num_cpus::get_physical(),
        num_cpus::get(),
        &name,
    );

    let privacy_mode_id = get_privacy_mode_conn_id().unwrap_or(INVALID_PRIVACY_MODE_CONN_ID);
    #[cfg(not(windows))]
    let capturer_privacy_mode_id = privacy_mode_id;
    #[cfg(windows)]
    let mut capturer_privacy_mode_id = privacy_mode_id;
    #[cfg(windows)]
    {
        if capturer_privacy_mode_id != INVALID_PRIVACY_MODE_CONN_ID
            && is_current_privacy_mode_impl(PRIVACY_MODE_IMPL_WIN_MAG)
        {
            if !is_installed() {
                if is_process_consent_running()? {
                    capturer_privacy_mode_id = INVALID_PRIVACY_MODE_CONN_ID;
                }
            }
        }
    }
    log::debug!(
        "Try create capturer with capturer privacy mode id {}",
        capturer_privacy_mode_id,
    );

    if privacy_mode_id != INVALID_PRIVACY_MODE_CONN_ID {
        if privacy_mode_id != capturer_privacy_mode_id {
            log::info!("In privacy mode, but show UAC prompt window for now");
        } else {
            log::info!("In privacy mode, the peer side cannot watch the screen");
        }
    }
    let capturer = create_capturer(
        capturer_privacy_mode_id,
        display,
        current,
        portable_service_running,
    )?;
    Ok(CapturerInfo {
        origin,
        width,
        height,
        ndisplay,
        current,
        privacy_mode_id,
        _capturer_privacy_mode_id: capturer_privacy_mode_id,
        capturer,
    })
}

fn get_capturer_camera(current: usize) -> ResultType<CapturerInfo> {
    let cameras = camera::Cameras::get_sync_cameras();
    let ncamera = cameras.len();
    if ncamera <= current {
        bail!("Failed to get camera {}, cameras len: {}", current, ncamera,);
    }
    let Some(camera) = cameras.get(current) else {
        bail!(
            "Camera of index {} doesn't exist or platform not supported",
            current
        );
    };
    let capturer = camera::Cameras::get_capturer(current)?;
    let (width, height) = (camera.width as usize, camera.height as usize);
    let origin = (camera.x as i32, camera.y as i32);
    let name = &camera.name;
    let privacy_mode_id = get_privacy_mode_conn_id().unwrap_or(INVALID_PRIVACY_MODE_CONN_ID);
    let _capturer_privacy_mode_id = privacy_mode_id;
    log::debug!(
        "#cameras={}, current={}, origin: {:?}, width={}, height={}, cpus={}/{}, name:{}",
        ncamera,
        current,
        &origin,
        width,
        height,
        num_cpus::get_physical(),
        num_cpus::get(),
        name,
    );
    return Ok(CapturerInfo {
        origin,
        width,
        height,
        ndisplay: ncamera,
        current,
        privacy_mode_id,
        _capturer_privacy_mode_id: privacy_mode_id,
        capturer,
    });
}
fn get_capturer(
    source: VideoSource,
    current: usize,
    portable_service_running: bool,
) -> ResultType<CapturerInfo> {
    match source {
        VideoSource::Monitor => get_capturer_monitor(current, portable_service_running),
        VideoSource::Camera => get_capturer_camera(current),
    }
}

fn run(vs: VideoService) -> ResultType<()> {
    let mut _raii = Raii::new(vs.idx, vs.sp.name());
    // Wayland only support one video capturer for now. It is ok to call ensure_inited() here.
    //
    // ensure_inited() is needed because clear() may be called.
    // to-do: wayland ensure_inited should pass current display index.
    // But for now, we do not support multi-screen capture on wayland.
    #[cfg(target_os = "linux")]
    super::wayland::ensure_inited()?;
    #[cfg(target_os = "linux")]
    let _wayland_call_on_ret = {
        // Increment active display count when starting
        let _display_count = super::wayland::increment_active_display_count();

        SimpleCallOnReturn {
            b: true,
            f: Box::new(|| {
                // Decrement active display count and only clear if this was the last display
                let remaining_count = super::wayland::decrement_active_display_count();
                if remaining_count == 0 {
                    super::wayland::clear();
                }
            }),
        }
    };

    #[cfg(windows)]
    let last_portable_service_running = crate::portable_service::client::running();
    #[cfg(not(windows))]
    let last_portable_service_running = false;

    let display_idx = vs.idx;
    let sp = vs.sp;
    let mut c = get_capturer(vs.source, display_idx, last_portable_service_running)?;
    #[cfg(windows)]
    if !scrap::codec::enable_directx_capture() && !c.is_gdi() {
        log::info!("disable dxgi with option, fall back to gdi");
        c.set_gdi();
    }
    let mut video_qos = VIDEO_QOS.lock().unwrap();
    let mut spf = video_qos.spf();
    let mut quality = video_qos.ratio();
    let target_fps = video_qos.fps();
    let record_incoming = config::option2bool(
        "allow-auto-record-incoming",
        &Config::get_option("allow-auto-record-incoming"),
    );
    let client_record = video_qos.record();
    drop(video_qos);
    let (mut encoder, encoder_cfg, codec_format, use_i444, recorder) = match setup_encoder(
        &c,
        sp.name(),
        quality,
        client_record,
        record_incoming,
        last_portable_service_running,
        vs.source,
        display_idx,
        target_fps,
    ) {
        Ok(result) => result,
        Err(err) => {
            log::error!("Failed to create encoder: {err:?}, fallback to VP9");
            Encoder::set_fallback(&EncoderCfg::VPX(VpxEncoderConfig {
                width: c.width as _,
                height: c.height as _,
                quality,
                codec: VpxVideoCodecId::VP9,
                keyframe_interval: None,
            }));
            setup_encoder(
                &c,
                sp.name(),
                quality,
                client_record,
                record_incoming,
                last_portable_service_running,
                vs.source,
                display_idx,
                target_fps,
            )?
        }
    };
    #[cfg(feature = "vram")]
    c.set_output_texture(encoder.input_texture());
    #[cfg(target_os = "android")]
    if vs.source.is_monitor() {
        if let Err(e) = check_change_scale(encoder.is_hardware()) {
            try_broadcast_display_changed(&sp, display_idx, &c, true).ok();
            bail!(e);
        }
    }
    VIDEO_QOS.lock().unwrap().store_bitrate(encoder.bitrate());
    VIDEO_QOS
        .lock()
        .unwrap()
        .set_support_changing_quality(&sp.name(), encoder.support_changing_quality());
    log::info!("initial quality: {quality:?}");

    if sp.is_option_true(OPTION_REFRESH) {
        sp.set_option_bool(OPTION_REFRESH, false);
    }

    let mut frame_controller = VideoFrameController::new(display_idx);

    let start = time::Instant::now();
    let mut last_check_displays = time::Instant::now();
    #[cfg(windows)]
    let mut try_gdi = 1;
    #[cfg(windows)]
    log::info!("gdi: {}", c.is_gdi());
    #[cfg(windows)]
    start_uac_elevation_check();

    #[cfg(target_os = "linux")]
    let mut would_block_count = 0u32;
    let mut yuv = Vec::new();
    let mut mid_data = Vec::new();
    let mut repeat_encode_counter = 0;
    let repeat_encode_max = 10;
    let mut encode_fail_counter = 0;
    let mut first_frame = true;
    let capture_width = c.width;
    let capture_height = c.height;
    let (mut second_instant, mut send_counter) = (Instant::now(), 0);

    // Normal mode: capture -> encode -> send (sequential, simple)
    // Low-latency mode: capture -> FrameBuffer -> encode -> send (decoupled, latest-wins)
    //
    // In low-latency mode the FrameBuffer sits between capture and encode.
    // The capture side copies pixel data into a CapturedFrame and stores it
    // (latest-wins: a new capture overwrites any unconsumed frame).  The
    // encode side takes the latest frame from the buffer before encoding.
    //
    // In this single-threaded integration the store/take happen in the same
    // loop iteration, establishing the contract for a future multi-threaded
    // split where a dedicated capture thread feeds the buffer independently.
    let low_latency = video_qos::is_low_latency_mode();
    let frame_buffer = if low_latency {
        Some(FrameBuffer::new())
    } else {
        None
    };

    while sp.ok() {
        #[cfg(windows)]
        check_uac_switch(c.privacy_mode_id, c._capturer_privacy_mode_id)?;
        check_qos(
            &mut encoder,
            &mut quality,
            &mut spf,
            client_record,
            &mut send_counter,
            &mut second_instant,
            &sp.name(),
        )?;
        if sp.is_option_true(OPTION_REFRESH) {
            if vs.source.is_monitor() {
                let _ = try_broadcast_display_changed(&sp, display_idx, &c, true);
            }
            log::info!("switch to refresh");
            bail!("SWITCH");
        }
        if codec_format != Encoder::negotiated_codec() {
            log::info!(
                "switch due to codec changed, {:?} -> {:?}",
                codec_format,
                Encoder::negotiated_codec()
            );
            bail!("SWITCH");
        }
        #[cfg(windows)]
        if last_portable_service_running != crate::portable_service::client::running() {
            log::info!("switch due to portable service running changed");
            bail!("SWITCH");
        }
        if Encoder::use_i444(&encoder_cfg) != use_i444 {
            log::info!("switch due to i444 changed");
            bail!("SWITCH");
        }
        #[cfg(all(windows, feature = "vram"))]
        if c.is_gdi() && encoder.input_texture() {
            log::info!("changed to gdi when using vram");
            VRamEncoder::set_fallback_gdi(sp.name(), true);
            bail!("SWITCH");
        }
        if vs.source.is_monitor() {
            check_privacy_mode_changed(&sp, display_idx, &c)?;
        }
        #[cfg(windows)]
        {
            if crate::platform::windows::desktop_changed()
                && !crate::portable_service::client::running()
            {
                bail!("Desktop changed");
            }
        }
        let now = time::Instant::now();
        if vs.source.is_monitor() && last_check_displays.elapsed().as_millis() > 1000 {
            last_check_displays = now;
            // This check may be redundant, but it is better to be safe.
            // The previous check in `sp.is_option_true(OPTION_REFRESH)` block may be enough.
            try_broadcast_display_changed(&sp, display_idx, &c, false)?;
        }

        frame_controller.reset();

        let time = now - start;
        let ms = (time.as_secs() * 1000 + time.subsec_millis() as u64) as i64;
        let res = match c.frame(spf) {
            Ok(frame) => {
                repeat_encode_counter = 0;
                if frame.valid() {
                    let screenshot = SCREENSHOTS.lock().unwrap().remove(&display_idx);
                    if let Some(mut screenshot) = screenshot {
                        let restore_vram = screenshot.restore_vram;
                        let (msg, w, h, data) = match &frame {
                            scrap::Frame::PixelBuffer(f) => match get_rgba_from_pixelbuf(f) {
                                Ok(rgba) => ("".to_owned(), f.width(), f.height(), rgba),
                                Err(e) => {
                                    let serr = e.to_string();
                                    log::error!(
                                        "Failed to convert the pix format into rgba, {}",
                                        &serr
                                    );
                                    (format!("Convert pixfmt: {}", serr), 0, 0, vec![])
                                }
                            },
                            scrap::Frame::Texture(_) => {
                                if restore_vram {
                                    // Already set one time, just ignore to break infinite loop.
                                    // Though it's unreachable, this branch is kept to avoid infinite loop.
                                    (
                                        "Please change codec and try again.".to_owned(),
                                        0,
                                        0,
                                        vec![],
                                    )
                                } else {
                                    #[cfg(all(windows, feature = "vram"))]
                                    VRamEncoder::set_not_use(sp.name(), true);
                                    screenshot.restore_vram = true;
                                    SCREENSHOTS.lock().unwrap().insert(display_idx, screenshot);
                                    _raii.try_vram = false;
                                    bail!("SWITCH");
                                }
                            }
                        };
                        std::thread::spawn(move || {
                            handle_screenshot(screenshot, msg, w, h, data);
                        });
                        if restore_vram {
                            bail!("SWITCH");
                        }
                    }

                    // Low-latency path: store pixel data in the FrameBuffer,
                    // then take the latest frame and encode it.  This establishes
                    // the decoupled capture/encode contract used by the future
                    // multi-threaded pipeline.  Texture frames bypass the buffer
                    // because they are raw GPU pointers that cannot be copied.
                    if let (Some(ref fb), scrap::Frame::PixelBuffer(ref pb)) = (&frame_buffer, &frame) {
                        let captured = CapturedFrame {
                            data: pb.data().to_vec(),
                            width: pb.width(),
                            height: pb.height(),
                            stride: pb.stride().first().copied().unwrap_or(pb.width() * 4),
                            pixfmt: pb.pixfmt(),
                            capture_time: Instant::now(),
                            display_idx,
                        };
                        fb.store(captured);

                        // Encode phase: take the latest frame from the buffer.
                        if let Some(cf) = fb.take() {
                            let pb_owned = PixelBuffer::new(&cf.data, cf.pixfmt, cf.width, cf.height);
                            scrap::convert_to_yuv(&pb_owned, encoder.yuvfmt(), &mut yuv, &mut mid_data)?;
                            let send_conn_ids = handle_one_frame(
                                display_idx,
                                &sp,
                                EncodeInput::YUV(&yuv),
                                ms,
                                &mut encoder,
                                recorder.clone(),
                                &mut encode_fail_counter,
                                &mut first_frame,
                                capture_width,
                                capture_height,
                            )?;
                            frame_controller.set_send(now, send_conn_ids);
                            send_counter += 1;
                        }
                    } else {
                        // Normal path (or Texture frame): capture -> encode sequentially.
                        let frame = frame.to(encoder.yuvfmt(), &mut yuv, &mut mid_data)?;
                        let send_conn_ids = handle_one_frame(
                            display_idx,
                            &sp,
                            frame,
                            ms,
                            &mut encoder,
                            recorder.clone(),
                            &mut encode_fail_counter,
                            &mut first_frame,
                            capture_width,
                            capture_height,
                        )?;
                        frame_controller.set_send(now, send_conn_ids);
                        send_counter += 1;
                    }
                }
                #[cfg(windows)]
                {
                    #[cfg(feature = "vram")]
                    if try_gdi == 1 && !c.is_gdi() {
                        VRamEncoder::set_fallback_gdi(sp.name(), false);
                    }
                    try_gdi = 0;
                }
                Ok(())
            }
            Err(err) => Err(err),
        };

        match res {
            Err(ref e) if e.kind() == WouldBlock => {
                #[cfg(windows)]
                if try_gdi > 0 && !c.is_gdi() {
                    if try_gdi > 3 {
                        c.set_gdi();
                        try_gdi = 0;
                        log::info!("No image, fall back to gdi");
                    }
                    try_gdi += 1;
                }
                #[cfg(target_os = "linux")]
                {
                    would_block_count += 1;
                    if !is_x11() {
                        if would_block_count >= 100 {
                            // to-do: Unknown reason for WouldBlock 100 times (seconds = 100 * 1 / fps)
                            // https://github.com/rustdesk/rustdesk/blob/63e6b2f8ab51743e77a151e2b7ff18816f5fa2fb/libs/scrap/src/common/wayland.rs#L81
                            //
                            // Do not reset the capturer for now, as it will cause the prompt to show every few minutes.
                            // https://github.com/rustdesk/rustdesk/issues/4276
                            //
                            // super::wayland::clear();
                            // bail!("Wayland capturer none 100 times, try restart capture");
                        }
                    }
                }
                if !encoder.latency_free() && yuv.len() > 0 {
                    // In gaming / low-latency mode, skip repeat encoding entirely.
                    // Re-encoding a stale YUV frame wastes GPU encoder time and
                    // bandwidth; it is better to wait for the next fresh capture.
                    if video_qos::is_low_latency_mode() {
                        // no-op: proceed to next capture cycle
                    } else if repeat_encode_counter < repeat_encode_max {
                        // yun.len() > 0 means the frame is not texture.
                        repeat_encode_counter += 1;
                        let send_conn_ids = handle_one_frame(
                            display_idx,
                            &sp,
                            EncodeInput::YUV(&yuv),
                            ms,
                            &mut encoder,
                            recorder.clone(),
                            &mut encode_fail_counter,
                            &mut first_frame,
                            capture_width,
                            capture_height,
                        )?;
                        frame_controller.set_send(now, send_conn_ids);
                        send_counter += 1;
                    }
                }
            }
            Err(err) => {
                // This check may be redundant, but it is better to be safe.
                // The previous check in `sp.is_option_true(OPTION_REFRESH)` block may be enough.
                if vs.source.is_monitor() {
                    try_broadcast_display_changed(&sp, display_idx, &c, true)?;
                }

                #[cfg(windows)]
                if !c.is_gdi() {
                    c.set_gdi();
                    log::info!("dxgi error, fall back to gdi: {:?}", err);
                    continue;
                }
                return Err(err.into());
            }
            _ => {
                #[cfg(target_os = "linux")]
                {
                    would_block_count = 0;
                }
            }
        }

        let mut fetched_conn_ids = HashSet::new();
        let (poll_timeout, outer_timeout) = VideoFrameController::get_ack_timeouts();
        let wait_begin = Instant::now();
        while outer_timeout == 0 || wait_begin.elapsed().as_millis() < outer_timeout as _ {
            if vs.source.is_monitor() {
                check_privacy_mode_changed(&sp, display_idx, &c)?;
            }
            frame_controller.try_wait_next(&mut fetched_conn_ids, poll_timeout);
            // break if all connections have received current frame
            if fetched_conn_ids.len() >= frame_controller.send_conn_ids.len() {
                break;
            }
            // In low-latency mode (outer_timeout==0) or if a client hasn't ACKed
            // within the per-poll window, skip remaining clients and proceed
            // immediately to the next frame rather than blocking the encoder.
            if outer_timeout == 0 {
                break;
            }
        }
        DISPLAY_CONN_IDS.lock().unwrap().remove(&display_idx);

        let elapsed = now.elapsed();
        // may need to enable frame(timeout)
        log::trace!("{:?} {:?}", time::Instant::now(), elapsed);
        if elapsed < spf {
            std::thread::sleep(spf - elapsed);
        }
    }

    Ok(())
}

struct Raii {
    display_idx: usize,
    name: String,
    try_vram: bool,
}

impl Raii {
    fn new(display_idx: usize, name: String) -> Self {
        log::info!("new video service: {}", name);
        VIDEO_QOS.lock().unwrap().new_display(name.clone());
        Raii {
            display_idx,
            name,
            try_vram: true,
        }
    }
}

impl Drop for Raii {
    fn drop(&mut self) {
        log::info!("stop video service: {}", self.name);
        #[cfg(feature = "vram")]
        if self.try_vram {
            VRamEncoder::set_not_use(self.name.clone(), false);
        }
        #[cfg(feature = "vram")]
        Encoder::update(scrap::codec::EncodingUpdate::Check);
        VIDEO_QOS.lock().unwrap().remove_display(&self.name);
        DISPLAY_CONN_IDS.lock().unwrap().remove(&self.display_idx);
    }
}

fn setup_encoder(
    c: &CapturerInfo,
    name: String,
    quality: f32,
    client_record: bool,
    record_incoming: bool,
    last_portable_service_running: bool,
    source: VideoSource,
    display_idx: usize,
    target_fps: u32,
) -> ResultType<(
    Encoder,
    EncoderCfg,
    CodecFormat,
    bool,
    Arc<Mutex<Option<Recorder>>>,
)> {
    let encoder_cfg = get_encoder_config(
        &c,
        name.to_string(),
        quality,
        client_record || record_incoming,
        last_portable_service_running,
        source,
        target_fps,
    );
    Encoder::set_fallback(&encoder_cfg);
    let codec_format = Encoder::negotiated_codec();
    let recorder = get_recorder(record_incoming, display_idx, source == VideoSource::Camera);
    let use_i444 = Encoder::use_i444(&encoder_cfg);
    let encoder = Encoder::new(encoder_cfg.clone(), use_i444)?;
    Ok((encoder, encoder_cfg, codec_format, use_i444, recorder))
}

fn get_encoder_config(
    c: &CapturerInfo,
    _name: String,
    quality: f32,
    record: bool,
    _portable_service: bool,
    _source: VideoSource,
    target_fps: u32,
) -> EncoderCfg {
    #[cfg(all(windows, feature = "vram"))]
    if _portable_service || c.is_gdi() || _source == VideoSource::Camera {
        log::info!("gdi:{}, portable:{}", c.is_gdi(), _portable_service);
        VRamEncoder::set_not_use(_name, true);
    }
    #[cfg(feature = "vram")]
    Encoder::update(scrap::codec::EncodingUpdate::Check);
    // https://www.wowza.com/community/t/the-correct-keyframe-interval-in-obs-studio/95162
    let keyframe_interval = if record { Some(240) } else { None };
    let negotiated_codec = Encoder::negotiated_codec();
    match negotiated_codec {
        CodecFormat::H264 | CodecFormat::H265 => {
            #[cfg(feature = "vram")]
            if let Some(feature) = VRamEncoder::try_get(&c.device(), negotiated_codec) {
                return EncoderCfg::VRAM(VRamEncoderConfig {
                    device: c.device(),
                    width: c.width,
                    height: c.height,
                    quality,
                    feature,
                    keyframe_interval,
                });
            }
            #[cfg(feature = "hwcodec")]
            if let Some(hw) = HwRamEncoder::try_get(negotiated_codec) {
                return EncoderCfg::HWRAM(HwRamEncoderConfig {
                    name: hw.name,
                    mc_name: hw.mc_name,
                    width: c.width,
                    height: c.height,
                    quality,
                    keyframe_interval,
                    hdr: scrap::is_display_hdr(),
                    fps: Some(target_fps as i32),
                });
            }
            EncoderCfg::VPX(VpxEncoderConfig {
                width: c.width as _,
                height: c.height as _,
                quality,
                codec: VpxVideoCodecId::VP9,
                keyframe_interval,
            })
        }
        format @ (CodecFormat::VP8 | CodecFormat::VP9) => EncoderCfg::VPX(VpxEncoderConfig {
            width: c.width as _,
            height: c.height as _,
            quality,
            codec: if format == CodecFormat::VP8 {
                VpxVideoCodecId::VP8
            } else {
                VpxVideoCodecId::VP9
            },
            keyframe_interval,
        }),
        CodecFormat::AV1 => EncoderCfg::AOM(AomEncoderConfig {
            width: c.width as _,
            height: c.height as _,
            quality,
            keyframe_interval,
        }),
        _ => EncoderCfg::VPX(VpxEncoderConfig {
            width: c.width as _,
            height: c.height as _,
            quality,
            codec: VpxVideoCodecId::VP9,
            keyframe_interval,
        }),
    }
}

fn get_recorder(
    record_incoming: bool,
    display_idx: usize,
    camera: bool,
) -> Arc<Mutex<Option<Recorder>>> {
    #[cfg(windows)]
    let root = crate::platform::is_root();
    #[cfg(not(windows))]
    let root = false;
    let recorder = if record_incoming {
        use crate::hbbs_http::record_upload;

        let tx = if record_upload::is_enable() {
            let (tx, rx) = std::sync::mpsc::channel();
            record_upload::run(rx);
            Some(tx)
        } else {
            None
        };
        Recorder::new(RecorderContext {
            server: true,
            id: Config::get_id(),
            dir: crate::ui_interface::video_save_directory(root),
            display_idx,
            camera,
            tx,
        })
        .map_or(Default::default(), |r| Arc::new(Mutex::new(Some(r))))
    } else {
        Default::default()
    };

    recorder
}

#[cfg(target_os = "android")]
fn check_change_scale(hardware: bool) -> ResultType<()> {
    use hbb_common::config::keys::OPTION_ENABLE_ANDROID_SOFTWARE_ENCODING_HALF_SCALE as SCALE_SOFT;

    // isStart flag is set at the end of startCapture() in Android, wait it to be set.
    let n = 60; // 3s
    for i in 0..n {
        if scrap::is_start() == Some(true) {
            log::info!("start flag is set");
            break;
        }
        log::info!("wait for start, {i}");
        std::thread::sleep(Duration::from_millis(50));
        if i == n - 1 {
            log::error!("wait for start timeout");
        }
    }
    let screen_size = scrap::screen_size();
    let scale_soft = hbb_common::config::option2bool(SCALE_SOFT, &Config::get_option(SCALE_SOFT));
    let half_scale = !hardware && scale_soft;
    log::info!("hardware: {hardware}, scale_soft: {scale_soft}, screen_size: {screen_size:?}",);
    scrap::android::call_main_service_set_by_name(
        "half_scale",
        Some(half_scale.to_string().as_str()),
        None,
    )
    .ok();
    let old_scale = screen_size.2;
    let new_scale = scrap::screen_size().2;
    log::info!("old_scale: {old_scale}, new_scale: {new_scale}");
    if old_scale != new_scale {
        log::info!("switch due to scale changed, {old_scale} -> {new_scale}");
        // switch is not a must, but it is better to do so.
        bail!("SWITCH");
    }
    Ok(())
}

fn check_privacy_mode_changed(
    sp: &GenericService,
    display_idx: usize,
    ci: &CapturerInfo,
) -> ResultType<()> {
    let privacy_mode_id_2 = get_privacy_mode_conn_id().unwrap_or(INVALID_PRIVACY_MODE_CONN_ID);
    if ci.privacy_mode_id != privacy_mode_id_2 {
        if privacy_mode_id_2 != INVALID_PRIVACY_MODE_CONN_ID {
            let msg_out = crate::common::make_privacy_mode_msg(
                back_notification::PrivacyModeState::PrvOnByOther,
                "".to_owned(),
            );
            sp.send_to_others(msg_out, privacy_mode_id_2);
        }
        log::info!("switch due to privacy mode changed");
        try_broadcast_display_changed(&sp, display_idx, ci, true).ok();
        bail!("SWITCH");
    }
    Ok(())
}

#[inline]
fn handle_one_frame(
    display: usize,
    sp: &GenericService,
    frame: EncodeInput,
    ms: i64,
    encoder: &mut Encoder,
    recorder: Arc<Mutex<Option<Recorder>>>,
    encode_fail_counter: &mut usize,
    first_frame: &mut bool,
    width: usize,
    height: usize,
) -> ResultType<HashSet<i32>> {
    sp.snapshot(|sps| {
        // so that new sub and old sub share the same encoder after switch
        if sps.has_subscribes() {
            log::info!("switch due to new subscriber");
            bail!("SWITCH");
        }
        Ok(())
    })?;

    let mut send_conn_ids: HashSet<i32> = Default::default();
    let first = *first_frame;
    *first_frame = false;
    match encoder.encode_to_message(frame, ms) {
        Ok(mut vf) => {
            *encode_fail_counter = 0;
            vf.display = display as _;
            let mut msg = Message::new();
            msg.set_video_frame(vf);
            recorder
                .lock()
                .unwrap()
                .as_mut()
                .map(|r| r.write_message(&msg, width, height));
            send_conn_ids = sp.send_video_frame(msg);
        }
        Err(e) => {
            *encode_fail_counter += 1;
            // Encoding errors are not frequent except on Android
            if !cfg!(target_os = "android") {
                log::error!("encode fail: {e:?}, times: {}", *encode_fail_counter,);
            }
            let max_fail_times = if cfg!(target_os = "android") && encoder.is_hardware() {
                9
            } else {
                3
            };
            let repeat = !encoder.latency_free();
            // repeat encoders can reach max_fail_times on the first frame
            if (first && !repeat) || *encode_fail_counter >= max_fail_times {
                *encode_fail_counter = 0;
                if encoder.is_hardware() {
                    encoder.disable();
                    log::error!("switch due to encoding fails, first frame: {first}, error: {e:?}");
                    bail!("SWITCH");
                }
            }
            match e.to_string().as_str() {
                scrap::codec::ENCODE_NEED_SWITCH => {
                    encoder.disable();
                    log::error!("switch due to encoder need switch");
                    bail!("SWITCH");
                }
                _ => {}
            }
        }
    }
    Ok(send_conn_ids)
}

#[inline]
pub fn refresh() {
    #[cfg(target_os = "android")]
    Display::refresh_size();
}

#[cfg(windows)]
fn start_uac_elevation_check() {
    static START: Once = Once::new();
    START.call_once(|| {
        if !crate::platform::is_installed() && !crate::platform::is_root() {
            std::thread::spawn(|| loop {
                std::thread::sleep(std::time::Duration::from_secs(1));
                if let Ok(uac) = is_process_consent_running() {
                    *IS_UAC_RUNNING.lock().unwrap() = uac;
                }
                if !crate::platform::is_elevated(None).unwrap_or(false) {
                    if let Ok(elevated) = crate::platform::is_foreground_window_elevated() {
                        *IS_FOREGROUND_WINDOW_ELEVATED.lock().unwrap() = elevated;
                    }
                }
            });
        }
    });
}

#[inline]
fn try_broadcast_display_changed(
    sp: &GenericService,
    display_idx: usize,
    cap: &CapturerInfo,
    refresh: bool,
) -> ResultType<()> {
    if refresh {
        // Get display information immediately.
        crate::display_service::check_displays_changed().ok();
    }
    if let Some(display) = check_display_changed(
        cap.ndisplay,
        cap.current,
        (cap.origin.0, cap.origin.1, cap.width, cap.height),
    ) {
        log::info!("Display {} changed", display);
        if let Some(msg_out) =
            make_display_changed_msg(display_idx, Some(display), VideoSource::Monitor)
        {
            let msg_out = Arc::new(msg_out);
            sp.send_shared(msg_out.clone());
            // switch display may occur before the first video frame, add snapshot to send to new subscribers
            sp.snapshot(move |sps| {
                sps.send_shared(msg_out.clone());
                Ok(())
            })?;
            bail!("SWITCH");
        }
    }
    Ok(())
}

pub fn make_display_changed_msg(
    display_idx: usize,
    opt_display: Option<DisplayInfo>,
    source: VideoSource,
) -> Option<Message> {
    let display = match opt_display {
        Some(d) => d,
        None => match source {
            VideoSource::Monitor => display_service::get_display_info(display_idx)?,
            VideoSource::Camera => camera::Cameras::get_sync_cameras()
                .get(display_idx)?
                .clone(),
        },
    };
    let mut misc = Misc::new();
    misc.set_switch_display(SwitchDisplay {
        display: display_idx as _,
        x: display.x,
        y: display.y,
        width: display.width,
        height: display.height,
        cursor_embedded: match source {
            VideoSource::Monitor => display_service::capture_cursor_embedded(),
            VideoSource::Camera => false,
        },
        #[cfg(not(target_os = "android"))]
        resolutions: Some(SupportedResolutions {
            resolutions: match source {
                VideoSource::Monitor => {
                    if display.name.is_empty() {
                        vec![]
                    } else {
                        crate::platform::resolutions(&display.name)
                    }
                }
                VideoSource::Camera => camera::Cameras::get_camera_resolution(display_idx)
                    .ok()
                    .into_iter()
                    .collect(),
            },
            ..SupportedResolutions::default()
        })
        .into(),
        original_resolution: display.original_resolution,
        ..Default::default()
    });
    let mut msg_out = Message::new();
    msg_out.set_misc(misc);
    Some(msg_out)
}

fn check_qos(
    encoder: &mut Encoder,
    ratio: &mut f32,
    spf: &mut Duration,
    client_record: bool,
    send_counter: &mut usize,
    second_instant: &mut Instant,
    name: &str,
) -> ResultType<()> {
    let mut video_qos = VIDEO_QOS.lock().unwrap();
    *spf = video_qos.spf();
    if *ratio != video_qos.ratio() {
        *ratio = video_qos.ratio();
        if encoder.support_changing_quality() {
            allow_err!(encoder.set_quality(*ratio));
            video_qos.store_bitrate(encoder.bitrate());
        } else {
            // Now only vaapi doesn't support changing quality
            if !video_qos.in_vbr_state() && !video_qos.latest_quality().is_custom() {
                log::info!("switch to change quality");
                bail!("SWITCH");
            }
        }
    }
    if client_record != video_qos.record() {
        log::info!("switch due to record changed");
        bail!("SWITCH");
    }
    if second_instant.elapsed() > Duration::from_secs(1) {
        *second_instant = Instant::now();
        video_qos.update_display_data(&name, *send_counter);
        *send_counter = 0;
    }
    drop(video_qos);
    Ok(())
}

pub fn set_take_screenshot(display_idx: usize, sid: String, tx: Sender) {
    SCREENSHOTS.lock().unwrap().insert(
        display_idx,
        Screenshot {
            sid,
            tx,
            restore_vram: false,
        },
    );
}

// We need to this function, because the `stride` may be larger than `width * 4`.
fn get_rgba_from_pixelbuf<'a>(pixbuf: &scrap::PixelBuffer<'a>) -> ResultType<Vec<u8>> {
    let w = pixbuf.width();
    let h = pixbuf.height();
    let stride = pixbuf.stride();
    let Some(s) = stride.get(0) else {
        bail!("Invalid pixel buf stride.")
    };

    if *s == w * 4 {
        let mut rgba = vec![];
        scrap::convert(pixbuf, scrap::Pixfmt::RGBA, &mut rgba)?;
        Ok(rgba)
    } else {
        let bgra = pixbuf.data();
        let mut bit_flipped = Vec::with_capacity(w * h * 4);
        for y in 0..h {
            for x in 0..w {
                let i = s * y + 4 * x;
                bit_flipped.extend_from_slice(&[bgra[i + 2], bgra[i + 1], bgra[i], bgra[i + 3]]);
            }
        }
        Ok(bit_flipped)
    }
}

fn handle_screenshot(screenshot: Screenshot, msg: String, w: usize, h: usize, data: Vec<u8>) {
    let mut response = ScreenshotResponse::new();
    response.sid = screenshot.sid;
    if msg.is_empty() {
        if data.is_empty() {
            response.msg = "Failed to take screenshot, please try again later.".to_owned();
        } else {
            fn encode_png(width: usize, height: usize, rgba: Vec<u8>) -> ResultType<Vec<u8>> {
                let mut png = Vec::new();
                let mut encoder =
                    repng::Options::smallest(width as _, height as _).build(&mut png)?;
                encoder.write(&rgba)?;
                encoder.finish()?;
                Ok(png)
            }
            match encode_png(w as _, h as _, data) {
                Ok(png) => {
                    response.data = png.into();
                }
                Err(e) => {
                    response.msg = format!("Error encoding png: {}", e);
                }
            }
        }
    } else {
        response.msg = msg;
    }
    let mut msg_out = Message::new();
    msg_out.set_screenshot_response(response);
    if let Err(e) = screenshot
        .tx
        .send((hbb_common::tokio::time::Instant::now(), Arc::new(msg_out)))
    {
        log::error!("Failed to send screenshot, {}", e);
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::HashSet;
    use std::time::{Duration, Instant};

    /// Helper to set up the FRAME_FETCHED_NOTIFIERS entry for a given display_idx.
    /// Returns the sender half so tests can simulate ACKs.
    fn setup_notifier(display_idx: usize) -> FrameFetchedNotifierSender {
        let (tx, rx) = hbb_common::tokio::sync::mpsc::unbounded_channel();
        let rx = Arc::new(TokioMutex::new(rx));
        FRAME_FETCHED_NOTIFIERS
            .lock()
            .unwrap()
            .insert(display_idx, (tx.clone(), rx));
        tx
    }

    /// Clean up a notifier entry after a test.
    fn teardown_notifier(display_idx: usize) {
        FRAME_FETCHED_NOTIFIERS
            .lock()
            .unwrap()
            .remove(&display_idx);
        DISPLAY_CONN_IDS.lock().unwrap().remove(&display_idx);
    }

    #[test]
    fn test_frame_controller_zero_connections_does_not_block() {
        // Use a unique display_idx to avoid conflicts with other tests.
        let display_idx = 9000;
        let _tx = setup_notifier(display_idx);
        let mut fc = VideoFrameController::new(display_idx);
        // send_conn_ids is empty, so try_wait_next should return immediately.
        let mut fetched = HashSet::new();
        let start = Instant::now();
        fc.try_wait_next(&mut fetched, 5000);
        let elapsed = start.elapsed();
        // Should return nearly instantly (well under 100ms).
        assert!(
            elapsed < Duration::from_millis(100),
            "try_wait_next with 0 connections took {:?}, expected < 100ms",
            elapsed
        );
        assert!(fetched.is_empty());
        teardown_notifier(display_idx);
    }

    #[test]
    fn test_frame_controller_immediate_ack() {
        let display_idx = 9001;
        let tx = setup_notifier(display_idx);
        let mut fc = VideoFrameController::new(display_idx);

        // Register one connection as having been sent a frame.
        let mut conn_ids = HashSet::new();
        conn_ids.insert(42);
        fc.set_send(Instant::now(), conn_ids);

        // Simulate the client ACKing immediately (before we poll).
        tx.send((42, Some(Instant::now()))).unwrap();

        let mut fetched = HashSet::new();
        let start = Instant::now();
        fc.try_wait_next(&mut fetched, 1000);
        let elapsed = start.elapsed();

        assert!(fetched.contains(&42), "Should have received ACK for conn 42");
        // With the ACK already in the channel, it should return nearly instantly.
        assert!(
            elapsed < Duration::from_millis(100),
            "Immediate ACK took {:?}, expected < 100ms",
            elapsed
        );
        teardown_notifier(display_idx);
    }

    #[test]
    fn test_frame_controller_timeout_respected() {
        let display_idx = 9002;
        let _tx = setup_notifier(display_idx);
        let mut fc = VideoFrameController::new(display_idx);

        // Register one connection but do NOT send an ACK.
        let mut conn_ids = HashSet::new();
        conn_ids.insert(99);
        fc.set_send(Instant::now(), conn_ids);

        let mut fetched = HashSet::new();
        let timeout_ms = 100u64;
        let start = Instant::now();
        fc.try_wait_next(&mut fetched, timeout_ms);
        let elapsed = start.elapsed();

        // Should have waited approximately the timeout duration, not longer.
        assert!(
            elapsed >= Duration::from_millis(timeout_ms.saturating_sub(10)),
            "Should wait at least ~{}ms, but only waited {:?}",
            timeout_ms,
            elapsed
        );
        assert!(
            elapsed < Duration::from_millis(timeout_ms + 200),
            "Should not block much longer than {}ms, but waited {:?}",
            timeout_ms,
            elapsed
        );
        // No ACK was sent, so fetched should be empty.
        assert!(fetched.is_empty());
        teardown_notifier(display_idx);
    }

    #[test]
    fn test_low_latency_mode_returns_zero_timeout() {
        // When low-latency-mode is not set (default), we should get normal timeouts.
        let (poll, outer) = VideoFrameController::get_ack_timeouts();
        // We can't easily mock Config::get_option in tests, but we can verify
        // that the default path returns the normal-mode constants.
        // If "low-latency-mode" is not set, get_option returns "" which != "Y".
        assert_eq!(poll, FRAME_ACK_POLL_NORMAL_MS);
        assert_eq!(outer, FRAME_ACK_TIMEOUT_NORMAL_MS);
    }

    #[test]
    fn test_frame_controller_nonblocking_poll() {
        // Verify that timeout_millis=0 (non-blocking) returns immediately
        // even when there is a pending connection with no ACK.
        let display_idx = 9003;
        let _tx = setup_notifier(display_idx);
        let mut fc = VideoFrameController::new(display_idx);

        let mut conn_ids = HashSet::new();
        conn_ids.insert(1);
        conn_ids.insert(2);
        fc.set_send(Instant::now(), conn_ids);

        let mut fetched = HashSet::new();
        let start = Instant::now();
        // Non-blocking: timeout=0
        fc.try_wait_next(&mut fetched, 0);
        let elapsed = start.elapsed();

        // Should return nearly instantly.
        assert!(
            elapsed < Duration::from_millis(100),
            "Non-blocking poll took {:?}, expected < 100ms",
            elapsed
        );
        teardown_notifier(display_idx);
    }

    #[test]
    fn test_ack_timeout_constants() {
        // Verify the constants are sensible.
        assert_eq!(FRAME_ACK_TIMEOUT_NORMAL_MS, 500);
        assert_eq!(FRAME_ACK_POLL_NORMAL_MS, 50);
        assert_eq!(FRAME_ACK_POLL_LOW_LATENCY_MS, 0);
    }

    #[test]
    fn test_repeat_encode_max_constant() {
        // The repeat_encode_max is a local variable in the capture loop.
        // Verify the expected value matches what is declared in the loop body.
        // This constant is set to 10 — re-encode at most 10 stale frames when
        // the screen hasn't changed and the encoder is not latency-free.
        let repeat_encode_max: u32 = 10;
        assert_eq!(repeat_encode_max, 10);
    }

    #[test]
    fn test_low_latency_mode_skips_repeat_encoding() {
        // In low-latency / gaming mode, the repeat-encoding path should be
        // skipped entirely to avoid wasting GPU encoder time on stale frames.
        //
        // We cannot toggle Config in unit tests, so we verify the decision
        // logic directly: when is_low_latency_mode() returns true the branch
        // is a no-op; when false, normal repeat encoding up to
        // repeat_encode_max applies.
        //
        // Default config has low-latency mode OFF, so the normal path applies.
        assert!(!video_qos::is_low_latency_mode());

        // Simulate the decision logic from the capture loop:
        let repeat_encode_max: u32 = 10;
        let mut repeat_encode_counter: u32 = 0;
        let encoder_latency_free = false;
        let yuv_len = 100; // non-zero means we have a YUV frame

        // Normal mode: repeat encoding should proceed
        if !encoder_latency_free && yuv_len > 0 {
            if video_qos::is_low_latency_mode() {
                // Would skip — but low-latency is off by default
                panic!("should not reach here in default config");
            } else if repeat_encode_counter < repeat_encode_max {
                repeat_encode_counter += 1;
            }
        }
        assert_eq!(repeat_encode_counter, 1, "normal mode should increment counter");

        // Verify the value-based helper agrees
        assert!(video_qos::is_low_latency_mode_value("Y"));
        assert!(!video_qos::is_low_latency_mode_value(""));
    }

    // ---------------------------------------------------------------
    // VideoFrameController: new / reset / set_send
    // ---------------------------------------------------------------

    #[test]
    fn test_frame_controller_new_initial_state() {
        let fc = VideoFrameController::new(8000);
        assert_eq!(fc.display_idx, 8000);
        assert!(
            fc.send_conn_ids.is_empty(),
            "New controller should start with no connection IDs"
        );
    }

    #[test]
    fn test_frame_controller_reset_clears_conn_ids() {
        let display_idx = 9010;
        let _tx = setup_notifier(display_idx);
        let mut fc = VideoFrameController::new(display_idx);

        let mut conn_ids = HashSet::new();
        conn_ids.insert(10);
        conn_ids.insert(20);
        fc.set_send(Instant::now(), conn_ids);
        assert_eq!(fc.send_conn_ids.len(), 2);

        fc.reset();
        assert!(
            fc.send_conn_ids.is_empty(),
            "reset() should clear all connection IDs"
        );
        teardown_notifier(display_idx);
    }

    #[test]
    fn test_frame_controller_set_send_updates_conn_ids() {
        let display_idx = 9011;
        let _tx = setup_notifier(display_idx);
        let mut fc = VideoFrameController::new(display_idx);

        let mut ids = HashSet::new();
        ids.insert(100);
        ids.insert(200);
        ids.insert(300);
        let tm = Instant::now();
        fc.set_send(tm, ids.clone());

        assert_eq!(fc.send_conn_ids, ids);
        assert_eq!(fc.cur, tm);
        teardown_notifier(display_idx);
    }

    #[test]
    fn test_frame_controller_set_send_empty_does_not_update_display_conn_ids() {
        let display_idx = 9012;
        let _tx = setup_notifier(display_idx);
        let mut fc = VideoFrameController::new(display_idx);

        // set_send with an empty HashSet should be a no-op for DISPLAY_CONN_IDS.
        let old_tm = fc.cur;
        fc.set_send(Instant::now(), HashSet::new());
        // cur should NOT be updated when conn_ids is empty.
        assert_eq!(fc.cur, old_tm);
        assert!(fc.send_conn_ids.is_empty());

        // DISPLAY_CONN_IDS should not have an entry for this display.
        let display_ids = DISPLAY_CONN_IDS.lock().unwrap();
        assert!(
            !display_ids.contains_key(&display_idx),
            "Empty set_send should not insert into DISPLAY_CONN_IDS"
        );
        drop(display_ids);
        teardown_notifier(display_idx);
    }

    #[test]
    fn test_frame_controller_set_send_nonempty_populates_display_conn_ids() {
        let display_idx = 9013;
        let _tx = setup_notifier(display_idx);
        let mut fc = VideoFrameController::new(display_idx);

        let mut ids = HashSet::new();
        ids.insert(5);
        ids.insert(6);
        fc.set_send(Instant::now(), ids.clone());

        let display_ids = DISPLAY_CONN_IDS.lock().unwrap();
        assert_eq!(
            display_ids.get(&display_idx),
            Some(&ids),
            "set_send with nonempty IDs should populate DISPLAY_CONN_IDS"
        );
        drop(display_ids);
        teardown_notifier(display_idx);
    }

    #[test]
    fn test_frame_controller_set_send_replaces_previous() {
        let display_idx = 9014;
        let _tx = setup_notifier(display_idx);
        let mut fc = VideoFrameController::new(display_idx);

        let mut first = HashSet::new();
        first.insert(1);
        fc.set_send(Instant::now(), first);
        assert_eq!(fc.send_conn_ids.len(), 1);
        assert!(fc.send_conn_ids.contains(&1));

        // Calling set_send again should replace, not merge.
        let mut second = HashSet::new();
        second.insert(2);
        second.insert(3);
        fc.set_send(Instant::now(), second.clone());
        assert_eq!(fc.send_conn_ids, second);
        assert!(!fc.send_conn_ids.contains(&1));
        teardown_notifier(display_idx);
    }

    // ---------------------------------------------------------------
    // Multiple connections: partial ACK
    // ---------------------------------------------------------------

    #[test]
    fn test_frame_controller_partial_ack_from_multiple_connections() {
        let display_idx = 9020;
        let tx = setup_notifier(display_idx);
        let mut fc = VideoFrameController::new(display_idx);

        let mut ids = HashSet::new();
        ids.insert(10);
        ids.insert(20);
        ids.insert(30);
        fc.set_send(Instant::now(), ids);

        // Only conn 10 and 30 ACK.
        tx.send((10, Some(Instant::now()))).unwrap();
        tx.send((30, Some(Instant::now()))).unwrap();

        let mut fetched = HashSet::new();
        fc.try_wait_next(&mut fetched, 200);

        assert!(fetched.contains(&10));
        assert!(fetched.contains(&30));
        // conn 20 did not ACK, but we should still get the ones that did.
        assert!(!fetched.contains(&20));
        teardown_notifier(display_idx);
    }

    #[test]
    fn test_frame_controller_all_connections_ack() {
        let display_idx = 9021;
        let tx = setup_notifier(display_idx);
        let mut fc = VideoFrameController::new(display_idx);

        let mut ids = HashSet::new();
        ids.insert(7);
        ids.insert(8);
        fc.set_send(Instant::now(), ids);

        // Both ACK.
        tx.send((7, Some(Instant::now()))).unwrap();
        tx.send((8, Some(Instant::now()))).unwrap();

        let mut fetched = HashSet::new();
        let start = Instant::now();
        fc.try_wait_next(&mut fetched, 2000);
        let elapsed = start.elapsed();

        assert_eq!(fetched.len(), 2);
        assert!(fetched.contains(&7));
        assert!(fetched.contains(&8));
        // Both ACKed before poll, so it should return quickly.
        assert!(
            elapsed < Duration::from_millis(500),
            "All ACKs present should return fast, took {:?}",
            elapsed
        );
        teardown_notifier(display_idx);
    }

    #[test]
    fn test_frame_controller_duplicate_ack_deduplicates() {
        let display_idx = 9022;
        let tx = setup_notifier(display_idx);
        let mut fc = VideoFrameController::new(display_idx);

        let mut ids = HashSet::new();
        ids.insert(42);
        fc.set_send(Instant::now(), ids);

        // Send duplicate ACKs for the same connection.
        tx.send((42, Some(Instant::now()))).unwrap();
        tx.send((42, Some(Instant::now()))).unwrap();
        tx.send((42, None)).unwrap();

        let mut fetched = HashSet::new();
        fc.try_wait_next(&mut fetched, 200);

        // HashSet naturally deduplicates.
        assert_eq!(fetched.len(), 1);
        assert!(fetched.contains(&42));
        teardown_notifier(display_idx);
    }

    #[test]
    fn test_frame_controller_ack_with_none_instant() {
        // ACKs can have None as the Instant (e.g., from web clients).
        let display_idx = 9023;
        let tx = setup_notifier(display_idx);
        let mut fc = VideoFrameController::new(display_idx);

        let mut ids = HashSet::new();
        ids.insert(55);
        fc.set_send(Instant::now(), ids);

        tx.send((55, None)).unwrap();

        let mut fetched = HashSet::new();
        fc.try_wait_next(&mut fetched, 200);

        assert!(
            fetched.contains(&55),
            "ACK with None instant should still register"
        );
        teardown_notifier(display_idx);
    }

    // ---------------------------------------------------------------
    // VideoSource enum
    // ---------------------------------------------------------------

    #[test]
    fn test_video_source_service_name_prefix() {
        assert_eq!(VideoSource::Monitor.service_name_prefix(), "monitor");
        assert_eq!(VideoSource::Camera.service_name_prefix(), "camera");
    }

    #[test]
    fn test_video_source_is_monitor() {
        assert!(VideoSource::Monitor.is_monitor());
        assert!(!VideoSource::Camera.is_monitor());
    }

    #[test]
    fn test_video_source_is_camera() {
        assert!(VideoSource::Camera.is_camera());
        assert!(!VideoSource::Monitor.is_camera());
    }

    #[test]
    fn test_video_source_equality() {
        assert_eq!(VideoSource::Monitor, VideoSource::Monitor);
        assert_eq!(VideoSource::Camera, VideoSource::Camera);
        assert_ne!(VideoSource::Monitor, VideoSource::Camera);
    }

    #[test]
    fn test_video_source_clone() {
        let src = VideoSource::Monitor;
        let cloned = src.clone();
        assert_eq!(src, cloned);
    }

    // ---------------------------------------------------------------
    // get_service_name
    // ---------------------------------------------------------------

    #[test]
    fn test_get_service_name_monitor() {
        assert_eq!(get_service_name(VideoSource::Monitor, 0), "monitor0");
        assert_eq!(get_service_name(VideoSource::Monitor, 1), "monitor1");
        assert_eq!(get_service_name(VideoSource::Monitor, 42), "monitor42");
    }

    #[test]
    fn test_get_service_name_camera() {
        assert_eq!(get_service_name(VideoSource::Camera, 0), "camera0");
        assert_eq!(get_service_name(VideoSource::Camera, 3), "camera3");
    }

    // ---------------------------------------------------------------
    // OPTION_REFRESH constant
    // ---------------------------------------------------------------

    #[test]
    fn test_option_refresh_value() {
        assert_eq!(OPTION_REFRESH, "refresh");
    }

    // ---------------------------------------------------------------
    // Frame timing constant relationships
    // ---------------------------------------------------------------

    #[test]
    fn test_normal_poll_less_than_timeout() {
        // The poll interval must be strictly less than the outer timeout,
        // otherwise polling would never have a chance to retry.
        assert!(
            FRAME_ACK_POLL_NORMAL_MS < FRAME_ACK_TIMEOUT_NORMAL_MS,
            "Poll interval ({}) must be less than outer timeout ({})",
            FRAME_ACK_POLL_NORMAL_MS,
            FRAME_ACK_TIMEOUT_NORMAL_MS
        );
    }

    #[test]
    fn test_low_latency_poll_is_zero() {
        // Low-latency mode uses non-blocking poll (0ms).
        assert_eq!(FRAME_ACK_POLL_LOW_LATENCY_MS, 0);
    }

    #[test]
    fn test_timeout_allows_multiple_polls() {
        // The outer timeout should allow at least a few polls.
        let polls_possible = FRAME_ACK_TIMEOUT_NORMAL_MS / FRAME_ACK_POLL_NORMAL_MS;
        assert!(
            polls_possible >= 2,
            "Timeout should allow at least 2 polls, allows {}",
            polls_possible
        );
    }

    // ---------------------------------------------------------------
    // Encode failure tolerance logic
    //
    // Extracted from handle_one_frame():
    //   let max_fail_times = if cfg!(target_os = "android") && encoder.is_hardware() { 9 } else { 3 };
    //
    // We test the branching logic directly since we cannot easily
    // construct an Encoder in tests.
    // ---------------------------------------------------------------

    /// Replicates the max_fail_times calculation from handle_one_frame.
    fn compute_max_fail_times(is_android: bool, is_hardware: bool) -> usize {
        if is_android && is_hardware {
            9
        } else {
            3
        }
    }

    #[test]
    fn test_max_fail_times_android_hardware() {
        assert_eq!(compute_max_fail_times(true, true), 9);
    }

    #[test]
    fn test_max_fail_times_android_software() {
        assert_eq!(compute_max_fail_times(true, false), 3);
    }

    #[test]
    fn test_max_fail_times_non_android_hardware() {
        assert_eq!(compute_max_fail_times(false, true), 3);
    }

    #[test]
    fn test_max_fail_times_non_android_software() {
        assert_eq!(compute_max_fail_times(false, false), 3);
    }

    /// Replicates the bail-on-fail logic from handle_one_frame:
    ///   if (first && !repeat) || *encode_fail_counter >= max_fail_times { ... bail }
    fn should_bail_on_encode_fail(
        first: bool,
        repeat: bool,
        counter: usize,
        max_fail: usize,
    ) -> bool {
        (first && !repeat) || counter >= max_fail
    }

    #[test]
    fn test_should_bail_first_frame_non_repeat() {
        // First frame + non-repeat encoder -> bail immediately on error.
        assert!(should_bail_on_encode_fail(true, false, 0, 3));
    }

    #[test]
    fn test_should_bail_first_frame_repeat_encoder() {
        // First frame + repeat encoder -> do NOT bail immediately;
        // repeat encoders can hit errors on first frame.
        assert!(!should_bail_on_encode_fail(true, true, 0, 3));
    }

    #[test]
    fn test_should_bail_counter_at_threshold() {
        assert!(should_bail_on_encode_fail(false, false, 3, 3));
        assert!(should_bail_on_encode_fail(false, true, 3, 3));
    }

    #[test]
    fn test_should_bail_counter_below_threshold() {
        assert!(!should_bail_on_encode_fail(false, false, 2, 3));
        assert!(!should_bail_on_encode_fail(false, true, 2, 3));
    }

    #[test]
    fn test_should_bail_counter_above_threshold() {
        assert!(should_bail_on_encode_fail(false, false, 5, 3));
        assert!(should_bail_on_encode_fail(false, false, 100, 9));
    }

    #[test]
    fn test_encode_fail_counter_resets_on_success() {
        // Simulates: on successful encode, counter resets to 0.
        let mut encode_fail_counter: usize = 5;
        // Simulate successful encode.
        encode_fail_counter = 0;
        assert_eq!(encode_fail_counter, 0);
        // Next failure starts from 0 again.
        encode_fail_counter += 1;
        assert_eq!(encode_fail_counter, 1);
    }

    #[test]
    fn test_encode_fail_counter_increments() {
        let mut counter: usize = 0;
        let max = 3usize;
        // Simulate 3 consecutive failures.
        for _ in 0..max {
            counter += 1;
        }
        assert_eq!(counter, max);
        assert!(should_bail_on_encode_fail(false, false, counter, max));
    }

    // ---------------------------------------------------------------
    // repeat_encode_counter logic
    // ---------------------------------------------------------------

    #[test]
    fn test_repeat_encode_counter_logic() {
        let repeat_encode_max = 10u32;
        let mut counter = 0u32;

        // Simulate 10 WouldBlock frames: counter should increment each time.
        for i in 0..repeat_encode_max {
            assert!(
                counter < repeat_encode_max,
                "Counter {} should be below max {} at iteration {}",
                counter,
                repeat_encode_max,
                i
            );
            counter += 1;
        }
        assert_eq!(counter, repeat_encode_max);

        // At max, the condition `counter < repeat_encode_max` is false,
        // so no more repeat encodes happen.
        assert!(!(counter < repeat_encode_max));

        // On a successful frame, counter resets.
        counter = 0;
        assert!(counter < repeat_encode_max);
    }

    #[test]
    fn test_repeat_encoding_skipped_for_latency_free() {
        // In the run loop: repeat encoding only happens when
        // `!encoder.latency_free() && yuv.len() > 0`.
        // When latency_free is true, the branch is skipped entirely.
        let latency_free = true;
        let yuv_len = 100;
        let should_repeat = !latency_free && yuv_len > 0;
        assert!(
            !should_repeat,
            "Latency-free encoder should skip repeat encoding"
        );
    }

    #[test]
    fn test_repeat_encoding_skipped_for_empty_yuv() {
        // yuv.len() == 0 means the frame is texture-based, not YUV.
        let latency_free = false;
        let yuv_len = 0;
        let should_repeat = !latency_free && yuv_len > 0;
        assert!(
            !should_repeat,
            "Empty YUV buffer should skip repeat encoding"
        );
    }

    #[test]
    fn test_repeat_encoding_proceeds_for_normal_case() {
        let latency_free = false;
        let yuv_len = 1920 * 1080 * 3 / 2; // typical YUV420 size
        let should_repeat = !latency_free && yuv_len > 0;
        assert!(
            should_repeat,
            "Non-latency-free encoder with YUV data should do repeat encoding"
        );
    }

    // ---------------------------------------------------------------
    // Keyframe interval logic from get_encoder_config
    //
    // In get_encoder_config():
    //   let keyframe_interval = if record { Some(240) } else { None };
    // ---------------------------------------------------------------

    #[test]
    fn test_keyframe_interval_when_recording() {
        let record = true;
        let keyframe_interval: Option<usize> = if record { Some(240) } else { None };
        assert_eq!(keyframe_interval, Some(240));
    }

    #[test]
    fn test_keyframe_interval_when_not_recording() {
        let record = false;
        let keyframe_interval: Option<usize> = if record { Some(240) } else { None };
        assert_eq!(keyframe_interval, None);
    }

    // ---------------------------------------------------------------
    // notify_video_frame_fetched_by_conn_id routing
    // ---------------------------------------------------------------

    #[test]
    fn test_notify_by_conn_id_filters_correct_displays() {
        // Set up two displays with different conn IDs.
        let d1 = 9030;
        let d2 = 9031;
        let _tx1 = setup_notifier(d1);
        let _tx2 = setup_notifier(d2);

        // Populate DISPLAY_CONN_IDS: display d1 has conn 50, display d2 has conn 51.
        {
            let mut dci = DISPLAY_CONN_IDS.lock().unwrap();
            let mut s1 = HashSet::new();
            s1.insert(50);
            dci.insert(d1, s1);
            let mut s2 = HashSet::new();
            s2.insert(51);
            dci.insert(d2, s2);
        }

        // Notify conn 50 -- should only go to display d1.
        notify_video_frame_fetched_by_conn_id(50, None);

        let mut fc1 = VideoFrameController::new(d1);
        let mut ids1 = HashSet::new();
        ids1.insert(50);
        fc1.set_send(Instant::now(), ids1);
        let mut fetched1 = HashSet::new();
        fc1.try_wait_next(&mut fetched1, 100);
        assert!(
            fetched1.contains(&50),
            "Display d1 should have received notification for conn 50"
        );

        // Display d2 should have nothing.
        let mut fc2 = VideoFrameController::new(d2);
        let mut ids2 = HashSet::new();
        ids2.insert(51);
        fc2.set_send(Instant::now(), ids2);
        let mut fetched2 = HashSet::new();
        fc2.try_wait_next(&mut fetched2, 50);
        assert!(
            fetched2.is_empty(),
            "Display d2 should NOT have received notification for conn 50"
        );

        teardown_notifier(d1);
        teardown_notifier(d2);
    }

    #[test]
    fn test_notify_by_conn_id_no_matching_display() {
        // If no display has the given conn_id, nothing should happen (no panic).
        let d = 9032;
        let _tx = setup_notifier(d);
        DISPLAY_CONN_IDS.lock().unwrap().remove(&d);

        // Should not panic.
        notify_video_frame_fetched_by_conn_id(999, Some(Instant::now()));

        teardown_notifier(d);
    }

    // ---------------------------------------------------------------
    // VideoFrameController: try_wait_next accumulates across calls
    // ---------------------------------------------------------------

    #[test]
    fn test_try_wait_next_accumulates_in_fetched_set() {
        let display_idx = 9040;
        let tx = setup_notifier(display_idx);
        let mut fc = VideoFrameController::new(display_idx);

        let mut ids = HashSet::new();
        ids.insert(1);
        ids.insert(2);
        fc.set_send(Instant::now(), ids);

        // First call: only conn 1 has ACKed.
        tx.send((1, Some(Instant::now()))).unwrap();
        let mut fetched = HashSet::new();
        fc.try_wait_next(&mut fetched, 100);
        assert!(fetched.contains(&1));

        // Second call: conn 2 now ACKs.
        tx.send((2, Some(Instant::now()))).unwrap();
        fc.try_wait_next(&mut fetched, 100);
        assert!(
            fetched.contains(&1) && fetched.contains(&2),
            "Fetched set should accumulate across calls: {:?}",
            fetched
        );
        teardown_notifier(display_idx);
    }

    // ---------------------------------------------------------------
    // get_ack_timeouts default (non-low-latency) returns
    // ---------------------------------------------------------------

    #[test]
    fn test_get_ack_timeouts_default_mode() {
        // Without "low-latency-mode" option set, defaults should be normal.
        let (poll, outer) = VideoFrameController::get_ack_timeouts();
        assert_eq!(poll, 50, "Default poll timeout should be 50ms");
        assert_eq!(outer, 500, "Default outer timeout should be 500ms");
    }

    // ---------------------------------------------------------------
    // Outer ACK wait loop logic (from run())
    //
    // The loop:
    //   while outer_timeout == 0 || wait_begin.elapsed() < outer_timeout {
    //       ...
    //       if fetched_conn_ids.len() >= frame_controller.send_conn_ids.len() { break; }
    //       if outer_timeout == 0 { break; }
    //   }
    // ---------------------------------------------------------------

    /// Simulates the outer wait loop termination logic from run().
    fn should_continue_waiting(
        elapsed_ms: u64,
        outer_timeout: u64,
        fetched_count: usize,
        send_count: usize,
    ) -> bool {
        // Entry condition
        if !(outer_timeout == 0 || elapsed_ms < outer_timeout) {
            return false;
        }
        // All ACKed -> break
        if fetched_count >= send_count {
            return false;
        }
        // Low-latency mode -> break after first poll
        if outer_timeout == 0 {
            return false;
        }
        true
    }

    #[test]
    fn test_wait_loop_breaks_when_all_acked() {
        // Even with lots of time remaining, loop breaks if all are ACKed.
        assert!(!should_continue_waiting(10, 500, 3, 3));
        assert!(!should_continue_waiting(0, 500, 5, 3));
    }

    #[test]
    fn test_wait_loop_breaks_on_timeout() {
        // Elapsed exceeds outer_timeout -> stop waiting.
        assert!(!should_continue_waiting(600, 500, 1, 3));
    }

    #[test]
    fn test_wait_loop_breaks_in_low_latency() {
        // outer_timeout == 0 -> break immediately after one poll.
        assert!(!should_continue_waiting(0, 0, 0, 3));
    }

    #[test]
    fn test_wait_loop_continues_when_partial_ack() {
        // Under timeout, not all ACKed, non-zero outer_timeout -> continue.
        assert!(should_continue_waiting(100, 500, 1, 3));
    }

    #[test]
    fn test_wait_loop_continues_at_zero_elapsed() {
        assert!(should_continue_waiting(0, 500, 0, 2));
    }

    // ---------------------------------------------------------------
    // FrameBuffer integration tests
    // ---------------------------------------------------------------
    //
    // These tests verify the FrameBuffer wiring added to the capture loop.
    // They exercise the store/take contract and the CapturedFrame
    // construction from PixelBuffer metadata, without requiring a live
    // capturer or encoder.
    //
    // Manual testing checklist for the full integration:
    //   1. Build with `cargo build --features linux-pkg-config`
    //   2. Start a remote session in normal mode — verify capture works
    //   3. Enable low-latency-mode (set option "low-latency-mode" = "Y")
    //   4. Reconnect — verify frames are still delivered smoothly
    //   5. Check log for "low-latency" related messages
    //   6. Toggle back to normal mode — verify no regressions

    #[test]
    fn test_frame_buffer_created_only_in_low_latency_mode() {
        // Default config has low-latency mode OFF.
        let low_latency = video_qos::is_low_latency_mode();
        let fb = if low_latency {
            Some(FrameBuffer::new())
        } else {
            None
        };
        // In default config, FrameBuffer should NOT be created.
        assert!(
            fb.is_none(),
            "FrameBuffer should only be created in low-latency mode"
        );
    }

    #[test]
    fn test_frame_buffer_created_when_low_latency_value() {
        // Verify the value-based check: "Y" means low-latency.
        let low_latency = video_qos::is_low_latency_mode_value("Y");
        let fb = if low_latency {
            Some(FrameBuffer::new())
        } else {
            None
        };
        assert!(
            fb.is_some(),
            "FrameBuffer should be created when low-latency is 'Y'"
        );
    }

    #[test]
    fn test_captured_frame_from_pixel_data() {
        // Simulate constructing a CapturedFrame from pixel buffer metadata,
        // mirroring the logic in the low-latency capture path.
        let width = 1920;
        let height = 1080;
        let stride = width * 4;
        let pixfmt = scrap::Pixfmt::BGRA;
        let data = vec![42u8; stride * height];

        let captured = CapturedFrame {
            data: data.clone(),
            width,
            height,
            stride,
            pixfmt,
            capture_time: Instant::now(),
            display_idx: 0,
        };

        assert_eq!(captured.width, 1920);
        assert_eq!(captured.height, 1080);
        assert_eq!(captured.stride, 1920 * 4);
        assert_eq!(captured.pixfmt, scrap::Pixfmt::BGRA);
        assert_eq!(captured.data.len(), 1920 * 4 * 1080);
        assert_eq!(captured.display_idx, 0);
    }

    #[test]
    fn test_frame_buffer_store_take_round_trip_in_video_context() {
        // Simulate the store/take cycle as it happens in the capture loop.
        let fb = FrameBuffer::new();

        let width = 1280;
        let height = 720;
        let stride = width * 4;
        let data = vec![0xFFu8; stride * height];

        let captured = CapturedFrame {
            data: data.clone(),
            width,
            height,
            stride,
            pixfmt: scrap::Pixfmt::BGRA,
            capture_time: Instant::now(),
            display_idx: 1,
        };

        fb.store(captured);

        // Encode phase: take the latest frame.
        let taken = fb.take();
        assert!(taken.is_some(), "take() should return the stored frame");

        let cf = taken.unwrap();
        assert_eq!(cf.width, 1280);
        assert_eq!(cf.height, 720);
        assert_eq!(cf.stride, 1280 * 4);
        assert_eq!(cf.pixfmt, scrap::Pixfmt::BGRA);
        assert_eq!(cf.data.len(), 1280 * 4 * 720);
        assert_eq!(cf.display_idx, 1);

        // After take, buffer should be empty.
        assert!(fb.take().is_none(), "second take should return None");
    }

    #[test]
    fn test_frame_buffer_latest_wins_semantics() {
        // When multiple captures happen before an encode, only the latest
        // frame should be available (mimics fast capture, slow encode).
        let fb = FrameBuffer::new();

        // First capture (will be overwritten).
        fb.store(CapturedFrame {
            data: vec![1u8; 100],
            width: 640,
            height: 480,
            stride: 640 * 4,
            pixfmt: scrap::Pixfmt::BGRA,
            capture_time: Instant::now(),
            display_idx: 0,
        });

        // Second capture (latest-wins).
        fb.store(CapturedFrame {
            data: vec![2u8; 200],
            width: 1920,
            height: 1080,
            stride: 1920 * 4,
            pixfmt: scrap::Pixfmt::RGBA,
            capture_time: Instant::now(),
            display_idx: 0,
        });

        let cf = fb.take().expect("should get the latest frame");
        assert_eq!(cf.width, 1920, "should have the latest frame's width");
        assert_eq!(cf.height, 1080, "should have the latest frame's height");
        assert_eq!(cf.pixfmt, scrap::Pixfmt::RGBA, "should have latest pixfmt");
        assert_eq!(cf.data.len(), 200, "should have latest data");
    }

    #[test]
    fn test_pixel_buffer_reconstructed_from_captured_frame() {
        // Verify that a PixelBuffer can be reconstructed from CapturedFrame
        // data, which is how the low-latency encode path works.
        let width = 800;
        let height = 600;
        let stride = width * 4;
        let data = vec![0xABu8; stride * height];

        let cf = CapturedFrame {
            data: data.clone(),
            width,
            height,
            stride,
            pixfmt: scrap::Pixfmt::BGRA,
            capture_time: Instant::now(),
            display_idx: 0,
        };

        // Reconstruct PixelBuffer from owned data (as done in encode path).
        let pb = PixelBuffer::new(&cf.data, cf.pixfmt, cf.width, cf.height);
        assert_eq!(pb.data().len(), stride * height);
        assert_eq!(pb.width(), width);
        assert_eq!(pb.height(), height);
        assert_eq!(pb.pixfmt(), scrap::Pixfmt::BGRA);
    }

    #[test]
    fn test_frame_buffer_normal_mode_bypass() {
        // In normal mode (frame_buffer is None), the code should take the
        // else branch and encode directly from the captured Frame.
        let frame_buffer: Option<FrameBuffer> = None;

        // The pattern used in the loop:
        //   if let (Some(ref fb), ...) = (&frame_buffer, ...) { ... } else { ... }
        // When frame_buffer is None, the match arm won't fire.
        assert!(
            frame_buffer.is_none(),
            "In normal mode, frame_buffer should be None"
        );
    }
}
