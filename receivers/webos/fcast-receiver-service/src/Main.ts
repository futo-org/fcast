/* eslint-disable @typescript-eslint/no-explicit-any */
// No node module for this package, only exists in webOS environment
// eslint-disable-next-line @typescript-eslint/ban-ts-comment
// @ts-ignore
const Service = __non_webpack_require__('webos-service');
// const Service = require('webos-service');

import { Opcode, PlayMessage, PlaybackErrorMessage, PlaybackUpdateMessage, SeekMessage, SetSpeedMessage, SetVolumeMessage, VolumeUpdateMessage } from 'common/Packets';
import { DiscoveryService } from 'common/DiscoveryService';
import { TcpListenerService } from 'common/TcpListenerService';
import { WebSocketListenerService } from 'common/WebSocketListenerService';
import { NetworkService } from 'common/NetworkService';
import { ConnectionMonitor } from 'common/ConnectionMonitor';
import { Logger, LoggerType } from 'common/Logger';
import * as os from 'os';
import { EventEmitter } from 'events';
import { ToastIcon } from 'common/components/Toast';
const logger = new Logger('Main', LoggerType.BACKEND);

export class Main {
    static tcpListenerService: TcpListenerService;
    static webSocketListenerService: WebSocketListenerService;
    static discoveryService: DiscoveryService;
    static connectionMonitor: ConnectionMonitor;
    static emitter: EventEmitter;

	static {
		try {
            logger.info(`OS: ${process.platform} ${process.arch}`);

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

            registerService(service, 'toast', (message: any) => { return objectCb.bind(this, message) });

            // getDeviceInfo and network-changed handled in frontend
            service.register("get_sessions", (message: any) => {
                message.respond({
                    returnValue: true,
                    value: [].concat(Main.tcpListenerService.getSenders(), Main.webSocketListenerService.getSessions())
                });
            });

            registerService(service, 'connect', (message: any) => { return objectCb.bind(this, message) });
            registerService(service, 'disconnect', (message: any) => { return objectCb.bind(this, message) });

            Main.connectionMonitor = new ConnectionMonitor();
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

            let stopClosureCb: any  = null;
            const seekCb = (message: any, seekMessage: SeekMessage) => { message.respond({ returnValue: true, value: seekMessage }); };
            const setVolumeCb = (message: any, volumeMessage: SetVolumeMessage) => { message.respond({ returnValue: true, value: volumeMessage }); };
            const setSpeedCb = (message: any, speedMessage: SetSpeedMessage) => { message.respond({ returnValue: true, value: speedMessage }); };

            // Note: When logging the `message` object, do NOT use JSON.stringify, you can log messages directly. Seems to be a circular reference causing errors...
            service.register('play', (message: any) => {
                if (message.isSubscription) {
                    playClosureCb = playCb.bind(this, message);
                    Main.emitter.on('play', playClosureCb);
                }

                message.respond({ returnValue: true, value: { subscribed: true, playData: playData }});
            },
            (message: any) => {
                logger.info('Canceled play service subscriber');
                Main.emitter.removeAllListeners('play');
                message.respond({ returnValue: true, value: message.payload });
            });

            registerService(service, 'pause', (message: any) => { return voidCb.bind(this, message) });
            registerService(service, 'resume', (message: any) => { return voidCb.bind(this, message) });

            service.register('stop', (message: any) => {
                playData = null;

                if (message.isSubscription) {
                    stopClosureCb = voidCb.bind(this, message);
                    Main.emitter.on('stop', stopClosureCb);
                }

                message.respond({ returnValue: true, value: { subscribed: true }});
            },
            (message: any) => {
                logger.info('Canceled stop service subscriber');
                Main.emitter.removeAllListeners('stop');
                message.respond({ returnValue: true, value: message.payload });
            });

            registerService(service, 'seek', (message: any) => { return seekCb.bind(this, message) });
            registerService(service, 'setvolume', (message: any) => { return setVolumeCb.bind(this, message) });
            registerService(service, 'setspeed', (message: any) => { return setSpeedCb.bind(this, message) });

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
                        logger.info(`Launch response: ${JSON.stringify(response)}`);
                        logger.info(`Relaunching FCast Receiver with args: ${JSON.stringify(message)}`);
                    });
                });
                l.emitter.on("pause", () => Main.emitter.emit('pause'));
                l.emitter.on("resume", () => Main.emitter.emit('resume'));
                l.emitter.on("stop", () => Main.emitter.emit('stop'));
                l.emitter.on("seek", (message) => Main.emitter.emit('seek', message));
                l.emitter.on("setvolume", (message) => Main.emitter.emit('setvolume', message));
                l.emitter.on("setspeed", (message) => Main.emitter.emit('setspeed', message));

                l.emitter.on('connect', (message) => {
                    ConnectionMonitor.onConnect(l, message, l instanceof WebSocketListenerService, () => {
                        Main.emitter.emit('connect', message);
                    });
                });
                l.emitter.on('disconnect', (message) => {
                    ConnectionMonitor.onDisconnect(l, message, l instanceof WebSocketListenerService, () => {
                        Main.emitter.emit('disconnect', message);
                    });
                });
                l.emitter.on('ping', (message) => {
                    ConnectionMonitor.onPingPong(message, l instanceof WebSocketListenerService);
                });
                l.emitter.on('pong', (message) => {
                    ConnectionMonitor.onPingPong(message, l instanceof WebSocketListenerService);
                });
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
                // logger.info("In send_playback_update callback");

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
            logger.error("Error initializing service:", err);
            Main.emitter.emit('toast', { message: `Error initializing service: ${err}`, icon: ToastIcon.ERROR });
        }

	}
}

export function getComputerName() {
    return os.hostname();
}

export async function errorHandler(error: Error) {
    logger.error(error);
    logger.shutdown();

    logger.error("Application error:", error);
    Main.emitter.emit('toast', { message: error, icon: ToastIcon.ERROR });
}

function registerService(service: Service, method: string, callback: (message: any) => any) {
    let callbackRef = null;
    service.register(method, (message: any) => {
        if (message.isSubscription) {
            callbackRef = callback(message);
            Main.emitter.on(method, callbackRef);
        }

        message.respond({ returnValue: true, value: { subscribed: true }});
    },
    (message: any) => {
        logger.info(`Canceled ${method} service subscriber`);
        Main.emitter.removeAllListeners(method);
        message.respond({ returnValue: true, value: message.payload });
    });
}
