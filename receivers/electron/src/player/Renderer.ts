import { videoElement, PlayerControlEvent, playerCtrlStateUpdate } from 'common/player/Renderer';

const playerCtrlFullscreen = document.getElementById("fullscreen");
playerCtrlFullscreen.onclick = () => { playerCtrlStateUpdate(PlayerControlEvent.ToggleFullscreen); };
videoElement.ondblclick = () => { playerCtrlStateUpdate(PlayerControlEvent.ToggleFullscreen); };

export function targetPlayerCtrlStateUpdate(event: PlayerControlEvent) {
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

            break;
        }

        case PlayerControlEvent.ExitFullscreen:
            window.electronAPI.exitFullScreen();
            playerCtrlFullscreen.setAttribute("class", "fullscreen_off");
            break;

        default:
            break;
    }
}

// eslint-disable-next-line @typescript-eslint/no-explicit-any
export function targetKeyDownEventListener(event: any) {
    switch (event.code) {
        case 'KeyF':
        case 'F11':
            playerCtrlStateUpdate(PlayerControlEvent.ToggleFullscreen);
            event.preventDefault();
            break;
        case 'Escape':
            playerCtrlStateUpdate(PlayerControlEvent.ExitFullscreen);
            event.preventDefault();
            break;
        default:
            break;
    }
};
