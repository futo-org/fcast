import { ipcRenderer } from 'electron';
import si from 'modules/systeminformation';

const networkStateChangeListenerTimeout = 2500;
let networkStateChangeListenerInterfaces = [];

networkStateChangeListener(true);
setInterval(networkStateChangeListener, networkStateChangeListenerTimeout);

function networkStateChangeListener(forceUpdate: boolean) {
    new Promise<void>((resolve) => {
        si.networkInterfaces((data) => {
            // console.log(data);
            const queriedInterfaces = Array.isArray(data) ? data : [data];

            si.wifiConnections((data) => {
                // console.log(data);
                const wifiConnections = Array.isArray(data) ? data : [data];

                const interfaces = [];
                for (const iface of queriedInterfaces) {
                    if (iface.ip4 !== '' && !iface.internal && !iface.virtual) {
                        const isWireless = wifiConnections.some(e => {
                            if (e.iface === iface.iface) {
                                interfaces.push({ type: 'wireless', name: e.ssid, address: iface.ip4, signalLevel: e.quality });
                                return true;
                            }

                            return false;
                        });

                        if (!isWireless) {
                            interfaces.push({ type: 'wired', name: iface.ifaceName, address: iface.ip4 });
                        }
                    }
                }

                if (forceUpdate || (JSON.stringify(interfaces) !== JSON.stringify(networkStateChangeListenerInterfaces))) {
                    networkStateChangeListenerInterfaces = interfaces;
                    ipcRenderer.send('network-changed', interfaces);
                }

                resolve();
            });
        });
    });
}
