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
fn color(rgba: [u8; 4]) -> slint::Color {
    slint::Color::from_argb_u8(rgba[3], rgba[0], rgba[1], rgba[2])
}

/// A missing colour maps to fully transparent (a transparent stroke simply
/// draws nothing, a transparent fill leaves the shape unfilled).
#[cfg(feature = "gui")]
fn brush(rgba: Option<[u8; 4]>) -> slint::Brush {
    slint::Brush::SolidColor(rgba.map_or(slint::Color::from_argb_u8(0, 0, 0, 0), color))
}

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

        // Let graphviz do the layout only (`-Tjson`), never rasterize. The JSON
        // is a flat list of draw ops we render as native Slint elements.
        let mut graphviz = Command::new("dot")
            .arg("-Tjson")
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

        let json = graphviz.wait_with_output().unwrap().stdout;
        let graph = remote_pipeline_dbg::render::parse(&json).unwrap();

        ui_weak
            .upgrade_in_event_loop(move |ui| {
                use slint::ToSharedString;

                let paths: Vec<UiGraphPath> = graph
                    .paths
                    .iter()
                    .map(|p| UiGraphPath {
                        commands: p.commands.as_str().into(),
                        fill: brush(p.fill),
                        stroke: brush(p.stroke),
                        stroke_width: p.stroke_width,
                    })
                    .collect();
                let texts: Vec<UiGraphText> = graph
                    .texts
                    .iter()
                    .map(|t| UiGraphText {
                        x: t.x,
                        y: t.y,
                        size: t.size,
                        text: t.text.as_str().into(),
                        color: color(t.color),
                        align: match t.align {
                            remote_pipeline_dbg::render::TextAlign::Left => 0,
                            remote_pipeline_dbg::render::TextAlign::Center => 1,
                            remote_pipeline_dbg::render::TextAlign::Right => 2,
                        },
                    })
                    .collect();

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
                    width: graph.width,
                    height: graph.height,
                    paths: std::rc::Rc::new(slint::VecModel::from(paths)).into(),
                    texts: std::rc::Rc::new(slint::VecModel::from(texts)).into(),
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
