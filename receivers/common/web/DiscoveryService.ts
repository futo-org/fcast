import mdns from 'modules/mdns-js';
import { Logger, LoggerType } from 'common/Logger';
import { getComputerName } from 'src/Main';
const logger = new Logger('DiscoveryService', LoggerType.BACKEND);

export class DiscoveryService {
    private serviceTcp: any;
    private serviceWs: any;

    start() {
        if (this.serviceTcp || this.serviceWs) {
            return;
        }

        const name = `FCast-${getComputerName()}`;
        logger.info(`Discovery service started: ${name}`);

        this.serviceTcp = mdns.createAdvertisement(mdns.tcp('_fcast'), 46899, { name: name });
        this.serviceTcp.start();
        this.serviceWs = mdns.createAdvertisement(mdns.tcp('_fcast-ws'), 46898, { name: name });
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
