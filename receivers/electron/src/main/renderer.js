const options = {
    textTrackSettings: false,
    autoplay: true,
    loop: true,
    controls: false
};

const player = videojs("video-player", options, function onPlayerReady() {
    player.src({ type: "video/mp4", src: "./c.mp4" });
});


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
    new QRCode(qrCodeElement, {
        text: url,
        width: 256,
        height: 256,
        colorDark : "#000000",
        colorLight : "#ffffff",
        correctLevel : QRCode.CorrectLevel.M
    });
}
