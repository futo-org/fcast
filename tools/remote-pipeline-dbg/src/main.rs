use std::{
    io::{Read, Write},
    net::TcpListener,
    process::{Command, Stdio},
};

slint::include_modules!();

fn run(ui_weak: slint::Weak<MainWindow>) {
    let listener = TcpListener::bind("0.0.0.0:3000").unwrap();
    let mut data_buf: Vec<u8> = Vec::new();
    for stream in listener.incoming() {
        let mut stream = match stream {
            Ok(s) => s,
            Err(err) => {
                eprintln!("Failed to accept stream: {err:?}");
                continue;
            }
        };

        ui_weak
            .upgrade_in_event_loop(|ui| {
                ui.global::<Bridge>()
                    .set_waiting_state(WaitingState::Receiving);
                ui.global::<Bridge>()
                    .set_graph(slint::Image::default());
            })
            .unwrap();

        let mut length_buf = [0u8; 4];
        stream.read_exact(&mut length_buf).unwrap();
        let data_len = u32::from_le_bytes(length_buf) as usize;
        data_buf.resize(data_len, 0);
        stream.read_exact(&mut data_buf).unwrap();

        let mut graphviz = Command::new("dot")
            // TODO: decoding the png is super slow, need to find a different lossless format graphviz can output
            .arg("-Tpng")
            .stdout(Stdio::piped())
            .stdin(Stdio::piped())
            .spawn()
            .unwrap();

        graphviz.stdin.take().unwrap().write_all(&data_buf).unwrap();

        ui_weak
            .upgrade_in_event_loop(|ui| {
                ui.global::<Bridge>()
                    .set_waiting_state(WaitingState::Decoding);
            })
            .unwrap();

        let png = graphviz.wait_with_output().unwrap().stdout;
        let img = image::load_from_memory_with_format(&png, image::ImageFormat::Png)
            .unwrap()
            .into_rgb8();
        let buffer = slint::SharedPixelBuffer::<slint::Rgb8Pixel>::clone_from_slice(
            img.as_raw(),
            img.width(),
            img.height(),
        );
        ui_weak
            .upgrade_in_event_loop(|ui| {
                let img = slint::Image::from_rgb8(buffer);
                ui.global::<Bridge>().set_graph(img);
                ui.global::<Bridge>()
                    .set_waiting_state(WaitingState::Waiting);
            })
            .unwrap();
    }
}

fn main() {
    slint::BackendSelector::new()
        .backend_name("skia".to_owned())
        .select()
        .unwrap();

    let ui = MainWindow::new().unwrap();

    let ui_weak = ui.as_weak();
    std::thread::spawn(move || {
        run(ui_weak);
    });

    ui.run().unwrap();
}
