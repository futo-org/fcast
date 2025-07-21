import  { keyDownEventHandler, keyUpEventHandler } from 'common/main/Renderer';
import { RemoteKeyCode } from 'lib/common';
import * as common from 'lib/common';

const backgroundVideo = document.getElementById('video-player');
const loadingScreen = document.getElementById('loading-screen');

// WebOS 6.0 requires global scope for access during callback invocation
// eslint-disable-next-line no-var
var backgroundVideoLoaded: boolean;
// eslint-disable-next-line no-var
var qrCodeRendered: boolean;
// eslint-disable-next-line no-var
var loadPollCount = 0;

// eslint-disable-next-line no-var
var loadScreenDone = setInterval(() => {
    // Show main screen regardless if resources not loaded within 10s
    if ((backgroundVideoLoaded && qrCodeRendered) || loadPollCount > 10) {
        clearInterval(loadScreenDone);
        loadingScreen.style.display = 'none';
    }

    loadPollCount++;
}, 1000);

backgroundVideo.onplaying = () => {
    backgroundVideoLoaded = true;
    backgroundVideo.onplaying = null;
};

window.parent.webOSApp.setKeyDownHandler(keyDownEventHandler);
window.parent.webOSApp.setKeyUpHandler(keyUpEventHandler);

export function onQRCodeRendered() {
    qrCodeRendered = true;
}

export function targetKeyDownEventListener(event: KeyboardEvent): { handledCase: boolean, key: string } {
    let handledCase = false;
    let key = '';

    switch (event.keyCode) {
        // Unhandled cases (used for replacing undefined key codes)
        case RemoteKeyCode.Stop:
            key = 'Stop';
            break;
        case RemoteKeyCode.Rewind:
            key = 'Rewind';
            break;
        case RemoteKeyCode.Play:
            key = 'Play';
            break;
        case RemoteKeyCode.Pause:
            key = 'Pause';
            break;
        case RemoteKeyCode.FastForward:
            key = 'FastForward';
            break;

        // Handled cases

        // WebOS 22 and earlier does not work well using the history API,
        // so manually handling page navigation...
        case RemoteKeyCode.Back:
            window.webOS.platformBack();
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
