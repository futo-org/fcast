
import QRCode from 'modules/qrcode';
import { onQRCodeRendered } from 'src/main/Renderer';
import { toast, ToastIcon } from '../components/Toast';

const connectionStatusText = document.getElementById("connection-status-text");
const connectionStatusSpinner = document.getElementById("connection-spinner");
const connectionStatusCheck = document.getElementById("connection-check");
let connections = [];

// Window might be re-created while devices are still connected
window.targetAPI.onPing((_event, value: any) => {
    if (value && connections.length === 0) {
        connections.push(value.id);
        onConnect(value.id);
    }
});

window.targetAPI.onDeviceInfo(renderIPsAndQRCode);
window.targetAPI.onConnect((_event, value: any) => {
    connections.push(value.id);
    onConnect(value);
});
window.targetAPI.onDisconnect((_event, value: any) => {
    console.log(`Device disconnected: ${JSON.stringify(value)}`);
    const index = connections.indexOf(value.id);
    if (index != -1) {
        connections.splice(index, 1);
    }

    if (connections.length === 0) {
        connectionStatusText.textContent = 'Waiting for a connection';
        connectionStatusSpinner.style.display = 'inline-block';
        connectionStatusCheck.style.display = 'none';
        toast("Device disconnected", ToastIcon.INFO);
    }
});

if(window.targetAPI.getDeviceInfo()) {
    console.log("device info already present");
    renderIPsAndQRCode();
}

function onConnect(value: any) {
    console.log(`Device connected: ${JSON.stringify(value)}`);
    connectionStatusText.textContent = 'Connected: Ready to cast';
    connectionStatusSpinner.style.display = 'none';
    connectionStatusCheck.style.display = 'inline-block';
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
    (err) => {
        if (err) {
            console.error(`Error rendering QR Code: ${err}`);
            toast(`Error rendering QR Code: ${err}`, ToastIcon.ERROR);
        }
        else {
            console.log(`Rendered QR Code`);
        }
    });

    onQRCodeRendered();
}
