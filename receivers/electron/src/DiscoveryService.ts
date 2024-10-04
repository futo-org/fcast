import mdns = require('mdns-js');
const cp = require('child_process');
const os = require('os');

export class DiscoveryService {
    private serviceTcp: any;
    private serviceWs: any;

    private static getComputerName() {
        switch (process.platform) {
            case "win32":
                return process.env.COMPUTERNAME;
            case "darwin":
                return cp.execSync("scutil --get ComputerName").toString().trim();
            case "linux":
                return os.hostname();
            default:
                return os.hostname();
        }
    }

    start() {
        if (this.serviceTcp || this.serviceWs) {
            return;
        }

        const name = `FCast-${DiscoveryService.getComputerName()}`;
        console.log("Discovery service started.", name);

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