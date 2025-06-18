import dashjs from 'modules/dashjs';
import Hls, { LevelLoadedData } from 'modules/hls.js';
import { EventMessage, EventType, GenericMediaMetadata, KeyEvent, MediaItem, MediaItemEvent, MetadataType, PlaybackState, PlaybackUpdateMessage, PlaylistContent, PlayMessage, SeekMessage, SetPlaylistItemMessage, SetSpeedMessage, SetVolumeMessage } from 'common/Packets';
import { Player, PlayerType } from './Player';
import * as connectionMonitor from 'common/ConnectionMonitor';
import { supportedAudioTypes } from 'common/MimeTypes';
import { mediaItemFromPlayMessage, playMessageFromMediaItem, Timer } from 'common/UtilityFrontend';
import { toast, ToastIcon } from 'common/components/Toast';
import {
    targetPlayerCtrlStateUpdate,
    targetKeyDownEventListener,
    captionsBaseHeightCollapsed,
    captionsBaseHeightExpanded,
    captionsLineHeight
} from 'src/player/Renderer';

const logger = window.targetAPI.logger;

// HTML elements
const idleIcon = document.getElementById('title-icon');
const loadingSpinner = document.getElementById('loading-spinner');
const idleBackground = document.getElementById('idle-background');
const thumbnailImage = document.getElementById('thumbnailImage') as HTMLImageElement;
const videoElement = document.getElementById("videoPlayer") as HTMLVideoElement;
const videoCaptions = document.getElementById("videoCaptions") as HTMLDivElement;
const mediaTitle = document.getElementById("mediaTitle");

const playerControls = document.getElementById("controls");

const playerCtrlPlayPrevious = document.getElementById("playPrevious");
const playerCtrlAction = document.getElementById("action");
const playerCtrlPlayNext = document.getElementById("playNext");
const playerCtrlVolume = document.getElementById("volume");

const playerCtrlProgressBar = document.getElementById("progressBar");
const playerCtrlProgressBarBuffer = document.getElementById("progressBarBuffer");
const playerCtrlProgressBarProgress = document.getElementById("progressBarProgress");
const playerCtrlProgressBarPosition = document.getElementById("progressBarPosition");
const playerCtrlProgressBarHandle = document.getElementById("progressBarHandle");
const PlayerCtrlProgressBarInteractiveArea = document.getElementById("progressBarInteractiveArea");

const playerCtrlVolumeBar = document.getElementById("volumeBar");
const playerCtrlVolumeBarProgress = document.getElementById("volumeBarProgress");
const playerCtrlVolumeBarHandle = document.getElementById("volumeBarHandle");
const playerCtrlVolumeBarInteractiveArea = document.getElementById("volumeBarInteractiveArea");

const playerCtrlLiveBadge = document.getElementById("liveBadge");
const playerCtrlPosition = document.getElementById("position");
const playerCtrlDurationSeparator = document.getElementById("durationSeparator");
const playerCtrlDuration = document.getElementById("duration");

const playerCtrlCaptions = document.getElementById("captions");
const playerCtrlSpeed = document.getElementById("speed");

const playerCtrlSpeedMenu = document.getElementById("speedMenu");
let playerCtrlSpeedMenuShown = false;


const playbackRates = ["0.25", "0.50", "0.75", "1.00", "1.25", "1.50", "1.75", "2.00"];
const playbackUpdateInterval = 1.0;
const playerVolumeUpdateInterval = 0.01;
const livePositionDelta = 5.0;
const livePositionWindow = livePositionDelta * 4;
let player: Player;
let playbackState: PlaybackState = PlaybackState.Idle;
let playerPrevTime: number = 1;
let playerPrevVolume: number = 1;
let lastPlayerUpdateGenerationTime = 0;
let isLive = false;
let isLivePosition = false;
let captionsBaseHeight = 0;
let captionsContentHeight = 0;

let cachedPlaylist: PlaylistContent = null;
let cachedPlayMediaItem: MediaItem = null;
let playlistIndex = 0;
let isMediaItem = false;
let playItemCached = false;

let uiHideTimer = new Timer(() => {
    uiVisible = false;
    playerCtrlStateUpdate(PlayerControlEvent.UiFadeOut);
}, 3000);
let loadingTimer = new Timer(() => { loadingSpinner.style.display = 'block'; }, 50, false);
let showDurationTimer = new Timer(mediaEndHandler, 0, false);
let mediaTitleShowTimer = new Timer(() => { mediaTitle.style.display = 'none'; }, 5000);

function formatDuration(duration: number) {
    if (isNaN(duration)) {
        return '00:00';
    }

    const totalSeconds = Math.floor(duration);
    const hours = Math.floor(totalSeconds / 3600);
    const minutes = Math.floor((totalSeconds % 3600) / 60);
    const seconds = Math.floor(totalSeconds % 60);

    const paddedMinutes = String(minutes).padStart(2, '0');
    const paddedSeconds = String(seconds).padStart(2, '0');

    if (hours > 0) {
        return `${hours}:${paddedMinutes}:${paddedSeconds}`;
    } else {
        return `${paddedMinutes}:${paddedSeconds}`;
    }
}

function sendPlaybackUpdate(updateState: PlaybackState) {
    const updateMessage = new PlaybackUpdateMessage(Date.now(), updateState, player?.getCurrentTime(), player?.getDuration(), player?.getPlaybackRate());
    playbackState = updateState;

    if (updateMessage.generationTime > lastPlayerUpdateGenerationTime) {
        lastPlayerUpdateGenerationTime = updateMessage.generationTime;
        window.targetAPI.sendPlaybackUpdate(updateMessage);
    }
};

function onPlayerLoad(value: PlayMessage) {
    playerCtrlStateUpdate(PlayerControlEvent.Load);
    loadingTimer.stop();

    if (player.getAutoplay()) {
        // Subtitles break when seeking post stream initialization for the DASH player.
        // Its currently done on player initialization.
        if (player.playerType === PlayerType.Hls || player.playerType === PlayerType.Html) {
            if (value.time) {
                player.setCurrentTime(value.time);
            }
        }
        if (value.speed) {
            player.setPlaybackRate(value.speed);
            playerCtrlStateUpdate(PlayerControlEvent.SetPlaybackRate);
        }
        if (value.volume !== null && value.volume >= 0) {
            volumeChangeHandler(value.volume);
        }
        else {
            // Protocol v2 FCast PlayMessage does not contain volume field and could result in the receiver
            // getting out-of-sync with the sender on 1st playback.
            volumeChangeHandler(1.0);
            window.targetAPI.sendVolumeUpdate({ generationTime: Date.now(), volume: 1.0 });
        }
        playerCtrlStateUpdate(PlayerControlEvent.VolumeChange);

        mediaPlayHandler(value);
        player.play();
    }
    else {
        setIdleScreenVisible(true, false, value);
    }
}

function onPlay(_event, value: PlayMessage) {
    if (!playItemCached) {
        cachedPlayMediaItem = mediaItemFromPlayMessage(value);
        isMediaItem = false;
    }
    window.targetAPI.sendEvent(new EventMessage(Date.now(), new MediaItemEvent(EventType.MediaItemChange, cachedPlayMediaItem)));
    logger.info('Media playback changed:', cachedPlayMediaItem);
    playItemCached = false;

    if (player) {
        if ((player.getSource() === value.url) || (player.getSource() === value.content)) {
            if (value.time) {
                console.info('Skipped changing video URL because URL is the same. Discarding time and using current receiver time instead');
            }
            return;
        }

        player.destroy();
        player = null;
    }

    setIdleScreenVisible(true, true);
    sendPlaybackUpdate(PlaybackState.Idle);
    playerPrevTime = 0;
    lastPlayerUpdateGenerationTime = 0;
    isLive = false;
    isLivePosition = false;
    captionsBaseHeight = captionsBaseHeightExpanded;

    if ((value.url || value.content) && value.container && videoElement) {
        player = new Player(videoElement, value);
        logger.info(`Loaded ${PlayerType[player.playerType]} player`);

        if (value.container === 'application/dash+xml') {
            // Player event handlers
            player.dashPlayer.on(dashjs.MediaPlayer.events.PLAYBACK_PLAYING, () => { mediaPlayHandler(value); });
            player.dashPlayer.on(dashjs.MediaPlayer.events.PLAYBACK_PAUSED, () => { sendPlaybackUpdate(PlaybackState.Paused); playerCtrlStateUpdate(PlayerControlEvent.Pause); });
            player.dashPlayer.on(dashjs.MediaPlayer.events.PLAYBACK_ENDED, () => { mediaEndHandler(); });
            player.dashPlayer.on(dashjs.MediaPlayer.events.PLAYBACK_TIME_UPDATED, () => {
                playerCtrlStateUpdate(PlayerControlEvent.TimeUpdate);

                if (Math.abs(player.dashPlayer.time() - playerPrevTime) >= playbackUpdateInterval) {
                    sendPlaybackUpdate(playbackState);
                    playerPrevTime = player.dashPlayer.time();
                }
            });
            player.dashPlayer.on(dashjs.MediaPlayer.events.PLAYBACK_RATE_CHANGED, () => { sendPlaybackUpdate(playbackState); });

            // Buffering UI update when paused
            player.dashPlayer.on(dashjs.MediaPlayer.events.PLAYBACK_PROGRESS, () => { playerCtrlStateUpdate(PlayerControlEvent.TimeUpdate); });

            player.dashPlayer.on(dashjs.MediaPlayer.events.PLAYBACK_VOLUME_CHANGED, () => {
                const updateVolume = player.dashPlayer.isMuted() ? 0 : player.dashPlayer.getVolume();
                playerCtrlStateUpdate(PlayerControlEvent.VolumeChange);

                if (Math.abs(updateVolume - playerPrevVolume) >= playerVolumeUpdateInterval) {
                    window.targetAPI.sendVolumeUpdate({ generationTime: Date.now(), volume: updateVolume });
                    playerPrevVolume = updateVolume;
                }
            });

            player.dashPlayer.on(dashjs.MediaPlayer.events.ERROR, (data) => {
                toast('Media playback error, please close the player and reconnect sender devices if you experience issues', ToastIcon.WARNING);
                logger.error('Dash player error:', data);

                window.targetAPI.sendPlaybackError({
                    message: JSON.stringify(data)
                });
            });

            player.dashPlayer.on(dashjs.MediaPlayer.events.PLAYBACK_ERROR, (data) => {
                toast('Media playback error, please close the player and reconnect sender devices if you experience issues', ToastIcon.WARNING);
                logger.error('Dash player playback error:', data);

                window.targetAPI.sendPlaybackError({
                    message: JSON.stringify(data)
                });
            });

            player.dashPlayer.on(dashjs.MediaPlayer.events.STREAM_INITIALIZED, () => { onPlayerLoad(value); });

            player.dashPlayer.on(dashjs.MediaPlayer.events.CUE_ENTER, (e: any) => {
                const subtitle = document.createElement("p")
                subtitle.setAttribute("id", "subtitle-" + e.cueID)

                subtitle.textContent = e.text;
                videoCaptions.appendChild(subtitle);

                captionsContentHeight = subtitle.getBoundingClientRect().height - captionsLineHeight;
                const captionsHeight = captionsBaseHeight + captionsContentHeight;

                if (player.isCaptionsEnabled()) {
                    videoCaptions.setAttribute("style", `display: block; bottom: ${captionsHeight}px;`);
                } else {
                    videoCaptions.setAttribute("style", `display: none; bottom: ${captionsHeight}px;`);
                }
            });

            player.dashPlayer.on(dashjs.MediaPlayer.events.CUE_EXIT, (e: any) => {
                document.getElementById("subtitle-" + e.cueID)?.remove();
            });

            player.dashPlayer.updateSettings({
                // debug: {
                //     logLevel: dashjs.LogLevel.LOG_LEVEL_INFO
                // },
                streaming: {
                    text: {
                        dispatchForManualRendering: true
                    }
                }
            });

        } else if ((value.container === 'application/vnd.apple.mpegurl' || value.container === 'application/x-mpegURL') && !videoElement.canPlayType(value.container)) {
            player.hlsPlayer.on(Hls.Events.ERROR, (_eventName, data) => {
                if (data.fatal) {
                    toast('Media playback error, please close the player and reconnect sender devices if you experience issues', ToastIcon.WARNING);
                    logger.error('HLS player error:', data);

                    window.targetAPI.sendPlaybackError({
                        message: JSON.stringify(data)
                    });

                    if (data.type === Hls.ErrorTypes.MEDIA_ERROR) {
                        player.hlsPlayer.recoverMediaError();
                    }
                }
                else {
                    logger.warn('HLS non-fatal error:', data);
                }
            });

            player.hlsPlayer.on(Hls.Events.LEVEL_LOADED, (eventName, level: LevelLoadedData) => {
                isLive = level.details.live;
                isLivePosition = isLive ? true : false;

                // Event can fire after video load and play initialization
                if (isLive && playerCtrlLiveBadge.style.display === "none") {
                    playerCtrlLiveBadge.style.display = "block";
                    playerCtrlPosition.style.display = "none";
                    playerCtrlDurationSeparator.style.display = "none";
                    playerCtrlDuration.style.display = "none";
                }
            });
        }

        // Player event handlers
        if (player.playerType === PlayerType.Hls || player.playerType === PlayerType.Html) {
            videoElement.onplay = () => { mediaPlayHandler(value); };
            videoElement.onpause = () => { sendPlaybackUpdate(PlaybackState.Paused); playerCtrlStateUpdate(PlayerControlEvent.Pause); };
            videoElement.onended = () => { mediaEndHandler(); };
            videoElement.ontimeupdate = () => {
                playerCtrlStateUpdate(PlayerControlEvent.TimeUpdate);

                if (Math.abs(videoElement.currentTime - playerPrevTime) >= playbackUpdateInterval) {
                    sendPlaybackUpdate(playbackState);
                    playerPrevTime = videoElement.currentTime;
                }
            };
            // Buffering UI update when paused
            videoElement.onprogress = () => { playerCtrlStateUpdate(PlayerControlEvent.TimeUpdate); };
            videoElement.onratechange = () => { sendPlaybackUpdate(playbackState); };
            videoElement.onvolumechange = () => {
                const updateVolume = videoElement.muted ? 0 : videoElement.volume;
                playerCtrlStateUpdate(PlayerControlEvent.VolumeChange);

                if (Math.abs(updateVolume - playerPrevVolume) >= playerVolumeUpdateInterval) {
                    window.targetAPI.sendVolumeUpdate({ generationTime: Date.now(), volume: updateVolume });
                    playerPrevVolume = updateVolume;
                }
            };

            // parameters seem to always be undefined...
            // videoElement.onerror = (event: Event | string, source?: string, lineno?: number, colno?: number, error?: Error) => {
            videoElement.onerror = () => {
                toast('Media playback error, please close the player and reconnect sender devices if you experience issues', ToastIcon.WARNING);
                logger.error('Html player error:', { playMessage: value, videoError: videoElement.error });

                window.targetAPI.sendPlaybackError({
                    message: JSON.stringify({ playMessage: value, videoError: videoElement.error })
                });
            };

            videoElement.onloadedmetadata = (ev) => {
                if (videoElement.duration === Infinity) {
                    isLive = true;
                    isLivePosition = true;
                }
                else {
                    isLive = false;
                    isLivePosition = false;
                }

                onPlayerLoad(value);
            };
        }

        player.setAutoPlay(true);
        player.load();
    }
}

// Sender generated event handlers
window.targetAPI.onPause(() => { player?.pause(); });
window.targetAPI.onResume(() => { player?.play(); });
window.targetAPI.onSeek((_event, value: SeekMessage) => { player?.setCurrentTime(value.time); });
window.targetAPI.onSetVolume((_event, value: SetVolumeMessage) => { volumeChangeHandler(value.volume); });
window.targetAPI.onSetSpeed((_event, value: SetSpeedMessage) => { player?.setPlaybackRate(value.speed); playerCtrlStateUpdate(PlayerControlEvent.SetPlaybackRate); });

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
}

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
window.targetAPI.onSetPlaylistItem((_event, value: SetPlaylistItemMessage) => { setPlaylistItem(value.itemIndex); });

let scrubbing = false;
let volumeChanging = false;

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
            if (isMediaItem) {
                playerCtrlPlayPrevious.style.display = 'block';
                playerCtrlPlayNext.style.display = 'block';
            }
            else {
                playerCtrlPlayPrevious.style.display = 'none';
                playerCtrlPlayNext.style.display = 'none';
            }

            playerCtrlProgressBarBuffer.setAttribute("style", "width: 0px");
            playerCtrlProgressBarProgress.setAttribute("style", "width: 0px");
            playerCtrlProgressBarHandle.setAttribute("style", `left: ${playerCtrlProgressBar.offsetLeft}px`);

            const volume = Math.round(player.getVolume() * playerCtrlVolumeBar.offsetWidth);
            playerCtrlVolumeBarProgress.setAttribute("style", `width: ${volume}px`);
            playerCtrlVolumeBarHandle.setAttribute("style", `left: ${volume + 8}px`);

            if (isLive) {
                playerCtrlLiveBadge.style.display = 'block';
                playerCtrlPosition.style.display = 'none';
                playerCtrlDurationSeparator.style.display = 'none';
                playerCtrlDuration.style.display = 'none';
            }
            else {
                playerCtrlLiveBadge.style.display = 'none';
                playerCtrlPosition.style.display = 'block';
                playerCtrlDurationSeparator.style.display = 'block';
                playerCtrlDuration.style.display = 'block';

                playerCtrlPosition.textContent = formatDuration(player.getCurrentTime());
                playerCtrlDuration.innerHTML = formatDuration(player.getDuration());
            }

            if (player.isCaptionsSupported()) {
                playerCtrlCaptions.style.display = 'block';
                videoCaptions.style.display = 'block';
            }
            else {
                playerCtrlCaptions.style.display = 'none';
                videoCaptions.style.display = 'none';
                player.enableCaptions(false);
            }
            playerCtrlStateUpdate(PlayerControlEvent.SetCaptions);

            if (supportedAudioTypes.find(v => v === cachedPlayMediaItem.container.toLocaleLowerCase())) {
                if (cachedPlayMediaItem.metadata && cachedPlayMediaItem.metadata?.type === MetadataType.Generic) {
                    const metadata = cachedPlayMediaItem.metadata as GenericMediaMetadata;

                    if (metadata.title) {
                        mediaTitle.innerHTML = metadata.title;

                        captionsContentHeight = mediaTitle.getBoundingClientRect().height - captionsLineHeight;
                        const captionsHeight = captionsBaseHeightExpanded + captionsContentHeight;
                        mediaTitle.setAttribute("style", `display: block; bottom: ${captionsHeight}px;`);
                        mediaTitleShowTimer.start();
                    }
                }
            }

            break;
        }

        case PlayerControlEvent.Pause:
            playerCtrlAction.setAttribute("class", "play iconSize");
            stopUiHideTimer();
            showDurationTimer.pause();
            break;

        case PlayerControlEvent.Play:
            playerCtrlAction.setAttribute("class", "pause iconSize");
            uiHideTimer.start();
            break;

        case PlayerControlEvent.VolumeChange: {
            // logger.info(`VolumeChange: isMute ${player?.isMuted()}, volume: ${player?.getVolume()}`);
            const volume = Math.round(player?.getVolume() * playerCtrlVolumeBar.offsetWidth);

            if (player?.isMuted()) {
                playerCtrlVolume.setAttribute("class", "mute iconSize");
                playerCtrlVolumeBarProgress.setAttribute("style", `width: 0px`);
                playerCtrlVolumeBarHandle.setAttribute("style", `left: 0px`);
            }
            else if (player?.getVolume() >= 0.5) {
                playerCtrlVolume.setAttribute("class", "volume_high iconSize");
                playerCtrlVolumeBarProgress.setAttribute("style", `width: ${volume}px`);
                playerCtrlVolumeBarHandle.setAttribute("style", `left: ${volume}px`);
            } else {
                playerCtrlVolume.setAttribute("class", "volume_low iconSize");
                playerCtrlVolumeBarProgress.setAttribute("style", `width: ${volume}px`);
                playerCtrlVolumeBarHandle.setAttribute("style", `left: ${volume}px`);
            }
            break;
        }

        case PlayerControlEvent.TimeUpdate: {
            // logger.info(`TimeUpdate: Position: ${player.getCurrentTime()}, Duration: ${player.getDuration()}`);

            if (isLive) {
                if (isLivePosition && player.getDuration() - player.getCurrentTime() > livePositionWindow) {
                    isLivePosition = false;
                    playerCtrlLiveBadge.setAttribute("style", `background-color: #595959`);
                }
                else if (!isLivePosition && player.getDuration() - player.getCurrentTime() <= livePositionWindow) {
                    isLivePosition = true;
                    playerCtrlLiveBadge.setAttribute("style", `background-color: red`);
                }
            }

            if (isLivePosition) {
                playerCtrlProgressBarProgress.setAttribute("style", `width: ${playerCtrlProgressBar.offsetWidth}px`);
                playerCtrlProgressBarHandle.setAttribute("style", `left: ${playerCtrlProgressBar.offsetWidth + playerCtrlProgressBar.offsetLeft}px`);
            }
            else {
                const buffer = Math.round((player.getBufferLength() / player.getDuration()) * playerCtrlProgressBar.offsetWidth);
                const progress = Math.round((player.getCurrentTime() / player.getDuration()) * playerCtrlProgressBar.offsetWidth);
                const handle = progress + playerCtrlProgressBar.offsetLeft;

                playerCtrlProgressBarBuffer.setAttribute("style", `width: ${buffer}px`);
                playerCtrlProgressBarProgress.setAttribute("style", `width: ${progress}px`);
                playerCtrlProgressBarHandle.setAttribute("style", `left: ${handle}px`);

                playerCtrlPosition.textContent = formatDuration(player.getCurrentTime());
            }

            break;
        }

        case PlayerControlEvent.UiFadeOut: {
            document.body.style.cursor = "none";
            playerControls.style.opacity = '0';
            captionsBaseHeight = captionsBaseHeightCollapsed;
            const captionsHeight = captionsBaseHeight + captionsContentHeight;

            if (player?.isCaptionsEnabled()) {
                videoCaptions.setAttribute("style", `display: block; transition: bottom 0.2s ease-in-out; bottom: ${captionsHeight}px;`);
            } else {
                videoCaptions.setAttribute("style", `display: none; bottom: ${captionsHeight}px;`);
            }

            break;
        }

        case PlayerControlEvent.UiFadeIn: {
            document.body.style.cursor = "default";
            playerControls.style.opacity = '1';
            captionsBaseHeight = captionsBaseHeightExpanded;
            const captionsHeight = captionsBaseHeight + captionsContentHeight;

            if (player?.isCaptionsEnabled()) {
                videoCaptions.setAttribute("style", `display: block; transition: bottom 0.2s ease-in-out; bottom: ${captionsHeight}px;`);
            } else {
                videoCaptions.setAttribute("style", `display: none; bottom: ${captionsHeight}px;`);
            }

            break;
        }

        case PlayerControlEvent.SetCaptions:
            if (player?.isCaptionsEnabled()) {
                playerCtrlCaptions.setAttribute("class", "captions_on iconSize");
                videoCaptions.style.display = 'block';
            } else {
                playerCtrlCaptions.setAttribute("class", "captions_off iconSize");
                videoCaptions.style.display = 'none';
            }

            break;

        case PlayerControlEvent.ToggleSpeedMenu: {
            if (playerCtrlSpeedMenuShown) {
                playerCtrlSpeedMenu.style.display = 'none';
            } else {
                playerCtrlSpeedMenu.style.display = 'block';
            }

            playerCtrlSpeedMenuShown = !playerCtrlSpeedMenuShown;
            break;
        }

        case PlayerControlEvent.SetPlaybackRate: {
            const rate = player?.getPlaybackRate().toFixed(2);
            const entryElement = document.getElementById(`speedMenuEntry_${rate}_enabled`);

            playbackRates.forEach(r => {
                const entry = document.getElementById(`speedMenuEntry_${r}_enabled`);
                entry.style.opacity = '0';
            });

            // Ignore updating GUI for custom rates
            if (entryElement !== null) {
                entryElement.style.opacity = '1';
            }

            break;
        }

        default:
            break;
    }
}

function scrubbingMouseUIHandler(e: MouseEvent) {
    const progressBarOffset = e.offsetX - playerCtrlProgressBar.offsetLeft;
    const progressBarWidth = PlayerCtrlProgressBarInteractiveArea.offsetWidth - (playerCtrlProgressBar.offsetLeft * 2);
    let time = isLive ? Math.round((1 - (progressBarOffset / progressBarWidth)) * player?.getDuration()) : Math.round((progressBarOffset / progressBarWidth) * player?.getDuration());
    time = Math.min(player?.getDuration(), Math.max(0.0, time));

    if (scrubbing && isLive && e.buttons === 1) {
        isLivePosition = false;
        playerCtrlLiveBadge.setAttribute("style", `background-color: #595959`);
    }

    const livePrefix = isLive && Math.floor(time) !== 0 ? "-" : "";
    playerCtrlProgressBarPosition.textContent = isLive ? `${livePrefix}${formatDuration(time)}` : formatDuration(time);

    let offset = e.offsetX - (playerCtrlProgressBarPosition.offsetWidth / 2);
    offset = Math.min(PlayerCtrlProgressBarInteractiveArea.offsetWidth - (playerCtrlProgressBarPosition.offsetWidth / 1), Math.max(8, offset));
    playerCtrlProgressBarPosition.setAttribute("style", `display: block; left: ${offset}px`);
}

// Receiver generated event handlers
playerCtrlAction.onclick = () => {
    if (player?.isPaused()) {
        player?.play();
    } else {
        player?.pause();
    }
};

playerCtrlPlayPrevious.onclick = () => { setPlaylistItem(playlistIndex - 1); }
playerCtrlPlayNext.onclick = () => { setPlaylistItem(playlistIndex + 1); }
playerCtrlVolume.onclick = () => { player?.setMute(!player?.isMuted()); };

PlayerCtrlProgressBarInteractiveArea.onmousedown = (e: MouseEvent) => { scrubbing = true; scrubbingMouseHandler(e) };
PlayerCtrlProgressBarInteractiveArea.onmouseup = () => { scrubbing = false; };
PlayerCtrlProgressBarInteractiveArea.onmouseenter = (e: MouseEvent) => {
    if (e.buttons === 0) {
        volumeChanging = false;
    }

    scrubbingMouseUIHandler(e);
};
PlayerCtrlProgressBarInteractiveArea.onmouseleave = () => { playerCtrlProgressBarPosition.setAttribute("style", "display: none"); };
PlayerCtrlProgressBarInteractiveArea.onmousemove = (e: MouseEvent) => { scrubbingMouseHandler(e) };

function scrubbingMouseHandler(e: MouseEvent) {
    const progressBarOffset = e.offsetX - playerCtrlProgressBar.offsetLeft;
    const progressBarWidth = PlayerCtrlProgressBarInteractiveArea.offsetWidth - (playerCtrlProgressBar.offsetLeft * 2);
    let time = Math.round((progressBarOffset / progressBarWidth) * player?.getDuration());
    time = Math.min(player?.getDuration(), Math.max(0.0, time));

    if (scrubbing && e.buttons === 1) {
        player?.setCurrentTime(time);
    }

    scrubbingMouseUIHandler(e);
}

playerCtrlVolumeBarInteractiveArea.onmousedown = (e: MouseEvent) => { volumeChanging = true; volumeChangeMouseHandler(e) };
playerCtrlVolumeBarInteractiveArea.onmouseup = () => { volumeChanging = false; };
playerCtrlVolumeBarInteractiveArea.onmouseenter = (e: MouseEvent) => {
    if (e.buttons === 0) {
        scrubbing = false;
    }
};
playerCtrlVolumeBarInteractiveArea.onmousemove = (e: MouseEvent) => { volumeChangeMouseHandler(e) };
playerCtrlVolumeBarInteractiveArea.onwheel = (e: WheelEvent) => {
    const delta = -e.deltaY;

    if (delta > 0 ) {
        volumeChangeHandler(Math.min(player?.getVolume() + volumeIncrement, 1));
    } else if (delta < 0) {
        volumeChangeHandler(Math.max(player?.getVolume() - volumeIncrement, 0));
    }
};

function volumeChangeMouseHandler(e: MouseEvent) {
    if (volumeChanging && e.buttons === 1) {
        const volumeBarOffsetX = e.offsetX - playerCtrlVolumeBar.offsetLeft;
        const volumeBarWidth = playerCtrlVolumeBarInteractiveArea.offsetWidth - (playerCtrlVolumeBar.offsetLeft * 2);
        const volume = volumeBarOffsetX / volumeBarWidth;
        volumeChangeHandler(volume);
    }
}

function volumeChangeHandler(volume: number) {
    if (!player?.isMuted() && volume <= 0) {
        player?.setMute(true);
    }
    else if (player?.isMuted() && volume > 0) {
        player?.setMute(false);
    }

    player?.setVolume(volume);
}

playerCtrlLiveBadge.onclick = () => { setLivePosition(); };

function setLivePosition() {
    if (!isLivePosition) {
        isLivePosition = true;

        player?.setCurrentTime(player?.getDuration() - livePositionDelta);
        playerCtrlLiveBadge.setAttribute("style", `background-color: red`);

        if (player?.isPaused()) {
            player?.play();
        }
    }
}

playerCtrlCaptions.onclick = () => { player?.enableCaptions(!player?.isCaptionsEnabled()); playerCtrlStateUpdate(PlayerControlEvent.SetCaptions); };
playerCtrlSpeed.onclick = () => { playerCtrlStateUpdate(PlayerControlEvent.ToggleSpeedMenu); };

playbackRates.forEach(r => {
    const entry = document.getElementById(`speedMenuEntry_${r}`);
    entry.onclick = () => {
        player?.setPlaybackRate(parseFloat(r));
        playerCtrlStateUpdate(PlayerControlEvent.SetPlaybackRate);
        playerCtrlStateUpdate(PlayerControlEvent.ToggleSpeedMenu);
    };
});

function videoClickedHandler() {
    if (!playerCtrlSpeedMenuShown) {
        if (player?.isPaused()) {
            player?.play();
        } else {
            player?.pause();
        }
    }
}

videoElement.onclick = () => { videoClickedHandler(); };
idleBackground.onclick = () => { videoClickedHandler(); };
thumbnailImage.onclick = () => { videoClickedHandler(); };
idleIcon.onclick = () => { videoClickedHandler(); };

function setIdleScreenVisible(visible: boolean, loading: boolean = false, message?: PlayMessage) {
    if (visible) {
        idleBackground.style.display = 'block';
        thumbnailImage.style.display = 'none';

        if (loading) {
            idleIcon.style.display = 'none';
            loadingTimer.start();
        }
        else {
            idleIcon.style.display = 'block';
            loadingSpinner.style.display = 'none';
        }
    }
    else {
        if (!supportedAudioTypes.find(v => v === message.container.toLocaleLowerCase())) {
            idleIcon.style.display = 'none';
            idleBackground.style.display = 'none';
            thumbnailImage.style.display = 'none';
        }
        else {
            let displayThumbnail = false;
            if (message?.metadata?.type === MetadataType.Generic) {
                const metadata = message.metadata as GenericMediaMetadata;
                displayThumbnail = metadata.thumbnailUrl ? true : false;
                thumbnailImage.src = metadata.thumbnailUrl;
            }

            if (displayThumbnail) {
                idleIcon.style.display = 'none';
                idleBackground.style.display = 'none';
                thumbnailImage.style.display = 'block';
            }
            else {
                idleIcon.style.display = 'block';
                idleBackground.style.display = 'block';
                thumbnailImage.style.display = 'none';
            }
        }

        loadingSpinner.style.display = 'none';
    }
}

function mediaPlayHandler(message: PlayMessage) {
    if (playbackState === PlaybackState.Idle) {
        logger.info('Media playback start:', cachedPlayMediaItem);
        window.targetAPI.sendEvent(new EventMessage(Date.now(), new MediaItemEvent(EventType.MediaItemStart, cachedPlayMediaItem)));
        setIdleScreenVisible(false, false, message);

        if (isMediaItem && cachedPlayMediaItem.showDuration && cachedPlayMediaItem.showDuration > 0) {
            showDurationTimer.start(cachedPlayMediaItem.showDuration * 1000);
        }
    }
    else {
        showDurationTimer.resume();
    }

    sendPlaybackUpdate(PlaybackState.Playing);
    playerCtrlStateUpdate(PlayerControlEvent.Play);
}

function mediaEndHandler() {
    showDurationTimer.stop();

    if (isMediaItem) {
        playlistIndex++;

        if (playlistIndex < cachedPlaylist.items.length) {
            cachedPlayMediaItem = cachedPlaylist.items[playlistIndex];
            playItemCached = true;
            window.targetAPI.sendPlayRequest(playMessageFromMediaItem(cachedPlaylist.items[playlistIndex]), playlistIndex);
        }
        else {
            logger.info('End of playlist:', cachedPlayMediaItem);
            sendPlaybackUpdate(PlaybackState.Idle);
            window.targetAPI.sendEvent(new EventMessage(Date.now(), new MediaItemEvent(EventType.MediaItemEnd, cachedPlayMediaItem)));

            setIdleScreenVisible(true);
            player.setAutoPlay(false);
            player.stop();
        }
    }
    else {
        logger.info('Media playback ended:', cachedPlayMediaItem);
        sendPlaybackUpdate(PlaybackState.Idle);
        window.targetAPI.sendEvent(new EventMessage(Date.now(), new MediaItemEvent(EventType.MediaItemEnd, cachedPlayMediaItem)));

        setIdleScreenVisible(true);
        player.setAutoPlay(false);
        player.stop();
    }
}

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

    if (player && !player.isPaused()) {
        uiHideTimer.start();
    }
};

window.onresize = () => { playerCtrlStateUpdate(PlayerControlEvent.TimeUpdate); };

// Listener for hiding speed menu when clicking outside element
document.addEventListener('click', (event: MouseEvent) => {
    const node = event.target as Node;
    if (playerCtrlSpeedMenuShown && !playerCtrlSpeed.contains(node) && !playerCtrlSpeedMenu.contains(node)){
        playerCtrlStateUpdate(PlayerControlEvent.ToggleSpeedMenu);
    }
});

// Add the keydown event listener to the document
const skipInterval = 10;
const volumeIncrement = 0.1;

function keyDownEventListener(event: KeyboardEvent) {
    // logger.info("KeyDown", event);
    let handledCase = targetKeyDownEventListener(event);

    if (!handledCase) {
        switch (event.code) {
            case 'ArrowLeft':
                skipBack();
                event.preventDefault();
                handledCase = true;
                break;
            case 'ArrowRight':
                skipForward();
                event.preventDefault();
                handledCase = true;
                break;
            case "Home":
                player?.setCurrentTime(0);
                event.preventDefault();
                handledCase = true;
                break;
            case "End":
                if (isLive) {
                    setLivePosition();
                }
                else {
                    player?.setCurrentTime(player?.getDuration());
                }
                event.preventDefault();
                handledCase = true;
                break;
            case 'KeyK':
            case 'Space':
            case 'Enter':
                // Play/pause toggle
                if (player?.isPaused()) {
                    player?.play();
                } else {
                    player?.pause();
                }
                event.preventDefault();
                handledCase = true;
                break;
            case 'KeyM':
                // Mute toggle
                player?.setMute(!player?.isMuted());
                handledCase = true;
                break;
            case 'ArrowUp':
                // Volume up
                volumeChangeHandler(Math.min(player?.getVolume() + volumeIncrement, 1));
                handledCase = true;
                break;
            case 'ArrowDown':
                // Volume down
                volumeChangeHandler(Math.max(player?.getVolume() - volumeIncrement, 0));
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

function skipBack() {
    player?.setCurrentTime(Math.max(player?.getCurrentTime() - skipInterval, 0));
}

function skipForward() {
    if (!isLivePosition) {
        player?.setCurrentTime(Math.min(player?.getCurrentTime() + skipInterval, player?.getDuration()));
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
    thumbnailImage,
    idleIcon,
    videoElement,
    videoCaptions,
    playerCtrlProgressBar,
    playerCtrlProgressBarBuffer,
    playerCtrlProgressBarProgress,
    playerCtrlProgressBarHandle,
    playerCtrlVolumeBar,
    playerCtrlVolumeBarProgress,
    playerCtrlVolumeBarHandle,
    playerCtrlLiveBadge,
    playerCtrlPosition,
    playerCtrlDuration,
    playerCtrlCaptions,
    player,
    isLive,
    captionsBaseHeight,
    captionsLineHeight,
    onPlay,
    playerCtrlStateUpdate,
    formatDuration,
    skipBack,
    skipForward,
};
