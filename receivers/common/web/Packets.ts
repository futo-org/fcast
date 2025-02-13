export enum Opcode {
    None = 0,
    Play = 1,
    Pause = 2,
    Resume = 3,
    Stop = 4,
    Seek = 5,
    PlaybackUpdate = 6,
    VolumeUpdate = 7,
    SetVolume = 8,
    PlaybackError = 9,
    SetSpeed = 10,
    Version = 11,
    Ping = 12,
    Pong = 13
};

export class PlayMessage {
    constructor(
        public container: string,
        public url: string = null,
        public content: string = null,
        public time: number = null,
        public speed: number = null,
        public headers: { [key: string]: string } = null
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
        public message: string
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
