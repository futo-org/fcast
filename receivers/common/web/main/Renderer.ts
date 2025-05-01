
import QRCode from 'modules/qrcode';
import * as connectionMonitor from '../ConnectionMonitor';
import { onQRCodeRendered } from 'src/main/Renderer';
import { toast, ToastIcon } from '../components/Toast';

const connectionStatusText = document.getElementById('connection-status-text');
const connectionStatusSpinner = document.getElementById('connection-spinner');
const connectionStatusCheck = document.getElementById('connection-check');
let renderedConnectionInfo = false;
let renderedAddresses = null;
let qrCodeUrl = null;
let qrWidth = null;

const logger = window.targetAPI.logger;
window.addEventListener('resize', (event) => calculateQRCodeWidth());

connectionMonitor.setUiUpdateCallbacks({
    onConnect: (connections: string[], initialUpdate: boolean = false) => {
        connectionStatusText.textContent = connections.length > 1 ? 'Multiple devices connected:\r\n Ready to cast' : 'Connected: Ready to cast';
        connectionStatusSpinner.style.display = 'none';
        connectionStatusCheck.style.display = 'inline-block';
    },
    onDisconnect: (connections: string[]) => {
        if (connections.length === 0) {
            connectionStatusText.textContent = 'Waiting for a connection';
            connectionStatusSpinner.style.display = 'inline-block';
            connectionStatusCheck.style.display = 'none';
            toast('Device disconnected', ToastIcon.INFO);
        }
        else {
            connectionStatusText.textContent = connections.length > 1 ? 'Multiple devices connected:\r\n Ready to cast' : 'Connected: Ready to cast';
            toast('A device has disconnected', ToastIcon.INFO);
        }
    },
});

window.targetAPI.onDeviceInfo(renderIPsAndQRCode);

if(window.targetAPI.getDeviceInfo()) {
    logger.info('device info already present');
    renderIPsAndQRCode();
}

function renderIPsAndQRCode() {
    const value = window.targetAPI.getDeviceInfo();
    logger.info(`Network Interface Info: ${JSON.stringify(value)}`);
    renderIPs(value.interfaces);

    const addresses = [];
    value.interfaces.forEach((e) => addresses.push(e.address));
    if (JSON.stringify(addresses) === JSON.stringify(renderedAddresses)) {
        return;
    }

    const connInfoLoading = document.getElementById('connection-information-loading');
    const connInfo = document.getElementById('connection-information');
    const connError = document.getElementById('connection-error');

    if (renderedAddresses !== null && addresses.length > 0) {
        toast('Network connections has changed, please reconnect sender devices to receiver if you experience issues', ToastIcon.WARNING);
    }
    else if (addresses.length === 0) {
        connInfoLoading.style.display = 'none';
        connInfo.style.display = 'none';
        connError.style.display = 'block';

        if (renderedAddresses !== null) {
            toast('Lost network connection, please reconnect to a network', ToastIcon.ERROR);
        }

        renderedAddresses = []
        return;
    }

    if (renderedAddresses !== null && renderedAddresses.length === 0) {
        connInfo.style.display = 'block';
        connError.style.display = 'none';
    }

    renderedAddresses = addresses;
    const fcastConfig = {
        name: value.name,
        addresses: addresses,
        services: [
            { port: 46899, type: 0 }, //TCP
            { port: 46898, type: 1 }, //WS
        ]
    };

    const json = JSON.stringify(fcastConfig);
    let base64 = btoa(json);
    base64 = base64.replace(/\+/g, '-').replace(/\//g, '_').replace(/=+$/, '');
    qrCodeUrl = `fcast://r/${base64}`;
    logger.info('QR Code:', {json, qrCodeUrl, base64});

    calculateQRCodeWidth();
    if (!renderedConnectionInfo) {
        connInfoLoading.style.display = 'none';
        connInfo.style.display = 'block';
    }
}

function renderIPs(interfaces: any) {
    const ipsElement = document.getElementById('ips');

    if (ipsElement) {
        const ipsIconColumn = document.getElementById('ips-iface-icon');
        ipsIconColumn.innerHTML = '';

        const ipsTextColumn = document.getElementById('ips-iface-text');
        ipsTextColumn.innerHTML = '';

        const ipsNameColumn = document.getElementById('ips-iface-name');
        ipsNameColumn.innerHTML = '';

        for (const iface of interfaces) {
            const ipIcon = document.createElement('div');
            let icon = 'iconSize ';
            if (iface.type === 'wired') {
                icon += 'ip-wired-icon';
            }
            else if (iface.type === 'wireless' && (iface.signalLevel === 0 || iface.signalLevel >= 90)) {
                icon += 'ip-wireless-4-icon';
            }
            else if (iface.type === 'wireless' && iface.signalLevel >= 70) {
                icon += 'ip-wireless-3-icon';
            }
            else if (iface.type === 'wireless' && iface.signalLevel >= 50) {
                icon += 'ip-wireless-2-icon';
            }
            else if (iface.type === 'wireless' && iface.signalLevel >= 30) {
                icon += 'ip-wireless-1-icon';
            }
            else if (iface.type === 'wireless') {
                icon += 'ip-wireless-0-icon';
            }

            ipIcon.className = icon;
            ipsIconColumn.append(ipIcon);

            const ipText = document.createElement('div');
            ipText.className = 'ip-entry-text';
            ipText.textContent = iface.address;
            ipsTextColumn.append(ipText);

            const ipName = document.createElement('div');
            ipName.className = 'ip-entry-text';
            ipName.textContent = iface.name;
            ipsNameColumn.append(ipName);
        }
    }
}

function calculateQRCodeWidth() {
    if (qrCodeUrl !== null) {
        let changedQrWidth = null;

        if ((window.innerWidth >= 2560) || (window.innerHeight >= 1440)) {
            changedQrWidth = 384;
        }
        if ((window.innerWidth >= 1920 && window.innerWidth < 2560) || (window.innerHeight >= 1080 && window.innerHeight < 1440)) {
            changedQrWidth = 256;
        }
        if ((window.innerWidth >= 1280 && window.innerWidth < 1920) || (window.innerHeight >= 720 && window.innerHeight < 1080)) {
            changedQrWidth = 192;
        }
        if (window.innerWidth < 1280 || window.innerHeight < 720) {
            changedQrWidth = 128;
        }

        if (qrWidth !== changedQrWidth) {
            qrWidth = changedQrWidth;
            renderQRCode(qrCodeUrl);
        }
    }
}

function renderQRCode(url: string) {
    const qrCodeElement = document.getElementById('qr-code');
    QRCode.toCanvas(qrCodeElement, url, {
        margin: 0,
        width: qrWidth,
        color: {
            dark : '#000000',
            light : '#ffffff',
        },
        errorCorrectionLevel : 'M',
    },
    (err) => {
        if (err) {
            logger.error(`Error rendering QR Code: ${err}`);
            toast(`Error rendering QR Code: ${err}`, ToastIcon.ERROR);
        }
        else {
            logger.info(`Rendered QR Code`);
        }
    });

    onQRCodeRendered();
}
