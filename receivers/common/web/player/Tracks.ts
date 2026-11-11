import { MediaTrackType } from 'common/v4/generated/fcast/v4/media-track-type';

export interface TrackInfo {
    id: number;
    trackType: MediaTrackType;
    language: string;
    title: string | null;
}

export interface TracksReport {
    tracks: TrackInfo[];
    selected: { video: number | null; audio: number | null; subtitle: number | null };
}

export interface ExternalSubtitle {
    id: number;
    name: string | null;
    vtt: string;
    select: boolean;
}

interface ExternalTrack {
    id: number;
    name: string | null;
    element: HTMLTrackElement;
    url: string;
}

// Lists and switches the tracks of the media in a video element. Video and audio tracks come from
// the element's own track lists where the browser has them; otherwise one of each is assumed.
// Subtitles are the WebVTT files added while the media plays. Ids are unique across kinds: video
// tracks first, then audio tracks, then the ids given to the added subtitles.
export class TrackController {
    private external: ExternalTrack[] = [];
    private videoDisabled = false;
    private audioDisabled = false;
    private subtitleId: number | null = null;

    constructor(
        private readonly video: HTMLVideoElement,
        private readonly report: (report: TracksReport) => void,
    ) {}

    get isAudioDisabled(): boolean {
        return this.audioDisabled;
    }

    // Forgets the tracks of the previous media and undoes what was disabled for it.
    reset(): void {
        for (const track of this.external) {
            track.element.remove();
            URL.revokeObjectURL(track.url);
        }
        this.external = [];
        this.subtitleId = null;
        if (this.videoDisabled) {
            this.videoDisabled = false;
            this.video.style.visibility = '';
        }
        if (this.audioDisabled) {
            this.audioDisabled = false;
            if (!this.nativeAudio()) {
                this.video.muted = false;
            }
        }
    }

    publish(): void {
        this.report({ tracks: this.list(), selected: this.selection() });
    }

    change(trackType: MediaTrackType, id: number | null): void {
        const videoCount = this.videoCount();
        switch (trackType) {
            case MediaTrackType.Video: {
                this.videoDisabled = id === null;
                this.video.style.visibility = this.videoDisabled ? 'hidden' : '';
                const native = this.nativeVideo();
                if (id !== null && native && native[id]) {
                    native[id].selected = true;
                }
                if (this.videoDisabled) {
                    // Subtitles are drawn over the picture, so they go with it.
                    this.subtitleId = null;
                    this.applySubtitle();
                }
                break;
            }
            case MediaTrackType.Audio: {
                this.audioDisabled = id === null;
                const native = this.nativeAudio();
                if (native) {
                    for (let i = 0; i < native.length; i++) {
                        native[i].enabled = id !== null && i === id - videoCount;
                    }
                } else {
                    this.video.muted = this.audioDisabled;
                }
                break;
            }
            case MediaTrackType.Subtitle:
                this.subtitleId = id;
                this.applySubtitle();
                break;
            default:
                return;
        }
        this.publish();
    }

    addSubtitle(subtitle: ExternalSubtitle): void {
        const url = URL.createObjectURL(new Blob([subtitle.vtt], { type: 'text/vtt' }));
        const element = document.createElement('track');
        element.kind = 'subtitles';
        element.label = subtitle.name !== null ? subtitle.name : '';
        element.src = url;
        this.video.appendChild(element);
        this.external.push({ id: subtitle.id, name: subtitle.name, element, url });

        if (subtitle.select && !this.videoDisabled) {
            this.subtitleId = subtitle.id;
        }
        this.applySubtitle();
        this.publish();
    }

    private nativeVideo(): any {
        const list = (this.video as any).videoTracks;
        return list && list.length > 0 ? list : null;
    }

    private nativeAudio(): any {
        const list = (this.video as any).audioTracks;
        return list && list.length > 0 ? list : null;
    }

    private videoCount(): number {
        const native = this.nativeVideo();
        if (native) {
            return native.length;
        }
        return this.video.videoWidth > 0 ? 1 : 0;
    }

    private audioCount(): number {
        const native = this.nativeAudio();
        return native ? native.length : 1;
    }

    private list(): TrackInfo[] {
        const tracks: TrackInfo[] = [];
        const nativeVideo = this.nativeVideo();
        const nativeAudio = this.nativeAudio();
        const videoCount = this.videoCount();
        const audioCount = this.audioCount();

        for (let i = 0; i < videoCount; i++) {
            const native = nativeVideo ? nativeVideo[i] : null;
            tracks.push({ id: i, trackType: MediaTrackType.Video, language: native && native.language ? native.language : 'und', title: native && native.label ? native.label : null });
        }
        for (let i = 0; i < audioCount; i++) {
            const native = nativeAudio ? nativeAudio[i] : null;
            tracks.push({ id: videoCount + i, trackType: MediaTrackType.Audio, language: native && native.language ? native.language : 'und', title: native && native.label ? native.label : null });
        }
        for (const track of this.external) {
            tracks.push({ id: track.id, trackType: MediaTrackType.Subtitle, language: 'und', title: track.name });
        }
        return tracks;
    }

    private selection(): TracksReport['selected'] {
        const videoCount = this.videoCount();
        let video: number | null = null;
        let audio: number | null = null;

        if (!this.videoDisabled && videoCount > 0) {
            video = 0;
            const native = this.nativeVideo();
            if (native && native.selectedIndex >= 0) {
                video = native.selectedIndex;
            }
        }
        if (!this.audioDisabled && this.audioCount() > 0) {
            audio = videoCount;
            const native = this.nativeAudio();
            if (native) {
                for (let i = 0; i < native.length; i++) {
                    if (native[i].enabled) {
                        audio = videoCount + i;
                        break;
                    }
                }
            }
        }
        return { video, audio, subtitle: this.subtitleId };
    }

    private applySubtitle(): void {
        for (const track of this.external) {
            track.element.track.mode = track.id === this.subtitleId ? 'showing' : 'disabled';
        }
    }
}
