import { PlayerControlEvent, playerCtrlStateUpdate, onPlay, onPlayPlaylist, setPlaylistItem, playlistIndex } from 'common/viewer/Renderer';
import { RemoteKeyCode } from 'lib/common';
import * as common from 'lib/common';

export function targetPlayerCtrlStateUpdate(event: PlayerControlEvent): boolean {
    let handledCase = false;
    return handledCase;
}

export function targetKeyDownEventListener(event: KeyboardEvent): { handledCase: boolean, key: string }  {
    let handledCase = false;
    let key = '';

    switch (event.keyCode) {
        case RemoteKeyCode.Stop:
            // history.back();
            window.open('../main_window/index.html', '_self');
            event.preventDefault();
            handledCase = true;
            key = 'Stop';
            break;

        case RemoteKeyCode.Rewind:
            setPlaylistItem(playlistIndex - 1);
            event.preventDefault();
            handledCase = true;
            key = 'Rewind';
            break;

        case RemoteKeyCode.Play:
            playerCtrlStateUpdate(PlayerControlEvent.Play);
            event.preventDefault();
            handledCase = true;
            key = 'Play';
            break;
        case RemoteKeyCode.Pause:
            playerCtrlStateUpdate(PlayerControlEvent.Pause);
            event.preventDefault();
            handledCase = true;
            key = 'Pause';
            break;

        case RemoteKeyCode.FastForward:
            setPlaylistItem(playlistIndex + 1);
            event.preventDefault();
            handledCase = true;
            key = 'FastForward';
            break;

        // WebOS 22 and earlier does not work well using the history API,
        // so manually handling page navigation...
        case RemoteKeyCode.Back:
            // history.back();
            window.open('../main_window/index.html', '_self');
            event.preventDefault();
            handledCase = true;
            key = 'Back';
            break;

        default:
            break;
    }

    return { handledCase: handledCase, key: key };
};

export function targetKeyUpEventListener(event: KeyboardEvent): { handledCase: boolean, key: string } {
    return common.targetKeyUpEventListener(event);
};

if (window.webOSAPI.pendingPlay !== null) {
    if (window.webOSAPI.pendingPlay.rendererEvent === 'play-playlist') {
        onPlayPlaylist(null, window.webOSAPI.pendingPlay.rendererMessage);
    }
    else {
        onPlay(null, window.webOSAPI.pendingPlay.rendererMessage);
    }
}
