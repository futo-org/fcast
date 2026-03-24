use crate::{Bridge, Event, Operation, SetVolumeMessage, UiMediaTrackType, log_if_err};
use fcast_protocol::v3;
use slint::{ComponentHandle, ToSharedString};
use tokio::sync::mpsc::UnboundedSender;

pub enum GuiCommand {}

pub fn register_callbacks(
    ui: &crate::MainWindow,
    bridge: &Bridge,
    event_tx: UnboundedSender<Event>,
) {
    bridge.on_resume_or_pause({
        let event_tx = event_tx.clone();
        move || {
            log_if_err!(event_tx.send(Event::ResumeOrPause));
        }
    });

    bridge.on_seek_to_percent({
        let event_tx = event_tx.clone();
        move |percent| {
            log_if_err!(event_tx.send(Event::SeekPercent(percent)));
        }
    });

    bridge.on_toggle_fullscreen({
        let ui_weak = ui.as_weak();
        move || {
            let ui = ui_weak
                .upgrade()
                .expect("callbacks always get called from the event loop");
            let is_fullscreen = !ui.window().is_fullscreen();
            ui.window().set_fullscreen(is_fullscreen);
            ui.global::<Bridge>().set_is_fullscreen(is_fullscreen);
        }
    });

    bridge.on_set_volume({
        let event_tx = event_tx.clone();
        move |volume| {
            log_if_err!(event_tx.send(Event::Op {
                session_id: 0,
                op: Operation::SetVolume(SetVolumeMessage {
                    volume: volume as f64,
                })
            }));
        }
    });

    bridge.on_force_quit(move || {
        log_if_err!(slint::quit_event_loop());
    });

    bridge.on_debug_toggled({
        let event_tx = event_tx.clone();
        move || {
            log_if_err!(event_tx.send(Event::ToggleDebug));
        }
    });

    bridge.on_change_playback_rate({
        let event_tx = event_tx.clone();
        move |new_rate: f32| {
            log_if_err!(event_tx.send(Event::Op {
                session_id: 0,
                op: Operation::SetSpeed(fcast_protocol::SetSpeedMessage {
                    speed: new_rate as f64
                }),
            }));
        }
    });

    bridge.on_hide_cursor_hack({
        let ui_weak = ui.as_weak();
        move || {
            let ui = ui_weak
                .upgrade()
                .expect("callbacks are always called from the event loop");
            let _ = ui
                .window()
                .try_dispatch_event(slint::platform::WindowEvent::PointerReleased {
                    position: slint::LogicalPosition::new(0.0, 0.0),
                    button: slint::platform::PointerEventButton::Other,
                });
        }
    });

    bridge.on_select_track({
        let event_tx = event_tx.clone();
        move |id: i32, variant: UiMediaTrackType| {
            log_if_err!(event_tx.send(Event::SelectTrack { id, variant }));
        }
    });

    bridge.on_select_playlist_item({
        let event_tx = event_tx.clone();
        move |idx: i32| {
            log_if_err!(event_tx.send(Event::Op {
                session_id: 0,
                op: Operation::SetPlaylistItem(v3::SetPlaylistItemMessage {
                    item_index: idx as u64
                }),
            }));
        }
    });

    bridge.on_sec_to_string(|sec: i32| -> slint::SharedString {
        crate::sec_to_string(sec as f64).to_shared_string()
    });
}
