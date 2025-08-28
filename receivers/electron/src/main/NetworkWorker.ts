import { ipcRenderer } from 'electron';
import si from 'modules/systeminformation';
import { Logger, LoggerType } from 'common/Logger';
const logger = new Logger('NetworkWorker', LoggerType.FRONTEND);

const networkStateChangeListenerTimeout = 2500;
let interfaces = new Map<string, any>();

networkStateChangeListener(true);
setInterval(networkStateChangeListener, networkStateChangeListenerTimeout);

function networkStateChangeListener(forceUpdate: boolean) {
    new Promise<void>((resolve) => {
        si.networkInterfaces((data) => {
            // logger.info(data);
            const queriedInterfaces = Array.isArray(data) ? data : [data];

            si.wifiConnections((data) => {
                // logger.info(data);
                const wifiConnections = Array.isArray(data) ? data : [data];

                let changed = false;
                let wifiSignalUpdate = false;
                let validInterfaces = queriedInterfaces.filter(v => v.ip4 !== '' && !v.internal && !v.virtual);
                if (validInterfaces.length !== interfaces.size) {
                    interfaces.clear();
                }

                for (const iface of validInterfaces) {
                    const wifiInterface = wifiConnections.find(e => e.iface === iface.iface);

                    if (wifiInterface === undefined) {
                        if (!interfaces.has(iface.ip4)) {
                            interfaces.set(iface.ip4, { type: 'wired', name: iface.iface, address: iface.ip4 });
                            changed = true;
                        }
                    }
                    else {
                        let entry = interfaces.get(iface.ip4);

                        if (entry === undefined) {
                            interfaces.set(iface.ip4, { type: 'wireless', name: wifiInterface.ssid, address: iface.ip4, signalLevel: wifiInterface.quality });
                            changed = true;
                        }
                        else if (entry.name !== wifiInterface.ssid || entry.signalLevel !== wifiInterface.quality) {
                            interfaces.set(iface.ip4, { type: 'wireless', name: wifiInterface.ssid, address: iface.ip4, signalLevel: wifiInterface.quality });
                            changed = true;
                            wifiSignalUpdate = true;
                        }
                    }
                }

                if (forceUpdate || changed) {
                    ipcRenderer.send('network-changed', Array.from(interfaces.values()), wifiSignalUpdate);
                }

                resolve();
            });
        });
    });
}
