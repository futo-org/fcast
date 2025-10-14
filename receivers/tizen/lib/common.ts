import { Logger, LoggerType } from 'common/Logger';

const logger = new Logger('Common', LoggerType.FRONTEND);

export enum RemoteKeyCode {
    Stop = 413,
    Rewind = 412,
    Play = 415,
    Pause = 19,
    FastForward = 417,
    Back = 10009,
    MediaPlayPause = 10252,
}

export enum KeyCode {
    ArrowUp = 38,
    ArrowDown = 40,
    ArrowLeft = 37,
    ArrowRight = 39,
    KeyK = 75,
    Space = 32,
    Enter = 13,
}

export enum ControlBarMode {
    KeyboardMouse,
    Remote
}

export function targetKeyUpEventListener(event: KeyboardEvent): { handledCase: boolean, key: string } {
    let handledCase = false;
    let key = '';

    // .keyCode instead of alternatives is required to work properly on webOS
    switch (event.keyCode) {
        // Unhandled cases (used for replacing undefined key codes)
        case RemoteKeyCode.Stop:
            key = 'Stop';
            break;
        case RemoteKeyCode.Rewind:
            key = 'Rewind';
            break;
        case RemoteKeyCode.Play:
            key = 'Play';
            break;
        case RemoteKeyCode.Pause:
            key = 'Pause';
            break;
        case RemoteKeyCode.FastForward:
            key = 'FastForward';
            break;
        case RemoteKeyCode.Back:
            key = 'Back';
            break;
        default:
            break;
    }

    return { handledCase: handledCase, key: key };
};
