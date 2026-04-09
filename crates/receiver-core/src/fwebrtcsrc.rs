use gst::{
    glib::{self, subclass::prelude::*},
    prelude::*,
};
use gstrswebrtc::signaller::Signallable;

#[derive(Clone, glib::Boxed)]
#[boxed_type(name = "SignallingChannel")]
pub struct SignallingChannel {
    pub tx: tokio::sync::mpsc::UnboundedSender<crate::fcast::InternalMessage>,
    pub offer_rx: crate::fcast::MirroringOfferRx,
}

impl std::fmt::Debug for SignallingChannel {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("SignallingChannel").finish()
    }
}

mod sig_imp {
    use std::sync::LazyLock;

    use super::SignallingChannel;
    use gst::{
        glib::{self, RustClosure},
        prelude::*,
        subclass::prelude::*,
    };
    use gst_webrtc::*;
    use gstrswebrtc::signaller::{Signallable, SignallableImpl};
    use parking_lot::Mutex;

    use crate::RUNTIME;

    static CAT: LazyLock<gst::DebugCategory> = LazyLock::new(|| {
        gst::DebugCategory::new(
            "fsignaller",
            gst::DebugColorFlags::empty(),
            Some("FCast WebRTC signaller"),
        )
    });

    const CLIENT_OFFER: &str = "client-offer";

    #[derive(Debug, Default, Clone)]
    enum State {
        #[default]
        Stopped,
        Negotiating,
    }

    #[derive(Default)]
    struct Settings {
        channel: Option<SignallingChannel>,
    }

    #[derive(Default)]
    pub struct FSignaller {
        state: Mutex<State>,
        settings: Mutex<Settings>,
    }

    impl FSignaller {
        async fn send_answer(&self, webrtcbin: gst::Element) {
            let local_desc =
                webrtcbin.property::<Option<WebRTCSessionDescription>>("local-description");

            let answer_sdp = match local_desc {
                None => {
                    gst::error!(CAT, imp = self, "No local description when ICE complete");
                    return;
                }
                Some(desc) => desc.sdp().as_text().expect("SDP to text"),
            };

            gst::debug!(CAT, imp = self, "Sending answer SDP");

            let settings = self.settings.lock();
            if let Some(chan) = settings.channel.as_ref() {
                if let Err(e) = chan
                    .tx
                    .send(crate::fcast::InternalMessage::Answer { sdp: answer_sdp })
                {
                    gst::error!(CAT, imp = self, "Failed to send answer: {e:?}");
                }
            } else {
                gst::error!(CAT, imp = self, "No signalling channel when ICE complete");
            }
        }

        async fn on_ice_gathering_complete(&self, webrtcbin: gst::Element) {
            let state = self.state.lock().clone();

            // Only send the answer once the session is actively negotiating, so a
            // terminated session doesn't emit a stale answer.
            match state {
                State::Negotiating => self.send_answer(webrtcbin).await,
                _ => {}
            }
        }

        pub fn on_webrtcbin_ready(&self) -> RustClosure {
            glib::closure!(|signaller: &super::FSignaller,
                            _consumer_identifier: &str,
                            webrtcbin: &gst::Element| {
                tracing::debug!("## Webrtcbin ready");
                let _webrtcbin_weak = webrtcbin.downgrade();
                let _sig = signaller.downgrade();
                webrtcbin.connect_notify(
                    Some("ice-gathering-state"),
                    glib::clone!(
                        #[weak]
                        signaller,
                        move |webrtcbin, _pspec| {
                            let state = webrtcbin
                                .property::<WebRTCICEGatheringState>("ice-gathering-state");
                            gst::debug!(
                                CAT,
                                obj = signaller,
                                "ICE gathering state changed state={state:?}"
                            );

                            match state {
                                WebRTCICEGatheringState::Gathering => {
                                    gst::info!(CAT, obj = signaller, "ICE gathering started");
                                }
                                WebRTCICEGatheringState::Complete => {
                                    gst::info!(CAT, obj = signaller, "ICE gathering complete");

                                    let webrtcbin = webrtcbin.clone();

                                    RUNTIME.spawn(async move {
                                        signaller.imp().on_ice_gathering_complete(webrtcbin).await
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
            *self.state.lock() = State::Negotiating;

            let this_weak = self.downgrade();

            crate::RUNTIME.spawn(async move {
                let Some(this) = this_weak.upgrade() else {
                    return;
                };

                this.obj()
                    .emit_by_name::<()>("session-started", &[&CLIENT_OFFER, &CLIENT_OFFER]);

                // Wait for the sender's SDP offer before emitting session-requested
                let offer_rx = {
                    let settings = this.settings.lock();
                    let Some(chan) = settings.channel.as_ref() else {
                        gst::error!(CAT, imp = this, "No signalling channel when start() called");
                        return;
                    };
                    chan.offer_rx.clone()
                };

                let mut taken_rx = offer_rx.0.lock().take();
                let offer = match taken_rx {
                    Some(ref mut rx) => rx.recv().await,
                    None => {
                        gst::error!(CAT, imp = this, "offer_rx already taken");
                        return;
                    }
                };

                let Some(offer) = offer else {
                    gst::error!(
                        CAT,
                        imp = this,
                        "Offer channel closed before offer received"
                    );
                    return;
                };

                let offer_sdp = match gst_sdp::SDPMessage::parse_buffer(offer.as_bytes()) {
                    Ok(sdp) => sdp,
                    Err(e) => {
                        gst::error!(CAT, imp = this, "Failed to parse offer SDP: {e:?}");
                        return;
                    }
                };

                let offer_desc = gst_webrtc::WebRTCSessionDescription::new(
                    gst_webrtc::WebRTCSDPType::Offer,
                    offer_sdp,
                );

                this.obj()
                    .emit_by_name::<()>("session-description", &[&CLIENT_OFFER, &offer_desc]);
            });
        }

        fn stop(&self) {
            gst::info!(CAT, imp = self, "Stopping");
        }

        fn end_session(&self, _session_id: &str) {}
    }

    impl ObjectImpl for FSignaller {
        fn properties() -> &'static [glib::ParamSpec] {
            static PROPS: LazyLock<Vec<glib::ParamSpec>> = LazyLock::new(|| {
                vec![
                    glib::ParamSpecBoolean::builder("manual-sdp-munging")
                        .nick("Manual SDP munging")
                        .blurb("Whether the signaller manages SDP munging itself")
                        .default_value(false)
                        .read_only()
                        .build(),
                    glib::ParamSpecBoxed::builder::<SignallingChannel>("signalling-channel")
                        .nick("Signalling channel")
                        .write_only()
                        .build(),
                ]
            });

            PROPS.as_ref()
        }

        fn set_property(&self, _id: usize, value: &glib::Value, pspec: &glib::ParamSpec) {
            match pspec.name() {
                "signalling-channel" => {
                    let chan = value
                        .get::<SignallingChannel>()
                        .expect("type checked upstream");
                    self.settings.lock().channel = Some(chan);
                }
                _ => unimplemented!(),
            }
        }

        fn property(&self, _id: usize, pspec: &glib::ParamSpec) -> glib::Value {
            match pspec.name() {
                "manual-sdp-munging" => false.to_value(),
                _ => unimplemented!(),
            }
        }
    }

    #[glib::object_subclass]
    impl ObjectSubclass for FSignaller {
        const NAME: &'static str = "FSignaller";
        type Type = super::FSignaller;
        type ParentType = glib::Object;
        type Interfaces = (Signallable,);
    }
}

mod imp {
    use super::SignallingChannel;
    use gst::{glib, prelude::*, subclass::prelude::*};
    use gst_webrtc::*;
    use gstrswebrtc::webrtcsrc::BaseWebRTCSrc;
    use parking_lot::Mutex;
    use std::sync::LazyLock;

    static CAT: LazyLock<gst::DebugCategory> = LazyLock::new(|| {
        gst::DebugCategory::new(
            "FWebrtcSrc",
            gst::DebugColorFlags::empty(),
            Some("FCast WebRTC source"),
        )
    });

    #[derive(Default)]
    struct Settings {
        uri: Option<String>,
    }

    #[derive(Default)]
    pub struct FWebRTCSrc {
        settings: Mutex<Settings>,
        signaller: Mutex<Option<super::FSignaller>>,
    }

    impl ObjectImpl for FWebRTCSrc {
        fn constructed(&self) {
            gst::debug!(CAT, imp = self, "FWebRTCSrc constructed");

            self.parent_constructed();
            let obj = &*self.obj();

            let signaller = super::FSignaller::default();

            // TODO: error handling
            obj.upcast_ref::<BaseWebRTCSrc>()
                .imp()
                .set_signaller(signaller.clone().upcast())
                .unwrap();
            *self.signaller.lock() = Some(signaller);

            obj.set_property("video-codecs", gst::Array::new(["VP8"]));
            obj.set_property("stun-server", "");

            obj.set_suppressed_flags(gst::ElementFlags::SINK | gst::ElementFlags::SOURCE);
            obj.set_element_flags(gst::ElementFlags::SOURCE);
        }

        fn properties() -> &'static [glib::ParamSpec] {
            static PROPS: LazyLock<Vec<glib::ParamSpec>> = LazyLock::new(|| {
                vec![
                    glib::ParamSpecBoxed::builder::<SignallingChannel>("signalling-channel")
                        .nick("Signalling channel")
                        .write_only()
                        .build(),
                ]
            });

            PROPS.as_ref()
        }

        fn set_property(&self, _id: usize, value: &glib::Value, pspec: &glib::ParamSpec) {
            match pspec.name() {
                "signalling-channel" => {
                    let chan = value
                        .get::<SignallingChannel>()
                        .expect("type checked upstream");
                    if let Some(sig) = self.signaller.lock().as_ref() {
                        sig.set_property("signalling-channel", chan);
                    }
                }
                _ => unimplemented!(),
            }
        }
    }

    impl GstObjectImpl for FWebRTCSrc {}

    impl ElementImpl for FWebRTCSrc {}

    impl URIHandlerImpl for FWebRTCSrc {
        const URI_TYPE: gst::URIType = gst::URIType::Src;

        fn protocols() -> &'static [&'static str] {
            &["fwebrtc"]
        }

        fn uri(&self) -> Option<String> {
            self.settings.lock().uri.clone()
        }

        fn set_uri(&self, uri: &str) -> Result<(), glib::Error> {
            self.settings.lock().uri = Some(uri.to_owned());
            Ok(())
        }
    }

    impl gstrswebrtc::webrtcsrc::BaseWebRTCSrcImpl for FWebRTCSrc {}

    impl BinImpl for FWebRTCSrc {}

    #[glib::object_subclass]
    impl ObjectSubclass for FWebRTCSrc {
        const NAME: &'static str = "FWebRTCSrc";
        type Type = super::FWebRTCSrc;
        type ParentType = BaseWebRTCSrc;
        type Interfaces = (gst::URIHandler,);
    }
}

glib::wrapper! {
    pub struct FWebRTCSrc(ObjectSubclass<imp::FWebRTCSrc>)
        @extends gstrswebrtc::webrtcsrc::BaseWebRTCSrc, gst::Bin, gst::Element, gst::Object,
        @implements gst::URIHandler;
}

glib::wrapper! {
    pub struct FSignaller(ObjectSubclass<sig_imp::FSignaller>) @implements Signallable;
}

impl Default for FSignaller {
    fn default() -> Self {
        let sig: FSignaller = glib::Object::new();
        sig.connect_closure("webrtcbin-ready", false, sig.imp().on_webrtcbin_ready());
        tracing::debug!("Added `webrtcbin-ready` callback handler");
        sig
    }
}

pub fn plugin_init() -> Result<(), glib::BoolError> {
    gst::Element::register(
        None,
        "fwebrtcsrc",
        gst::Rank::PRIMARY,
        FWebRTCSrc::static_type(),
    )
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::fcast::{InternalMessage, MirroringOfferRx};
    use gstrswebrtc::signaller::SignallableExt;
    use std::{
        sync::{Arc, Mutex, mpsc},
        time::Duration,
    };

    fn init() {
        use std::sync::Once;
        static INIT: Once = Once::new();

        INIT.call_once(|| {
            gst::init().unwrap();
        });
    }

    const OFFER_SDP: &str = "v=0\r\n\
o=- 0 0 IN IP4 127.0.0.1\r\n\
s=-\r\n\
t=0 0\r\n\
m=video 9 UDP/TLS/RTP/SAVPF 96\r\n\
c=IN IP4 0.0.0.0\r\n\
a=rtpmap:96 VP8/90000\r\n\
a=sendonly\r\n";
    const CLIENT_OFFER: &str = "client-offer";

    // ---------------------------------------------------
    // --- The following tests was generated by claude ---
    // ---------------------------------------------------

    fn make_channel() -> (
        SignallingChannel,
        tokio::sync::mpsc::UnboundedReceiver<InternalMessage>,
        tokio::sync::mpsc::UnboundedSender<String>,
    ) {
        let (answer_tx, answer_rx) = tokio::sync::mpsc::unbounded_channel::<InternalMessage>();
        let (offer_tx, offer_rx) = tokio::sync::mpsc::unbounded_channel::<String>();
        let channel = SignallingChannel {
            tx: answer_tx,
            offer_rx: MirroringOfferRx(Arc::new(parking_lot::Mutex::new(Some(offer_rx)))),
        };
        (channel, answer_rx, offer_tx)
    }

    #[test]
    fn signaller_start_emits_session_started_then_description() {
        init();

        let signaller = FSignaller::default();
        let (channel, _answer_rx, offer_tx) = make_channel();
        signaller.set_property("signalling-channel", &channel);

        let (started_tx, started_rx) = mpsc::channel::<(String, String)>();
        let started_tx = Mutex::new(started_tx);
        signaller.connect("session-started", false, move |values| {
            let peer_id = values[1].get::<String>().unwrap();
            let session_id = values[2].get::<String>().unwrap();
            let _ = started_tx.lock().unwrap().send((peer_id, session_id));
            None
        });

        let (desc_tx, desc_rx) = mpsc::channel::<(gst_webrtc::WebRTCSDPType, String)>();
        let desc_tx = Mutex::new(desc_tx);
        signaller.connect("session-description", false, move |values| {
            let desc = values[2]
                .get::<gst_webrtc::WebRTCSessionDescription>()
                .unwrap();
            let sdp_text = desc.sdp().as_text().unwrap().to_string();
            let _ = desc_tx.lock().unwrap().send((desc.type_(), sdp_text));
            None
        });

        signaller.start();

        let (peer_id, session_id) = started_rx
            .recv_timeout(Duration::from_secs(5))
            .expect("session-started should be emitted");
        assert_eq!(peer_id, CLIENT_OFFER);
        assert_eq!(session_id, CLIENT_OFFER);

        assert!(
            desc_rx.try_recv().is_err(),
            "session-description emitted before the offer was provided"
        );

        offer_tx.send(OFFER_SDP.to_owned()).unwrap();

        let (sdp_type, sdp_text) = desc_rx
            .recv_timeout(Duration::from_secs(5))
            .expect("session-description should be emitted after the offer");
        assert_eq!(sdp_type, gst_webrtc::WebRTCSDPType::Offer);
        assert!(
            sdp_text.contains("m=video"),
            "round-tripped SDP missing media line: {sdp_text}"
        );
    }

    #[test]
    fn signaller_start_without_offer_emits_only_session_started() {
        init();

        let signaller = FSignaller::default();
        let (channel, _answer_rx, _offer_tx) = make_channel();
        signaller.set_property("signalling-channel", &channel);

        let (started_tx, started_rx) = mpsc::channel::<()>();
        let started_tx = Mutex::new(started_tx);
        signaller.connect("session-started", false, move |_values| {
            let _ = started_tx.lock().unwrap().send(());
            None
        });

        let (desc_tx, desc_rx) = mpsc::channel::<()>();
        let desc_tx = Mutex::new(desc_tx);
        signaller.connect("session-description", false, move |_values| {
            let _ = desc_tx.lock().unwrap().send(());
            None
        });

        signaller.start();

        started_rx
            .recv_timeout(Duration::from_secs(5))
            .expect("session-started should be emitted");

        assert!(
            desc_rx.recv_timeout(Duration::from_millis(500)).is_err(),
            "session-description emitted without an offer"
        );
    }
}
