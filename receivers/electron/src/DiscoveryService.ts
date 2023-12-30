import mdns = require('mdns-js');
const cp = require('child_process');
const os = require('os');

export class DiscoveryService {
    private serviceTcp: any;
    private serviceTls: any;
    private serviceWs: any;
    private serviceWss: any;

    private static getComputerName() {
        switch (process.platform) {
            case "win32":
                return process.env.COMPUTERNAME;
            case "darwin":
                return cp.execSync("scutil --get ComputerName").toString().trim();
            case "linux":
                const prettyname = cp.execSync("hostnamectl --pretty").toString().trim();
                return prettyname === "" ? os.hostname() : prettyname;
            default:
                return os.hostname();
        }
    }

    start() {
        if (this.serviceTcp || this.serviceTls || this.serviceWs || this.serviceWss) {
            return;
        }

        const name = `FCast-${DiscoveryService.getComputerName()}`;
        console.log("Discovery service started.", name);

        this.serviceTcp = mdns.createAdvertisement(mdns.tcp('_fcast'), 46899, { name: name });
        this.serviceTcp.start();
        this.serviceTls = mdns.createAdvertisement(mdns.tcp('_fcast-tls'), 46897, { name: name });
        this.serviceTls.start();
        this.serviceWs = mdns.createAdvertisement(mdns.tcp('_fcast-ws'), 46898, { name: name });
        this.serviceWs.start();
        this.serviceWss = mdns.createAdvertisement(mdns.tcp('_fcast-wss'), 46896, { name: name });
        this.serviceWss.start();
    }

    stop() {
        if (this.serviceTcp) {
            this.serviceTcp.stop();
            this.serviceTcp = null;
        }

        if (this.serviceTls) {
            this.serviceTls.stop();
            this.serviceTls = null;
        }

        if (this.serviceWs) {
            this.serviceWs.stop();
            this.serviceWs = null;
        }

        if (this.serviceWss) {
            this.serviceWss.stop();
            this.serviceWss = null;
        }
    }
}