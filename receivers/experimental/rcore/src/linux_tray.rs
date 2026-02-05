use image::GenericImageView;
use ksni::menu::*;
use tokio::sync::mpsc::UnboundedSender;

use crate::{Event, TrayEvent};

pub struct LinuxSysTray {
    pub event_tx: UnboundedSender<Event>,
}

impl LinuxSysTray {
    fn toggle_window(&self) {
        let _ = self.event_tx.send(Event::Tray(TrayEvent::Toggle));
    }
}

impl ksni::Tray for LinuxSysTray {
    fn id(&self) -> String {
        "FCast Receiver".to_owned()
    }

    fn icon_pixmap(&self) -> Vec<ksni::Icon> {
        let icn = include_bytes!("../../../electron/assets/icons/app/icon.png");
        let img = image::load_from_memory_with_format(icn, image::ImageFormat::Png).unwrap();
        let (width, height) = img.dimensions();
        let mut data = img.into_rgba8().into_vec();
        for pixel in data.chunks_exact_mut(4) {
            pixel.rotate_right(1) // rgba to argb
        }

        vec![ksni::Icon {
            width: width as i32,
            height: height as i32,
            data,
        }]
    }

    fn activate(&mut self, _x: i32, _y: i32) {
        self.toggle_window();
    }

    fn menu(&self) -> Vec<ksni::MenuItem<Self>> {
        vec![
            StandardItem {
                label: "Toggle window".to_owned(),
                activate: Box::new(|this: &mut Self| {
                    this.toggle_window();
                }),
                ..Default::default()
            }
            .into(),
            ksni::MenuItem::Separator,
            StandardItem {
                label: "Quit".to_owned(),
                activate: Box::new(|this: &mut Self| {
                    let _ = this.event_tx.send(Event::Tray(TrayEvent::Quit));
                }),
                ..Default::default()
            }
            .into(),
        ]
    }
}
