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
    private static serviceChannelEvents = [
        'toast',
        'connect',
        'disconnect',
        'play',
        'pause',
        'resume',
        'stop',
        'seek',
        'setvolume',
        'setspeed',
        'setplaylistitem',
        'event_subscribed_keys_update'
    ];
    private static serviceChannelEventTimestamps: Map<string, number> = new Map();

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

            Main.windowVisible = true;
            Main.windowType = 'player';
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

            service.register('service_channel', (message: any) => {
                if (message.isSubscription) {
                    Main.serviceChannelEvents.forEach((event) => {
                        Main.emitter.on(event, (value) => {
                            const timestamp = Date.now();
                            const lastTimestamp = Main.serviceChannelEventTimestamps.get(event) ? Main.serviceChannelEventTimestamps.get(event) : -1;

                            if (lastTimestamp < timestamp) {
                                Main.serviceChannelEventTimestamps.set(event, timestamp);
                                message.respond({ returnValue: true, subscriptionId: message.payload.subscriptionId, timestamp: timestamp, event: event, value: value });
                            }
                        });
                    });
                }

                message.respond({ returnValue: true, subscriptionId: message.payload.subscriptionId, timestamp: Date.now(), event: 'register', value: { subscribed: true }});
            },
            (message: any) => {
                logger.info(`Canceled 'service_channel' service subscriber`);
                    Main.serviceChannelEvents.forEach((event) => {
                        Main.emitter.removeAllListeners(event);
                    });

                message.respond({ returnValue: true, value: {} });
            });

            service.register('app_channel', (message: any) => {
                switch (message.payload.event) {
                    case 'send_playback_error': {
                        const value: PlaybackErrorMessage = message.payload.value;
                        Main.listeners.forEach(l => l.send(Opcode.PlaybackError, value));
                        break;
                    }

                    case 'send_playback_update': {
                        const value: PlaybackUpdateMessage = message.payload.value;
                        Main.listeners.forEach(l => l.send(Opcode.PlaybackUpdate, value));
                        break;
                    }

                    case 'send_volume_update': {
                        const value: VolumeUpdateMessage = message.payload.value;
                        Main.cache.playerVolume = value.volume;
                        Main.listeners.forEach(l => l.send(Opcode.VolumeUpdate, value));
                        break;
                    }

                    case 'send_event': {
                        const value: EventMessage = message.payload.value;
                        Main.listeners.forEach(l => l.send(Opcode.Event, value));
                        break;
                    }

                    case 'play_request': {
                        const value: PlayMessage = message.payload.value.message;
                        const playlistIndex: number = message.payload.value.playlistIndex;

                        logger.debug(`Received play request for index ${playlistIndex}:`, value);
                        value.url = Main.mediaCache?.has(playlistIndex) ? Main.mediaCache?.getUrl(playlistIndex) : value.url;
                        Main.mediaCache?.cacheItems(playlistIndex);
                        Main.play(value);
                        break;
                    }

                    case 'get_sessions': {
                        // Having to mix and match session ids and ip addresses until querying websocket remote addresses is fixed
                        message.respond({
                            returnValue: true,
                            value: [].concat(Main.tcpListenerService.getSenders(), Main.webSocketListenerService.getSessions())
                        });
                        return;
                    }

                    case 'get_subscribed_keys': {
                        const tcpListenerSubscribedKeys = Main.tcpListenerService.getAllSubscribedKeys();
                        const webSocketListenerSubscribedKeys = Main.webSocketListenerService.getAllSubscribedKeys();
                        // webOS compatibility: Need to convert set objects to array objects since data needs to be JSON compatible
                        const subscribeData = {
                            keyDown: Array.from(new Set([...tcpListenerSubscribedKeys.keyDown, ...webSocketListenerSubscribedKeys.keyDown])),
                            keyUp: Array.from(new Set([...tcpListenerSubscribedKeys.keyUp, ...webSocketListenerSubscribedKeys.keyUp])),
                        };

                        message.respond({
                            returnValue: true,
                            value: subscribeData
                        });
                        return;
                    }

                    case 'network_changed': {
                        logger.info('Network interfaces have changed', message);
                        Main.discoveryService.stop();
                        Main.discoveryService.start();

                        if (message.payload.value.fallback) {
                            message.respond({
                                returnValue: true,
                                value: getAllIPv4Addresses()
                            });
                        }
                        else {
                            message.respond({ returnValue: true, value: {} });
                        }
                        return;
                    }

                    case 'visibility_changed': {
                        logger.info('Window visibility has changed', message.payload.value);
                        Main.windowVisible = !message.payload.value.hidden;
                        Main.windowType = message.payload.value.window;
                        break;
                    }

                    default:
                        break;
                }

                message.respond({ returnValue: true, value: { success: true } });
            });

            Main.listeners = [Main.tcpListenerService, Main.webSocketListenerService];
            Main.listeners.forEach(l => {
                l.emitter.on('play', (message: PlayMessage) => Main.play(message));
                l.emitter.on('pause', () => Main.emitter.emit('pause'));
                l.emitter.on('resume', () => Main.emitter.emit('resume'));
                l.emitter.on('stop', () => Main.emitter.emit('stop'));
                l.emitter.on('seek', (message: SeekMessage) => Main.emitter.emit('seek', message));
                l.emitter.on('setvolume', (message: SetVolumeMessage) => {
                    Main.cache.playerVolume = message.volume;
                    Main.emitter.emit('setvolume', message);
                });
                l.emitter.on('setspeed', (message: SetSpeedMessage) => Main.emitter.emit('setspeed', message));

                l.emitter.on('connect', (message) => {
                    ConnectionMonitor.onConnect(l, message, l instanceof WebSocketListenerService, () => {
                        Main.emitter.emit('connect', message);
                    });
                });
                l.emitter.on('disconnect', (message) => {
                    ConnectionMonitor.onDisconnect(message, l instanceof WebSocketListenerService, () => {
                        Main.emitter.emit('disconnect', message);
                    });
                });
                l.emitter.on('ping', (sessionId: string) => {
                    ConnectionMonitor.onPingPong(sessionId, l instanceof WebSocketListenerService);
                });
                l.emitter.on('pong', (sessionId: string) => {
                    ConnectionMonitor.onPingPong(sessionId, l instanceof WebSocketListenerService);
                });
                l.emitter.on('initial', (message) => {
                    logger.info(`Received 'Initial' message from sender: ${message}`);
                });
                l.emitter.on('setplaylistitem', (message: SetPlaylistItemMessage) => Main.emitter.emit('setplaylistitem', message));
                l.emitter.on('subscribeevent', (message) => {
                    l.subscribeEvent(message.sessionId, message.body.event);

                    if (message.body.event.type === EventType.KeyDown.valueOf() || message.body.event.type === EventType.KeyUp.valueOf()) {
                        const tcpListenerSubscribedKeys = Main.tcpListenerService.getAllSubscribedKeys();
                        const webSocketListenerSubscribedKeys = Main.webSocketListenerService.getAllSubscribedKeys();
                        // webOS compatibility: Need to convert set objects to array objects since data needs to be JSON compatible
                        const subscribeData = {
                            keyDown: Array.from(new Set([...tcpListenerSubscribedKeys.keyDown, ...webSocketListenerSubscribedKeys.keyDown])),
                            keyUp: Array.from(new Set([...tcpListenerSubscribedKeys.keyUp, ...webSocketListenerSubscribedKeys.keyUp])),
                        };

                        console.log('emitting set info ON SUBSCRIBE ONLY', subscribeData)
                        Main.emitter.emit('event_subscribed_keys_update', subscribeData);
                    }
                });
                l.emitter.on('unsubscribeevent', (message) => {
                    l.unsubscribeEvent(message.sessionId, message.body.event);

                    if (message.body.event.type === EventType.KeyDown.valueOf() || message.body.event.type === EventType.KeyUp.valueOf()) {
                        const tcpListenerSubscribedKeys = Main.tcpListenerService.getAllSubscribedKeys();
                        const webSocketListenerSubscribedKeys = Main.webSocketListenerService.getAllSubscribedKeys();
                        // webOS compatibility: Need to convert set objects to array objects since data needs to be JSON compatible
                        const subscribeData = {
                            keyDown: Array.from(new Set([...tcpListenerSubscribedKeys.keyDown, ...webSocketListenerSubscribedKeys.keyDown])),
                            keyUp: Array.from(new Set([...tcpListenerSubscribedKeys.keyUp, ...webSocketListenerSubscribedKeys.keyUp])),
                        };

                        Main.emitter.emit('event_subscribed_keys_update', subscribeData);
                    }
                });
                l.start();
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
