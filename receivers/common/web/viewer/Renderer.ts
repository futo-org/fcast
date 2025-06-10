import { EventMessage, EventType, KeyEvent, MediaItem, MediaItemEvent, PlaylistContent, PlayMessage, SeekMessage, SetPlaylistItemMessage, SetSpeedMessage, SetVolumeMessage } from 'common/Packets';
import { mediaItemFromPlayMessage, playMessageFromMediaItem } from 'common/UtilityFrontend';
import { supportedImageTypes } from 'common/MimeTypes';
import * as connectionMonitor from 'common/ConnectionMonitor';
import { toast, ToastIcon } from 'common/components/Toast';
import {
    targetPlayerCtrlStateUpdate,
    targetKeyDownEventListener,
} from 'src/viewer/Renderer';

const logger = window.targetAPI.logger;

const idleBackground = document.getElementById('video-player');
const idleIcon = document.getElementById('title-icon');
// todo: add callbacks for on-load events for image and generic content viewer
const loadingSpinner = document.getElementById('loading-spinner');
const imageViewer = document.getElementById('viewer-image') as HTMLImageElement;
const genericViewer = document.getElementById('viewer-generic') as HTMLIFrameElement;
let cachedPlaylist: PlaylistContent = null;
let cachedPlayMediaItem: MediaItem = null;
let showDurationTimeout: number = null;
let playlistIndex = 0;
let isMediaItem = false;
let playItemCached = false;

function onPlay(_event, value: PlayMessage) {
    if (!playItemCached) {
        cachedPlayMediaItem = mediaItemFromPlayMessage(value);
        isMediaItem = false;
    }
    window.targetAPI.sendEvent(new EventMessage(Date.now(), new MediaItemEvent(EventType.MediaItemChange, cachedPlayMediaItem)));
    logger.info('Media playback changed:', cachedPlayMediaItem);
    playItemCached = false;

    window.targetAPI.sendEvent(new EventMessage(Date.now(), new MediaItemEvent(EventType.MediaItemChange, cachedPlayMediaItem)));
    const src = value.url ? value.url : value.content;

    if (src && value.container && supportedImageTypes.find(v => v === value.container.toLocaleLowerCase()) && imageViewer) {
        logger.info('Loading image viewer');

        genericViewer.style.display = 'none';
        genericViewer.src = '';
        idleBackground.style.display = 'none';
        idleIcon.style.display = 'none';

        imageViewer.src = src;
        imageViewer.style.display = 'block';
    }
    else if (src && genericViewer) {
        logger.info('Loading generic viewer');

        imageViewer.style.display = 'none';
        imageViewer.src = '';
        idleBackground.style.display = 'none';
        idleIcon.style.display = 'none';

        genericViewer.src = src;
        genericViewer.style.display = 'block';
    } else {
        logger.error('Error loading content');

        imageViewer.style.display = 'none';
        imageViewer.src = '';

        genericViewer.style.display = 'none';
        genericViewer.src = '';

        idleBackground.style.display = 'block';
        idleIcon.style.display = 'block';
    }

    if (isMediaItem && cachedPlayMediaItem.showDuration && cachedPlayMediaItem.showDuration > 0) {
        showDurationTimeout = window.setTimeout(() => {
            playlistIndex++;

            if (playlistIndex < cachedPlaylist.items.length) {
                cachedPlayMediaItem = cachedPlaylist.items[playlistIndex];
                playItemCached = true;
                window.targetAPI.sendPlayRequest(playMessageFromMediaItem(cachedPlaylist.items[playlistIndex]), playlistIndex);
            }
            else {
                logger.info('End of playlist');
                imageViewer.style.display = 'none';
                imageViewer.src = '';

                genericViewer.style.display = 'none';
                genericViewer.src = '';

                idleBackground.style.display = 'block';
                idleIcon.style.display = 'block';
            }
        }, cachedPlayMediaItem.showDuration * 1000);
    }
};

function onPlayPlaylist(_event, value: PlaylistContent) {
    logger.info('Handle play playlist message', JSON.stringify(value));
    cachedPlaylist = value;

    const offset = value.offset ? value.offset : 0;
    const volume = value.items[offset].volume ? value.items[offset].volume : value.volume;
    const speed = value.items[offset].speed ? value.items[offset].speed : value.speed;
    const playMessage = new PlayMessage(
        value.items[offset].container, value.items[offset].url, value.items[offset].content,
        value.items[offset].time, volume, speed, value.items[offset].headers, value.items[offset].metadata
    );

    isMediaItem = true;
    cachedPlayMediaItem = value.items[offset];
    playItemCached = true;
    window.targetAPI.sendPlayRequest(playMessage, playlistIndex);
}

window.targetAPI.onPause(() => { logger.warn('onPause handler invoked for generic content viewer'); });
window.targetAPI.onResume(() => { logger.warn('onResume handler invoked for generic content viewer'); });
window.targetAPI.onSeek((_event, value: SeekMessage) => { logger.warn('onSeek handler invoked for generic content viewer'); });
window.targetAPI.onSetVolume((_event, value: SetVolumeMessage) => { logger.warn('onSetVolume handler invoked for generic content viewer'); });
window.targetAPI.onSetSpeed((_event, value: SetSpeedMessage) => { logger.warn('onSetSpeed handler invoked for generic content viewer'); });
window.targetAPI.onSetPlaylistItem((_event, value: SetPlaylistItemMessage) => {
    if (value.itemIndex >= 0 && value.itemIndex < cachedPlaylist.items.length) {
        logger.info(`Setting playlist item to index ${value.itemIndex}`);
        playlistIndex = value.itemIndex;
        cachedPlayMediaItem = cachedPlaylist.items[playlistIndex];
        playItemCached = true;
        window.targetAPI.sendPlayRequest(playMessageFromMediaItem(cachedPlaylist.items[playlistIndex]), playlistIndex);

        if (showDurationTimeout) {
            window.clearTimeout(showDurationTimeout);
            showDurationTimeout = null;
        }
    }
    else {
        logger.warn(`Playlist index out of bounds ${value.itemIndex}, ignoring...`);
    }
});

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
window.targetAPI.onPlayPlaylist(onPlayPlaylist);

enum PlayerControlEvent {
    Load,
    Pause,
    Play,
    VolumeChange,
    TimeUpdate,
    UiFadeOut,
    UiFadeIn,
    SetCaptions,
    ToggleSpeedMenu,
    SetPlaybackRate,
    ToggleFullscreen,
    ExitFullscreen,
}

// UI update handlers
function playerCtrlStateUpdate(event: PlayerControlEvent) {
    const handledCase = targetPlayerCtrlStateUpdate(event);
    if (handledCase) {
        return;
    }

    switch (event) {
        case PlayerControlEvent.Load: {
            break;
        }

        case PlayerControlEvent.UiFadeOut: {
            break;
        }

        case PlayerControlEvent.UiFadeIn: {
            break;
        }

        default:
            break;
    }
}

document.addEventListener('keydown', (event: KeyboardEvent) => {
    // logger.info("KeyDown", event);
    let handledCase = targetKeyDownEventListener(event);

    if (!handledCase) {
        switch (event.code) {
            case 'ArrowLeft': {
                // skipBack();
                // event.preventDefault();
                // handledCase = true;

                // const value = { itemIndex: playlistIndex - 1 };
                // if (value.itemIndex >= 0 && value.itemIndex < cachedPlaylist.items.length) {
                //     logger.info(`Setting playlist item to index ${value.itemIndex}`);
                //     playlistIndex = value.itemIndex;
                //     cachedPlayMediaItem = cachedPlaylist.items[playlistIndex];
                //     playItemCached = true;
                //     window.targetAPI.sendPlayRequest(playMessageFromMediaItem(cachedPlaylist.items[playlistIndex]), playlistIndex);

                //     if (showDurationTimeout) {
                //         window.clearTimeout(showDurationTimeout);
                //         showDurationTimeout = null;
                //     }
                // }
                // else {
                //     logger.warn(`Playlist index out of bounds ${value.itemIndex}, ignoring...`);
                // }

                break;
            }
            case 'ArrowRight': {
                // skipForward();
                // event.preventDefault();
                // handledCase = true;

                // const value = { itemIndex: playlistIndex + 1 };
                // if (value.itemIndex >= 0 && value.itemIndex < cachedPlaylist.items.length) {
                //     logger.info(`Setting playlist item to index ${value.itemIndex}`);
                //     playlistIndex = value.itemIndex;
                //     cachedPlayMediaItem = cachedPlaylist.items[playlistIndex];
                //     playItemCached = true;
                //     window.targetAPI.sendPlayRequest(playMessageFromMediaItem(cachedPlaylist.items[playlistIndex]), playlistIndex);

                //     if (showDurationTimeout) {
                //         window.clearTimeout(showDurationTimeout);
                //         showDurationTimeout = null;
                //     }
                // }
                // else {
                //     logger.warn(`Playlist index out of bounds ${value.itemIndex}, ignoring...`);
                // }

                break;
            }

            default:
                break;
        }
    }

    if (window.targetAPI.getSubscribedKeys().keyDown.has(event.key)) {
        window.targetAPI.sendEvent(new EventMessage(Date.now(), new KeyEvent(EventType.KeyDown, event.key, event.repeat, handledCase)));
    }
});
document.addEventListener('keyup', (event: KeyboardEvent) => {
    if (window.targetAPI.getSubscribedKeys().keyUp.has(event.key)) {
        window.targetAPI.sendEvent(new EventMessage(Date.now(), new KeyEvent(EventType.KeyUp, event.key, event.repeat, false)));
    }
});

export {
    PlayerControlEvent,
    onPlay,
    playerCtrlStateUpdate,
};
