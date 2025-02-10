import {
    isLive,
    onPlay,
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

const captionsBaseHeightCollapsed = 150;
const captionsBaseHeightExpanded = 320;
const captionsLineHeight = 68;

enum RemoteKeyCode {
    Stop = 413,
    Rewind = 412,
    Play = 415,
    Pause = 19,
    FastForward = 417,
    Back = 461,
}

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

// eslint-disable-next-line @typescript-eslint/no-explicit-any
export function targetKeyDownEventListener(event: any): boolean {
    let handledCase = false;

    switch (event.keyCode) {
        case RemoteKeyCode.Stop:
            // history.back();
            window.open('../main_window/index.html');
            handledCase = true;
            break;

        case RemoteKeyCode.Rewind:
            skipBack();
            event.preventDefault();
            handledCase = true;
            break;

        case RemoteKeyCode.Play:
            if (player.isPaused()) {
                player.play();
            }
            event.preventDefault();
            handledCase = true;
            break;
        case RemoteKeyCode.Pause:
            if (!player.isPaused()) {
                player.pause();
            }
            event.preventDefault();
            handledCase = true;
            break;

        case RemoteKeyCode.FastForward:
            skipForward();
            event.preventDefault();
            handledCase = true;
            break;

        // WebOS 22 and earlier does not work well using the history API,
        // so manually handling page navigation...
        case RemoteKeyCode.Back:
            // history.back();
            window.open('../main_window/index.html');
            event.preventDefault();
            handledCase = true;
            break;

        default:
            break;
    }

    return handledCase;
};

if (window.webOSAPI.pendingPlay !== null) {
    onPlay(null, window.webOSAPI.pendingPlay);
}

export {
    captionsBaseHeightCollapsed,
    captionsBaseHeightExpanded,
    captionsLineHeight,
}
