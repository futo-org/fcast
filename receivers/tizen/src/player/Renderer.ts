import {
    onPlay,
    onPlayPlaylist,
    setPlaylistItem,
    playerCtrlStateUpdate,
    playlistIndex,
    player,
    uiHideTimer,
    PlayerControlEvent,
    playerCtrlCaptions,
    videoCaptions,
    skipBack,
    skipForward,
    playerCtrlProgressBarHandle,
} from 'common/player/Renderer';
import { KeyCode, RemoteKeyCode, ControlBarMode } from 'lib/common';
import * as common from 'lib/common';

const logger = window.targetAPI.logger;
const captionsBaseHeightCollapsed = 150;
const captionsBaseHeightExpanded = 320;
const captionsLineHeight = 68;

const playPreviousContainer = document.getElementById('playPreviousContainer');
const actionContainer = document.getElementById('actionContainer');
const playNextContainer = document.getElementById('playNextContainer');

const playPrevious = document.getElementById('playPrevious');
const playNext = document.getElementById('playNext');

tizen.tvinputdevice.registerKeyBatch(['MediaRewind',
    'MediaFastForward', 'MediaPlay', 'MediaPause', 'MediaStop'
]);

enum ControlFocus {
    ProgressBar,
    Action,
    PlayPrevious,
    PlayNext,
}

let controlMode = ControlBarMode.KeyboardMouse;
let controlFocus = ControlFocus.ProgressBar;

// Hide
// [<<][>][>>]
// [|<][>][>|]
// Hide
let locationMap = {
    ProgressBar: playerCtrlProgressBarHandle,
    Action: actionContainer,
    PlayPrevious: playPreviousContainer,
    PlayNext: playNextContainer,
};

uiHideTimer.setDelay(5000);
uiHideTimer.setCallback(() => {
    if (!player?.isPaused()) {
        controlMode = ControlBarMode.KeyboardMouse;
        removeFocus(controlFocus);
        playerCtrlStateUpdate(PlayerControlEvent.UiFadeOut);
    }
});

// Leave control bar on screen if magic remote cursor leaves window
document.onmouseout = () => {
    if (controlMode === ControlBarMode.KeyboardMouse) {
        uiHideTimer.end();
    }
}

function addFocus(location: ControlFocus) {
    if (location === ControlFocus.ProgressBar) {
        locationMap[ControlFocus[location]].classList.remove('progressBarHandleHide');
    }
    else {
        locationMap[ControlFocus[location]].classList.add('buttonFocus');
    }
}

function removeFocus(location: ControlFocus) {
    if (location === ControlFocus.ProgressBar) {
        locationMap[ControlFocus[location]].classList.add('progressBarHandleHide');
    }
    else {
        locationMap[ControlFocus[location]].classList.remove('buttonFocus');
    }
}

function remoteNavigateTo(location: ControlFocus) {
    // Issues with using standard focus, so manually managing styles
    removeFocus(controlFocus);
    controlFocus = location;
    addFocus(controlFocus);
}

function setControlMode(mode: ControlBarMode, immediateHide: boolean = true) {
    if (mode === ControlBarMode.KeyboardMouse) {
        uiHideTimer.enable();

        if (immediateHide) {
            removeFocus(controlFocus);
            playerCtrlStateUpdate(PlayerControlEvent.UiFadeOut);
        }
        else {
            uiHideTimer.start();
        }
    }
    else {
        remoteNavigateTo(ControlFocus.ProgressBar);
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
            player.setPlayPauseCallback(() => {
                uiHideTimer.enable();
                uiHideTimer.start();
            }, () => {
                uiHideTimer.disable();
            });

            if (player.isCaptionsSupported()) {
                // Disabling receiver captions control on TV players
                // playerCtrlCaptions.style.display = 'block';
                playerCtrlCaptions.style.display = 'none';
                videoCaptions.style.display = 'block';
            }
            else {
                playerCtrlCaptions.style.display = 'none';
                videoCaptions.style.display = 'none';
                player.enableCaptions(false);
            }

            break;
        }

        default:
            break;
    }
}

export function targetKeyDownEventListener(event: KeyboardEvent): { handledCase: boolean, key: string }  {
    // logger.info("KeyDown", event.keyCode);
    let handledCase = false;
    let key = '';

    switch (event.keyCode) {
        case KeyCode.KeyK:
        case KeyCode.Space:
            // Play/pause toggle
            if (player?.isPaused()) {
                player?.play();
            } else {
                player?.pause();
            }
            event.preventDefault();
            handledCase = true;
            break;

        case KeyCode.Enter:
            if (controlMode === ControlBarMode.KeyboardMouse) {
                setControlMode(ControlBarMode.Remote);
            }
            else {
                if (controlFocus === ControlFocus.ProgressBar || controlFocus === ControlFocus.Action) {
                    // Play/pause toggle
                    if (player?.isPaused()) {
                        player?.play();
                    } else {
                        player?.pause();
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
            break;
        case KeyCode.ArrowUp:
            if (controlMode === ControlBarMode.KeyboardMouse) {
                setControlMode(ControlBarMode.Remote);
            }
            else {
                if (controlFocus === ControlFocus.ProgressBar) {
                    setControlMode(ControlBarMode.KeyboardMouse);
                }
                else {
                    remoteNavigateTo(ControlFocus.ProgressBar);
                }
            }

            event.preventDefault();
            handledCase = true;
            break;
        case KeyCode.ArrowDown:
            if (controlMode === ControlBarMode.KeyboardMouse) {
                setControlMode(ControlBarMode.Remote);
            }
            else {
                if (controlFocus === ControlFocus.ProgressBar) {
                    remoteNavigateTo(ControlFocus.Action);
                }
                else {
                    setControlMode(ControlBarMode.KeyboardMouse);
                }
            }

            event.preventDefault();
            handledCase = true;
            break;
        case KeyCode.ArrowLeft:
            if (controlMode === ControlBarMode.KeyboardMouse) {
                setControlMode(ControlBarMode.Remote);
            }
            else {
                if (controlFocus === ControlFocus.ProgressBar || playPrevious?.style.display === 'none') {
                    // Note that skip repeat does not trigger in simulator
                    skipBack(event.repeat);
                }
                else {
                    if (controlFocus === ControlFocus.Action) {
                        remoteNavigateTo(ControlFocus.PlayPrevious);
                    }
                    else if (controlFocus === ControlFocus.PlayNext) {
                        remoteNavigateTo(ControlFocus.Action);
                    }
                }
            }

            event.preventDefault();
            handledCase = true;
            break;
        case KeyCode.ArrowRight:
            if (controlMode === ControlBarMode.KeyboardMouse) {
                setControlMode(ControlBarMode.Remote);
            }
            else {
                if (controlFocus === ControlFocus.ProgressBar || playNext?.style.display === 'none') {
                    // Note that skip repeat does not trigger in simulator
                    skipForward(event.repeat);
                }
                else {
                    if (controlFocus === ControlFocus.Action) {
                        remoteNavigateTo(ControlFocus.PlayNext);
                    }
                    else if (controlFocus === ControlFocus.PlayPrevious) {
                        remoteNavigateTo(ControlFocus.Action);
                    }
                }
            }

            event.preventDefault();
            handledCase = true;
            break;

        case RemoteKeyCode.Stop:
            window.open('../main_window/index.html', '_self');
            handledCase = true;
            key = 'Stop';
            break;

        // Note that in simulator rewind and fast forward key codes are sent twice...
        case RemoteKeyCode.Rewind:
            skipBack(event.repeat);
            event.preventDefault();
            handledCase = true;
            key = 'Rewind';
            break;

        case RemoteKeyCode.Play:
            if (player.isPaused()) {
                player.play();
            }
            event.preventDefault();
            handledCase = true;
            key = 'Play';
            break;
        case RemoteKeyCode.Pause:
            if (!player.isPaused()) {
                player.pause();
            }
            event.preventDefault();
            handledCase = true;
            key = 'Pause';
            break;

        // Default behavior is to bring up a secondary menu where the user
        // can use the arrow keys for other media controls, so don't handle
        // this key manually
        // case RemoteKeyCode.MediaPlayPause:
        //     if (!player.isPaused()) {
        //         player.pause();
        //     }
        //     else {
        //         player.play();
        //     }
        //     event.preventDefault();
        //     handledCase = true;
        //     break;

        // Note that in simulator rewind and fast forward key codes are sent twice...
        case RemoteKeyCode.FastForward:
            skipForward(event.repeat);
            event.preventDefault();
            handledCase = true;
            key = 'FastForward';
            break;

        case RemoteKeyCode.Back:
            window.open('../main_window/index.html', '_self');
            event.preventDefault();
            handledCase = true;
            key = 'Back';
            break;

        default:
            break;
    }

    return { handledCase: handledCase, key: key };
}

export function targetKeyUpEventListener(event: KeyboardEvent): { handledCase: boolean, key: string } {
    return common.targetKeyUpEventListener(event);
};

if (window.tizenOSAPI.pendingPlay !== null) {
    if (window.tizenOSAPI.pendingPlay.rendererEvent === 'play-playlist') {
        onPlayPlaylist(null, window.tizenOSAPI.pendingPlay.rendererMessage);
    }
    else {
        onPlay(null, window.tizenOSAPI.pendingPlay.rendererMessage);
    }
}

export {
    captionsBaseHeightCollapsed,
    captionsBaseHeightExpanded,
    captionsLineHeight,
}
