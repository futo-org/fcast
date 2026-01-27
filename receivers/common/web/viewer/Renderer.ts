import { EventMessage, EventType, GenericMediaMetadata, KeyEvent, MediaItem, MediaItemEvent, MetadataType, PlaybackState, PlaylistContent, PlayMessage, SeekMessage, SetPlaylistItemMessage, SetSpeedMessage, SetVolumeMessage, PlaybackUpdateMessage } from 'common/Packets';
import { mediaItemFromPlayMessage, playMessageFromMediaItem, Timer } from 'common/UtilityFrontend';
import { supportedImageTypes } from 'common/MimeTypes';
import * as connectionMonitor from 'common/ConnectionMonitor';
import { toast, ToastIcon } from 'common/components/Toast';
import {
    targetPlayerCtrlStateUpdate,
    targetPlayerCtrlPostStateUpdate,
    targetKeyDownEventListener,
    targetKeyUpEventListener,
} from 'src/viewer/Renderer';

const logger = window.targetAPI.logger;
window.targetAPI.initializeSubscribedKeys();

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
let isPlaylistPlayRequestCounter = 0;
let imageViewerPlaybackState: PlaybackState = PlaybackState.Idle;

let uiHideTimer = new Timer(() => { playerCtrlStateUpdate(PlayerControlEvent.UiFadeOut); }, 3000);
let loadingTimer = new Timer(() => { loadingSpinner.style.display = 'block'; }, 100, false);
let showDurationTimer = new Timer(mediaEndHandler, 0, false);

function sendPlaybackUpdate(updateState: PlaybackState) {
    const updateMessage = new PlaybackUpdateMessage(
        Date.now(),
        updateState,
        null,
        null,
        null,
        isMediaItem ? playlistIndex : null
    );
    imageViewerPlaybackState = updateState;

    window.targetAPI.sendPlaybackUpdate(updateMessage);
};

function onPlay(_event, value: PlayMessage) {
    if (isPlaylistPlayRequestCounter === 0) {
        cachedPlayMediaItem = mediaItemFromPlayMessage(value);
        isMediaItem = false;
    }
    window.targetAPI.sendEvent(new EventMessage(Date.now(), new MediaItemEvent(EventType.MediaItemChange, cachedPlayMediaItem)));
    logger.info('Media playback changed:', cachedPlayMediaItem);
    isPlaylistPlayRequestCounter = isPlaylistPlayRequestCounter <= 0 ? 0 : isPlaylistPlayRequestCounter - 1;
    showDurationTimer.stop();
    const src = value.url ? value.url : value.content;

    loadingTimer.start();
    if (src && value.container && supportedImageTypes.find(v => v === value.container.toLocaleLowerCase()) && imageViewer) {
        logger.info('Loading image viewer');
        imageViewer.onload = (ev) => {
            loadingTimer.stop();
            loadingSpinner.style.display = 'none';
            mediaPlayHandler();
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
            mediaPlayHandler();
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

    playlistIndex = offset;
    isMediaItem = true;
    cachedPlayMediaItem = value.items[offset];
    isPlaylistPlayRequestCounter++;
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
        isPlaylistPlayRequestCounter++;
        sendPlaybackUpdate(imageViewerPlaybackState);
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
                        sendPlaybackUpdate(PlaybackState.Playing);
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
            sendPlaybackUpdate(PlaybackState.Paused);
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

                mediaPlayHandler();
                sendPlaybackUpdate(PlaybackState.Playing);
            }
            break;

        case PlayerControlEvent.UiFadeOut: {
            uiVisible = false;
            document.body.style.cursor = "none";
            playerControls.style.opacity = '0';
            break;
        }

        case PlayerControlEvent.UiFadeIn: {
            uiVisible = true;
            document.body.style.cursor = "default";
            playerControls.style.opacity = '1';
            break;
        }

        default:
            break;
    }

    targetPlayerCtrlPostStateUpdate(event);
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

function mediaPlayHandler() {
    if (imageViewerPlaybackState === PlaybackState.Idle) {
        logger.info('Media playback start:', cachedPlayMediaItem);
        window.targetAPI.sendEvent(new EventMessage(Date.now(), new MediaItemEvent(EventType.MediaItemStart, cachedPlayMediaItem)));
    }
}

function mediaEndHandler() {
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
        sendPlaybackUpdate(PlaybackState.Idle);
    }

    window.targetAPI.sendEvent(new EventMessage(Date.now(), new MediaItemEvent(EventType.MediaItemEnd, cachedPlayMediaItem)));
}

// Component hiding
let uiVisible = true;

function stopUiHideTimer() {
    uiHideTimer.stop();

    if (!uiVisible) {
        playerCtrlStateUpdate(PlayerControlEvent.UiFadeIn);
    }
}

document.onmouseout = () => { uiHideTimer.end(); }
document.onmousemove = () => {
    stopUiHideTimer();
    uiHideTimer.start();
};

function keyDownEventHandler(event: KeyboardEvent) {
    // logger.info("KeyDown", event);
    let result = targetKeyDownEventListener(event);
    let handledCase = result.handledCase;

    // @ts-ignore
    let key = (TARGET === 'webOS' && result.key !== '') ? result.key : event.key;

    if (!handledCase && isMediaItem) {
        switch (event.key.toLowerCase()) {
            case 'arrowleft':
                setPlaylistItem(playlistIndex - 1);
                event.preventDefault();
                handledCase = true;
                break;
            case 'arrowright':
                setPlaylistItem(playlistIndex + 1);
                event.preventDefault();
                handledCase = true;
                break;
            case "home":
                setPlaylistItem(0);
                event.preventDefault();
                handledCase = true;
                break;
            case "end":
                setPlaylistItem(cachedPlaylist.items.length - 1);
                event.preventDefault();
                handledCase = true;
                break;
            case 'k':
            case ' ':
            case 'enter':
                // Play/pause toggle
                if (cachedPlayMediaItem.showDuration && cachedPlayMediaItem.showDuration > 0) {
                    if (imageViewerPlaybackState === PlaybackState.Paused || imageViewerPlaybackState === PlaybackState.Idle) {
                        playerCtrlStateUpdate(PlayerControlEvent.Play);
                    } else {
                        playerCtrlStateUpdate(PlayerControlEvent.Pause);
                    }
                }

                event.preventDefault();
                handledCase = true;
                break;
            default:
                break;
        }
    }

    if (window.targetAPI.getSubscribedKeys().keyDown.has(key)) {
        window.targetAPI.sendEvent(new EventMessage(Date.now(), new KeyEvent(EventType.KeyDown, key, event.repeat, handledCase)));
    }
}

function keyUpEventHandler(event: KeyboardEvent) {
    // logger.info("KeyUp", event);
    let result = targetKeyUpEventListener(event);
    let handledCase = result.handledCase;

    // @ts-ignore
    let key = (TARGET === 'webOS' && result.key !== '') ? result.key : event.key;

    if (!handledCase) {
        switch (event.key.toLowerCase()) {
            default:
                break;
        }
    }

    if (window.targetAPI.getSubscribedKeys().keyUp.has(key)) {
        window.targetAPI.sendEvent(new EventMessage(Date.now(), new KeyEvent(EventType.KeyUp, key, event.repeat, handledCase)));
    }
}

document.addEventListener('keydown', keyDownEventHandler);
document.addEventListener('keyup', keyUpEventHandler);

export {
    PlayerControlEvent,
    idleBackground,
    idleIcon,
    imageViewer,
    genericViewer,
    uiHideTimer,
    showDurationTimer,
    isMediaItem,
    playlistIndex,
    cachedPlayMediaItem,
    imageViewerPlaybackState,
    onPlay,
    onPlayPlaylist,
    playerCtrlStateUpdate,
    setPlaylistItem,
    keyDownEventHandler,
    keyUpEventHandler,
};
