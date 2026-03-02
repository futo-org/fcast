#[cfg(feature = "gui")]
use slint::Model;
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

        let client_addr = stream
            .peer_addr()
            .map(|s| s.to_string())
            .unwrap_or("n/a".to_owned());

        ui_weak
            .upgrade_in_event_loop(|ui| {
                ui.global::<Bridge>()
                    .set_waiting_state(WaitingState::Receiving);
            })
            .unwrap();

        let now = chrono::Local::now();
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
        assert_eq!(argb.len(), width as usize * height as usize * 4);
        image_swizzle::argb_to_rgba_inplace(argb);
        image_swizzle::rgb0_to_bgrx_inplace(argb);

        let buffer = slint::SharedPixelBuffer::<slint::Rgba8Pixel>::clone_from_slice(
            argb,
            width as u32,
            height as u32,
        );
        ui_weak
            .upgrade_in_event_loop(move |ui| {
                use slint::ToSharedString;

                let img = slint::Image::from_rgba8(buffer);
                let bridge = ui.global::<Bridge>();
                let dumps = bridge.get_dumps();
                let model = dumps
                    .as_any()
                    .downcast_ref::<slint::VecModel<UiGraphDump>>()
                    .unwrap();
                model.push(UiGraphDump {
                    title: now.format("%H:%M:%S").to_shared_string(),
                    client: client_addr.to_shared_string(),
                    pipeline: source.to_string().to_shared_string(),
                    trigger: trigger.to_string().to_shared_string(),
                    graph: img,
                });
                let idx = model.row_count() as i32;
                bridge.set_selected_dump(idx - 1);
                bridge.set_waiting_state(WaitingState::Waiting);
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

    let bridge = ui.global::<Bridge>();
    bridge.set_dumps(std::rc::Rc::new(slint::VecModel::default()).into());

    bridge.on_remove_element(|model: slint::ModelRc<UiGraphDump>, idx: i32| {
        let model = model
            .as_any()
            .downcast_ref::<slint::VecModel<UiGraphDump>>()
            .unwrap();
        assert!(idx >= 0);
        model.remove(idx as usize);
    });

    bridge.on_dump_remote(|addr: slint::SharedString| {
        use std::str::FromStr;

        let Ok(addr) = std::net::SocketAddr::from_str(addr.as_str()) else {
            eprintln!("Invalid address");
            return;
        };

        let Ok(mut stream) = std::net::TcpStream::connect(addr) else {
            eprintln!("Failed to connect");
            return;
        };

        if stream.write_all(&[0xFF]).is_err() {
            eprintln!("Failed to write payload");
        }
    });

    let ui_weak = ui.as_weak();
    std::thread::spawn(move || {
        run(ui_weak);
    });

    ui.run().unwrap();
}
