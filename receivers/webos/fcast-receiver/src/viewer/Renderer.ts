import {
    PlayerControlEvent,
    playerCtrlStateUpdate,
    onPlay,
    onPlayPlaylist,
    setPlaylistItem,
    playlistIndex,
    uiHideTimer,
    showDurationTimer,
    isMediaItem,
    cachedPlayMediaItem,
    imageViewerPlaybackState,
    keyDownEventHandler,
    keyUpEventHandler
} from 'common/viewer/Renderer';
import { KeyCode, RemoteKeyCode, ControlBarMode } from 'lib/common';
import * as common from 'lib/common';
import { PlaybackState } from 'common/Packets';

const logger = window.targetAPI.logger;

const playPreviousContainer = document.getElementById('playPreviousContainer');
const actionContainer = document.getElementById('actionContainer');
const playNextContainer = document.getElementById('playNextContainer');
const action = document.getElementById('action');

enum ControlFocus {
    Action,
    PlayPrevious,
    PlayNext,
}

let controlMode = ControlBarMode.KeyboardMouse;
let controlFocus = ControlFocus.Action;

// Hide
// [|<][>][>|]
// Hide
let locationMap = {
    Action: actionContainer,
    PlayPrevious: playPreviousContainer,
    PlayNext: playNextContainer,
};


window.parent.webOSApp.setKeyDownHandler(keyDownEventHandler);
window.parent.webOSApp.setKeyUpHandler(keyUpEventHandler);

uiHideTimer.setDelay(5000);
uiHideTimer.setCallback(() => {
    if (controlMode === ControlBarMode.KeyboardMouse || !showDurationTimer.isPaused()) {
        controlMode = ControlBarMode.KeyboardMouse;
        locationMap[ControlFocus[controlFocus]].classList.remove('buttonFocus');
        playerCtrlStateUpdate(PlayerControlEvent.UiFadeOut);
    }
});

// Leave control bar on screen if magic remote cursor leaves window
document.onmouseout = () => {
    if (controlMode === ControlBarMode.KeyboardMouse) {
        uiHideTimer.end();
    }
}

function remoteNavigateTo(location: ControlFocus) {
    // Issues with using standard focus, so manually managing styles
    locationMap[ControlFocus[controlFocus]].classList.remove('buttonFocus');
    controlFocus = location;
    locationMap[ControlFocus[controlFocus]].classList.add('buttonFocus');
}

function setControlMode(mode: ControlBarMode, immediateHide: boolean = true) {
    if (mode === ControlBarMode.KeyboardMouse) {
        uiHideTimer.enable();

        if (immediateHide) {
            locationMap[ControlFocus[controlFocus]].classList.remove('buttonFocus');
            playerCtrlStateUpdate(PlayerControlEvent.UiFadeOut);
        }
        else {
            uiHideTimer.start();
        }
    }
    else {
        const focus = action?.style.display === 'none' ? ControlFocus.PlayNext : ControlFocus.Action;
        remoteNavigateTo(focus);
        playerCtrlStateUpdate(PlayerControlEvent.UiFadeIn);
        uiHideTimer.start();
    }

    controlMode = mode;
}

export function targetPlayerCtrlStateUpdate(event: PlayerControlEvent): boolean {
    let handledCase = false;

    switch (event) {
        default:
            break;
    }

    return handledCase;
}

export function targetPlayerCtrlPostStateUpdate(event: PlayerControlEvent) {
    switch (event) {
        case PlayerControlEvent.Load: {
            if (!isMediaItem && controlMode === ControlBarMode.Remote) {
                setControlMode(ControlBarMode.KeyboardMouse);
            }
            if (action?.style.display === 'none') {
                actionContainer.style.display = 'none';
            }
            else {
                actionContainer.style.display = 'block';
            }
            break;
        }

        default:
            break;
    }
}

export function targetKeyDownEventListener(event: KeyboardEvent): { handledCase: boolean, key: string }  {
    let handledCase = false;
    let key = '';

    switch (event.keyCode) {
        case KeyCode.KeyK:
        case KeyCode.Space:
            if (isMediaItem) {
                // Play/pause toggle
                if (cachedPlayMediaItem.showDuration && cachedPlayMediaItem.showDuration > 0) {
                    if (imageViewerPlaybackState === PlaybackState.Paused || imageViewerPlaybackState === PlaybackState.Idle) {
                        playerCtrlStateUpdate(PlayerControlEvent.Play);
                    } else {
                        playerCtrlStateUpdate(PlayerControlEvent.Pause);
                    }
                }

                event.preventDefault();
                handledCase = true;
            }
            break;

        case KeyCode.Enter:
            if (isMediaItem) {
                if (controlMode === ControlBarMode.KeyboardMouse) {
                    setControlMode(ControlBarMode.Remote);
                }
                else {
                    if (controlFocus === ControlFocus.Action) {
                        // Play/pause toggle
                        if (cachedPlayMediaItem.showDuration && cachedPlayMediaItem.showDuration > 0) {
                            if (imageViewerPlaybackState === PlaybackState.Paused || imageViewerPlaybackState === PlaybackState.Idle) {
                                playerCtrlStateUpdate(PlayerControlEvent.Play);
                            } else {
                                playerCtrlStateUpdate(PlayerControlEvent.Pause);
                            }
                        }
                    }
                    else if (controlFocus === ControlFocus.PlayPrevious) {
                        setPlaylistItem(playlistIndex - 1);
                    }
                    else if (controlFocus === ControlFocus.PlayNext) {
                        setPlaylistItem(playlistIndex + 1);
                    }
                }

                event.preventDefault();
                handledCase = true;
            }
            break;
        case KeyCode.ArrowUp:
        case KeyCode.ArrowDown:
            if (isMediaItem) {
                if (controlMode === ControlBarMode.KeyboardMouse) {
                    setControlMode(ControlBarMode.Remote);
                }
                else {
                    setControlMode(ControlBarMode.KeyboardMouse);
                }

                event.preventDefault();
                handledCase = true;
            }
            break;
        case KeyCode.ArrowLeft:
            if (isMediaItem) {
                if (controlMode === ControlBarMode.KeyboardMouse) {
                    setPlaylistItem(playlistIndex - 1);
                }
                else {
                    if (controlFocus === ControlFocus.Action || action?.style.display === 'none') {
                        remoteNavigateTo(ControlFocus.PlayPrevious);
                    }
                    else if (controlFocus === ControlFocus.PlayNext) {
                        remoteNavigateTo(ControlFocus.Action);
                    }
                }

                event.preventDefault();
                handledCase = true;
            }
            break;
        case KeyCode.ArrowRight:
            if (isMediaItem) {
                if (controlMode === ControlBarMode.KeyboardMouse) {
                    setPlaylistItem(playlistIndex + 1);
                }
                else {
                    if (controlFocus === ControlFocus.Action || action?.style.display === 'none') {
                        remoteNavigateTo(ControlFocus.PlayNext);
                    }
                    else if (controlFocus === ControlFocus.PlayPrevious) {
                        remoteNavigateTo(ControlFocus.Action);
                    }
                }

                event.preventDefault();
                handledCase = true;
            }
            break;

        case RemoteKeyCode.Stop:
            window.parent.webOSApp.loadPage('main_window/index.html');
            event.preventDefault();
            handledCase = true;
            key = 'Stop';
            break;

        // Note that in simulator rewind and fast forward key codes are sent twice...
        case RemoteKeyCode.Rewind:
            if (isMediaItem) {
                setPlaylistItem(playlistIndex - 1);
                event.preventDefault();
                handledCase = true;
                key = 'Rewind';
            }
            break;

        case RemoteKeyCode.Play:
            if (isMediaItem) {
                playerCtrlStateUpdate(PlayerControlEvent.Play);
                event.preventDefault();
                handledCase = true;
                key = 'Play';
            }
            break;
        case RemoteKeyCode.Pause:
            if (isMediaItem) {
                playerCtrlStateUpdate(PlayerControlEvent.Pause);
                event.preventDefault();
                handledCase = true;
                key = 'Pause';
            }
            break;

        // Note that in simulator rewind and fast forward key codes are sent twice...
        case RemoteKeyCode.FastForward:
            if (isMediaItem) {
                setPlaylistItem(playlistIndex + 1);
                event.preventDefault();
                handledCase = true;
                key = 'FastForward';
            }
            break;

        case RemoteKeyCode.Back:
            window.parent.webOSApp.loadPage('main_window/index.html');
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

if (window.parent.webOSApp.pendingPlay !== null) {
    if (window.parent.webOSApp.pendingPlay.rendererEvent === 'play-playlist') {
        onPlayPlaylist(null, window.parent.webOSApp.pendingPlay.rendererMessage);
    }
    else {
        onPlay(null, window.parent.webOSApp.pendingPlay.rendererMessage);
    }
}
