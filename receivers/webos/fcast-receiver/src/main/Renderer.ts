import 'common/main/Renderer';

const backgroundVideo = document.getElementById('video-player');
const loadingScreen = document.getElementById('loading-screen');
let backgroundVideoLoaded = false;
let qrCodeRendered = false;

backgroundVideo.onplaying = () => {
    backgroundVideoLoaded = true;

    if (backgroundVideoLoaded && qrCodeRendered) {
        loadingScreen.style.display = 'none';
    }
};

export function onQRCodeRendered() {
    qrCodeRendered = true;

    if (backgroundVideoLoaded && qrCodeRendered) {
        loadingScreen.style.display = 'none';
    }
}
