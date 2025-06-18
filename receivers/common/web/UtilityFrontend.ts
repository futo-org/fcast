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

    constructor(callback: () => void, delay: number, autoStart: boolean = true) {
        this.handle = null;
        this.callback = callback;
        this.delay = delay;

        if (autoStart) {
            this.start();
        }
    }

    public start(delay?: number) {
        this.delay = delay ? delay : this.delay;

        if (this.handle) {
            window.clearTimeout(this.handle);
        }

        this.startTime = Date.now();
        this.remainingTime = null;
        this.handle = window.setTimeout(this.callback, this.delay);
    }

    public pause() {
        if (this.handle) {
            window.clearTimeout(this.handle);
            this.handle = null;
            this.remainingTime = this.delay - (Date.now() - this.startTime);
        }
    }

    public resume() {
        if (this.remainingTime) {
            this.start(this.remainingTime);
        }
    }

    public stop() {
        if (this.handle) {
            window.clearTimeout(this.handle);
            this.handle = null;
            this.remainingTime = null;
        }
    }
}
