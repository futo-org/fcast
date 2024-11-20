import QRCode from 'qrcode';

const updateView = document.getElementById("update-view");
const updateViewTitle = document.getElementById("update-view-title");
const updateText = document.getElementById("update-text");
const updateButton = document.getElementById("update-button");
const restartButton = document.getElementById("restart-button");
const updateLaterButton = document.getElementById("update-later-button");
const progressBar = document.getElementById("progress-bar");
const progressBarProgress = document.getElementById("progress-bar-progress");

let updaterProgressUIUpdateTimer = null;
window.electronAPI.onDeviceInfo(renderIPsAndQRCode);

if(window.electronAPI.getDeviceInfo()) {
    console.log("device info already present");
    renderIPsAndQRCode();
}

function renderIPsAndQRCode() {
    const value = window.electronAPI.getDeviceInfo();
    console.log("device info", value);

    const ipsElement = document.getElementById('ips');
    if (ipsElement) {
        ipsElement.innerHTML = `IPs<br>${value.addresses.join('<br>')}`;
    }

    const fcastConfig = {
        name: value.name,
        addresses: value.addresses,
        services: [
            { port: 46899, type: 0 }, //TCP
            { port: 46898, type: 1 }, //WS
        ]
    };

    const json = JSON.stringify(fcastConfig);
    let base64 = btoa(json);
    base64 = base64.replace(/\+/g, '-').replace(/\//g, '_').replace(/=+$/, '');
    const url = `fcast://r/${base64}`;
    console.log("qr", {json, url, base64});

    const qrCodeElement = document.getElementById('qr-code');
    QRCode.toCanvas(qrCodeElement, url, {
        margin: 0,
        width: 256,
        color: {
            dark : "#000000",
            light : "#ffffff",
        },
        errorCorrectionLevel : "M",
    },
    (e) => {
        console.log(`Error rendering QR Code: ${e}`)
    });
}

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
