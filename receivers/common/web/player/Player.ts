import { PlayMessage } from 'common/Packets';
import dashjs from 'modules/dashjs';
import Hls from 'modules/hls.js';

const logger = window.targetAPI.logger;

export enum PlayerType {
    Html,
    Dash,
    Hls,
}

export class Player {
    private player: HTMLVideoElement;
    private playMessage: PlayMessage;
    private source: string;
    private playCb: any;
    private pauseCb: any;

    // Todo: use a common event handler interface instead of exposing internal players
    public playerType: PlayerType;
    public dashPlayer: dashjs.MediaPlayerClass = null;
    public hlsPlayer: Hls = null;

    constructor(player: HTMLVideoElement, message: PlayMessage) {
        this.player = player;
        this.playMessage = message;
        this.playCb = null;
        this.pauseCb = null;

        if (message.container === 'application/dash+xml') {
            this.playerType = PlayerType.Dash;
            this.source = message.content ? message.content : message.url;
            this.dashPlayer = dashjs.MediaPlayer().create();

            this.dashPlayer.extend("RequestModifier", () => {
                return {
                    modifyRequestHeader: function (xhr) {
                        if (message.headers) {
                            for (const [key, val] of Object.entries(message.headers)) {
                                xhr.setRequestHeader(key, val);
                            }
                        }

                        return xhr;
                    }
                };
            }, true);

        } else if ((message.container === 'application/vnd.apple.mpegurl' || message.container === 'application/x-mpegURL') && !player.canPlayType(message.container)) {
            this.playerType = PlayerType.Hls;
            this.source = message.url;

            const config = {
                xhrSetup: function (xhr: XMLHttpRequest) {
                    if (message.headers) {
                        for (const [key, val] of Object.entries(message.headers)) {
                            xhr.setRequestHeader(key, val);
                        }
                    }
                },
            };

            this.hlsPlayer = new Hls(config);

        } else {
            this.playerType = PlayerType.Html;
            this.source = message.url;
        }
    }

    public destroy() {
        switch (this.playerType) {
            case PlayerType.Dash:
                try {
                    this.dashPlayer.destroy();
                } catch (e) {
                    logger.warn("Failed to destroy dash player", e);
                }

                break;

            case PlayerType.Hls:
                // HLS also uses html player
                try {
                    this.hlsPlayer.destroy();
                } catch (e) {
                    logger.warn("Failed to destroy hls player", e);
                }
                // fallthrough

            case PlayerType.Html: {
                this.player.src = "";
                this.player.onerror = null;
                this.player.onloadedmetadata = null;
                this.player.ontimeupdate = null;
                this.player.onplay = null;
                this.player.onpause = null;
                this.player.onended = null;
                this.player.ontimeupdate = null;
                this.player.onratechange = null;
                this.player.onvolumechange = null;

                break;
            }

            default:
                break;
        }

        this.player = null;
        this.playerType = null;
        this.dashPlayer = null;
        this.hlsPlayer = null;
        this.playMessage = null;
        this.source = null;
        this.playCb = null;
        this.pauseCb = null;
    }

    /**
     * Load media specified in the PlayMessage provided on object initialization
     */
    public load() {
        if (this.playerType === PlayerType.Dash) {
            if (this.playMessage.content) {
                this.dashPlayer.initialize(this.player, `data:${this.playMessage.container};base64,` + window.btoa(this.playMessage.content), true, this.playMessage.time);
                // dashPlayer.initialize(videoElement, "https://dash.akamaized.net/akamai/test/caption_test/ElephantsDream/elephants_dream_480p_heaac5_1_https.mpd", true);
            } else {
                // value.url = 'https://dash.akamaized.net/akamai/bbb_30fps/bbb_30fps.mpd';
                this.dashPlayer.initialize(this.player, this.playMessage.url, true, this.playMessage.time);
            }
        } else if (this.playerType === PlayerType.Hls) {
            // value.url = "https://devstreaming-cdn.apple.com/videos/streaming/examples/adv_dv_atmos/main.m3u8?ref=developerinsider.co";
            this.hlsPlayer.loadSource(this.playMessage.url);
            this.hlsPlayer.attachMedia(this.player);
            // hlsPlayer.subtitleDisplay = true;
        } else { // HTML
            this.player.src = this.playMessage.url;
            this.player.load();
        }
    }

    public play() {
        logger.info("Player: play");

        if (this.playerType === PlayerType.Dash) {
            this.dashPlayer.play();
        } else { // HLS, HTML
            this.player.play();
        }

        if (this.playCb) {
            this.playCb();
        }
    }

    public isPaused(): boolean {
        if (this.playerType === PlayerType.Dash) {
            return this.dashPlayer.isPaused();
        } else { // HLS, HTML
            return this.player.paused;
        }
    }

    public pause() {
        logger.info("Player: pause");

        if (this.playerType === PlayerType.Dash) {
            this.dashPlayer.pause();
        } else { // HLS, HTML
            this.player.pause();
        }

        if (this.pauseCb) {
            this.pauseCb();
        }
    }

    public setPlayPauseCallback(playCallback: (() => void), pauseCallback: (() => void)) {
        this.playCb = playCallback;
        this.pauseCb = pauseCallback;
    }

    public stop() {
        const playbackRate = this.getPlaybackRate();
        const volume = this.getVolume();

        if (this.playerType === PlayerType.Dash) {
            if (this.playMessage.content) {
                this.dashPlayer.initialize(this.player, `data:${this.playMessage.container};base64,` + window.btoa(this.playMessage.content), false);
            } else {
                this.dashPlayer.initialize(this.player, this.playMessage.url, false);
            }
        } else if (this.playerType === PlayerType.Hls) {
            this.hlsPlayer.loadSource(this.source);
        } else {
            this.player.load();
        }

        this.setPlaybackRate(playbackRate);
        this.setVolume(volume);
    }

    public getVolume(): number {
        if (this.playerType === PlayerType.Dash) {
            return this.dashPlayer.getVolume();
        } else { // HLS, HTML
            return this.player.volume;
        }
    }
    public setVolume(value: number) {
        // logger.info(`Player: setVolume ${value}`);
        const sanitizedVolume = Math.min(1.0, Math.max(0.0, value));

        if (this.playerType === PlayerType.Dash) {
            this.dashPlayer.setVolume(sanitizedVolume);
        } else { // HLS, HTML
            this.player.volume = sanitizedVolume;
        }
    }

    public isMuted(): boolean {
        if (this.playerType === PlayerType.Dash) {
            return this.dashPlayer.isMuted();
        } else { // HLS, HTML
            return this.player.muted;
        }
    }
    public setMute(value: boolean) {
        logger.info(`Player: setMute ${value}`);

        if (this.playerType === PlayerType.Dash) {
            this.dashPlayer.setMute(value);
        } else { // HLS, HTML
            this.player.muted = value;
        }
    }

    public getPlaybackRate(): number {
        if (this.playerType === PlayerType.Dash) {
            return this.dashPlayer.getPlaybackRate();
        } else { // HLS, HTML
            return this.player.playbackRate;
        }
    }
    public setPlaybackRate(value: number) {
        logger.info(`Player: setPlaybackRate ${value}`);
        const sanitizedSpeed = Math.min(16.0, Math.max(0.0, value));

        if (this.playerType === PlayerType.Dash) {
            this.dashPlayer.setPlaybackRate(sanitizedSpeed);
        } else { // HLS, HTML
            this.player.playbackRate = sanitizedSpeed;
        }
    }

    public getDuration(): number {
        if (this.playerType === PlayerType.Dash) {
            return isFinite(this.dashPlayer.duration()) ? this.dashPlayer.duration() : 0;
        } else { // HLS, HTML
            return isFinite(this.player.duration) ? this.player.duration : 0;
        }
    }

    public getCurrentTime(): number {
        if (this.playerType === PlayerType.Dash) {
            return this.dashPlayer.time();
        } else { // HLS, HTML
            return this.player.currentTime;
        }
    }
    public setCurrentTime(value: number) {
        // logger.info(`Player: setCurrentTime ${value}`);
        const sanitizedTime = Math.min(this.getDuration(), Math.max(0.0, value));

        if (this.playerType === PlayerType.Dash) {
            this.dashPlayer.seek(sanitizedTime);

            if (!this.dashPlayer.isSeeking()) {
                this.dashPlayer.seek(sanitizedTime);
            }

        } else { // HLS, HTML
            this.player.currentTime = sanitizedTime;
        }
    }

    public getSource(): string {
        return this.source;
    }

    public getAutoplay(): boolean {
        if (this.playerType === PlayerType.Dash) {
            return this.dashPlayer.getAutoPlay();
        } else { // HLS, HTML
            return this.player.autoplay;
        }
    }

    public setAutoPlay(value: boolean) {
        if (this.playerType === PlayerType.Dash) {
            return this.dashPlayer.setAutoPlay(value);
        } else { // HLS, HTML
            return this.player.autoplay = value;
        }
    }

    public getBufferLength(): number {
        if (this.playerType === PlayerType.Dash) {
            let dashBufferLength = this.dashPlayer.getBufferLength("video")
                ?? this.dashPlayer.getBufferLength("audio")
                ?? this.dashPlayer.getBufferLength("text")
                ?? this.dashPlayer.getBufferLength("image")
                ?? 0;
            if (Number.isNaN(dashBufferLength))
                dashBufferLength = 0;

            dashBufferLength += this.dashPlayer.time();
            return dashBufferLength;
        } else { // HLS, HTML
            let maxBuffer = 0;

            if (this.player.buffered) {
                for (let i = 0; i < this.player.buffered.length; i++) {
                    const start = this.player.buffered.start(i);
                    const end = this.player.buffered.end(i);

                    if (this.player.currentTime >= start && this.player.currentTime <= end) {
                        maxBuffer = end;
                    }
                }
            }

            return maxBuffer;
        }
    }

    public isCaptionsSupported(): boolean {
        if (this.playerType === PlayerType.Dash) {
            return this.dashPlayer.getTracksFor('text').length > 0;
        } else if (this.playerType === PlayerType.Hls) {
            return this.hlsPlayer.allSubtitleTracks.length > 0;
        } else {
            return false; // HTML captions not currently supported
        }
    }

    public isCaptionsEnabled(): boolean {
        if (this.playerType === PlayerType.Dash) {
            return this.dashPlayer.isTextEnabled();
        } else if (this.playerType === PlayerType.Hls) {
            return this.hlsPlayer.subtitleDisplay;
        } else {
            return false; // HTML captions not currently supported
        }
    }

    public enableCaptions(enable: boolean) {
        if (this.playerType === PlayerType.Dash) {
            this.dashPlayer.enableText(enable);
        } else if (this.playerType === PlayerType.Hls) {
            this.hlsPlayer.subtitleDisplay = enable;
        }
        // HTML captions not currently supported
    }

}
