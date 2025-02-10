import 'common/main/Renderer';

const backgroundVideo = document.getElementById('video-player');
const loadingScreen = document.getElementById('loading-screen');

// WebOS 6.0 requires global scope for access during callback invocation
// eslint-disable-next-line no-var
var backgroundVideoLoaded: boolean;
// eslint-disable-next-line no-var
var qrCodeRendered: boolean;

backgroundVideo.onplaying = () => {
    backgroundVideoLoaded = true;

    if (backgroundVideoLoaded && qrCodeRendered) {
        loadingScreen.style.display = 'none';
        backgroundVideo.onplaying = null;
    }
};

export function onQRCodeRendered() {
    qrCodeRendered = true;

    if (backgroundVideoLoaded && qrCodeRendered) {
        loadingScreen.style.display = 'none';
    }
}
