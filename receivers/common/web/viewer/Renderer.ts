import { PlayMessage, SeekMessage, SetSpeedMessage, SetVolumeMessage } from 'common/Packets';
import { supportedImageTypes } from 'common/MimeTypes';
import * as connectionMonitor from '../ConnectionMonitor';
import { toast, ToastIcon } from '../components/Toast';
const logger = window.targetAPI.logger;



const imageViewer = document.getElementById('viewer-image') as HTMLImageElement;
const genericViewer = document.getElementById('viewer-generic') as HTMLIFrameElement;

function onPlay(_event, value: PlayMessage) {
    logger.info("Handle play message renderer", JSON.stringify(value));
    const src = value.url ? value.url : value.content;

    if (src && value.container && supportedImageTypes.find(v => v === value.container.toLocaleLowerCase()) && imageViewer) {
        logger.info("Loading image viewer");

        genericViewer.style.display = "none";
        genericViewer.src = "";

        imageViewer.src = src;
        imageViewer.style.display = "block";
    }
    else if (src && genericViewer) {
        logger.info("Loading generic viewer");

        imageViewer.style.display = "none";
        imageViewer.src = "";

        genericViewer.src = src;
        genericViewer.style.display = "block";
    } else {
        logger.error("Error loading content");

        imageViewer.style.display = "none";
        imageViewer.src = "";

        genericViewer.style.display = "none";
        genericViewer.src = "";
    }
};

window.targetAPI.onPause(() => { logger.warn('onPause handler invoked for generic content viewer'); });
window.targetAPI.onResume(() => { logger.warn('onResume handler invoked for generic content viewer'); });
window.targetAPI.onSeek((_event, value: SeekMessage) => { logger.warn('onSeek handler invoked for generic content viewer'); });
window.targetAPI.onSetVolume((_event, value: SetVolumeMessage) => { logger.warn('onSetVolume handler invoked for generic content viewer'); });
window.targetAPI.onSetSpeed((_event, value: SetSpeedMessage) => { logger.warn('onSetSpeed handler invoked for generic content viewer'); });

connectionMonitor.setUiUpdateCallbacks({
    onConnect: (connections: string[], initialUpdate: boolean = false) => {
        if (!initialUpdate) {
            toast('Device connected', ToastIcon.INFO);
        }
    },
    onDisconnect: (connections: string[]) => {
        toast('Device disconnected. If you experience playback issues, please reconnect.', ToastIcon.INFO);
    },
});

window.targetAPI.onPlay(onPlay);
