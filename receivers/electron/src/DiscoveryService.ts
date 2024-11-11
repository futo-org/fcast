import mdns from 'mdns-js';
const cp = require('child_process');
const os = require('os');

export class DiscoveryService {
    // eslint-disable-next-line @typescript-eslint/no-explicit-any
    private serviceTcp: any;
    // eslint-disable-next-line @typescript-eslint/no-explicit-any
    private serviceWs: any;

    private static getComputerName() {
        switch (process.platform) {
            case "win32":
                return process.env.COMPUTERNAME;
            case "darwin":
                return cp.execSync("scutil --get ComputerName").toString().trim();
            case "linux": {
                let hostname: string;

                // Some distro's don't work with `os.hostname()`, but work with `hostnamectl` and vice versa...
                try {
                    hostname = os.hostname();
                }
                catch (err) {
                    console.warn('Error fetching hostname, trying different method...');
                    console.warn(err);

                    try {
                        hostname = cp.execSync("hostnamectl hostname").toString().trim();
                    }
                    catch (err2) {
                        console.warn('Error fetching hostname again, using generic name...');
                        console.warn(err2);

                        hostname = 'linux device';
                    }
                }

                return hostname;
            }

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