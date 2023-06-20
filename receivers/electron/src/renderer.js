function toggleFullScreen(ev) {
    window.electronAPI.toggleFullScreen();
}

const options = {
    textTrackSettings: false
};
const player = videojs("video-player", options, function onPlayerReady() {
    const fullScreenControls = document.getElementsByClassName("vjs-fullscreen-control");
    for (let i = 0; i < fullScreenControls.length; i++) {
        const node = fullScreenControls[i].cloneNode(true);
        fullScreenControls[i].parentNode.replaceChild(node, fullScreenControls[i]);
        fullScreenControls[i].onclick = toggleFullScreen;
        fullScreenControls[i].ontap = toggleFullScreen;
    }
});

player.on("pause", () => { window.electronAPI.sendPlaybackUpdate({ time: Math.round(player.currentTime()), state: 2 }) });
player.on("play", () => { window.electronAPI.sendPlaybackUpdate({ time: Math.round(player.currentTime()), state: 1 }) });
player.on("seeked", () => { window.electronAPI.sendPlaybackUpdate({ time: Math.round(player.currentTime()), state: player.paused() ? 2 : 1 }) });
player.on("volumechange", () => { window.electronAPI.sendVolumeUpdate({ volume: player.volume() }); });

window.electronAPI.onPlay((_event, value) => {
    console.log("Handle play message renderer", value);

    if (value.content) {
        player.src({ type: value.container, src: `data:${value.container};base64,` + window.btoa(value.content) });
    } else {
        player.src({ type: value.container, src: value.url });
    }

    player.play();

    if (value.time) {
        player.currentTime(value.time);
    }
});

window.electronAPI.onPause((_event) => {
    console.log("Handle pause");
    player.pause();
});

window.electronAPI.onResume((_event) => {
    console.log("Handle resume");
    player.play();
});

window.electronAPI.onSeek((_event, value) => {
    console.log("Handle seek");
    player.currentTime(value.time);
});

window.electronAPI.onSetVolume((_event, value) => {
    console.log("Handle setVolume");
    player.volume(Math.min(1.0, Math.max(0.0, value.volume)));
});

setInterval(() => {
    window.electronAPI.sendPlaybackUpdate({ time: Math.round(player.currentTime()), state: player.paused() ? 2 : 1 });
}, 1000);

let mouseTimer = null;
let cursorVisible = true;

//Hide mouse cursor

function startMouseHideTimer() {
    mouseTimer = window.setTimeout(() => {
        mouseTimer = null;
        document.body.style.cursor = "none";
        cursorVisible = false;
    }, 3000);
}

document.onmousemove = function() {
    if (mouseTimer) {
        window.clearTimeout(mouseTimer);
    }

    if (!cursorVisible) {
        document.body.style.cursor = "default";
        cursorVisible = true;
    }

    startMouseHideTimer();
};

startMouseHideTimer();

// Add the keydown event listener to the document
const skipInterval = 10;
const volumeIncrement = 0.1;

document.addEventListener('keydown', (event) => {
console.log("KeyDown", event);

    switch (event.code) {
        case 'F11':
            window.electronAPI.toggleFullScreen();
            event.preventDefault();
            break;
        case 'Escape':
            window.electronAPI.exitFullScreen();
            event.preventDefault();
            break;
        case 'ArrowLeft':
            // Skip back
            player.currentTime(Math.max(player.currentTime() - skipInterval, 0));
            event.preventDefault();
            break;
        case 'ArrowRight':
            // Skip forward
            player.currentTime(Math.min(player.currentTime() + skipInterval, player.duration()));
            event.preventDefault();
            break;
        case 'Space':
        case 'Enter':
            // Pause/Continue
            if (player.paused()) {
                player.play();
            } else {
                player.pause();
            }
            event.preventDefault();
            break;
        case 'KeyM':
            // Mute toggle
            player.muted(!player.muted());
            break;
        case 'ArrowUp':
            // Volume up
            player.volume(Math.min(player.volume() + volumeIncrement, 1));
            break;
        case 'ArrowDown':
            // Volume down
            player.volume(Math.max(player.volume() - volumeIncrement, 0));
            break;
    }
});

//Select subtitle track by default
player.ready(() => {
    const textTracks = player.textTracks();
    textTracks.addEventListener("change", function () {
        console.log("Text tracks changed", textTracks);
        for (let i = 0; i < textTracks.length; i++) {
            if (textTracks[i].language === "en" && textTracks[i].mode !== "showing") {
                textTracks[i].mode = "showing";
            }
        }
    });

    player.on('loadedmetadata', function () {
        console.log("Metadata loaded", textTracks);
        for (let i = 0; i < textTracks.length; i++) {
            if (textTracks[i].language === "en" && textTracks[i].mode !== "showing") {
                textTracks[i].mode = "showing";
            }
        }
    });    
});