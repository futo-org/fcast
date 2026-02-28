#[cfg(feature = "gui")]
use std::{
    io::Write,
    net::TcpListener,
    process::{Command, Stdio},
};

#[cfg(feature = "gui")]
slint::include_modules!();

#[cfg(feature = "gui")]
fn run(ui_weak: slint::Weak<MainWindow>) {
    let listener = TcpListener::bind("0.0.0.0:3000").unwrap();
    for stream in listener.incoming() {
        let stream = match stream {
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

        let (data_buf, source, trigger) = remote_pipeline_dbg::read_graph(stream).unwrap();

        let mut graphviz = Command::new("dot")
            .arg("-Tgd")
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

        let mut image_data = graphviz.wait_with_output().unwrap().stdout;

        let width = u16::from_be_bytes([image_data[2], image_data[3]]);
        let height = u16::from_be_bytes([image_data[4], image_data[5]]);
        let argb = &mut image_data[11..];
        println!("Got image width={width} height={height}");
        assert_eq!(argb.len(), width as usize * height as usize * 4);
        image_swizzle::argb_to_rgba_inplace(argb);
        image_swizzle::rgb0_to_bgrx_inplace(argb);

        let buffer = slint::SharedPixelBuffer::<slint::Rgba8Pixel>::clone_from_slice(
            argb,
            width as u32,
            height as u32,
        );
        ui_weak
            .upgrade_in_event_loop(|ui| {
                let img = slint::Image::from_rgba8(buffer);
                ui.global::<Bridge>().set_graph(img);
                ui.global::<Bridge>()
                    .set_waiting_state(WaitingState::Waiting);
            })
            .unwrap();
    }
}

#[cfg(feature = "gui")]
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
