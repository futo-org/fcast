import mdns from 'modules/@futo/mdns-js';
import { Logger, LoggerType } from 'common/Logger';
import { getAppName, getAppVersion, getComputerName } from 'src/Main';
import { PROTOCOL_VERSION } from 'common/Packets';
const logger = new Logger('DiscoveryService', LoggerType.BACKEND);

export class DiscoveryService {
    private serviceTcp: any;
    private serviceWs: any;

    start() {
        if (this.serviceTcp || this.serviceWs) {
            return;
        }

        const name = getComputerName();
        logger.info(`Discovery service started: ${name}`);

        // Note that txt field must be populated, otherwise certain mdns stacks have undefined behavior/issues
        // when connecting to the receiver. Also mdns-js internally gets a lot of parsing errors when txt records
        // are not defined.
        this.serviceTcp = mdns.createAdvertisement(mdns.tcp('_fcast'), 46899, { name: name, txt: {
            version: PROTOCOL_VERSION,
            appName: getAppName(),
            appVersion: getAppVersion(),
        } });
        this.serviceTcp.start();
        this.serviceWs = mdns.createAdvertisement(mdns.tcp('_fcast-ws'), 46898, { name: name, txt: {
            version: PROTOCOL_VERSION,
            appName: getAppName(),
            appVersion: getAppVersion(),
        } });
        this.serviceWs.start();
    }

    stop() {
        if (this.serviceTcp) {
            this.serviceTcp.stop();
            this.serviceTcp = null;
        }

        if (this.serviceWs) {
            this.serviceWs.stop();
            this.serviceWs = null;
        }
    }
}
