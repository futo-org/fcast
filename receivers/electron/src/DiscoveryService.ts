import mdns = require('mdns-js');
const cp = require('child_process');
const os = require('os');

export class DiscoveryService {
    private service: any;

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
        if (this.service) {
            return;
        }

        const name = `FCast-${DiscoveryService.getComputerName()}`;
        console.log("Discovery service started.", name);

        this.service = mdns.createAdvertisement(mdns.tcp('_fcast'), 46899, { name: name });
        this.service.start();
    }

    stop() {
        if (!this.service) {
            return;
        }

        this.service.stop();
        this.service = null;
    }
}