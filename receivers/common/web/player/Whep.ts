// Copyright (c) 2021 medooze
//
// This file is taken from the whip-whep-js project (https://github.com/medooze/whip-whep-js) licensed under the MIT license.
// Changes include porting to TypeScript, removing the use of extensions and ICE trickle features. THIS IS NOT INTENDED TO
// BE USED OUTSIDE OF LOCAL NETWORKS.

const logger = window.targetAPI.logger;

export class WHEPClient extends EventTarget {
    private pc: RTCPeerConnection | null;
    private candidates: any;
    private onOffer: any;
    private onAnswer: any;
    private resourceURL: URL | null;

    constructor() {
        super();
        this.candidates = [];
        this.pc = null;
        this.resourceURL = null;

        this.onOffer = offer => offer;
        this.onAnswer = answer => answer;
    }

    async view(pc: RTCPeerConnection, url: string): Promise<void> {
        if (this.pc) {
            throw new Error("Already viewing")
        }

        this.pc = pc;

        pc.onicecandidate = (event) => {
            if (event.candidate) {
                // Ignore candidates not from the first m line
                if (event.candidate.sdpMLineIndex == null || event.candidate.sdpMLineIndex > 0) {
                    return;
                }
                this.candidates.push(event.candidate);
            }
        }

        const offer = await pc.createOffer();
        offer.sdp = this.onOffer(offer.sdp);
        const headers = {
            "Content-Type": "application/sdp"
        };

        const offerSdp = offer.sdp;
        if (offerSdp == null) {
            throw new Error("Offer is missing SDP");
        }
        const fetched = await fetch(url, { method: "POST", body: offerSdp, headers: headers });

        if (!fetched.ok) {
            throw new Error("Request rejected with status " + fetched.status)
        }
        if (!fetched.headers.get("location")) {
            throw new Error("Response missing location header")
        }

        const location = fetched.headers.get("location");
        if (location == null) {
            throw new Error("Headers does not contain the `location` field");
        }
        this.resourceURL = new URL(location, url);

        const answer = await fetched.text();
        await pc.setLocalDescription(offer);
        await pc.setRemoteDescription({ type: "answer", sdp: this.onAnswer(answer) });
    }

    async stop(): Promise<void> {
        if (!this.pc) {
            return
        }

        this.pc.close();
        this.pc = null;

        if (!this.resourceURL) {
            throw new Error("WHEP resource url not available yet");
        }

        const headers = {};
        await fetch(this.resourceURL, {
            method: "DELETE",
            headers
        });
    }
}
