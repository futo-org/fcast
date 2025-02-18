import dashjs from 'modules/dashjs';
import Hls, { LevelLoadedData } from 'modules/hls.js';
import { PlaybackUpdateMessage, PlayMessage, SeekMessage, SetSpeedMessage, SetVolumeMessage } from 'common/Packets';
import { Player, PlayerType } from './Player';
import {
    targetPlayerCtrlStateUpdate,
    targetKeyDownEventListener,
    captionsBaseHeightCollapsed,
    captionsBaseHeightExpanded,
    captionsLineHeight
} from 'src/player/Renderer';

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

function sendPlaybackUpdate(updateState: number) {
    const updateMessage = new PlaybackUpdateMessage(Date.now(), player.getCurrentTime(), player.getDuration(), updateState, player.getPlaybackRate());

    if (updateMessage.generationTime > lastPlayerUpdateGenerationTime) {
        lastPlayerUpdateGenerationTime = updateMessage.generationTime;
        window.targetAPI.sendPlaybackUpdate(updateMessage);
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
        volumeChangeHandler(currentVolume);
    }
    else {
        // FCast PlayMessage does not contain volume field and could result in the receiver
        // getting out-of-sync with the sender on 1st playback.
        volumeChangeHandler(1.0);
        window.targetAPI.sendVolumeUpdate({ generationTime: Date.now(), volume: 1.0 });
    }

    player.play();
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
const playerCtrlDurationSeparator = document.getElementById("durationSeparator");
const playerCtrlDuration = document.getElementById("duration");

const playerCtrlCaptions = document.getElementById("captions");
const playerCtrlSpeed = document.getElementById("speed");

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
let captionsBaseHeight = 0;
let captionsContentHeight = 0;

function onPlay(_event, value: PlayMessage) {
    console.log("Handle play message renderer", JSON.stringify(value));
    const currentVolume = player ? player.getVolume() : null;
    const currentPlaybackRate = player ? player.getPlaybackRate() : null;

    playerPrevTime = 0;
    lastPlayerUpdateGenerationTime = 0;
    isLive = false;
    isLivePosition = false;
    captionsBaseHeight = captionsBaseHeightExpanded;

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
            dashPlayer.on(dashjs.MediaPlayer.events.PLAYBACK_PLAYING, () => { sendPlaybackUpdate(1); playerCtrlStateUpdate(PlayerControlEvent.Play); });
            dashPlayer.on(dashjs.MediaPlayer.events.PLAYBACK_PAUSED, () => { sendPlaybackUpdate(2); playerCtrlStateUpdate(PlayerControlEvent.Pause); });
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
                const updateVolume = dashPlayer.isMuted() ? 0 : dashPlayer.getVolume();
                playerCtrlStateUpdate(PlayerControlEvent.VolumeChange);
                window.targetAPI.sendVolumeUpdate({ generationTime: Date.now(), volume: updateVolume });
            });

            dashPlayer.on(dashjs.MediaPlayer.events.ERROR, (data) => { window.targetAPI.sendPlaybackError({
                message: `DashJS ERROR: ${JSON.stringify(data)}`
            })});

            dashPlayer.on(dashjs.MediaPlayer.events.PLAYBACK_ERROR, (data) => { window.targetAPI.sendPlaybackError({
                message: `DashJS PLAYBACK_ERROR: ${JSON.stringify(data)}`
            })});

            dashPlayer.on(dashjs.MediaPlayer.events.STREAM_INITIALIZED, () => { onPlayerLoad(value, currentPlaybackRate, currentVolume); });

            dashPlayer.on(dashjs.MediaPlayer.events.CUE_ENTER, (e: any) => {
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

            dashPlayer.on(dashjs.MediaPlayer.events.CUE_EXIT, (e: any) => {
                document.getElementById("subtitle-" + e.cueID)?.remove();
            });

            dashPlayer.updateSettings({
                // debug: {
                //     logLevel: dashjs.LogLevel.LOG_LEVEL_INFO
                // },
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
                // value.url = 'https://dash.akamaized.net/akamai/bbb_30fps/bbb_30fps.mpd';
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
                window.targetAPI.sendPlaybackError({
                    message: `HLS player error: ${JSON.stringify(data)}`
                });
            });

            hlsPlayer.on(Hls.Events.LEVEL_LOADED, (eventName, level: LevelLoadedData) => {
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
            videoElement.onplay = () => { sendPlaybackUpdate(1); playerCtrlStateUpdate(PlayerControlEvent.Play); };
            videoElement.onpause = () => { sendPlaybackUpdate(2); playerCtrlStateUpdate(PlayerControlEvent.Pause); };
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
                const updateVolume = videoElement.muted ? 0 : videoElement.volume;
                playerCtrlStateUpdate(PlayerControlEvent.VolumeChange);
                window.targetAPI.sendVolumeUpdate({ generationTime: Date.now(), volume: updateVolume });
            };

            videoElement.onerror = (event: Event | string, source?: string, lineno?: number, colno?: number, error?: Error) => {
                console.error("Player error", {source, lineno, colno, error});
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

                onPlayerLoad(value, currentPlaybackRate, currentVolume); };
        }
    }

    // Sender generated event handlers
    window.targetAPI.onPause(() => { player.pause(); });
    window.targetAPI.onResume(() => { player.play(); });
    window.targetAPI.onSeek((_event, value: SeekMessage) => { player.setCurrentTime(value.time); });
    window.targetAPI.onSetVolume((_event, value: SetVolumeMessage) => { volumeChangeHandler(value.volume); });
    window.targetAPI.onSetSpeed((_event, value: SetSpeedMessage) => { player.setPlaybackRate(value.speed); playerCtrlStateUpdate(PlayerControlEvent.SetPlaybackRate); });
};

window.targetAPI.onPlay(onPlay);

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
            playerCtrlProgressBarBuffer.setAttribute("style", "width: 0px");
            playerCtrlProgressBarProgress.setAttribute("style", "width: 0px");
            playerCtrlProgressBarHandle.setAttribute("style", `left: ${playerCtrlProgressBar.offsetLeft}px`);

            const volume = Math.round(player.getVolume() * playerCtrlVolumeBar.offsetWidth);
            playerCtrlVolumeBarProgress.setAttribute("style", `width: ${volume}px`);
            playerCtrlVolumeBarHandle.setAttribute("style", `left: ${volume + 8}px`);

            if (isLive) {
                playerCtrlLiveBadge.setAttribute("style", "display: block");
                playerCtrlPosition.setAttribute("style", "display: none");
                playerCtrlDurationSeparator.setAttribute("style", "display: none");
                playerCtrlDuration.setAttribute("style", "display: none");
            }
            else {
                playerCtrlLiveBadge.setAttribute("style", "display: none");
                playerCtrlPosition.setAttribute("style", "display: block");
                playerCtrlDurationSeparator.setAttribute("style", "display: block");
                playerCtrlDuration.setAttribute("style", "display: block");
                playerCtrlPosition.textContent = formatDuration(player.getCurrentTime());
                playerCtrlDuration.innerHTML = formatDuration(player.getDuration());
            }

            if (player.isCaptionsSupported()) {
                playerCtrlCaptions.setAttribute("style", "display: block");
                videoCaptions.setAttribute("style", "display: block");
            }
            else {
                playerCtrlCaptions.setAttribute("style", "display: none");
                videoCaptions.setAttribute("style", "display: none");
                player.enableCaptions(false);
            }
            playerCtrlStateUpdate(PlayerControlEvent.SetCaptions);
            break;
        }

        case PlayerControlEvent.Pause:
            playerCtrlAction.setAttribute("class", "play iconSize");
            stopUiHideTimer();
            break;

        case PlayerControlEvent.Play:
            playerCtrlAction.setAttribute("class", "pause iconSize");
            startUiHideTimer();
            break;

        case PlayerControlEvent.VolumeChange: {
            // console.log(`VolumeChange: isMute ${player?.isMuted()}, volume: ${player?.getVolume()}`);
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
            // console.log(`TimeUpdate: Position: ${player.getCurrentTime()}, Duration: ${player.getDuration()}`);

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
            playerControls.setAttribute("style", "opacity: 0");
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
            playerControls.setAttribute("style", "opacity: 1");
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
                videoCaptions.setAttribute("style", "display: block");
            } else {
                playerCtrlCaptions.setAttribute("class", "captions_off iconSize");
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
            const rate = player?.getPlaybackRate().toFixed(2);
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

videoElement.onclick = () => {
    if (!playerCtrlSpeedMenuShown) {
        if (player?.isPaused()) {
            player?.play();
        } else {
            player?.pause();
        }
    }
};

// Component hiding
let uiHideTimer = null;
let uiVisible = true;

function startUiHideTimer() {
    if (uiHideTimer === null) {
        uiHideTimer = window.setTimeout(() => {
            uiHideTimer = null;
            uiVisible = false;
            playerCtrlStateUpdate(PlayerControlEvent.UiFadeOut);
        }, 3000);
    }
}

function stopUiHideTimer() {
    if (uiHideTimer) {
        window.clearTimeout(uiHideTimer);
        uiHideTimer = null;
    }

    if (!uiVisible) {
        uiVisible = true;
        playerCtrlStateUpdate(PlayerControlEvent.UiFadeIn);
    }
}

document.onmouseout = () => {
    if (uiHideTimer) {
        window.clearTimeout(uiHideTimer);
        uiHideTimer = null;
    }

    uiVisible = false;
    playerCtrlStateUpdate(PlayerControlEvent.UiFadeOut);
}

document.onmousemove = () => {
    stopUiHideTimer();

    if (player && !player.isPaused()) {
        startUiHideTimer();
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

function keyDownEventListener(event: any) {
    // console.log("KeyDown", event);
    const handledCase = targetKeyDownEventListener(event);
    if (handledCase) {
        return;
    }

    switch (event.code) {
        case 'KeyF':
        case 'F11':
            playerCtrlStateUpdate(PlayerControlEvent.ToggleFullscreen);
            event.preventDefault();
            break;
        case 'Escape':
            playerCtrlStateUpdate(PlayerControlEvent.ExitFullscreen);
            event.preventDefault();
            break;
        case 'ArrowLeft':
            skipBack();
            event.preventDefault();
            break;
        case 'ArrowRight':
            skipForward();
            event.preventDefault();
            break;
        case "Home":
            player?.setCurrentTime(0);
            event.preventDefault();
            break;
        case "End":
            if (isLive) {
                setLivePosition();
            }
            else {
                player?.setCurrentTime(player?.getDuration());
            }
            event.preventDefault();
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
            break;
        case 'KeyM':
            // Mute toggle
            player?.setMute(!player?.isMuted());
            break;
        case 'ArrowUp':
            // Volume up
            volumeChangeHandler(Math.min(player?.getVolume() + volumeIncrement, 1));
            break;
        case 'ArrowDown':
            // Volume down
            volumeChangeHandler(Math.max(player?.getVolume() - volumeIncrement, 0));
            break;
        default:
            break;
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

export {
    PlayerControlEvent,
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
