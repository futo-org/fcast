import 'common/main/Renderer';

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

export function onQRCodeRendered() {
    qrCodeRendered = true;
}
