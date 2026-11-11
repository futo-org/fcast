import * as tls from 'tls';
import { ReceiverIdentity } from './Identity';
import { ReceiverCapabilitiesInput } from './Encoder';

export interface V4Context {
    secureContext: tls.SecureContext;
    fingerprint: string;
    receiverIntroduction: Uint8Array;
}

export function createV4Context(identity: ReceiverIdentity, intro: Uint8Array): V4Context {
    return {
        secureContext: tls.createSecureContext({
            key: identity.keyPem,
            cert: identity.certPem,
            minVersion: 'TLSv1.3',
            maxVersion: 'TLSv1.3',
        }),
        fingerprint: identity.fingerprint,
        receiverIntroduction: intro,
    };
}

export function defaultCapabilities(): ReceiverCapabilitiesInput {
    return {
        media: {
            protocols: ['http', 'https'],
            containers: ['mp4', 'webm', 'hls', 'dash'],
            videoFormats: ['h264', 'h265', 'vp9'],
            audioFormats: ['aac', 'mp3', 'opus', 'ac3', 'eac3'],
            subtitleFormats: ['vtt', 'srt'],
            hdrFormats: [],
            imageFormats: ['png', 'jpeg', 'gif', 'webp', 'bmp'],
            externalSubtitles: true,
            mirroring: false,
        },
        display: null,
        volumeStepInterval: 0.01,
    };
}
