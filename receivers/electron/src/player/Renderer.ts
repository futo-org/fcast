import dashjs from 'dashjs';
import Hls, { LevelLoadedData } from 'hls.js';
import { PlaybackUpdateMessage, PlayMessage, SeekMessage, SetSpeedMessage, SetVolumeMessage } from '../Packets';
import { Player, PlayerType } from './Player';

function formatDuration(duration: number) {
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

function sendPlaybackUpdate(updateState: number) {
    const updateMessage = new PlaybackUpdateMessage(Date.now(), player.getCurrentTime(), player.getDuration(), updateState, player.getPlaybackRate());

    if (updateMessage.generationTime > lastPlayerUpdateGenerationTime) {
        lastPlayerUpdateGenerationTime = updateMessage.generationTime;
        window.electronAPI.sendPlaybackUpdate(updateMessage);
    }
};

function onPlayerLoad(value: PlayMessage, currentPlaybackRate?: number, currentVolume?: number) {
    playerCtrlStateUpdate(PlayerControlEvent.Load);

    // Subtitles break when seeking post stream initialization for the DASH player.
    // Its currently done on player initialization.
    if (player.playerType === PlayerType.Hls || player.playerType === PlayerType.Html) {
        if (value.time) {
            player.setCurrentTime(value.time);
        }
    }

    if (value.speed) {
        player.setPlaybackRate(value.speed);
    } else if (currentPlaybackRate) {
        player.setPlaybackRate(currentPlaybackRate);
    } else {
        player.setPlaybackRate(1.0);
    }
    playerCtrlStateUpdate(PlayerControlEvent.SetPlaybackRate);

    if (currentVolume) {
        player.setVolume(currentVolume);
    }

    playerCtrlStateUpdate(PlayerControlEvent.Play);
}

// HTML elements
const videoElement = document.getElementById("videoPlayer") as HTMLVideoElement;
const videoCaptions = document.getElementById("videoCaptions") as HTMLDivElement;

const playerControls = document.getElementById("controls");

const playerCtrlAction = document.getElementById("action");
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
const playerCtrlDuration = document.getElementById("duration");

const playerCtrlCaptions = document.getElementById("captions");
const playerCtrlSpeed = document.getElementById("speed");
const playerCtrlFullscreen = document.getElementById("fullscreen");

const playerCtrlSpeedMenu = document.getElementById("speedMenu");
let playerCtrlSpeedMenuShown = false;


const playbackRates = ["0.25", "0.50", "0.75", "1.00", "1.25", "1.50", "1.75", "2.00"];
const playbackUpdateInterval = 1.0;
const livePositionDelta = 5.0;
const livePositionWindow = livePositionDelta * 4;
let player: Player;
let playerPrevTime: number = 0;
let lastPlayerUpdateGenerationTime = 0;
let isLive = false;
let isLivePosition = false;


window.electronAPI.onPlay((_event, value: PlayMessage) => {
    console.log("Handle play message renderer", JSON.stringify(value));
    const currentVolume = player ? player.getVolume() : null;
    const currentPlaybackRate = player ? player.getPlaybackRate() : null;

    playerPrevTime = 0;

    if (player) {
        if (player.getSource() === value.url) {
            if (value.time) {
                if (Math.abs(value.time - player.getCurrentTime()) < 5000) {
                    console.warn(`Skipped changing video URL because URL and time is (nearly) unchanged: ${value.url}, ${player.getSource()}, ${formatDuration(value.time)}, ${formatDuration(player.getCurrentTime())}`);
                } else {
                    console.info(`Skipped changing video URL because URL is the same, but time was changed, seeking instead: ${value.url}, ${player.getSource()}, ${formatDuration(value.time)}, ${formatDuration(player.getCurrentTime())}`);

                    player.setCurrentTime(value.time);
                }
            }
            return;
        }

        player.destroy();
    }

    if ((value.url || value.content) && value.container && videoElement) {
        if (value.container === 'application/dash+xml') {
            console.log("Loading dash player");
            const dashPlayer = dashjs.MediaPlayer().create();
            player = new Player(PlayerType.Dash, dashPlayer);

            dashPlayer.extend("RequestModifier", () => {
                return {
                    modifyRequestHeader: function (xhr) {
                        if (value.headers) {
                            for (const [key, val] of Object.entries(value.headers)) {
                                xhr.setRequestHeader(key, val);
                            }
                        }

                        return xhr;
                    }
                };
            }, true);

            // Player event handlers
            dashPlayer.on(dashjs.MediaPlayer.events.PLAYBACK_PLAYING, () => { sendPlaybackUpdate(1) });
            dashPlayer.on(dashjs.MediaPlayer.events.PLAYBACK_PAUSED, () => { sendPlaybackUpdate(2) });
            dashPlayer.on(dashjs.MediaPlayer.events.PLAYBACK_ENDED, () => { sendPlaybackUpdate(0) });
            dashPlayer.on(dashjs.MediaPlayer.events.PLAYBACK_TIME_UPDATED, () => {
                playerCtrlStateUpdate(PlayerControlEvent.TimeUpdate);

                if (Math.abs(dashPlayer.time() - playerPrevTime) >= playbackUpdateInterval) {
                    sendPlaybackUpdate(dashPlayer.isPaused() ? 2 : 1);
                    playerPrevTime = dashPlayer.time();
                }
            });
            dashPlayer.on(dashjs.MediaPlayer.events.PLAYBACK_RATE_CHANGED, () => { sendPlaybackUpdate(dashPlayer.isPaused() ? 2 : 1) });

            // Buffering UI update when paused
            dashPlayer.on(dashjs.MediaPlayer.events.PLAYBACK_PROGRESS, () => { playerCtrlStateUpdate(PlayerControlEvent.TimeUpdate); });

            dashPlayer.on(dashjs.MediaPlayer.events.PLAYBACK_VOLUME_CHANGED, () => {
                playerCtrlStateUpdate(PlayerControlEvent.VolumeChange);
                window.electronAPI.sendVolumeUpdate({ generationTime: Date.now(), volume: dashPlayer.getVolume() });
            });

            dashPlayer.on(dashjs.MediaPlayer.events.ERROR, (data) => { window.electronAPI.sendPlaybackError({
                message: `DashJS ERROR: ${JSON.stringify(data)}`
            })});

            dashPlayer.on(dashjs.MediaPlayer.events.PLAYBACK_ERROR, (data) => { window.electronAPI.sendPlaybackError({
                message: `DashJS PLAYBACK_ERROR: ${JSON.stringify(data)}`
            })});

            dashPlayer.on(dashjs.MediaPlayer.events.STREAM_INITIALIZED, () => { onPlayerLoad(value, currentPlaybackRate, currentVolume); });

            dashPlayer.on(dashjs.MediaPlayer.events.CUE_ENTER, (e: any) => {
                // console.log("cueEnter", e);
                const subtitle = document.createElement("p")
                subtitle.setAttribute("id", "subtitle-" + e.cueID)

                subtitle.textContent = e.text;
                videoCaptions.appendChild(subtitle);
            });

            dashPlayer.on(dashjs.MediaPlayer.events.CUE_EXIT, (e: any) => {
                // console.log("cueExit ", e);
                document.getElementById("subtitle-" + e.cueID).remove();
            });

            dashPlayer.updateSettings({
                streaming: {
                    text: {
                        dispatchForManualRendering: true
                    }
                }
            });

            if (value.content) {
                dashPlayer.initialize(videoElement, `data:${value.container};base64,` + window.btoa(value.content), true, value.time);
                // dashPlayer.initialize(videoElement, "https://dash.akamaized.net/akamai/test/caption_test/ElephantsDream/elephants_dream_480p_heaac5_1_https.mpd", true);
            } else {
                dashPlayer.initialize(videoElement, value.url, true, value.time);
            }

        } else if ((value.container === 'application/vnd.apple.mpegurl' || value.container === 'application/x-mpegURL') && !videoElement.canPlayType(value.container)) {
            console.log("Loading hls player");

            const config = {
                xhrSetup: function (xhr: XMLHttpRequest) {
                    if (value.headers) {
                        for (const [key, val] of Object.entries(value.headers)) {
                            xhr.setRequestHeader(key, val);
                        }
                    }
                },
            };

            const hlsPlayer = new Hls(config);

            hlsPlayer.on(Hls.Events.ERROR, (eventName, data) => {
                window.electronAPI.sendPlaybackError({
                    message: `HLS player error: ${JSON.stringify(data)}`
                });
            });

            hlsPlayer.on(Hls.Events.LEVEL_LOADED, (eventName, level: LevelLoadedData) => {
                isLive = level.details.live;
                isLivePosition = isLive ? true : false;
            });

            player = new Player(PlayerType.Hls, videoElement, hlsPlayer);

            // value.url = "https://devstreaming-cdn.apple.com/videos/streaming/examples/adv_dv_atmos/main.m3u8?ref=developerinsider.co";
            hlsPlayer.loadSource(value.url);
            hlsPlayer.attachMedia(videoElement);
            // hlsPlayer.subtitleDisplay = true;

        } else {
            console.log("Loading html player");
            player = new Player(PlayerType.Html, videoElement);

            videoElement.src = value.url;
            videoElement.load();
        }

        // Player event handlers
        if (player.playerType === PlayerType.Hls || player.playerType === PlayerType.Html) {
            videoElement.onplay = () => { sendPlaybackUpdate(1) };
            videoElement.onpause = () => { sendPlaybackUpdate(2) };
            videoElement.onended = () => { sendPlaybackUpdate(0) };
            videoElement.ontimeupdate = () => {
                playerCtrlStateUpdate(PlayerControlEvent.TimeUpdate);

                if (Math.abs(videoElement.currentTime - playerPrevTime) >= playbackUpdateInterval) {
                    sendPlaybackUpdate(videoElement.paused ? 2 : 1);
                    playerPrevTime = videoElement.currentTime;
                }
            };
            // Buffering UI update when paused
            videoElement.onprogress = () => { playerCtrlStateUpdate(PlayerControlEvent.TimeUpdate); };
            videoElement.onratechange = () => { sendPlaybackUpdate(videoElement.paused ? 2 : 1) };
            videoElement.onvolumechange = () => {
                playerCtrlStateUpdate(PlayerControlEvent.VolumeChange);
                window.electronAPI.sendVolumeUpdate({ generationTime: Date.now(), volume: videoElement.volume });
            };

            videoElement.onerror = (event: Event | string, source?: string, lineno?: number, colno?: number, error?: Error) => {
                console.error("Player error", {source, lineno, colno, error});
            };

            videoElement.onloadedmetadata = () => { onPlayerLoad(value, currentPlaybackRate, currentVolume); };
        }
    }

    // Sender generated event handlers
    window.electronAPI.onPause(() => { playerCtrlStateUpdate(PlayerControlEvent.Pause); });
    window.electronAPI.onResume(() => { playerCtrlStateUpdate(PlayerControlEvent.Play); });
    window.electronAPI.onSeek((_event, value: SeekMessage) => { player.setCurrentTime(value.time); });
    window.electronAPI.onSetVolume((_event, value: SetVolumeMessage) => { volumeChangeHandler(value.volume); });
    window.electronAPI.onSetSpeed((_event, value: SetSpeedMessage) => { player.setPlaybackRate(value.speed); playerCtrlStateUpdate(PlayerControlEvent.SetPlaybackRate); });
});

let scrubbing = false;
let volumeChanging = false;

enum PlayerControlEvent {
    Load,
    Pause,
    Play,
    ToggleMute,
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

// UI update handler
function playerCtrlStateUpdate(event: PlayerControlEvent) {
    switch (event) {
        case PlayerControlEvent.Load: {
            playerCtrlProgressBarBuffer.setAttribute("style", "width: 0px");
            playerCtrlProgressBarProgress.setAttribute("style", "width: 0px");
            playerCtrlProgressBarHandle.setAttribute("style", `left: ${playerCtrlProgressBar.offsetLeft}px`);

            const volume = Math.round(player.getVolume() * playerCtrlVolumeBar.offsetWidth);
            playerCtrlVolumeBarProgress.setAttribute("style", `width: ${volume}px`);
            playerCtrlVolumeBarHandle.setAttribute("style", `left: ${volume + 8}px`);

            if (isLive) {
                playerCtrlLiveBadge.setAttribute("style", "display: block");
                playerCtrlPosition.setAttribute("style", "display: none");
                playerCtrlDuration.setAttribute("style", "display: none");
            }
            else {
                playerCtrlLiveBadge.setAttribute("style", "display: none");
                playerCtrlPosition.textContent = formatDuration(player.getCurrentTime());
                playerCtrlDuration.innerHTML = `/&nbsp&nbsp${formatDuration(player.getDuration())}`;
            }

            playerCtrlStateUpdate(PlayerControlEvent.SetCaptions);

            break;
        }

        case PlayerControlEvent.Pause:
            playerCtrlAction.setAttribute("class", "play");
            stopUiHideTimer();
            player.pause();
            break;

        case PlayerControlEvent.Play:
            playerCtrlAction.setAttribute("class", "pause");
            startUiHideTimer();
            player.play();
            break;

        case PlayerControlEvent.ToggleMute:
            player.setMute(!player.isMuted());
            window.electronAPI.sendVolumeUpdate({ generationTime: Date.now(), volume: 0 });
            // fallthrough

        case PlayerControlEvent.VolumeChange: {
            const volume = Math.round(player.getVolume() * playerCtrlVolumeBar.offsetWidth);

            if (player.isMuted()) {
                playerCtrlVolume.setAttribute("class", "mute");
                playerCtrlVolumeBarProgress.setAttribute("style", `width: 0px`);
                playerCtrlVolumeBarHandle.setAttribute("style", `left: 0px`);
            }
            else if (player.getVolume() >= 0.5) {
                playerCtrlVolume.setAttribute("class", "volume_high");
                playerCtrlVolumeBarProgress.setAttribute("style", `width: ${volume}px`);
                playerCtrlVolumeBarHandle.setAttribute("style", `left: ${volume}px`);
            } else {
                playerCtrlVolume.setAttribute("class", "volume_low");
                playerCtrlVolumeBarProgress.setAttribute("style", `width: ${volume}px`);
                playerCtrlVolumeBarHandle.setAttribute("style", `left: ${volume}px`);
            }

            break;
        }

        case PlayerControlEvent.TimeUpdate: {
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

        case PlayerControlEvent.UiFadeOut:
            document.body.style.cursor = "none";
            playerControls.setAttribute("style", "opacity: 0");
            break;

        case PlayerControlEvent.UiFadeIn:
            document.body.style.cursor = "default";
            playerControls.setAttribute("style", "opacity: 1");
            break;

        case PlayerControlEvent.SetCaptions:
            if (player.isCaptionsEnabled()) {
                playerCtrlCaptions.setAttribute("class", "captions_on");
                videoCaptions.setAttribute("style", "display: block");
            } else {
                playerCtrlCaptions.setAttribute("class", "captions_off");
                videoCaptions.setAttribute("style", "display: none");
            }

            break;

        case PlayerControlEvent.ToggleSpeedMenu: {
            if (playerCtrlSpeedMenuShown) {
                playerCtrlSpeedMenu.setAttribute("style", "display: none");
            } else {
                playerCtrlSpeedMenu.setAttribute("style", "display: block");
            }

            playerCtrlSpeedMenuShown = !playerCtrlSpeedMenuShown;
            break;
        }

        case PlayerControlEvent.SetPlaybackRate: {
            const rate = player.getPlaybackRate().toFixed(2);
            const entryElement = document.getElementById(`speedMenuEntry_${rate}_enabled`);

            playbackRates.forEach(r => {
                const entry = document.getElementById(`speedMenuEntry_${r}_enabled`);
                entry.setAttribute("style", "opacity: 0");
            });

            // Ignore updating GUI for custom rates
            if (entryElement !== null) {
                entryElement.setAttribute("style", "opacity: 1");
            }

            break;
        }

        case PlayerControlEvent.ToggleFullscreen: {
            window.electronAPI.toggleFullScreen();

            window.electronAPI.isFullScreen().then((isFullScreen: boolean) => {
                if (isFullScreen) {
                    playerCtrlFullscreen.setAttribute("class", "fullscreen_on");
                } else {
                    playerCtrlFullscreen.setAttribute("class", "fullscreen_off");
                }
            });

            break;
        }

        case PlayerControlEvent.ExitFullscreen:
            window.electronAPI.exitFullScreen();
            playerCtrlFullscreen.setAttribute("class", "fullscreen_off");
            break;

        default:
            break;
    }
}

// Receiver generated event handlers
playerCtrlAction.onclick = () => {
    if (player.isPaused()) {
        playerCtrlStateUpdate(PlayerControlEvent.Play);
    } else {
        playerCtrlStateUpdate(PlayerControlEvent.Pause);
    }
};

playerCtrlVolume.onclick = () => { playerCtrlStateUpdate(PlayerControlEvent.ToggleMute); };

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
    const progressBarOffset = e.offsetX - 8;
    const progressBarWidth = PlayerCtrlProgressBarInteractiveArea.offsetWidth - 16;
    let time = Math.round((progressBarOffset / progressBarWidth) * player.getDuration());
    time = Math.min(player.getDuration(), Math.max(0.0, time));

    if (scrubbing && e.buttons === 1) {
        player.setCurrentTime(time);
    }

    scrubbingMouseUIHandler(e);
}

function scrubbingMouseUIHandler(e: MouseEvent) {
    const progressBarOffset = e.offsetX - 8;
    const progressBarWidth = PlayerCtrlProgressBarInteractiveArea.offsetWidth - 16;
    let time = isLive ? Math.round((1 - (progressBarOffset / progressBarWidth)) * player.getDuration()) : Math.round((progressBarOffset / progressBarWidth) * player.getDuration());
    time = Math.min(player.getDuration(), Math.max(0.0, time));

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
        volumeChangeHandler(Math.min(player.getVolume() + volumeIncrement, 1));
    } else if (delta < 0) {
        volumeChangeHandler(Math.max(player.getVolume() - volumeIncrement, 0));
    }
};

function volumeChangeMouseHandler(e: MouseEvent) {
    if (volumeChanging && e.buttons === 1) {
        const volumeBarOffsetX = e.offsetX - 8;
        const volumeBarWidth = playerCtrlVolumeBarInteractiveArea.offsetWidth - 16;
        const volume = volumeBarOffsetX / volumeBarWidth;
        volumeChangeHandler(volume);
    }
}

function volumeChangeHandler(volume: number) {
    if (!player.isMuted() && volume <= 0) {
        player.setMute(true);
    }
    else if (player.isMuted() && volume > 0) {
        player.setMute(false);
    }

    player.setVolume(volume);
}

playerCtrlLiveBadge.onclick = () => {
    if (!isLivePosition) {
        isLivePosition = true;

        player.setCurrentTime(player.getDuration() - livePositionDelta);
        playerCtrlLiveBadge.setAttribute("style", `background-color: red`);
    }
};

playerCtrlCaptions.onclick = () => { player.enableCaptions(!player.isCaptionsEnabled()); playerCtrlStateUpdate(PlayerControlEvent.SetCaptions); };
playerCtrlSpeed.onclick = () => { playerCtrlStateUpdate(PlayerControlEvent.ToggleSpeedMenu); };
playerCtrlFullscreen.onclick = () => { playerCtrlStateUpdate(PlayerControlEvent.ToggleFullscreen); };

playbackRates.forEach(r => {
    const entry = document.getElementById(`speedMenuEntry_${r}`);
    entry.onclick = () => { player.setPlaybackRate(parseFloat(r)); playerCtrlStateUpdate(PlayerControlEvent.SetPlaybackRate); };
});

videoElement.onclick = () => {
    if (player.isPaused()) {
        playerCtrlStateUpdate(PlayerControlEvent.Play);
    } else {
        playerCtrlStateUpdate(PlayerControlEvent.Pause);
    }
};
videoElement.ondblclick = () => { playerCtrlStateUpdate(PlayerControlEvent.ToggleFullscreen); };

// Component hiding
let uiHideTimer = null;
let uiVisible = true;

function startUiHideTimer() {
    uiHideTimer = window.setTimeout(() => {
        uiHideTimer = null;
        uiVisible = false;
        playerCtrlStateUpdate(PlayerControlEvent.UiFadeOut);
    }, 3000);
}

function stopUiHideTimer() {
    if (uiHideTimer) {
        window.clearTimeout(uiHideTimer);
    }

    if (!uiVisible) {
        uiVisible = true;
        playerCtrlStateUpdate(PlayerControlEvent.UiFadeIn);
    }
}

document.onmousemove = function() {
    stopUiHideTimer();

    if (player && !player.isPaused()) {
        startUiHideTimer();
    }
};

window.onresize = () => { playerCtrlStateUpdate(PlayerControlEvent.TimeUpdate); };

// Add the keydown event listener to the document
const skipInterval = 10;
const volumeIncrement = 0.1;

document.addEventListener('keydown', (event) => {
// console.log("KeyDown", event);

    switch (event.code) {
        case 'F11':
            playerCtrlStateUpdate(PlayerControlEvent.ToggleFullscreen);
            event.preventDefault();
            break;
        case 'Escape':
            playerCtrlStateUpdate(PlayerControlEvent.ExitFullscreen);
            event.preventDefault();
            break;
        case 'ArrowLeft':
            // Skip back
            player.setCurrentTime(Math.max(player.getCurrentTime() - skipInterval, 0));
            event.preventDefault();
            break;
        case 'ArrowRight': {
            // Skip forward
            const duration = player.getDuration();
            if (duration) {
                player.setCurrentTime(Math.min(player.getCurrentTime() + skipInterval, duration));
            } else {
                player.setCurrentTime(player.getCurrentTime());
            }
            event.preventDefault();
            break;
        }
        case 'Space':
        case 'Enter':
            // Pause/Continue
            if (player.isPaused()) {
                playerCtrlStateUpdate(PlayerControlEvent.Play);
            } else {
                playerCtrlStateUpdate(PlayerControlEvent.Pause);
            }
            event.preventDefault();
            break;
        case 'KeyM':
            // Mute toggle
            playerCtrlStateUpdate(PlayerControlEvent.ToggleMute);
            break;
        case 'ArrowUp':
            // Volume up
            volumeChangeHandler(Math.min(player.getVolume() + volumeIncrement, 1));
            break;
        case 'ArrowDown':
            // Volume down
            volumeChangeHandler(Math.max(player.getVolume() - volumeIncrement, 0));
            break;
    }
});