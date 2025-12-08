use crate::FetchEvent;
use anyhow::Result;
use ashpd::desktop::{
    PersistMode,
    screencast::{CursorMode, Screencast, SourceType},
};
use mcore::{Event, VideoSource};
use tracing::{debug, error};
use xcb::{
    Xid,
    randr::{GetCrtcInfo, GetOutputInfo, GetScreenResources},
};

#[derive(Default)]
struct TargetIdGenerator(usize);

impl TargetIdGenerator {
    pub fn next(&mut self) -> usize {
        self.0 += 1;
        self.0 - 1
    }
}

fn get_x11_targets(conn: &xcb::Connection) -> Result<Vec<(usize, VideoSource)>> {
    let setup = conn.get_setup();
    let screens = setup.roots();

    let mut targets = Vec::new();
    let mut target_id_gen = TargetIdGenerator::default();
    for screen in screens {
        let resources = conn.send_request(&GetScreenResources {
            window: screen.root(),
        });
        let resources = conn.wait_for_reply(resources)?;
        for output in resources.outputs() {
            let info = conn.send_request(&GetOutputInfo {
                output: *output,
                config_timestamp: 0,
            });
            let info = conn.wait_for_reply(info)?;
            if info.connection() == xcb::randr::Connection::Connected {
                let crtc = info.crtc();
                let crtc_info = conn.send_request(&GetCrtcInfo {
                    crtc,
                    config_timestamp: 0,
                });
                let crtc_info = conn.wait_for_reply(crtc_info)?;
                let title = String::from_utf8(info.name().to_vec()).unwrap_or(String::from("n/a"));
                targets.push((
                    target_id_gen.next(),
                    VideoSource::XDisplay {
                        name: title,
                        id: screen.root().resource_id(),
                        width: crtc_info.width(),
                        height: crtc_info.height(),
                        x_offset: crtc_info.x(),
                        y_offset: crtc_info.y(),
                    },
                ));
            }
        }
    }

    Ok(targets)
}

pub async fn video_source_fetch_worker(
    mut rx: tokio::sync::mpsc::Receiver<FetchEvent>,
    event_tx: tokio::sync::mpsc::UnboundedSender<Event>,
) {
    let mut _proxy = None;
    let mut _session: Option<ashpd::desktop::Session<'_, ashpd::desktop::screencast::Screencast>> =
        None;
    let mut _stream = None;
    let mut conn = None;
    enum WindowingSystem {
        Wayland,
        X11,
    }

    let winsys = if std::env::var("WAYLAND_DISPLAY").is_ok() {
        WindowingSystem::Wayland
    } else if std::env::var("DISPLAY").is_ok() {
        let xcb_connection = match xcb::Connection::connect(None) {
            Ok(conn) => conn,
            Err(err) => {
                error!(?err, "Failed to connect with XCB");
                event_tx
                    .send(Event::UnsupportedDisplaySystem)
                    .expect("event loop is not running");
                return;
            }
        };
        conn = Some(xcb_connection.0);
        WindowingSystem::X11
    } else {
        event_tx.send(Event::UnsupportedDisplaySystem).unwrap();
        return;
    };

    loop {
        let Some(event) = rx.recv().await else {
            error!("Failed to receive new video source fetcher event");
            break;
        };

        match (event, &winsys) {
            (FetchEvent::Fetch, WindowingSystem::Wayland) => {
                if let Some(old_session) = _session.take() {
                    if let Err(err) = old_session.close().await {
                        error!(?err, "Failed to close old session");
                    }
                }

                let new_proxy = match Screencast::new().await {
                    Ok(proxy) => proxy,
                    Err(err) => {
                        error!(?err, "Failed to create Screencast proxy");
                        continue;
                    }
                };
                let new_session = match new_proxy.create_session().await {
                    Ok(session) => session,
                    Err(err) => {
                        error!(?err, "Failed to create screencast session");
                        continue;
                    }
                };
                let cursor_modes = match new_proxy.available_cursor_modes().await {
                    Ok(modes) => modes,
                    Err(err) => {
                        error!(?err, "Failed to get available cursor modes");
                        continue;
                    }
                };
                debug!(?cursor_modes, "Cursor modes");
                let cursor_mode = if cursor_modes.contains(CursorMode::Embedded) {
                    CursorMode::Embedded
                } else if cursor_modes.contains(CursorMode::Hidden) {
                    CursorMode::Hidden
                } else {
                    CursorMode::Metadata
                };

                let mut source_types = SourceType::Monitor | SourceType::Window;
                if let Ok(true) = new_proxy
                    .available_source_types()
                    .await
                    .map(|types| types.contains(SourceType::Virtual))
                {
                    source_types |= SourceType::Virtual;
                }

                if let Err(err) = new_proxy
                    .select_sources(
                        &new_session,
                        cursor_mode,
                        source_types,
                        false,
                        None,
                        PersistMode::DoNot,
                    )
                    .await
                {
                    error!(?err, "Failed to select source");
                    continue;
                }

                let response = match new_proxy.start(&new_session, None).await {
                    Ok(resp) => resp,
                    Err(err) => {
                        error!(?err, "Failed to start screencast session");
                        continue;
                    }
                };
                let response = match response.response() {
                    Ok(resp) => resp,
                    Err(err) => {
                        error!(?err, "Failed to get response");
                        continue;
                    }
                };

                let Some(stream) = response.streams().first() else {
                    error!("No screencast streams available");
                    continue;
                };
                let stream = stream.to_owned();

                let fd = match new_proxy.open_pipe_wire_remote(&new_session).await {
                    Ok(fd) => fd,
                    Err(err) => {
                        error!(?err, "Failed to open PipeWire remote");
                        continue;
                    }
                };

                event_tx
                    .send(Event::VideosAvailable(vec![(
                        0,
                        VideoSource::PipeWire {
                            node_id: stream.pipe_wire_node_id(),
                            fd,
                        },
                    )]))
                    .expect("event loop is not running");

                _proxy = Some(new_proxy);
                _session = Some(new_session);
                _stream = Some(stream);
            }
            (FetchEvent::Fetch, WindowingSystem::X11) => {
                let Some(xconn) = conn.as_ref() else {
                    error!("No xcb connection available");
                    continue;
                };

                let sources = match get_x11_targets(xconn) {
                    Ok(s) => s,
                    Err(err) => {
                        error!(?err, "Failed to get x11 targets");
                        continue;
                    }
                };
                event_tx
                    .send(Event::VideosAvailable(sources))
                    .expect("event loop is not running");
            }
            (FetchEvent::Quit, _) => break,
        }
    }

    if let Some(session) = _session.take() {
        if let Err(err) = session.close().await {
            error!(?err, "Failed to close screen capture session");
        }
    }

    debug!("Video source fetch loop quit");
}
