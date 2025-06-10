import { PlayerControlEvent, playerCtrlStateUpdate } from 'common/viewer/Renderer';

export function targetPlayerCtrlStateUpdate(event: PlayerControlEvent): boolean {
    let handledCase = false;

    switch (event) {
        case PlayerControlEvent.ToggleFullscreen: {
            window.electronAPI.toggleFullScreen();

            // window.electronAPI.isFullScreen().then((isFullScreen: boolean) => {
            //     if (isFullScreen) {
            //         playerCtrlFullscreen.setAttribute("class", "fullscreen_on");
            //     } else {
            //         playerCtrlFullscreen.setAttribute("class", "fullscreen_off");
            //     }
            // });

            handledCase = true;
            break;
        }

        case PlayerControlEvent.ExitFullscreen:
            window.electronAPI.exitFullScreen();
            // playerCtrlFullscreen.setAttribute("class", "fullscreen_off");

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
