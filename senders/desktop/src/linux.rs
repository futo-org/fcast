use crate::FetchEvent;
use anyhow::{Context, Result, bail};
use ashpd::desktop::{
    PersistMode,
    screencast::{CursorMode, Screencast, SourceType},
};
use mcore::{Event, VideoSource};
use std::ffi::{CStr, CString};
use tracing::{debug, error};
use x11::xlib::{XFreeStringList, XGetTextProperty, XTextProperty, XmbTextPropertyToTextList};
use xcb::{
    Xid,
    randr::{GetCrtcInfo, GetOutputInfo, GetScreenResources},
    x::{self, GetPropertyReply},
};

fn get_x11_atom(conn: &xcb::Connection, atom_name: &str) -> Result<x::Atom, xcb::Error> {
    let cookie = conn.send_request(&x::InternAtom {
        only_if_exists: true,
        name: atom_name.as_bytes(),
    });
    Ok(conn.wait_for_reply(cookie)?.atom())
}

fn get_x11_property(
    conn: &xcb::Connection,
    win: x::Window,
    prop: x::Atom,
    typ: x::Atom,
    length: u32,
) -> Result<GetPropertyReply, xcb::Error> {
    let cookie = conn.send_request(&x::GetProperty {
        delete: false,
        window: win,
        property: prop,
        r#type: typ,
        long_offset: 0,
        long_length: length,
    });
    conn.wait_for_reply(cookie)
}

fn decode_x11_compound_text(
    conn: &xcb::Connection,
    value: &[u8],
    client: &xcb::x::Window,
    ttype: xcb::x::Atom,
) -> anyhow::Result<String> {
    let display = conn.get_raw_dpy();
    if display.is_null() {
        bail!("Display is null");
    }

    let c_string = CString::new(value.to_vec())?;
    let mut text_prop = XTextProperty {
        value: std::ptr::null_mut(),
        encoding: 0,
        format: 0,
        nitems: 0,
    };
    let res = unsafe {
        XGetTextProperty(
            display,
            client.resource_id() as u64,
            &mut text_prop,
            x::ATOM_WM_NAME.resource_id() as u64,
        )
    };
    if res == 0 || text_prop.nitems == 0 {
        return Ok(String::from("n/a"));
    }

    let xname = XTextProperty {
        value: c_string.as_ptr() as *mut u8,
        encoding: ttype.resource_id() as u64,
        format: 8,
        nitems: text_prop.nitems,
    };
    let mut list: *mut *mut i8 = std::ptr::null_mut();
    let mut count: i32 = 0;
    let result = unsafe { XmbTextPropertyToTextList(display, &xname, &mut list, &mut count) };
    if result < 1 || list.is_null() || count < 1 {
        Ok(String::from("n/a"))
    } else {
        let title = unsafe { CStr::from_ptr(*list).to_string_lossy().into_owned() };
        unsafe { XFreeStringList(list) };
        Ok(title)
    }
}

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

    let wm_client_list =
        get_x11_atom(conn, "_NET_CLIENT_LIST").context("Failed to get `_NET_CLIENT_LIST`")?;

    let atom_net_wm_name =
        get_x11_atom(conn, "_NET_WM_NAME").context("Failed to get `_NET_WM_NAME`")?;
    let atom_text = get_x11_atom(conn, "TEXT").context("Failed to get `TEXT`")?;
    let atom_utf8_string =
        get_x11_atom(conn, "UTF8_STRING").context("Failed to get `UTF8_STRING`")?;
    let atom_compound_text =
        get_x11_atom(conn, "COMPOUND_TEXT").context("Failed to get `COMPOUND_TEXT`")?;

    let mut targets = Vec::new();
    let mut target_id_gen = TargetIdGenerator::default();
    for screen in screens {
        let window_list = get_x11_property(conn, screen.root(), wm_client_list, x::ATOM_NONE, 100)
            .context("Failed to get window list")?;

        for client in window_list.value::<x::Window>() {
            let cr = get_x11_property(conn, *client, atom_net_wm_name, x::ATOM_STRING, 4096)
                .context("Failed to get client name")?;
            if !cr.value::<x::Atom>().is_empty() {
                targets.push((
                    target_id_gen.next(),
                    VideoSource::XWindow {
                        id: client.resource_id(),
                        name: String::from_utf8(cr.value().to_vec())?,
                    },
                ));
                continue;
            }

            let reply = get_x11_property(conn, *client, x::ATOM_WM_NAME, x::ATOM_ANY, 4096)?;
            let value: &[u8] = reply.value();
            if !value.is_empty() {
                let ttype = reply.r#type();
                let title =
                    if ttype == x::ATOM_STRING || ttype == atom_utf8_string || ttype == atom_text {
                        String::from_utf8(reply.value().to_vec()).unwrap_or(String::from("n/a"))
                    } else if ttype == atom_compound_text {
                        decode_x11_compound_text(conn, value, client, ttype)
                            .unwrap_or("n/a".to_owned())
                    } else {
                        String::from_utf8(reply.value().to_vec()).unwrap_or(String::from("n/a"))
                    };

                targets.push((
                    target_id_gen.next(),
                    VideoSource::XWindow {
                        id: client.resource_id(),
                        name: title,
                    },
                ));
                continue;
            }
            targets.push((
                target_id_gen.next(),
                VideoSource::XWindow {
                    id: client.resource_id(),
                    name: "n/a".to_owned(),
                },
            ));
        }

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
    event_tx: tokio::sync::mpsc::Sender<Event>,
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
                    .await
                    .expect("event loop is not running");
                return;
            }
        };
        conn = Some(xcb_connection.0);
        WindowingSystem::X11
    } else {
        event_tx
            .send(Event::UnsupportedDisplaySystem)
            .await
            .unwrap();
        return;
    };

    loop {
        let Some(event) = rx.recv().await else {
            error!("Failed to receive new video source fetcher event");
            break;
        };

        match (event, &winsys) {
            (FetchEvent::ClearState, WindowingSystem::Wayland) => {
                // Since some genius put a lifetime on the `Screencast` related types, we
                // have to take care of this here because we can't move them around
                if let Some(session) = _session.take()
                    && let Err(err) = session.close().await
                {
                    error!(?err, "Failed to close xdg portal session");
                }
                _proxy = None;
                _stream = None;
            }
            (FetchEvent::ClearState, WindowingSystem::X11) => (),
            (FetchEvent::Fetch, WindowingSystem::Wayland) => {
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
                    .await
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
                    .await
                    .expect("event loop is not running");
            }
            (FetchEvent::Quit, _) => break,
        }
    }

    debug!("Video source fetch loop quit");
}
