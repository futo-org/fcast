import { PlayerControlEvent, playerCtrlStateUpdate, idleBackground, idleIcon, imageViewer, genericViewer } from 'common/viewer/Renderer';

const playerCtrlFullscreen = document.getElementById("fullscreen");
playerCtrlFullscreen.onclick = () => { playerCtrlStateUpdate(PlayerControlEvent.ToggleFullscreen); };
idleBackground.ondblclick = () => { playerCtrlStateUpdate(PlayerControlEvent.ToggleFullscreen); };
idleIcon.ondblclick = () => { playerCtrlStateUpdate(PlayerControlEvent.ToggleFullscreen); };
imageViewer.ondblclick = () => { playerCtrlStateUpdate(PlayerControlEvent.ToggleFullscreen); };
genericViewer.ondblclick = () => { playerCtrlStateUpdate(PlayerControlEvent.ToggleFullscreen); };

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

export function targetKeyDownEventListener(event: KeyboardEvent): { handledCase: boolean, key: string } {
    let handledCase = false;

    switch (event.key.toLowerCase()) {
        case 'f':
        case 'f11':
            playerCtrlStateUpdate(PlayerControlEvent.ToggleFullscreen);
            event.preventDefault();
            handledCase = true;
            break;
        case 'escape':
            playerCtrlStateUpdate(PlayerControlEvent.ExitFullscreen);
            event.preventDefault();
            handledCase = true;
            break;
        default:
            break;
    }

    return { handledCase: handledCase, key: event.key, };
};

export function targetKeyUpEventListener(_event: KeyboardEvent): { handledCase: boolean, key: string } {
    // unused in Electron currently
    return { handledCase: false, key: '', };
};
