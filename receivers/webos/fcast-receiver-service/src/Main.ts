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

export class Main {
    static tcpListenerService: TcpListenerService;
    static webSocketListenerService: WebSocketListenerService;
    static discoveryService: DiscoveryService;
    static logger: log4js.Logger;

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

            // Not compatible with WebOS 22 and earlier simulator?
            // Service will timeout and casting will disconnect if not forced to be kept alive
            // let keepAlive;
            // service.activityManager.create("keepAlive", function(activity) {
            //     keepAlive = activity;
            // });

            service.register("keepAlive", (_message: any) => {
                Main.logger.info("In keepAlive callback");
                // Do not respond to keep service alive
            });

            service.register("getDeviceInfo", (message: any) => {
                Main.logger.info("In getDeviceInfo callback");

                message.respond({
                    returnValue: true,
                    value: { name: os.hostname(), addresses: NetworkService.getAllIPv4Addresses() }
                });
            });

            Main.discoveryService = new DiscoveryService();
            Main.discoveryService.start();

            Main.tcpListenerService = new TcpListenerService();
            Main.webSocketListenerService = new WebSocketListenerService();

            const emitter = new EventEmitter();
            let playData: PlayMessage = null;

            let playClosureCb = null;
            const playCb = (message: any, playMessage: PlayMessage) => {
                playData = playMessage;
                message.respond({ returnValue: true, value: { playData: playData } });
            };

            let pauseClosureCb: any = null;
            let resumeClosureCb: any = null;
            let stopClosureCb: any  = null;
            const voidCb = (message: any) => { message.respond({ returnValue: true, value: {} }); };

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
                    emitter.on('play', playClosureCb);
                }

                message.respond({ returnValue: true, value: { subscribed: true, playData: playData }});
            },
            (message: any) => {
                Main.logger.info('Canceled play service subscriber');
                emitter.off('play', playClosureCb);
                message.respond({ returnValue: true, value: message.payload });
            });

            service.register("pause", (message: any) => {
                if (message.isSubscription) {
                    pauseClosureCb = voidCb.bind(this, message);
                    emitter.on('pause', pauseClosureCb);
                }

                message.respond({ returnValue: true, value: { subscribed: true }});
            },
            (message: any) => {
                Main.logger.info('Canceled pause service subscriber');
                emitter.off('pause', pauseClosureCb);
                message.respond({ returnValue: true, value: message.payload });
            });

            service.register("resume", (message: any) => {
                if (message.isSubscription) {
                    resumeClosureCb = voidCb.bind(this, message);
                    emitter.on('resume', resumeClosureCb);
                }

                message.respond({ returnValue: true, value: { subscribed: true }});
            },
            (message: any) => {
                Main.logger.info('Canceled resume service subscriber');
                emitter.off('resume', resumeClosureCb);
                message.respond({ returnValue: true, value: message.payload });
            });

            service.register("stop", (message: any) => {
                playData = null;

                if (message.isSubscription) {
                    stopClosureCb = voidCb.bind(this, message);
                    emitter.on('stop', stopClosureCb);
                }

                message.respond({ returnValue: true, value: { subscribed: true }});
            },
            (message: any) => {
                Main.logger.info('Canceled stop service subscriber');
                emitter.off('stop', stopClosureCb);
                message.respond({ returnValue: true, value: message.payload });
            });

            service.register("seek", (message: any) => {
                if (message.isSubscription) {
                    seekClosureCb = seekCb.bind(this, message);
                    emitter.on('seek', seekClosureCb);
                }

                message.respond({ returnValue: true, value: { subscribed: true }});
            },
            (message: any) => {
                Main.logger.info('Canceled seek service subscriber');
                emitter.off('seek', seekClosureCb);
                message.respond({ returnValue: true, value: message.payload });
            });

            service.register("setvolume", (message: any) => {
                if (message.isSubscription) {
                    setVolumeClosureCb = setVolumeCb.bind(this, message);
                    emitter.on('setvolume', setVolumeClosureCb);
                }

                message.respond({ returnValue: true, value: { subscribed: true }});
            },
            (message: any) => {
                Main.logger.info('Canceled setvolume service subscriber');
                emitter.off('setvolume', setVolumeClosureCb);
                message.respond({ returnValue: true, value: message.payload });
            });

            service.register("setspeed", (message: any) => {
                if (message.isSubscription) {
                    setSpeedClosureCb = setSpeedCb.bind(this, message);
                    emitter.on('setspeed', setSpeedClosureCb);
                }

                message.respond({ returnValue: true, value: { subscribed: true }});
            },
            (message: any) => {
                Main.logger.info('Canceled setspeed service subscriber');
                emitter.off('setspeed', setSpeedClosureCb);
                message.respond({ returnValue: true, value: message.payload });
            });

            const listeners = [Main.tcpListenerService, Main.webSocketListenerService];
            listeners.forEach(l => {
                l.emitter.on("play", async (message) => {
                    await NetworkService.proxyPlayIfRequired(message);
                    emitter.emit('play', message);

                    const appId = 'com.futo.fcast.receiver';
                    service.call("luna://com.webos.applicationManager/launch", {
                        'id': appId,
                        'params': { playData: message }
                    }, (response: any) => {
                        Main.logger.info(`Launch response: ${JSON.stringify(response)}`);
                        Main.logger.info(`Relaunching FCast Receiver with args: ${JSON.stringify(message)}`);
                    });
                });
                l.emitter.on("pause", () => emitter.emit('pause'));
                l.emitter.on("resume", () => emitter.emit('resume'));
                l.emitter.on("stop", () => emitter.emit('stop'));
                l.emitter.on("seek", (message) => emitter.emit('seek', message));
                l.emitter.on("setvolume", (message) => emitter.emit('setvolume', message));
                l.emitter.on("setspeed", (message) => emitter.emit('setspeed', message));
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
        }

	}
}

export function getComputerName() {
    return os.hostname();
}

export async function errorHandler(err: NodeJS.ErrnoException) {
    Main.logger.error("Application error:", err);
}
