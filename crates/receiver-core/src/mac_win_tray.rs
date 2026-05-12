use tracing::error;
use tray_icon::{
    TrayIcon, TrayIconBuilder,
    menu::{Menu, MenuEvent, MenuItem, PredefinedMenuItem},
};

use crate::{MessageSender, message::Tray};

pub struct MenuItemIds {
    pub toggle_window: String,
    pub quit: String,
}

pub fn set_event_handler(msg_tx: MessageSender, ids: MenuItemIds) {
    MenuEvent::set_event_handler(Some(move |event: MenuEvent| {
        let msg = if event.id.0 == ids.toggle_window {
            Tray::Toggle
        } else if event.id.0 == ids.quit {
            Tray::Quit
        } else {
            return;
        };

        msg_tx.tray(msg);
    }));
}

pub fn create_tray_icon() -> (TrayIcon, MenuItemIds) {
    let menu = Menu::new();

    let toggle_window = MenuItem::new("Toggle window", true, None);
    let quit = MenuItem::new("Quit", true, None);

    if let Err(err) = menu.append_items(&[&toggle_window, &PredefinedMenuItem::separator(), &quit])
    {
        error!(?err, "Failed to add items to tray menu");
    }

    let (icon_rgba, icon_width, icon_height) = {
        let image = imagelib::load_from_memory_with_format(
            include_bytes!("../../../electron/assets/icons/app/icon.png"),
            imagelib::ImageFormat::Png,
        )
        .unwrap()
        .into_rgba8();
        let (width, height) = image.dimensions();
        let rgba = image.into_raw();
        (rgba, width, height)
    };
    let icon = tray_icon::Icon::from_rgba(icon_rgba, icon_width, icon_height)
        .expect("Failed to open icon");

    let ids = MenuItemIds {
        toggle_window: toggle_window.id().0.clone(),
        quit: quit.id().0.clone(),
    };

    (
        TrayIconBuilder::new()
            .with_menu(Box::new(menu))
            .with_tooltip("FCast Receiver")
            .with_icon(icon)
            .build()
            .unwrap(),
        ids,
    )
}
