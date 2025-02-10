import 'common/main/Renderer';

export function onQRCodeRendered() {}

const updateView = document.getElementById("update-view");
const updateViewTitle = document.getElementById("update-view-title");
const updateText = document.getElementById("update-text");
const updateButton = document.getElementById("update-button");
const restartButton = document.getElementById("restart-button");
const updateLaterButton = document.getElementById("update-later-button");
const progressBar = document.getElementById("progress-bar");
const progressBarProgress = document.getElementById("progress-bar-progress");

let updaterProgressUIUpdateTimer = null;

window.electronAPI.onUpdateAvailable(() => {
    console.log(`Received UpdateAvailable event`);
    updateViewTitle.textContent = 'FCast update available';

    updateText.textContent = 'Do you wish to update now?';
    updateButton.setAttribute("style", "display: block");
    updateLaterButton.setAttribute("style", "display: block");
    restartButton.setAttribute("style", "display: none");
    progressBar.setAttribute("style", "display: none");
    updateView.setAttribute("style", "display: flex");
});

window.electronAPI.onDownloadComplete(() => {
    console.log(`Received DownloadComplete event`);
    window.clearTimeout(updaterProgressUIUpdateTimer);
    updateViewTitle.textContent = 'FCast update ready';

    updateText.textContent = 'Restart now to apply the changes?';
    updateButton.setAttribute("style", "display: none");
    progressBar.setAttribute("style", "display: none");
    restartButton.setAttribute("style", "display: block");
    updateLaterButton.setAttribute("style", "display: block");
    updateView.setAttribute("style", "display: flex");
});

window.electronAPI.onDownloadFailed(() => {
    console.log(`Received DownloadFailed event`);
    window.clearTimeout(updaterProgressUIUpdateTimer);
    updateView.setAttribute("style", "display: none");
});

updateLaterButton.onclick = () => { updateView.setAttribute("style", "display: none"); };
updateButton.onclick = () => {
    updaterProgressUIUpdateTimer = window.setInterval( async () => {
        const updateProgress = await window.electronAPI.updaterProgress();

        if (updateProgress >= 1.0) {
            updateText.textContent = "Preparing update...";
            progressBarProgress.setAttribute("style", `width: 100%`);
        }
        else {
            progressBarProgress.setAttribute("style", `width: ${Math.max(12, updateProgress * 100)}%`);
        }
    }, 500);

    updateText.textContent = 'Downloading...';
    updateButton.setAttribute("style", "display: none");
    updateLaterButton.setAttribute("style", "display: none");
    progressBarProgress.setAttribute("style", "width: 12%");
    progressBar.setAttribute("style", "display: block");
    window.electronAPI.sendDownloadRequest();
};
restartButton.onclick = () => { window.electronAPI.sendRestartRequest(); };
