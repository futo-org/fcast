use gst::glib::{self, object::ObjectExt};
use gstrswebrtc::signaller::Signallable;

mod imp {
    use fcast_sender_sdk::device::MirroringOfferSink;
    use gst::{glib, prelude::*, subclass::prelude::*};
    use gst_sdp::SDPMessage;
    use gst_webrtc::{WebRTCICEGatheringState, WebRTCSDPType, WebRTCSessionDescription};
    use parking_lot::Mutex;
    use tokio::sync::oneshot;
    use tracing::{debug, error};

    use gstrswebrtc::signaller::{Signallable, SignallableImpl};

    use std::sync::{Arc, LazyLock};

    const SESSION_ID: &str = "mirroring-session";

    struct Settings {
        offer_sink: Option<Arc<MirroringOfferSink>>,
        answer_tx: Option<oneshot::Sender<String>>,
        rt_handle: tokio::runtime::Handle,
    }

    impl Default for Settings {
        fn default() -> Self {
            Self {
                offer_sink: None,
                answer_tx: None,
                rt_handle: tokio::runtime::Handle::try_current().unwrap(),
            }
        }
    }

    #[derive(Default)]
    pub struct FSignaller {
        settings: Mutex<Settings>,
    }

    impl FSignaller {
        pub fn set_offer_sink(&self, sink: Arc<MirroringOfferSink>) {
            self.settings.lock().offer_sink = Some(sink);
        }

        pub fn take_answer_tx(&self) -> Option<oneshot::Sender<String>> {
            self.settings.lock().answer_tx.take()
        }

        pub fn on_webrtcbin_ready(&self) -> gst::glib::RustClosure {
            glib::closure!(|signaller: &super::FSignaller,
                            _session_id: &str,
                            webrtcbin: &gst::Element| {
                webrtcbin.connect_notify(
                    Some("ice-gathering-state"),
                    glib::clone!(
                        #[weak]
                        signaller,
                        move |webrtcbin, _pspec| {
                            let state = webrtcbin
                                .property::<WebRTCICEGatheringState>("ice-gathering-state");

                            match state {
                                WebRTCICEGatheringState::Gathering => {
                                    debug!("ICE gathering started");
                                }
                                WebRTCICEGatheringState::Complete => {
                                    debug!("ICE gathering complete");

                                    let offer_sdp = webrtcbin
                                        .property::<Option<WebRTCSessionDescription>>(
                                            "local-description",
                                        )
                                        .map(|d| d.sdp().as_text().expect("SDP to text"));

                                    let Some(offer) = offer_sdp else {
                                        error!("No local description when ICE complete");
                                        return;
                                    };

                                    let (answer_tx, answer_rx) = oneshot::channel::<String>();
                                    let (offer_sink, rt_handle) = {
                                        let mut settings = signaller.imp().settings.lock();
                                        settings.answer_tx = Some(answer_tx);
                                        let offer_sink = settings.offer_sink.clone();
                                        let rt = settings.rt_handle.clone();
                                        (offer_sink, rt)
                                    };

                                    let Some(offer_sink) = offer_sink else {
                                        error!("No offer sink set on signaller");
                                        return;
                                    };

                                    offer_sink.send_offer(offer);

                                    let signaller_weak = signaller.downgrade();
                                    rt_handle.spawn(async move {
                                        match answer_rx.await {
                                            Ok(answer) => {
                                                let Some(signaller) = signaller_weak.upgrade()
                                                else {
                                                    return;
                                                };
                                                let sdp =
                                                    SDPMessage::parse_buffer(answer.as_bytes())
                                                        .expect("valid answer SDP");
                                                let answer_desc = WebRTCSessionDescription::new(
                                                    WebRTCSDPType::Answer,
                                                    sdp,
                                                );
                                                signaller.emit_by_name::<()>(
                                                    "session-description",
                                                    &[&SESSION_ID, &answer_desc],
                                                );
                                            }
                                            Err(e) => error!(?e, "Answer channel closed"),
                                        }
                                    });
                                }
                                _ => (),
                            }
                        }
                    ),
                );
            })
        }
    }

    impl SignallableImpl for FSignaller {
        fn start(&self) {
            let rt_handle = self.settings.lock().rt_handle.clone();
            let this_weak = self.downgrade();
            rt_handle.spawn(async move {
                let Some(this) = this_weak.upgrade() else {
                    return;
                };
                this.obj()
                    .emit_by_name::<()>("session-started", &[&SESSION_ID, &SESSION_ID]);
                this.obj().emit_by_name::<()>(
                    "session-requested",
                    &[&SESSION_ID, &SESSION_ID, &None::<WebRTCSessionDescription>],
                );
            });
        }
    }

    #[glib::object_subclass]
    impl ObjectSubclass for FSignaller {
        const NAME: &'static str = "FServerSignaller";
        type Type = super::FSignaller;
        type ParentType = glib::Object;
        type Interfaces = (Signallable,);
    }

    impl ObjectImpl for FSignaller {
        fn properties() -> &'static [glib::ParamSpec] {
            static PROPERTIES: LazyLock<Vec<glib::ParamSpec>> = LazyLock::new(|| {
                vec![
                    glib::ParamSpecBoolean::builder("manual-sdp-munging")
                        .nick("Manual SDP munging")
                        .blurb("Whether the signaller manages SDP munging itself")
                        .default_value(false)
                        .read_only()
                        .build(),
                ]
            });
            PROPERTIES.as_ref()
        }

        fn property(&self, _id: usize, pspec: &glib::ParamSpec) -> glib::Value {
            match pspec.name() {
                "manual-sdp-munging" => false.to_value(),
                _ => unimplemented!(),
            }
        }
    }
}

glib::wrapper! {
    pub struct FSignaller(ObjectSubclass<imp::FSignaller>) @implements Signallable;
}

impl fcast_sender_sdk::device::FWRTCSignaller for FSignaller {
    fn set_offer_sink(&self, sink: std::sync::Arc<fcast_sender_sdk::device::MirroringOfferSink>) {
        use gst::subclass::prelude::ObjectSubclassIsExt;
        self.imp().set_offer_sink(sink);
    }

    fn on_answer_received(&self, answer: String) {
        use gst::subclass::prelude::ObjectSubclassIsExt;
        if let Some(tx) = self.imp().take_answer_tx() {
            let _ = tx.send(answer);
        }
    }
}

impl Default for FSignaller {
    fn default() -> Self {
        use gst::subclass::prelude::ObjectSubclassIsExt;
        let sig: FSignaller = glib::Object::new();
        sig.connect_closure("webrtcbin-ready", false, sig.imp().on_webrtcbin_ready());
        sig
    }
}
