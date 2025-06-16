import { videoElement, PlayerControlEvent, playerCtrlStateUpdate, idleBackground, thumbnailImage, idleIcon } from 'common/player/Renderer';

const captionsBaseHeightCollapsed = 75;
const captionsBaseHeightExpanded = 160;
const captionsLineHeight = 34;

const playerCtrlFullscreen = document.getElementById("fullscreen");
playerCtrlFullscreen.onclick = () => { playerCtrlStateUpdate(PlayerControlEvent.ToggleFullscreen); };
videoElement.ondblclick = () => { playerCtrlStateUpdate(PlayerControlEvent.ToggleFullscreen); };
idleBackground.ondblclick = () => { playerCtrlStateUpdate(PlayerControlEvent.ToggleFullscreen); };
thumbnailImage.ondblclick = () => { playerCtrlStateUpdate(PlayerControlEvent.ToggleFullscreen); };
idleIcon.ondblclick = () => { playerCtrlStateUpdate(PlayerControlEvent.ToggleFullscreen); };

export function targetPlayerCtrlStateUpdate(event: PlayerControlEvent): boolean {
    let handledCase = false;

    switch (event) {
        case PlayerControlEvent.ToggleFullscreen: {
            window.electronAPI.toggleFullScreen();

            window.electronAPI.isFullScreen().then((isFullScreen: boolean) => {
                if (isFullScreen) {
                    playerCtrlFullscreen.setAttribute("class", "fullscreen_on");
                } else {
                    playerCtrlFullscreen.setAttribute("class", "fullscreen_off");
                }
            });

            handledCase = true;
            break;
        }

        case PlayerControlEvent.ExitFullscreen:
            window.electronAPI.exitFullScreen();
            playerCtrlFullscreen.setAttribute("class", "fullscreen_off");

            handledCase = true;
            break;

        default:
            break;
    }

    return handledCase;
}

export function targetKeyDownEventListener(event: KeyboardEvent): boolean {
    let handledCase = false;

    switch (event.code) {
        case 'KeyF':
        case 'F11':
            playerCtrlStateUpdate(PlayerControlEvent.ToggleFullscreen);
            event.preventDefault();
            handledCase = true;
            break;
        case 'Escape':
            playerCtrlStateUpdate(PlayerControlEvent.ExitFullscreen);
            event.preventDefault();
            handledCase = true;
            break;
        default:
            break;
    }

    return handledCase
};

export {
    captionsBaseHeightCollapsed,
    captionsBaseHeightExpanded,
    captionsLineHeight,
}
