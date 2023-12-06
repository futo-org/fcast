export class PlayMessage {
    constructor(
        public container: String,
        public url: String = null,
        public content: String = null,
        public time: number = null
    ) {}
}

export class SeekMessage {
    constructor(
        public time: number,
    ) {}
}

export class PlaybackUpdateMessage {
    constructor(
        public generationTime: number,
        public time: number,
        public duration: number,
        public state: number
    ) {}
}

export class VolumeUpdateMessage {
    constructor(
        public generationTime: number,
        public volume: number
    ) {}
}

export class SetVolumeMessage {
    constructor(
        public volume: number,
    ) {}
}