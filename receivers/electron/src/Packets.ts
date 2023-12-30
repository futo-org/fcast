export class PlayMessage {
    constructor(
        public container: String,
        public url: String = null,
        public content: String = null,
        public time: number = null,
        public speed: number = null
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
        public state: number,
        public speed: number
    ) {}
}

export class PlaybackErrorMessage {
    constructor(
        public message: String
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

export class SetSpeedMessage {
    constructor(
        public speed: number,
    ) {}
}

export class VersionMessage {
    constructor(
        public version: number,
    ) {}
}