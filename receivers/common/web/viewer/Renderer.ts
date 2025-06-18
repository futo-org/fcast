import { EventMessage, EventType, GenericMediaMetadata, KeyEvent, MediaItem, MediaItemEvent, MetadataType, PlaybackState, PlaylistContent, PlayMessage, SeekMessage, SetPlaylistItemMessage, SetSpeedMessage, SetVolumeMessage } from 'common/Packets';
import { mediaItemFromPlayMessage, playMessageFromMediaItem, Timer } from 'common/UtilityFrontend';
import { supportedImageTypes } from 'common/MimeTypes';
import * as connectionMonitor from 'common/ConnectionMonitor';
import { toast, ToastIcon } from 'common/components/Toast';
import {
    targetPlayerCtrlStateUpdate,
    targetKeyDownEventListener,
} from 'src/viewer/Renderer';

const logger = window.targetAPI.logger;

// HTML elements
const idleBackground = document.getElementById('idleBackground');
const idleIcon = document.getElementById('titleIcon');
const loadingSpinner = document.getElementById('loadingSpinner');
const imageViewer = document.getElementById('viewerImage') as HTMLImageElement;
const genericViewer = document.getElementById('viewerGeneric') as HTMLIFrameElement;

const mediaTitle = document.getElementById("mediaTitle");
const playerControls = document.getElementById("controls");

const playerCtrlPlayPrevious = document.getElementById("playPrevious");
const playerCtrlAction = document.getElementById("action");
const playerCtrlPlaylistLength = document.getElementById("playlistLength");
const playerCtrlPlayNext = document.getElementById("playNext");

let cachedPlaylist: PlaylistContent = null;
let cachedPlayMediaItem: MediaItem = null;
let playlistIndex = 0;
let isMediaItem = false;
let playItemCached = false;
let imageViewerPlaybackState: PlaybackState = PlaybackState.Idle;

let uiHideTimer = new Timer(() => {
    uiVisible = false;
    playerCtrlStateUpdate(PlayerControlEvent.UiFadeOut);
}, 3000);
let loadingTimer = new Timer(() => { loadingSpinner.style.display = 'block'; }, 100, false);

let showDurationTimer = new Timer(() => {
    if (playlistIndex < cachedPlaylist.items.length - 1) {
        setPlaylistItem(playlistIndex + 1);
    }
    else {
        logger.info('End of playlist');
        imageViewer.style.display = 'none';
        imageViewer.src = '';

        genericViewer.style.display = 'none';
        genericViewer.src = '';

        idleBackground.style.display = 'block';
        idleIcon.style.display = 'block';

        playerCtrlAction.setAttribute("class", "play iconSize");
        imageViewerPlaybackState = PlaybackState.Idle;
    }
}, 0, false);

function onPlay(_event, value: PlayMessage) {
    if (!playItemCached) {
        cachedPlayMediaItem = mediaItemFromPlayMessage(value);
        isMediaItem = false;
    }
    window.targetAPI.sendEvent(new EventMessage(Date.now(), new MediaItemEvent(EventType.MediaItemChange, cachedPlayMediaItem)));
    logger.info('Media playback changed:', cachedPlayMediaItem);
    playItemCached = false;
    showDurationTimer.stop();

    window.targetAPI.sendEvent(new EventMessage(Date.now(), new MediaItemEvent(EventType.MediaItemChange, cachedPlayMediaItem)));
    const src = value.url ? value.url : value.content;

    loadingTimer.start();
    if (src && value.container && supportedImageTypes.find(v => v === value.container.toLocaleLowerCase()) && imageViewer) {
        logger.info('Loading image viewer');
        imageViewer.onload = (ev) => {
            loadingTimer.stop();
            loadingSpinner.style.display = 'none';
            playerCtrlStateUpdate(PlayerControlEvent.Load);
        };

        genericViewer.onload = (ev) => {};

        genericViewer.style.display = 'none';
        genericViewer.src = '';
        idleBackground.style.display = 'none';
        idleIcon.style.display = 'none';

        imageViewer.src = src;
        imageViewer.style.display = 'block';
        playerControls.style.display = 'block';
    }
    else if (src && genericViewer) {
        logger.info('Loading generic viewer');
        imageViewer.onload = (ev) => {};

        genericViewer.onload = (ev) => {
            loadingTimer.stop();
            loadingSpinner.style.display = 'none';
            playerCtrlStateUpdate(PlayerControlEvent.Load);
        };

        imageViewer.style.display = 'none';
        imageViewer.src = '';
        playerControls.style.display = 'none';
        idleBackground.style.display = 'none';
        idleIcon.style.display = 'none';

        genericViewer.src = src;
        genericViewer.style.display = 'block';
    } else {
        logger.error('Error loading content');
        loadingTimer.stop();
        loadingSpinner.style.display = 'none';
        imageViewer.onload = (ev) => {};
        genericViewer.onload = (ev) => {};

        imageViewer.style.display = 'none';
        imageViewer.src = '';
        playerControls.style.display = 'none';

        genericViewer.style.display = 'none';
        genericViewer.src = '';

        idleBackground.style.display = 'block';
        idleIcon.style.display = 'block';
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

function setPlaylistItem(index: number) {
    if (index === -1) {
        logger.info('Looping playlist to end');
        index = cachedPlaylist.items.length - 1;

    }
    else if (index === cachedPlaylist.items.length) {
        logger.info('Looping playlist to start');
        index = 0;
    }

    if (index >= 0 && index < cachedPlaylist.items.length) {
        logger.info(`Setting playlist item to index ${index}`);
        playlistIndex = index;
        cachedPlayMediaItem = cachedPlaylist.items[playlistIndex];
        playItemCached = true;
        window.targetAPI.sendPlayRequest(playMessageFromMediaItem(cachedPlaylist.items[playlistIndex]), playlistIndex);
        showDurationTimer.stop();
    }
    else {
        logger.warn(`Playlist index out of bounds ${index}, ignoring...`);
    }

    playerCtrlPlaylistLength.textContent= `${playlistIndex+1} of ${cachedPlaylist.items.length}`;
}

window.targetAPI.onPause(() => { logger.warn('onPause handler invoked for generic content viewer'); });
window.targetAPI.onResume(() => { logger.warn('onResume handler invoked for generic content viewer'); });
window.targetAPI.onSeek((_event, value: SeekMessage) => { logger.warn('onSeek handler invoked for generic content viewer'); });
window.targetAPI.onSetVolume((_event, value: SetVolumeMessage) => { logger.warn('onSetVolume handler invoked for generic content viewer'); });
window.targetAPI.onSetSpeed((_event, value: SetSpeedMessage) => { logger.warn('onSetSpeed handler invoked for generic content viewer'); });
window.targetAPI.onSetPlaylistItem((_event, value: SetPlaylistItemMessage) => { setPlaylistItem(value.itemIndex); });

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
    UiFadeOut,
    UiFadeIn,
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
            if (isMediaItem) {
                playerCtrlPlayPrevious.style.display = 'block';
                playerCtrlPlayNext.style.display = 'block';

                if (cachedPlayMediaItem.showDuration && cachedPlayMediaItem.showDuration > 0) {
                    playerCtrlAction.style.display = 'block';
                    playerCtrlPlaylistLength.style.display = 'none';

                    if (imageViewerPlaybackState === PlaybackState.Idle || imageViewerPlaybackState === PlaybackState.Playing) {
                        showDurationTimer.start(cachedPlayMediaItem.showDuration * 1000);
                        playerCtrlAction.setAttribute("class", "pause iconSize");
                        imageViewerPlaybackState = PlaybackState.Playing;
                    }
                }
                else {
                    playerCtrlAction.style.display = 'none';
                    playerCtrlPlaylistLength.textContent= `${playlistIndex+1} of ${cachedPlaylist.items.length}`;
                    playerCtrlPlaylistLength.style.display = 'block';
                }
            }
            else {
                playerCtrlPlayPrevious.style.display = 'none';
                playerCtrlPlayNext.style.display = 'none';
                playerCtrlAction.style.display = 'none';
                playerCtrlPlaylistLength.style.display = 'none';
            }

            if (cachedPlayMediaItem.metadata && cachedPlayMediaItem.metadata?.type === MetadataType.Generic) {
                const metadata = cachedPlayMediaItem.metadata as GenericMediaMetadata;

                if (metadata.title) {
                    mediaTitle.innerHTML = metadata.title;
                }
            }

            break;
        }

        case PlayerControlEvent.Pause:
            playerCtrlAction.setAttribute("class", "play iconSize");
            imageViewerPlaybackState = PlaybackState.Paused;
            showDurationTimer.pause();
            break;

        case PlayerControlEvent.Play:
            playerCtrlAction.setAttribute("class", "pause iconSize");

            if (imageViewerPlaybackState === PlaybackState.Idle) {
                setPlaylistItem(0);
            }
            else {
                if (showDurationTimer.started) {
                    showDurationTimer.resume();
                }
                else {
                    showDurationTimer.start(cachedPlayMediaItem.showDuration * 1000);
                }

                imageViewerPlaybackState = PlaybackState.Playing;
            }
            break;

        case PlayerControlEvent.UiFadeOut: {
            document.body.style.cursor = "none";
            playerControls.style.opacity = '0';
            break;
        }

        case PlayerControlEvent.UiFadeIn: {
            document.body.style.cursor = "default";
            playerControls.style.opacity = '1';
            break;
        }

        default:
            break;
    }
}

// Receiver generated event handlers
playerCtrlAction.onclick = () => {
    if (imageViewerPlaybackState === PlaybackState.Paused || imageViewerPlaybackState === PlaybackState.Idle) {
        playerCtrlStateUpdate(PlayerControlEvent.Play);
    } else {
        playerCtrlStateUpdate(PlayerControlEvent.Pause);
    }
};

playerCtrlPlayPrevious.onclick = () => { setPlaylistItem(playlistIndex - 1); }
playerCtrlPlayNext.onclick = () => { setPlaylistItem(playlistIndex + 1); }

// Component hiding
let uiVisible = true;

function stopUiHideTimer() {
    uiHideTimer.stop();

    if (!uiVisible) {
        uiVisible = true;
        playerCtrlStateUpdate(PlayerControlEvent.UiFadeIn);
    }
}

document.onmouseout = () => {
    uiHideTimer.stop();
    uiVisible = false;
    playerCtrlStateUpdate(PlayerControlEvent.UiFadeOut);
}

document.onmousemove = () => {
    stopUiHideTimer();
    uiHideTimer.start();
};

function keyDownEventListener(event: KeyboardEvent) {
    // logger.info("KeyDown", event);
    let handledCase = targetKeyDownEventListener(event);

    if (!handledCase) {
        switch (event.code) {
            case 'ArrowLeft':
                setPlaylistItem(playlistIndex - 1);
                event.preventDefault();
                handledCase = true;
                break;
            case 'ArrowRight':
                setPlaylistItem(playlistIndex + 1);
                event.preventDefault();
                handledCase = true;
                break;
            case "Home":
                setPlaylistItem(0);
                event.preventDefault();
                handledCase = true;
                break;
            case "End":
                setPlaylistItem(cachedPlaylist.items.length - 1);
                event.preventDefault();
                handledCase = true;
                break;
            case 'KeyK':
            case 'Space':
            case 'Enter':
                // Play/pause toggle
                if (imageViewerPlaybackState === PlaybackState.Paused || imageViewerPlaybackState === PlaybackState.Idle) {
                    playerCtrlStateUpdate(PlayerControlEvent.Play);
                } else {
                    playerCtrlStateUpdate(PlayerControlEvent.Pause);
                }
                event.preventDefault();
                handledCase = true;
                break;
            default:
                break;
        }
    }

    if (window.targetAPI.getSubscribedKeys().keyDown.has(event.key)) {
        window.targetAPI.sendEvent(new EventMessage(Date.now(), new KeyEvent(EventType.KeyDown, event.key, event.repeat, handledCase)));
    }
}

document.addEventListener('keydown', keyDownEventListener);
document.addEventListener('keyup', (event: KeyboardEvent) => {
    if (window.targetAPI.getSubscribedKeys().keyUp.has(event.key)) {
        window.targetAPI.sendEvent(new EventMessage(Date.now(), new KeyEvent(EventType.KeyUp, event.key, event.repeat, false)));
    }
});

export {
    PlayerControlEvent,
    idleBackground,
    idleIcon,
    imageViewer,
    genericViewer,
    onPlay,
    playerCtrlStateUpdate,
};
