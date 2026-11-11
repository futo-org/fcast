import mdns from 'modules/@futo/mdns-js';
import { Logger, LoggerType } from 'common/Logger';
import { getAppName, getAppVersion, getComputerName, getV4Context } from 'src/Main';
import { PROTOCOL_VERSION } from 'common/Packets';
import { TcpListenerService } from './TcpListenerService';
const logger = new Logger('DiscoveryService', LoggerType.BACKEND);

export function buildTxtRecord(appName: string, appVersion: string, v4: { fingerprint: string } | null): Record<string, string | number> {
    const txt: Record<string, string | number> = {
        version: PROTOCOL_VERSION,
        appName: appName,
        appVersion: appVersion,
    };
    if (v4 !== null) {
        txt.v = '4';
        txt.fp = v4.fingerprint;
    }
    return txt;
}

export interface Advertisement {
    start(): void;
    stop(): void;
}

function createAdvertisement(): Advertisement {
    // Note that txt field must be populated, otherwise certain mdns stacks have undefined behavior/issues
    // when connecting to the receiver. Also mdns-js internally gets a lot of parsing errors when txt records
    // are not defined.
    return mdns.createAdvertisement(mdns.tcp('_fcast'), TcpListenerService.PORT,
        { name: getComputerName(), txt: buildTxtRecord(getAppName(), getAppVersion(), getV4Context()) });
}

export class DiscoveryService {
    private serviceTcp: Advertisement = null;
    private advertisedAddresses: string = null;

    constructor(private readonly create: () => Advertisement = createAdvertisement) {}

    start() {
        if (this.serviceTcp) {
            return;
        }

        logger.info(`Discovery service started: ${getComputerName()}`);
        this.serviceTcp = this.create();
        this.serviceTcp.start();
    }

    stop() {
        if (this.serviceTcp) {
            this.serviceTcp.stop();
            this.serviceTcp = null;
        }
    }

    // Starts the advertisement, or starts it again when the device's addresses are no longer the
    // ones it was started on. The app reports its network status every time its main screen
    // loads; restarting on each report would pile up advertisements in the mDNS library, which
    // keeps answering for every one it was ever asked to stop.
    // Returns true when the advertisement was (re)started.
    update(addresses: string[]): boolean {
        const key = addresses.slice().sort().join(',');
        if (this.serviceTcp && key === this.advertisedAddresses) {
            return false;
        }

        this.stop();
        this.start();
        this.advertisedAddresses = key;
        return true;
    }
}
