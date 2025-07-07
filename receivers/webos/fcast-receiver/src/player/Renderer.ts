import {
    isLive,
    onPlay,
    onPlayPlaylist,
    player,
    PlayerControlEvent,
    playerCtrlCaptions,
    playerCtrlDuration,
    playerCtrlLiveBadge,
    playerCtrlPosition,
    playerCtrlProgressBar,
    playerCtrlProgressBarBuffer,
    playerCtrlProgressBarHandle,
    playerCtrlProgressBarProgress,
    playerCtrlStateUpdate,
    playerCtrlVolumeBar,
    playerCtrlVolumeBarHandle,
    playerCtrlVolumeBarProgress,
    videoCaptions,
    formatDuration,
    skipBack,
    skipForward,
} from 'common/player/Renderer';
import { RemoteKeyCode } from 'lib/common';
import * as common from 'lib/common';

const captionsBaseHeightCollapsed = 150;
const captionsBaseHeightExpanded = 320;
const captionsLineHeight = 68;

export function targetPlayerCtrlStateUpdate(event: PlayerControlEvent): boolean {
    let handledCase = false;

    switch (event) {
        case PlayerControlEvent.Load: {
            playerCtrlProgressBarBuffer.setAttribute("style", "width: 0px");
            playerCtrlProgressBarProgress.setAttribute("style", "width: 0px");
            playerCtrlProgressBarHandle.setAttribute("style", `left: ${playerCtrlProgressBar.offsetLeft}px`);

            const volume = Math.round(player.getVolume() * playerCtrlVolumeBar.offsetWidth);
            playerCtrlVolumeBarProgress.setAttribute("style", `width: ${volume}px`);
            playerCtrlVolumeBarHandle.setAttribute("style", `left: ${volume + 8}px`);

            if (isLive) {
                playerCtrlLiveBadge.setAttribute("style", "display: block");
                playerCtrlPosition.setAttribute("style", "display: none");
                playerCtrlDuration.setAttribute("style", "display: none");
            }
            else {
                playerCtrlLiveBadge.setAttribute("style", "display: none");
                playerCtrlPosition.setAttribute("style", "display: block");
                playerCtrlDuration.setAttribute("style", "display: block");
                playerCtrlPosition.textContent = formatDuration(player.getCurrentTime());
                playerCtrlDuration.innerHTML = formatDuration(player.getDuration());
            }

            if (player.isCaptionsSupported()) {
                // Disabling receiver captions control on TV players
                playerCtrlCaptions.setAttribute("style", "display: none");
                // playerCtrlCaptions.setAttribute("style", "display: block");
                videoCaptions.setAttribute("style", "display: block");
            }
            else {
                playerCtrlCaptions.setAttribute("style", "display: none");
                videoCaptions.setAttribute("style", "display: none");
                player.enableCaptions(false);
            }
            playerCtrlStateUpdate(PlayerControlEvent.SetCaptions);

            handledCase = true;
            break;
        }

        default:
            break;
    }

    return handledCase;
}

export function targetKeyDownEventListener(event: KeyboardEvent): { handledCase: boolean, key: string }  {
    let handledCase = false;
    let key = '';

    switch (event.keyCode) {
        case RemoteKeyCode.Stop:
            // history.back();
            window.open('../main_window/index.html', '_self');
            handledCase = true;
            key = 'Stop';
            break;

        case RemoteKeyCode.Rewind:
            skipBack();
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

        case RemoteKeyCode.FastForward:
            skipForward();
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

export {
    captionsBaseHeightCollapsed,
    captionsBaseHeightExpanded,
    captionsLineHeight,
}
