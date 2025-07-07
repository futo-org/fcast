/* eslint-disable @typescript-eslint/no-explicit-any */
// No node module for this package, only exists in webOS environment
// eslint-disable-next-line @typescript-eslint/ban-ts-comment
// @ts-ignore
const Service = __non_webpack_require__('webos-service');
// const Service = require('webos-service');

import { EventMessage, EventType, Opcode, PlayMessage, PlayUpdateMessage, PlaybackErrorMessage, PlaybackUpdateMessage, PlaylistContent, SeekMessage,
    SetPlaylistItemMessage, SetSpeedMessage, SetVolumeMessage, VolumeUpdateMessage } from 'common/Packets';
import { DiscoveryService } from 'common/DiscoveryService';
import { TcpListenerService } from 'common/TcpListenerService';
import { WebSocketListenerService } from 'common/WebSocketListenerService';
import { ConnectionMonitor } from 'common/ConnectionMonitor';
import { Logger, LoggerType } from 'common/Logger';
import { MediaCache } from 'common/MediaCache';
import { preparePlayMessage } from 'common/UtilityBackend';
import * as os from 'os';
import { EventEmitter } from 'events';
import { ToastIcon } from 'common/components/Toast';
const logger = new Logger('Main', LoggerType.BACKEND);
const serviceId = 'com.futo.fcast.receiver.service';
const service = new Service(serviceId);

class AppCache {
    public interfaces: any = null;
    public appName: string = null;
    public appVersion: string = null;
    public playMessage: PlayMessage = null;
    public playerVolume: number = null;
    public subscribedKeys = new Set<string>();
}

export class Main {
    static tcpListenerService: TcpListenerService;
    static webSocketListenerService: WebSocketListenerService;
    static discoveryService: DiscoveryService;
    static connectionMonitor: ConnectionMonitor;
    static emitter: EventEmitter;
    static cache: AppCache = new AppCache();

    private static listeners = [];
    private static mediaCache: MediaCache = null;

    private static windowVisible: boolean = false;
    private static windowType: string = 'main';

    private static async play(message: PlayMessage) {
        Main.listeners.forEach(l => l.send(Opcode.PlayUpdate, new PlayUpdateMessage(Date.now(), message)));
        Main.cache.playMessage = message;
        const messageInfo = await preparePlayMessage(message, Main.cache.playerVolume, (playMessage: PlaylistContent) => {
            Main.mediaCache?.destroy();
            Main.mediaCache = new MediaCache(playMessage);
        });

        Main.emitter.emit('play', messageInfo);
        if (!Main.windowVisible) {
            const appId = 'com.futo.fcast.receiver';
            service.call("luna://com.webos.applicationManager/launch", {
                'id': appId,
                'params': { timestamp: Date.now(), messageInfo: messageInfo }
            }, (response: any) => {
                logger.info(`Launch response: ${JSON.stringify(response)}`);
                logger.info(`Relaunching FCast Receiver with args: ${messageInfo.rendererEvent} ${JSON.stringify(messageInfo.rendererMessage)}`);
            });
        }
    }

	static {
		try {
            logger.info(`OS: ${process.platform} ${process.arch}`);

            // Service will timeout and casting will disconnect if not forced to be kept alive
            // eslint-disable-next-line @typescript-eslint/no-unused-vars
            let keepAlive;
            service.activityManager.create("keepAlive", function(activity) {
                keepAlive = activity;
            });

            Main.connectionMonitor = new ConnectionMonitor();
            Main.discoveryService = new DiscoveryService();
            Main.discoveryService.start();

            Main.tcpListenerService = new TcpListenerService();
            Main.webSocketListenerService = new WebSocketListenerService();

            Main.emitter = new EventEmitter();

            const voidCb = (message: any) => { message.respond({ returnValue: true, value: {} }); };
            const objectCb = (message: any, value: any) => { message.respond({ returnValue: true, value: value }); };

            registerService(service, 'toast', (message: any) => { return objectCb.bind(this, message) });
            registerService(service, 'connect', (message: any) => { return objectCb.bind(this, message) });
            registerService(service, 'disconnect', (message: any) => { return objectCb.bind(this, message) });
            registerService(service, 'play', (message: any) => { return objectCb.bind(this, message) });
            registerService(service, 'pause', (message: any) => { return voidCb.bind(this, message) });
            registerService(service, 'resume', (message: any) => { return voidCb.bind(this, message) });
            registerService(service, 'stop', (message: any) => { return voidCb.bind(this, message) });
            registerService(service, 'seek', (message: any) => { return objectCb.bind(this, message) });
            registerService(service, 'setvolume', (message: any) => { return objectCb.bind(this, message) });
            registerService(service, 'setspeed', (message: any) => { return objectCb.bind(this, message) });
            registerService(service, 'setplaylistitem', (message: any) => { return objectCb.bind(this, message) });
            registerService(service, 'event_subscribed_keys_update', (message: any) => { return objectCb.bind(this, message) });

            Main.listeners = [Main.tcpListenerService, Main.webSocketListenerService];
            Main.listeners.forEach(l => {
                l.emitter.on("play", (message: PlayMessage) => Main.play(message));
                l.emitter.on("pause", () => Main.emitter.emit('pause'));
                l.emitter.on("resume", () => Main.emitter.emit('resume'));
                l.emitter.on("stop", () => Main.emitter.emit('stop'));
                l.emitter.on("seek", (message: SeekMessage) => Main.emitter.emit('seek', message));
                l.emitter.on("setvolume", (message: SetVolumeMessage) => {
                    Main.cache.playerVolume = message.volume;
                    Main.emitter.emit('setvolume', message);
                });
                l.emitter.on("setspeed", (message: SetSpeedMessage) => Main.emitter.emit('setspeed', message));

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
                l.emitter.on('initial', (message) => {
                    logger.info(`Received 'Initial' message from sender: ${message}`);
                });
                l.emitter.on("setplaylistitem", (message: SetPlaylistItemMessage) => Main.emitter.emit('setplaylistitem', message));
                l.emitter.on('subscribeevent', (message) => {
                    const subscribeData = l.subscribeEvent(message.sessionId, message.body.event);

                    if (message.body.event.type === EventType.KeyDown.valueOf() || message.body.event.type === EventType.KeyUp.valueOf()) {
                        Main.emitter.emit('event_subscribed_keys_update', subscribeData);
                    }
                });
                l.emitter.on('unsubscribeevent', (message) => {
                    const unsubscribeData = l.unsubscribeEvent(message.sessionId, message.body.event);

                    if (message.body.event.type === EventType.KeyDown.valueOf() || message.body.event.type === EventType.KeyUp.valueOf()) {
                        Main.emitter.emit('event_subscribed_keys_update', unsubscribeData);
                    }
                });
                l.start();
            });

            service.register("send_playback_error", (message: any) => {
                const value: PlaybackErrorMessage = message.payload.error;
                Main.listeners.forEach(l => l.send(Opcode.PlaybackError, value));
                message.respond({ returnValue: true, value: { success: true } });
            });

            service.register("send_playback_update", (message: any) => {
                // logger.info("In send_playback_update callback");
                const value: PlaybackUpdateMessage = message.payload.update;
                Main.listeners.forEach(l => l.send(Opcode.PlaybackUpdate, value));
                message.respond({ returnValue: true, value: { success: true } });
            });

            service.register("send_volume_update", (message: any) => {
                const value: VolumeUpdateMessage = message.payload.update;
                Main.cache.playerVolume = value.volume;
                Main.listeners.forEach(l => l.send(Opcode.VolumeUpdate, value));
                message.respond({ returnValue: true, value: { success: true } });
            });

            service.register("send_event", (message: any) => {
                const value: EventMessage = message.payload.event;
                Main.listeners.forEach(l => l.send(Opcode.Event, value));
                message.respond({ returnValue: true, value: { success: true } });
            });

            service.register("play_request", (message: any) => {
                const value: PlayMessage = message.payload.message;
                const playlistIndex: number = message.payload.playlistIndex;

                logger.debug(`Received play request for index ${playlistIndex}:`, value);
                value.url = Main.mediaCache?.has(playlistIndex) ? Main.mediaCache?.getUrl(playlistIndex) : value.url;
                Main.mediaCache?.cacheItems(playlistIndex);
                Main.play(value);

                message.respond({ returnValue: true, value: { success: true } });
            });

            // Having to mix and match session ids and ip addresses until querying websocket remote addresses is fixed
            service.register("get_sessions", (message: any) => {
                message.respond({
                    returnValue: true,
                    value: [].concat(Main.tcpListenerService.getSenders(), Main.webSocketListenerService.getSessions())
                });
            });

            service.register("network_changed", (message: any) => {
                logger.info('Network interfaces have changed', message);
                Main.discoveryService.stop();
                Main.discoveryService.start();

                if (message.payload.fallback) {
                    message.respond({
                        returnValue: true,
                        value: getAllIPv4Addresses()
                    });
                }
                else {
                    message.respond({ returnValue: true, value: {} });
                }
            });

            service.register("visibility_changed", (message: any) => {
                logger.info('Window visibility has changed', message.payload);
                Main.windowVisible = !message.payload.hidden;
                Main.windowType = message.payload.window;
                message.respond({ returnValue: true, value: {} });
            });
        }
        catch (err)  {
            logger.error("Error initializing service:", err);
            Main.emitter.emit('toast', { message: `Error initializing service: ${err}`, icon: ToastIcon.ERROR });
        }
	}
}

export function getComputerName() {
    return `FCast-${os.hostname()}`;
}

export function getAppName() {
    return Main.cache.appName;
}

export function getAppVersion() {
    return Main.cache.appVersion;
}

export function getPlayMessage() {
    return Main.cache.playMessage;
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

// Fallback for simulator or TV devices that don't work with the luna://com.palm.connectionmanager/getStatus method
function getAllIPv4Addresses() {
    const interfaces = os.networkInterfaces();
    const ipv4Addresses: string[] = [];

    for (const interfaceName in interfaces) {
        const addresses = interfaces[interfaceName];
        if (!addresses) continue;

        for (const addressInfo of addresses) {
            if (addressInfo.family === 'IPv4' && !addressInfo.internal) {
                ipv4Addresses.push(addressInfo.address);
            }
        }
    }

    return ipv4Addresses;
}
