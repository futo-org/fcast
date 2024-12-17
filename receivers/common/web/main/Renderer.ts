
import QRCode from 'modules/qrcode';
import { onQRCodeRendered } from 'src/main/Renderer';

window.targetAPI.onDeviceInfo(renderIPsAndQRCode);

if(window.targetAPI.getDeviceInfo()) {
    console.log("device info already present");
    renderIPsAndQRCode();
}

function renderIPsAndQRCode() {
    const value = window.targetAPI.getDeviceInfo();
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

    onQRCodeRendered();
}
