import { MediaItem, PlayMessage } from 'common/Packets';

export function playMessageFromMediaItem(item: MediaItem) {
    return item ? new PlayMessage(
            item.container, item.url, item.content,
            item.time, item.volume, item.speed,
            item.headers, item.metadata
        ) : new PlayMessage("");
}

export function mediaItemFromPlayMessage(message: PlayMessage) {
    return message ? new MediaItem(
            message.container, message.url, message.content,
            message.time, message.volume, message.speed,
            null, null, message.headers, message.metadata
    ) : new MediaItem("");
}

export class Timer {
    private handle: number;
    private callback: () => void;
    private delay: number;
    private startTime: number;
    private remainingTime: number;
    private enabled: boolean;

    public started: boolean;

    constructor(callback: () => void, delay: number, autoStart: boolean = true) {
        this.handle = null;
        this.callback = callback;
        this.delay = delay;
        this.started = false;
        this.enabled = true;

        if (autoStart) {
            this.start();
        }
    }

    public start(delay?: number) {
        if (this.enabled) {
            this.delay = delay ? delay : this.delay;

            if (this.handle) {
                window.clearTimeout(this.handle);
            }

            this.started = true;
            this.startTime = Date.now();
            this.remainingTime = null;
            this.handle = window.setTimeout(this.callback, this.delay);
        }
    }

    public pause() {
        if (this.enabled && this.handle) {
            window.clearTimeout(this.handle);
            this.handle = null;
            this.remainingTime = this.delay - (Date.now() - this.startTime);
        }
    }

    public resume() {
        if (this.enabled && this.remainingTime) {
            this.start(this.remainingTime);
        }
    }

    public stop() {
        if (this.handle) {
            window.clearTimeout(this.handle);
            this.handle = null;
            this.remainingTime = null;
            this.started = false;
        }
    }

    public end() {
        this.stop();
        this.callback();
    }

    public enable() {
        this.enabled = true;
    }

    public disable() {
        this.enabled = false;
        this.stop();
    }

    public setDelay(delay: number) {
        this.stop();
        this.delay = delay;
    }

    public setCallback(callback: () => void) {
        this.stop();
        this.callback = callback;
    }

    public isPaused(): boolean {
        return this.remainingTime !== null;
    }
}
