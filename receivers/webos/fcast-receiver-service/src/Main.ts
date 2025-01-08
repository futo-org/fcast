/* eslint-disable @typescript-eslint/no-explicit-any */
// No node module for this package, only exists in webOS environment
// eslint-disable-next-line @typescript-eslint/ban-ts-comment
// @ts-ignore
const Service = __non_webpack_require__('webos-service');
// const Service = require('webos-service');

import { PlayMessage, PlaybackErrorMessage, PlaybackUpdateMessage, SeekMessage, SetSpeedMessage, SetVolumeMessage, VolumeUpdateMessage } from 'common/Packets';
import { DiscoveryService } from 'common/DiscoveryService';
import { TcpListenerService } from 'common/TcpListenerService';
import { WebSocketListenerService } from 'common/WebSocketListenerService';
import { NetworkService } from 'common/NetworkService';
import { Opcode } from 'common/FCastSession';
import * as os from 'os';
import * as log4js from "log4js";
import { EventEmitter } from 'events';
import { ToastIcon } from 'common/components/Toast';

export class Main {
    static tcpListenerService: TcpListenerService;
    static webSocketListenerService: WebSocketListenerService;
    static discoveryService: DiscoveryService;
    static logger: log4js.Logger;
    static emitter: EventEmitter;

	static {
		try {
            log4js.configure({
                appenders: {
                    console: { type: 'console' },
                },
                categories: {
                    default: { appenders: ['console'], level: 'info' },
                },
            });
            Main.logger = log4js.getLogger();
            Main.logger.info(`OS: ${process.platform} ${process.arch}`);

            const serviceId = 'com.futo.fcast.receiver.service';
            const service = new Service(serviceId);

            // Service will timeout and casting will disconnect if not forced to be kept alive
            // eslint-disable-next-line @typescript-eslint/no-unused-vars
            let keepAlive;
            service.activityManager.create("keepAlive", function(activity) {
                keepAlive = activity;
            });

            const voidCb = (message: any) => { message.respond({ returnValue: true, value: {} }); };
            const objectCb = (message: any, value: any) => { message.respond({ returnValue: true, value: value }); };

            let toastClosureCb = null;
            service.register("toast", (message: any) => {
                if (message.isSubscription) {
                    toastClosureCb = objectCb.bind(this, message);
                    Main.emitter.on('toast', toastClosureCb);
                }

                message.respond({ returnValue: true, value: { subscribed: true }});
            },
            (message: any) => {
                Main.logger.info('Canceled toast service subscriber');
                Main.emitter.off('toast', toastClosureCb);
                message.respond({ returnValue: true, value: message.payload });
            });

            service.register("getDeviceInfo", (message: any) => {
                Main.logger.info("In getDeviceInfo callback");

                message.respond({
                    returnValue: true,
                    value: { name: os.hostname(), addresses: NetworkService.getAllIPv4Addresses() }
                });
            });

            let connectClosureCb = null;
            service.register("connect", (message: any) => {
                if (message.isSubscription) {
                    connectClosureCb = objectCb.bind(this, message);
                    Main.emitter.on('connect', connectClosureCb);
                }

                message.respond({ returnValue: true, value: { subscribed: true }});
            },
            (message: any) => {
                Main.logger.info('Canceled connect service subscriber');
                Main.emitter.off('connect', connectClosureCb);
                message.respond({ returnValue: true, value: message.payload });
            });

            let disconnectClosureCb = null;
            service.register("disconnect", (message: any) => {
                if (message.isSubscription) {
                    disconnectClosureCb = objectCb.bind(this, message);
                    Main.emitter.on('disconnect', disconnectClosureCb);
                }

                message.respond({ returnValue: true, value: { subscribed: true }});
            },
            (message: any) => {
                Main.logger.info('Canceled disconnect service subscriber');
                Main.emitter.off('disconnect', disconnectClosureCb);
                message.respond({ returnValue: true, value: message.payload });
            });

            let pingClosureCb = null;
            service.register("ping", (message: any) => {
                if (message.isSubscription) {
                    pingClosureCb = objectCb.bind(this, message);
                    Main.emitter.on('ping', pingClosureCb);
                }

                message.respond({ returnValue: true, value: { subscribed: true }});
            },
            (message: any) => {
                Main.logger.info('Canceled ping service subscriber');
                Main.emitter.off('ping', pingClosureCb);
                message.respond({ returnValue: true, value: message.payload });
            });

            Main.discoveryService = new DiscoveryService();
            Main.discoveryService.start();

            Main.tcpListenerService = new TcpListenerService();
            Main.webSocketListenerService = new WebSocketListenerService();

            Main.emitter = new EventEmitter();
            let playData: PlayMessage = null;

            let playClosureCb = null;
            const playCb = (message: any, playMessage: PlayMessage) => {
                playData = playMessage;
                message.respond({ returnValue: true, value: { playData: playData } });
            };

            let pauseClosureCb: any = null;
            let resumeClosureCb: any = null;
            let stopClosureCb: any  = null;

            let seekClosureCb = null;
            const seekCb = (message: any, seekMessage: SeekMessage) => { message.respond({ returnValue: true, value: seekMessage }); };

            let setVolumeClosureCb = null;
            const setVolumeCb = (message: any, volumeMessage: SetVolumeMessage) => { message.respond({ returnValue: true, value: volumeMessage }); };

            let setSpeedClosureCb = null;
            const setSpeedCb = (message: any, speedMessage: SetSpeedMessage) => { message.respond({ returnValue: true, value: speedMessage }); };

            // Note: When logging the `message` object, do NOT use JSON.stringify, you can log messages directly. Seems to be a circular reference causing errors...
            // const playService = service.register("play", (message) => {
            service.register("play", (message: any) => {
                if (message.isSubscription) {
                    playClosureCb = playCb.bind(this, message);
                    Main.emitter.on('play', playClosureCb);
                }

                message.respond({ returnValue: true, value: { subscribed: true, playData: playData }});
            },
            (message: any) => {
                Main.logger.info('Canceled play service subscriber');
                Main.emitter.off('play', playClosureCb);
                message.respond({ returnValue: true, value: message.payload });
            });

            service.register("pause", (message: any) => {
                if (message.isSubscription) {
                    pauseClosureCb = voidCb.bind(this, message);
                    Main.emitter.on('pause', pauseClosureCb);
                }

                message.respond({ returnValue: true, value: { subscribed: true }});
            },
            (message: any) => {
                Main.logger.info('Canceled pause service subscriber');
                Main.emitter.off('pause', pauseClosureCb);
                message.respond({ returnValue: true, value: message.payload });
            });

            service.register("resume", (message: any) => {
                if (message.isSubscription) {
                    resumeClosureCb = voidCb.bind(this, message);
                    Main.emitter.on('resume', resumeClosureCb);
                }

                message.respond({ returnValue: true, value: { subscribed: true }});
            },
            (message: any) => {
                Main.logger.info('Canceled resume service subscriber');
                Main.emitter.off('resume', resumeClosureCb);
                message.respond({ returnValue: true, value: message.payload });
            });

            service.register("stop", (message: any) => {
                playData = null;

                if (message.isSubscription) {
                    stopClosureCb = voidCb.bind(this, message);
                    Main.emitter.on('stop', stopClosureCb);
                }

                message.respond({ returnValue: true, value: { subscribed: true }});
            },
            (message: any) => {
                Main.logger.info('Canceled stop service subscriber');
                Main.emitter.off('stop', stopClosureCb);
                message.respond({ returnValue: true, value: message.payload });
            });

            service.register("seek", (message: any) => {
                if (message.isSubscription) {
                    seekClosureCb = seekCb.bind(this, message);
                    Main.emitter.on('seek', seekClosureCb);
                }

                message.respond({ returnValue: true, value: { subscribed: true }});
            },
            (message: any) => {
                Main.logger.info('Canceled seek service subscriber');
                Main.emitter.off('seek', seekClosureCb);
                message.respond({ returnValue: true, value: message.payload });
            });

            service.register("setvolume", (message: any) => {
                if (message.isSubscription) {
                    setVolumeClosureCb = setVolumeCb.bind(this, message);
                    Main.emitter.on('setvolume', setVolumeClosureCb);
                }

                message.respond({ returnValue: true, value: { subscribed: true }});
            },
            (message: any) => {
                Main.logger.info('Canceled setvolume service subscriber');
                Main.emitter.off('setvolume', setVolumeClosureCb);
                message.respond({ returnValue: true, value: message.payload });
            });

            service.register("setspeed", (message: any) => {
                if (message.isSubscription) {
                    setSpeedClosureCb = setSpeedCb.bind(this, message);
                    Main.emitter.on('setspeed', setSpeedClosureCb);
                }

                message.respond({ returnValue: true, value: { subscribed: true }});
            },
            (message: any) => {
                Main.logger.info('Canceled setspeed service subscriber');
                Main.emitter.off('setspeed', setSpeedClosureCb);
                message.respond({ returnValue: true, value: message.payload });
            });

            const listeners = [Main.tcpListenerService, Main.webSocketListenerService];
            listeners.forEach(l => {
                l.emitter.on("play", async (message) => {
                    await NetworkService.proxyPlayIfRequired(message);
                    Main.emitter.emit('play', message);

                    const appId = 'com.futo.fcast.receiver';
                    service.call("luna://com.webos.applicationManager/launch", {
                        'id': appId,
                        'params': { timestamp: Date.now(), playData: message }
                    }, (response: any) => {
                        Main.logger.info(`Launch response: ${JSON.stringify(response)}`);
                        Main.logger.info(`Relaunching FCast Receiver with args: ${JSON.stringify(message)}`);
                    });
                });
                l.emitter.on("pause", () => Main.emitter.emit('pause'));
                l.emitter.on("resume", () => Main.emitter.emit('resume'));
                l.emitter.on("stop", () => Main.emitter.emit('stop'));
                l.emitter.on("seek", (message) => Main.emitter.emit('seek', message));
                l.emitter.on("setvolume", (message) => Main.emitter.emit('setvolume', message));
                l.emitter.on("setspeed", (message) => Main.emitter.emit('setspeed', message));

                l.emitter.on('connect', (message) => Main.emitter.emit('connect', message));
                l.emitter.on('disconnect', (message) => Main.emitter.emit('disconnect', message));
                l.emitter.on('ping', (message) => Main.emitter.emit('ping', message));
                l.start();
            });

            service.register("send_playback_error", (message: any) => {
                listeners.forEach(l => {
                    const value: PlaybackErrorMessage = message.payload.error;
                    l.send(Opcode.PlaybackError, value);
                });

                message.respond({ returnValue: true, value: { success: true } });
            });

            service.register("send_playback_update", (message: any) => {
                // Main.logger.info("In send_playback_update callback");

                listeners.forEach(l => {
                    const value: PlaybackUpdateMessage = message.payload.update;
                    l.send(Opcode.PlaybackUpdate, value);
                });

                message.respond({ returnValue: true, value: { success: true } });
            });

            service.register("send_volume_update", (message: any) => {
                listeners.forEach(l => {
                    const value: VolumeUpdateMessage = message.payload.update;
                    l.send(Opcode.VolumeUpdate, value);
                });

                message.respond({ returnValue: true, value: { success: true } });
            });
        }
        catch (err)  {
            Main.logger.error("Error initializing service:", err);
            Main.emitter.emit('toast', { message: `Error initializing service: ${err}`, icon: ToastIcon.ERROR });
        }

	}
}

export function getComputerName() {
    return os.hostname();
}

export async function errorHandler(err: NodeJS.ErrnoException) {
    Main.logger.error("Application error:", err);
    Main.emitter.emit('toast', { message: err, icon: ToastIcon.ERROR });
}
