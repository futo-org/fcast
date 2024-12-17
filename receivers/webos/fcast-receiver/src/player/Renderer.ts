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
} from 'common/player/Renderer';

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

export {
    captionsBaseHeightCollapsed,
    captionsBaseHeightExpanded,
    captionsLineHeight,
}
