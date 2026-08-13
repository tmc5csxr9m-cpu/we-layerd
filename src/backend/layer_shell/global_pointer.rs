use std::{
    fs::{self, OpenOptions},
    future::Future,
    io::{Cursor, Write},
    mem,
    os::{
        fd::OwnedFd,
        unix::fs::{OpenOptionsExt, PermissionsExt},
    },
    path::{Path, PathBuf},
    sync::{
        atomic::{AtomicBool, AtomicU64, Ordering},
        mpsc, Arc,
    },
    thread::{self, JoinHandle},
    time::{Duration, Instant},
};

use anyhow::{anyhow, Context, Result};
use ashpd::desktop::{
    screencast::{CursorMode, Screencast, SelectSourcesOptions, SourceType},
    PersistMode, Session,
};
use pipewire as pw;
use pw::{properties::properties, spa};
use tokio::sync::watch;

const METADATA_START_TIMEOUT: Duration = Duration::from_secs(5);
const STOP_CHECK_INTERVAL: Duration = Duration::from_millis(50);
// Values from drm_fourcc.h. The cursor metadata consumer never maps the
// accompanying image planes, but compositors still require a compatible DRM
// modifier before PipeWire can link the stream.
const DRM_FORMAT_MOD_LINEAR: i64 = 0;
const DRM_FORMAT_MOD_INVALID: i64 = 0x00ff_ffff_ffff_ffff;
const CURSOR_BITMAP_DEFAULT_SIDE: usize = 384;
const CURSOR_BITMAP_MAX_SIDE: usize = 1024;
const CURSOR_BITMAP_BYTES_PER_PIXEL: usize = 4;
const SCREENCAST_PERSISTENCE_VERSION: u32 = 4;
static RESTORE_TOKEN_TEMP_SEQUENCE: AtomicU64 = AtomicU64::new(1);

#[derive(Debug)]
pub(super) enum GlobalPointerEvent {
    Position { normalized_x: f64, normalized_y: f64 },
    Unavailable(String),
}

pub(super) struct GlobalPointerTracker {
    events: mpsc::Receiver<GlobalPointerEvent>,
    stop_tx: watch::Sender<bool>,
    stop: Arc<AtomicBool>,
    worker: Option<JoinHandle<()>>,
}

impl GlobalPointerTracker {
    pub(super) fn start(output_name: &str) -> std::io::Result<Self> {
        let (event_tx, events) = mpsc::channel();
        let (stop_tx, stop_rx) = watch::channel(false);
        let stop = Arc::new(AtomicBool::new(false));
        let worker_stop = Arc::clone(&stop);
        let restore_token_path = match restore_token_path(output_name) {
            Ok(path) => Some(path),
            Err(error) => {
                tracing::warn!(%error, "ScreenCast permission will not persist across restarts");
                None
            }
        };
        let worker = thread::Builder::new().name("we-layerd-global-pointer".to_string()).spawn(
            move || worker_main(event_tx, stop_rx, worker_stop, restore_token_path.as_deref()),
        )?;

        Ok(Self { events, stop_tx, stop, worker: Some(worker) })
    }

    pub(super) fn try_recv(&self) -> Option<GlobalPointerEvent> {
        self.events.try_recv().ok()
    }
}

impl Drop for GlobalPointerTracker {
    fn drop(&mut self) {
        self.stop.store(true, Ordering::Release);
        let _ = self.stop_tx.send(true);

        // Portal UI belongs to another process and may take time to close. Never
        // make daemon shutdown wait without a bound; the stop signals above let
        // the worker close its portal session or PipeWire loop asynchronously.
        if self.worker.as_ref().is_some_and(|worker| worker.is_finished()) {
            if let Some(worker) = self.worker.take() {
                let _ = worker.join();
            }
        }
    }
}

struct PortalStream {
    portal: Screencast,
    session: Session<Screencast>,
    node_id: u32,
    fd: OwnedFd,
}

enum PortalOpen {
    Ready(PortalStream),
    Stopped,
}

fn worker_main(
    event_tx: mpsc::Sender<GlobalPointerEvent>,
    stop_rx: watch::Receiver<bool>,
    stop: Arc<AtomicBool>,
    restore_token_path: Option<&Path>,
) {
    let runtime = match tokio::runtime::Builder::new_current_thread().enable_all().build() {
        Ok(runtime) => runtime,
        Err(error) => {
            send_unavailable(&event_tx, format!("failed to create portal runtime: {error}"));
            return;
        }
    };

    let portal_stream = match runtime.block_on(open_portal(stop_rx, restore_token_path)) {
        Ok(PortalOpen::Ready(stream)) => stream,
        Ok(PortalOpen::Stopped) => return,
        Err(error) => {
            send_unavailable(&event_tx, error.to_string());
            return;
        }
    };

    let result = run_pipewire(portal_stream.node_id, portal_stream.fd, event_tx.clone(), stop);
    let _ = runtime.block_on(portal_stream.session.close());
    drop(portal_stream.portal);

    if let Err(error) = result {
        send_unavailable(&event_tx, error.to_string());
    }
}

async fn open_portal(
    mut stop: watch::Receiver<bool>,
    restore_token_path: Option<&Path>,
) -> Result<PortalOpen> {
    if *stop.borrow() {
        return Ok(PortalOpen::Stopped);
    }
    // ashpd::Screencast::new() uses a process-global cached connection. This
    // worker owns a short-lived Tokio runtime, so reusing that connection on a
    // later worker can leave portal calls attached to an already-dropped
    // runtime. Keep the connection local to this worker instead.
    let connection = tokio::select! {
        result = ashpd::zbus::Connection::session() => {
            result.context("failed to connect to the session bus for ScreenCast portal")?
        }
        _ = wait_for_stop(&mut stop) => return Ok(PortalOpen::Stopped),
    };
    let Some(portal) = portal_step(&mut stop, Screencast::with_connection(connection)).await?
    else {
        return Ok(PortalOpen::Stopped);
    };

    let Some(cursor_modes) = portal_step(&mut stop, portal.available_cursor_modes()).await? else {
        return Ok(PortalOpen::Stopped);
    };
    if !cursor_modes.contains(CursorMode::Metadata) {
        return Err(anyhow!(
            "the ScreenCast portal does not advertise cursor metadata; using surface-local pointer input"
        ));
    }

    let Some(source_types) = portal_step(&mut stop, portal.available_source_types()).await? else {
        return Ok(PortalOpen::Stopped);
    };
    if !source_types.contains(SourceType::Monitor) {
        return Err(anyhow!(
            "the ScreenCast portal does not advertise monitor capture; using surface-local pointer input"
        ));
    }

    let portal_version = portal.version();
    let persistence_supported = portal_version >= SCREENCAST_PERSISTENCE_VERSION;
    let restore_token = if persistence_supported {
        restore_token_path.and_then(|path| match load_restore_token(path) {
            Ok(token) => token,
            Err(error) => {
                tracing::warn!(path = %path.display(), %error, "failed to load ScreenCast restore token");
                None
            }
        })
    } else {
        None
    };

    let Some(session) = portal_step(&mut stop, portal.create_session(Default::default())).await?
    else {
        return Ok(PortalOpen::Stopped);
    };

    let mut select_options = SelectSourcesOptions::default()
        .set_cursor_mode(CursorMode::Metadata)
        .set_sources(Some(SourceType::Monitor.into()))
        .set_multiple(false);
    if persistence_supported {
        select_options = select_options
            .set_persist_mode(PersistMode::ExplicitlyRevoked)
            .set_restore_token(restore_token.as_deref());
    }
    let select = portal.select_sources(&session, select_options);
    let select_request = match portal_step(&mut stop, select).await {
        Ok(Some(request)) => request,
        Ok(None) => {
            let _ = session.close().await;
            return Ok(PortalOpen::Stopped);
        }
        Err(error) => {
            let _ = session.close().await;
            return Err(error);
        }
    };
    if let Err(error) = select_request.response() {
        let _ = session.close().await;
        return Err(error)
            .context("monitor selection was cancelled or denied by the ScreenCast portal");
    }

    let start_request =
        match portal_step(&mut stop, portal.start(&session, None, Default::default())).await {
            Ok(Some(request)) => request,
            Ok(None) => {
                let _ = session.close().await;
                return Ok(PortalOpen::Stopped);
            }
            Err(error) => {
                let _ = session.close().await;
                return Err(error);
            }
        };
    let response = match start_request.response() {
        Ok(response) => response,
        Err(error) => {
            let _ = session.close().await;
            return Err(error)
                .context("monitor selection was cancelled or denied by the ScreenCast portal");
        }
    };
    if let Some(path) = restore_token_path.filter(|_| persistence_supported) {
        match response.restore_token() {
            Some(token) => {
                if let Err(error) = store_restore_token(path, token) {
                    tracing::warn!(path = %path.display(), %error, "failed to persist ScreenCast restore token");
                }
            }
            None if restore_token.is_some() => {
                if let Err(error) = remove_restore_token(path) {
                    tracing::warn!(path = %path.display(), %error, "failed to remove stale ScreenCast restore token");
                }
            }
            None => {}
        }
    }
    let Some(stream) = response.streams().first() else {
        let _ = session.close().await;
        return Err(anyhow!("the ScreenCast portal returned no selected monitor stream"));
    };
    // ScreenCast v6 prefers PipeWire serial targeting, but ashpd 0.13.12 does
    // not expose the stream's pipewire-serial property. Its public API only
    // provides this compatibility node id, so use it with the portal-scoped FD.
    let node_id = stream.pipe_wire_node_id();

    let fd =
        match portal_step(&mut stop, portal.open_pipe_wire_remote(&session, Default::default()))
            .await
        {
            Ok(Some(fd)) => fd,
            Ok(None) => {
                let _ = session.close().await;
                return Ok(PortalOpen::Stopped);
            }
            Err(error) => {
                let _ = session.close().await;
                return Err(error);
            }
        };

    tracing::info!(
        portal_version,
        pipewire_node_id = node_id,
        restored = restore_token.is_some(),
        persistent = response.restore_token().is_some(),
        "ScreenCast portal granted one monitor for cursor metadata"
    );
    Ok(PortalOpen::Ready(PortalStream { portal, session, node_id, fd }))
}

fn restore_token_path(output_name: &str) -> Result<PathBuf> {
    let state_home = match std::env::var_os("XDG_STATE_HOME").filter(|value| !value.is_empty()) {
        Some(path) => PathBuf::from(path),
        None => {
            let home = std::env::var_os("HOME")
                .filter(|value| !value.is_empty())
                .ok_or_else(|| anyhow!("HOME and XDG_STATE_HOME are unset"))?;
            PathBuf::from(home).join(".local/state")
        }
    };
    if !state_home.is_absolute() {
        return Err(anyhow!("XDG state directory must be absolute"));
    }
    Ok(restore_token_path_in(&state_home, output_name))
}

fn restore_token_path_in(state_home: &Path, output_name: &str) -> PathBuf {
    const HEX: &[u8; 16] = b"0123456789abcdef";
    let mut encoded = String::with_capacity(output_name.len().saturating_mul(2));
    for byte in output_name.as_bytes() {
        encoded.push(HEX[(byte >> 4) as usize] as char);
        encoded.push(HEX[(byte & 0x0f) as usize] as char);
    }
    if encoded.is_empty() {
        encoded.push_str("default");
    }
    state_home.join("we-layerd/screencast").join(format!("{encoded}.restore-token"))
}

fn load_restore_token(path: &Path) -> Result<Option<String>> {
    match fs::read_to_string(path) {
        Ok(token) if token.is_empty() => Ok(None),
        Ok(token) => Ok(Some(token)),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(None),
        Err(error) => Err(error).with_context(|| format!("failed to read {}", path.display())),
    }
}

fn store_restore_token(path: &Path, token: &str) -> Result<()> {
    if token.is_empty() {
        return Err(anyhow!("portal returned an empty restore token"));
    }
    let parent = path.parent().ok_or_else(|| anyhow!("restore token path has no parent"))?;
    fs::create_dir_all(parent).with_context(|| format!("failed to create {}", parent.display()))?;
    fs::set_permissions(parent, fs::Permissions::from_mode(0o700))
        .with_context(|| format!("failed to protect {}", parent.display()))?;

    let file_name = path.file_name().and_then(|name| name.to_str()).unwrap_or("restore-token");
    let sequence = RESTORE_TOKEN_TEMP_SEQUENCE.fetch_add(1, Ordering::Relaxed);
    let temporary =
        path.with_file_name(format!(".{file_name}.{}.{}.tmp", std::process::id(), sequence));
    let mut file = OpenOptions::new()
        .write(true)
        .create_new(true)
        .mode(0o600)
        .open(&temporary)
        .with_context(|| format!("failed to create {}", temporary.display()))?;
    if let Err(error) = file.write_all(token.as_bytes()).and_then(|_| file.sync_all()) {
        drop(file);
        let _ = fs::remove_file(&temporary);
        return Err(error).with_context(|| format!("failed to write {}", temporary.display()));
    }
    drop(file);
    if let Err(error) = fs::rename(&temporary, path) {
        let _ = fs::remove_file(&temporary);
        return Err(error).with_context(|| format!("failed to replace {}", path.display()));
    }
    fs::set_permissions(path, fs::Permissions::from_mode(0o600))
        .with_context(|| format!("failed to protect {}", path.display()))?;
    if let Ok(directory) = fs::File::open(parent) {
        let _ = directory.sync_all();
    }
    Ok(())
}

fn remove_restore_token(path: &Path) -> Result<()> {
    match fs::remove_file(path) {
        Ok(()) => Ok(()),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(()),
        Err(error) => Err(error).with_context(|| format!("failed to remove {}", path.display())),
    }
}

async fn portal_step<T, F>(stop: &mut watch::Receiver<bool>, future: F) -> Result<Option<T>>
where
    F: Future<Output = ashpd::Result<T>>,
{
    if *stop.borrow() {
        return Ok(None);
    }

    tokio::select! {
        result = future => result.map(Some).map_err(Into::into),
        _ = wait_for_stop(stop) => Ok(None),
    }
}

async fn wait_for_stop(stop: &mut watch::Receiver<bool>) {
    loop {
        if *stop.borrow() || stop.changed().await.is_err() {
            return;
        }
    }
}

struct PipeWireUserData {
    width: u32,
    height: u32,
    event_tx: mpsc::Sender<GlobalPointerEvent>,
}

fn cursor_metadata_param() -> Result<Vec<u8>> {
    let base_size =
        mem::size_of::<spa::sys::spa_meta_cursor>() + mem::size_of::<spa::sys::spa_meta_bitmap>();
    let size_for_side =
        |side: usize| (base_size + side * side * CURSOR_BITMAP_BYTES_PER_PIXEL) as i32;
    let metadata = spa::pod::object!(
        spa::utils::SpaTypes::ObjectParamMeta,
        spa::param::ParamType::Meta,
        spa::pod::Property::new(
            spa::sys::SPA_PARAM_META_type,
            spa::pod::Value::Id(spa::utils::Id(spa::sys::SPA_META_Cursor)),
        ),
        spa::pod::Property::new(
            spa::sys::SPA_PARAM_META_size,
            spa::pod::Value::Choice(spa::pod::ChoiceValue::Int(spa::utils::Choice(
                spa::utils::ChoiceFlags::empty(),
                spa::utils::ChoiceEnum::Range {
                    default: size_for_side(CURSOR_BITMAP_DEFAULT_SIDE),
                    min: size_for_side(1),
                    max: size_for_side(CURSOR_BITMAP_MAX_SIDE),
                },
            ))),
        ),
    );

    Ok(spa::pod::serialize::PodSerializer::serialize(
        Cursor::new(Vec::new()),
        &spa::pod::Value::Object(metadata),
    )
    .context("failed to serialize the PipeWire cursor metadata request")?
    .0
    .into_inner())
}

fn run_pipewire(
    node_id: u32,
    fd: OwnedFd,
    event_tx: mpsc::Sender<GlobalPointerEvent>,
    stop: Arc<AtomicBool>,
) -> Result<()> {
    pw::init();

    let mainloop =
        pw::main_loop::MainLoopRc::new(None).context("failed to create PipeWire main loop")?;
    let context = pw::context::ContextRc::new(&mainloop, None)
        .context("failed to create PipeWire context")?;
    let core = context
        .connect_fd_rc(fd, None)
        .context("failed to connect to the portal PipeWire remote")?;
    let stream = pw::stream::StreamRc::new(
        core,
        "we-layerd-cursor-metadata",
        properties! {
            *pw::keys::MEDIA_TYPE => "Video",
            *pw::keys::MEDIA_CATEGORY => "Capture",
            *pw::keys::MEDIA_ROLE => "Screen",
        },
    )
    .context("failed to create PipeWire cursor metadata stream")?;

    let metadata_seen = Arc::new(AtomicBool::new(false));
    let terminal = Arc::new(AtomicBool::new(false));

    let listener_mainloop = mainloop.clone();
    let listener_terminal = Arc::clone(&terminal);
    let listener_metadata_seen = Arc::clone(&metadata_seen);
    let listener = stream
        .add_local_listener_with_user_data(PipeWireUserData {
            width: 0,
            height: 0,
            event_tx: event_tx.clone(),
        })
        .state_changed(move |_, user_data, old, new| {
            let reason = match new {
                pw::stream::StreamState::Error(error) => {
                    Some(format!("PipeWire cursor stream failed: {error}"))
                }
                pw::stream::StreamState::Unconnected
                    if old != pw::stream::StreamState::Unconnected =>
                {
                    Some("PipeWire cursor stream disconnected".to_string())
                }
                _ => None,
            };
            if let Some(reason) = reason {
                if !listener_terminal.swap(true, Ordering::AcqRel) {
                    send_unavailable(&user_data.event_tx, reason);
                }
                listener_mainloop.quit();
            }
        })
        .param_changed(|stream, user_data, id, param| {
            if id != spa::param::ParamType::Format.as_raw() {
                return;
            }
            let Some(param) = param else {
                return;
            };
            let Ok((media_type, media_subtype)) = spa::param::format_utils::parse_format(param)
            else {
                return;
            };
            if media_type != spa::param::format::MediaType::Video
                || media_subtype != spa::param::format::MediaSubtype::Raw
            {
                return;
            }

            let mut format = spa::param::video::VideoInfoRaw::new();
            if format.parse(param).is_ok() {
                let size = format.size();
                user_data.width = size.width;
                user_data.height = size.height;
            }

            // Cursor metadata is optional buffer storage and must be requested
            // by the input stream after format negotiation. CursorMode::Metadata
            // alone only asks the compositor to make it available.
            let metadata_bytes = match cursor_metadata_param() {
                Ok(bytes) => bytes,
                Err(error) => {
                    tracing::warn!(%error, "failed to build PipeWire cursor metadata request");
                    return;
                }
            };
            let Some(metadata) = spa::pod::Pod::from_bytes(&metadata_bytes) else {
                tracing::warn!("failed to create PipeWire cursor metadata pod");
                return;
            };
            if let Err(error) = stream.update_params(&mut [metadata]) {
                tracing::warn!(%error, "failed to request PipeWire cursor metadata");
            }
        })
        .process(move |stream, user_data| {
            let Some(buffer) = stream.dequeue_buffer() else {
                return;
            };
            let Some(cursor) = buffer.find_meta::<spa::buffer::meta::MetaCursor>() else {
                return;
            };
            listener_metadata_seen.store(true, Ordering::Release);

            if !cursor.is_valid()
                || cursor.id() == 0
                || user_data.width == 0
                || user_data.height == 0
            {
                return;
            }

            let position = cursor.position();
            let Some((normalized_x, normalized_y)) = normalize_cursor_position(
                position.x,
                position.y,
                user_data.width,
                user_data.height,
            ) else {
                return;
            };
            let _ = user_data
                .event_tx
                .send(GlobalPointerEvent::Position { normalized_x, normalized_y });

            // Deliberately do not call Buffer::datas_mut(): the image planes
            // are never mapped, inspected, copied, or saved by we-layerd.
        })
        .register()
        .context("failed to register PipeWire stream listener")?;

    let timer_mainloop = mainloop.clone();
    let timer_terminal = Arc::clone(&terminal);
    let timer_metadata_seen = Arc::clone(&metadata_seen);
    let timer_event_tx = event_tx.clone();
    let started_at = Instant::now();
    let timer = mainloop.loop_().add_timer(move |_| {
        if stop.load(Ordering::Acquire) {
            timer_mainloop.quit();
            return;
        }

        if !timer_metadata_seen.load(Ordering::Acquire)
            && started_at.elapsed() >= METADATA_START_TIMEOUT
        {
            if !timer_terminal.swap(true, Ordering::AcqRel) {
                send_unavailable(
                    &timer_event_tx,
                    "the PipeWire stream exposed no cursor metadata; using surface-local pointer input"
                        .to_string(),
                );
            }
            timer_mainloop.quit();
        }
    });
    timer
        .update_timer(Some(STOP_CHECK_INTERVAL), Some(STOP_CHECK_INTERVAL))
        .into_result()
        .context("failed to arm PipeWire stop timer")?;

    let format = spa::pod::object!(
        spa::utils::SpaTypes::ObjectParamFormat,
        spa::param::ParamType::EnumFormat,
        spa::pod::property!(
            spa::param::format::FormatProperties::MediaType,
            Id,
            spa::param::format::MediaType::Video
        ),
        spa::pod::property!(
            spa::param::format::FormatProperties::MediaSubtype,
            Id,
            spa::param::format::MediaSubtype::Raw
        ),
        spa::pod::property!(
            spa::param::format::FormatProperties::VideoFormat,
            Choice,
            Enum,
            Id,
            spa::param::video::VideoFormat::BGRx,
            spa::param::video::VideoFormat::BGRx,
            spa::param::video::VideoFormat::BGRA,
            spa::param::video::VideoFormat::RGBx,
            spa::param::video::VideoFormat::RGBA,
            spa::param::video::VideoFormat::RGB,
            spa::param::video::VideoFormat::BGR,
            spa::param::video::VideoFormat::NV12,
            spa::param::video::VideoFormat::I420,
        ),
        spa::pod::property!(
            spa::param::format::FormatProperties::VideoModifier,
            spa::pod::Value::Choice(spa::pod::ChoiceValue::Long(spa::utils::Choice(
                spa::utils::ChoiceFlags::empty(),
                spa::utils::ChoiceEnum::Enum {
                    default: DRM_FORMAT_MOD_INVALID,
                    alternatives: vec![DRM_FORMAT_MOD_INVALID, DRM_FORMAT_MOD_LINEAR],
                },
            )))
        ),
        spa::pod::property!(
            spa::param::format::FormatProperties::VideoSize,
            Choice,
            Range,
            Rectangle,
            spa::utils::Rectangle { width: 1920, height: 1080 },
            spa::utils::Rectangle { width: 1, height: 1 },
            spa::utils::Rectangle { width: 16384, height: 16384 }
        ),
        spa::pod::property!(
            spa::param::format::FormatProperties::VideoFramerate,
            Choice,
            Range,
            Fraction,
            spa::utils::Fraction { num: 60, denom: 1 },
            spa::utils::Fraction { num: 0, denom: 1 },
            spa::utils::Fraction { num: 240, denom: 1 }
        ),
    );
    let bytes = spa::pod::serialize::PodSerializer::serialize(
        Cursor::new(Vec::new()),
        &spa::pod::Value::Object(format),
    )
    .context("failed to serialize PipeWire video format")?
    .0
    .into_inner();
    let pod = spa::pod::Pod::from_bytes(&bytes)
        .ok_or_else(|| anyhow!("failed to create PipeWire video format pod"))?;
    let mut params = [pod];

    stream
        .connect(
            spa::utils::Direction::Input,
            Some(node_id),
            pw::stream::StreamFlags::AUTOCONNECT,
            &mut params,
        )
        .context("failed to connect to the selected PipeWire monitor stream")?;

    tracing::info!(
        pipewire_node_id = node_id,
        "reading PipeWire cursor metadata without mapping image planes"
    );
    mainloop.run();

    drop(timer);
    drop(listener);
    Ok(())
}

fn send_unavailable(event_tx: &mpsc::Sender<GlobalPointerEvent>, reason: String) {
    let _ = event_tx.send(GlobalPointerEvent::Unavailable(reason));
}

fn normalize_cursor_position(x: i32, y: i32, width: u32, height: u32) -> Option<(f64, f64)> {
    if width == 0 || height == 0 {
        return None;
    }
    Some(((x as f64 / width as f64).clamp(0.0, 1.0), (y as f64 / height as f64).clamp(0.0, 1.0)))
}

#[cfg(test)]
mod tests {
    use std::{fs, os::unix::fs::PermissionsExt, path::PathBuf};

    use super::{
        load_restore_token, normalize_cursor_position, remove_restore_token, restore_token_path_in,
        store_restore_token,
    };

    fn unique_state_home(label: &str) -> PathBuf {
        std::env::temp_dir().join(format!(
            "we-layerd-{label}-{}-{}",
            std::process::id(),
            super::RESTORE_TOKEN_TEMP_SEQUENCE.fetch_add(1, std::sync::atomic::Ordering::Relaxed)
        ))
    }

    #[test]
    fn cursor_metadata_uses_the_negotiated_video_size() {
        assert_eq!(normalize_cursor_position(960, 540, 1920, 1080), Some((0.5, 0.5)));
        assert_eq!(normalize_cursor_position(-1, 1200, 1920, 1080), Some((0.0, 1.0)));
        assert_eq!(normalize_cursor_position(1, 1, 0, 1080), None);
    }

    #[test]
    fn restore_token_paths_are_stable_and_output_scoped() {
        let state_home = PathBuf::from("/tmp/we-layerd-state");
        assert_eq!(
            restore_token_path_in(&state_home, "DP-3"),
            state_home.join("we-layerd/screencast/44502d33.restore-token")
        );
        assert_ne!(
            restore_token_path_in(&state_home, "DP-3"),
            restore_token_path_in(&state_home, "HDMI-A-1")
        );
    }

    #[test]
    fn restore_token_is_replaced_atomically_with_private_permissions() {
        let state_home = unique_state_home("restore-token");
        let path = restore_token_path_in(&state_home, "DP-3");

        assert_eq!(load_restore_token(&path).expect("missing token"), None);
        store_restore_token(&path, "first").expect("store first token");
        store_restore_token(&path, "second").expect("replace token");
        assert_eq!(load_restore_token(&path).expect("load token").as_deref(), Some("second"));
        assert_eq!(
            fs::metadata(path.parent().expect("token parent"))
                .expect("parent metadata")
                .permissions()
                .mode()
                & 0o777,
            0o700
        );
        assert_eq!(
            fs::metadata(&path).expect("token metadata").permissions().mode() & 0o777,
            0o600
        );

        remove_restore_token(&path).expect("remove token");
        remove_restore_token(&path).expect("remove missing token");
        fs::remove_dir_all(&state_home).expect("remove state fixture");
    }
}
