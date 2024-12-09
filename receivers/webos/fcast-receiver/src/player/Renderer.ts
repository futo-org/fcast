import { onPlay, PlayerControlEvent } from 'common/player/Renderer';

export function targetPlayerCtrlStateUpdate(event: PlayerControlEvent) {
    switch (event) {
        default:
            break;
    }
}

// eslint-disable-next-line @typescript-eslint/no-explicit-any
export function targetKeyDownEventListener(event: any) {
    switch (event.code) {
        default:
            break;
    }
};

if (window.webOSAPI.pendingPlay !== null) {
    onPlay(null, window.webOSAPI.pendingPlay);
}
