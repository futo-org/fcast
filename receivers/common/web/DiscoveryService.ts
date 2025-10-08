import mdns from 'modules/@futo/mdns-js';
import { Logger, LoggerType } from 'common/Logger';
import { getAppName, getAppVersion, getComputerName } from 'src/Main';
import { PROTOCOL_VERSION } from 'common/Packets';
import { TcpListenerService } from './TcpListenerService';
import { WebSocketListenerService } from './WebSocketListenerService';
const logger = new Logger('DiscoveryService', LoggerType.BACKEND);

export class DiscoveryService {
    private serviceTcp: any;

    start() {
        if (this.serviceTcp) {
            return;
        }

        const name = getComputerName();
        logger.info(`Discovery service started: ${name}`);

        // Note that txt field must be populated, otherwise certain mdns stacks have undefined behavior/issues
        // when connecting to the receiver. Also mdns-js internally gets a lot of parsing errors when txt records
        // are not defined.
        this.serviceTcp = mdns.createAdvertisement(mdns.tcp('_fcast'), TcpListenerService.PORT,
        { name: name, txt: {
            version: PROTOCOL_VERSION,
            appName: getAppName(),
            appVersion: getAppVersion(),
        } });
        this.serviceTcp.start();
    }

    stop() {
        if (this.serviceTcp) {
            this.serviceTcp.stop();
            this.serviceTcp = null;
        }
    }
}
