/* eslint-disable @typescript-eslint/no-explicit-any */
// No node module for this package, only exists in webOS environment
// eslint-disable-next-line @typescript-eslint/ban-ts-comment
// @ts-ignore
const Service = __non_webpack_require__('webos-service');
// const Service = require('webos-service');

import { EventMessage, EventType, Opcode, PlayMessage, PlaybackErrorMessage, PlaybackUpdateMessage, PlaylistContent, SeekMessage,
    SetPlaylistItemMessage, SetSpeedMessage, SetVolumeMessage, VolumeUpdateMessage } from 'common/Packets';
import { DiscoveryService } from 'common/DiscoveryService';
import { TcpListenerService } from 'common/TcpListenerService';
import { ConnectionMonitor } from 'common/ConnectionMonitor';
import { Logger, LoggerType } from 'common/Logger';
import { MediaCache } from 'common/MediaCache';
import { preparePlayMessage } from 'common/UtilityBackend';
import * as os from 'os';
import { EventEmitter } from 'events';
import { ToastIcon } from 'common/components/Toast';
import * as path from 'path';
import { isV4Capable, runtimeDescription } from 'common/v4/Capability';
import { loadOrCreateIdentity } from 'common/v4/Identity';
import { createV4Context, defaultCapabilities, V4Context } from 'common/v4/V4Context';
import { encodeReceiverIntroduction } from 'common/v4/Encoder';
import { CompanionBroker } from 'common/v4/Companion';
import { fetchSubtitle, isReachable } from 'common/v4/ResourceFetch';
import { NetworkService } from 'common/NetworkService';
import { PlaybackCoordinator, PlaybackHost, PlaybackOptions } from 'common/PlaybackCoordinator';

// eslint-disable-next-line @typescript-eslint/ban-ts-comment
// @ts-ignore
const { version } = __non_webpack_require__('./package.json');

const logger = new Logger('Main', LoggerType.BACKEND);
const serviceId = 'com.futo.fcast.receiver.service';
const service = new Service(serviceId);

let v4Context: V4Context | null = null;

class AppCache {
    public interfaces: any = null;
    public appName: string = null;
    public appVersion: string = null;
    public playerVolume: number = null;
    public subscribedKeys = new Set<string>();
}

export class Main {
    static tcpListenerService: TcpListenerService;
    static discoveryService: DiscoveryService;
    static connectionMonitor: ConnectionMonitor;
    static emitter: EventEmitter;
    static cache: AppCache = new AppCache();

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
        'changetrack',
        'addsubtitle',
        'event_subscribed_keys_update'
    ];
    static companion: CompanionBroker;
    static coordinator: PlaybackCoordinator;

    // Hands the message to the player window, launching the app when it is not on screen.
    static async startPlayback(message: PlayMessage, options: PlaybackOptions) {
        const preparedMessage = await preparePlayMessage(message, (playMessage: PlaylistContent) => {
            Main.mediaCache?.destroy();
            Main.mediaCache = new MediaCache(playMessage);
        });
        const messageInfo = { ...preparedMessage, playerVolume: Main.cache.playerVolume, options: options };

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
            Main.cache.appName = "FCast Receiver webOS"
            Main.cache.appVersion = version

            logger.info(`${Main.cache.appName}: ${Main.cache.appVersion}, OS: ${process.platform} ${process.arch}`);

            // Service will timeout and casting will disconnect if not forced to be kept alive
            // eslint-disable-next-line @typescript-eslint/no-unused-vars
            let keepAlive;
            service.activityManager.create("keepAlive", function(activity) {
                keepAlive = activity;
            });

            Main.connectionMonitor = new ConnectionMonitor();
            initializeV4();
            Main.discoveryService = new DiscoveryService();
            Main.discoveryService.update(getAllIPv4Addresses());

            Main.tcpListenerService = new TcpListenerService();
            Main.emitter = new EventEmitter();
            Main.companion = new CompanionBroker((sessionId, bytes) => Main.tcpListenerService.sendV4(bytes, sessionId));
            NetworkService.companion = Main.companion;
            Main.coordinator = new PlaybackCoordinator(createPlaybackHost(), Main.companion);

            service.register('service_channel', (message: any) => {
                if (message.isSubscription) {
                    // Only the newest subscription receives events: the app subscribes once, and a
                    // second subscription replaces a stale one instead of doubling every event.
                    Main.serviceChannelEvents.forEach((event) => {
                        Main.emitter.removeAllListeners(event);
                        Main.emitter.on(event, (value) => {
                            message.respond({ returnValue: true, subscriptionId: message.payload.subscriptionId, timestamp: Date.now(), event: event, value: value });
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
                        Main.coordinator.onPlaybackError(value);
                        break;
                    }

                    case 'send_playback_update': {
                        const value: PlaybackUpdateMessage = message.payload.value;
                        Main.coordinator.onPlaybackUpdate(value);
                        break;
                    }

                    case 'send_volume_update': {
                        const value: VolumeUpdateMessage = message.payload.value;
                        Main.cache.playerVolume = value.volume;
                        Main.coordinator.onVolumeUpdate(value);
                        break;
                    }

                    case 'send_event': {
                        const value: EventMessage = message.payload.value;
                        Main.tcpListenerService.send(Opcode.Event, value);
                        if (value && value.event && value.event.type === EventType.MediaItemEnd.valueOf()) {
                            Main.coordinator.onMediaEnded();
                        }
                        break;
                    }

                    case 'send_tracks': {
                        Main.coordinator.onTracks(message.payload.value);
                        break;
                    }

                    case 'queue_select_request': {
                        Main.coordinator.selectFromPlayer(Number(message.payload.value.offset));
                        break;
                    }

                    case 'play_request': {
                        const value: PlayMessage = message.payload.value.message;
                        const playlistIndex: number = message.payload.value.playlistIndex;

                        logger.debug(`Received play request for index ${playlistIndex}:`, value);
                        value.url = Main.mediaCache?.has(playlistIndex) ? Main.mediaCache?.getUrl(playlistIndex) : value.url;
                        Main.mediaCache?.cacheItems(playlistIndex);
                        Main.coordinator.playFromPlayer(value);
                        break;
                    }

                    case 'get_sessions': {
                        message.respond({
                            returnValue: true,
                            value: Main.tcpListenerService.getSenders()
                        });
                        return;
                    }

                    case 'get_connection_info': {
                        message.respond({
                            returnValue: true,
                            value: {
                                txt: v4Context ? { v: '4', fp: v4Context.fingerprint } : null,
                                maxVersion: v4Context ? 4 : 3
                            }
                        });
                        return;
                    }

                    case 'get_subscribed_keys': {
                        const tcpListenerSubscribedKeys = Main.tcpListenerService.getAllSubscribedKeys();
                        // webOS compatibility: Need to convert set objects to array objects since data needs to be JSON compatible
                        const subscribeData = {
                            keyDown: Array.from(new Set([...tcpListenerSubscribedKeys.keyDown])),
                            keyUp: Array.from(new Set([...tcpListenerSubscribedKeys.keyUp])),
                        };

                        message.respond({
                            returnValue: true,
                            value: subscribeData
                        });
                        return;
                    }

                    case 'network_changed': {
                        if (Main.discoveryService.update(getAllIPv4Addresses())) {
                            logger.info('Network interfaces have changed, discovery restarted');
                        }

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

            Main.tcpListenerService.emitter.on('play', (message: { sessionId: string, body: PlayMessage }) => Main.coordinator.playLegacy(message.sessionId, message.body));
            Main.tcpListenerService.emitter.on('pause', () => Main.coordinator.pauseLegacy());
            Main.tcpListenerService.emitter.on('resume', () => Main.coordinator.resumeLegacy());
            Main.tcpListenerService.emitter.on('stop', () => Main.coordinator.stopLegacy());
            Main.tcpListenerService.emitter.on('seek', (message: SeekMessage) => Main.coordinator.seekLegacy(message.time));
            Main.tcpListenerService.emitter.on('setvolume', (message: SetVolumeMessage) => Main.coordinator.setVolumeLegacy(message.volume));
            Main.tcpListenerService.emitter.on('setspeed', (message: SetSpeedMessage) => Main.coordinator.setSpeedLegacy(message.speed));

            Main.tcpListenerService.emitter.on('connect', (message) => {
                ConnectionMonitor.onConnect(Main.tcpListenerService, message, () => {
                    Main.emitter.emit('connect', message);
                });
            });
            Main.tcpListenerService.emitter.on('disconnect', (message) => {
                Main.coordinator.onDisconnect(message.sessionId);
                ConnectionMonitor.onDisconnect(message, () => {
                    Main.emitter.emit('disconnect', message);
                });
            });
            Main.tcpListenerService.emitter.on('ping', (sessionId: string) => {
                ConnectionMonitor.onPingPong(sessionId);
            });
            Main.tcpListenerService.emitter.on('pong', (sessionId: string) => {
                ConnectionMonitor.onPingPong(sessionId);
            });
            Main.tcpListenerService.emitter.on('initial', (message) => {
                logger.info(`Received 'Initial' message from sender: ${message}`);
            });
            Main.tcpListenerService.emitter.on('v4secured', (message) => Main.coordinator.onV4Secured(message.sessionId));
            Main.tcpListenerService.emitter.on('v4message', (message) => {
                logger.info('v4 message', message.sessionId, message.message && message.message.type);
                Main.coordinator.onV4Message(message.sessionId, message.packetNum, message.message);
            });
            Main.tcpListenerService.emitter.on('v4resource', (message) => {
                try {
                    Main.companion.onResource(message.sessionId, message.body);
                } catch (e) {
                    logger.warn(`Malformed resource packet from session ${message.sessionId}. Closing connection.`, e);
                    Main.tcpListenerService.disconnect(message.sessionId);
                }
            });
            Main.tcpListenerService.emitter.on('senderintroduction', (message) => logger.info('v4 sender introduced', message.sessionId, JSON.stringify(message.deviceInfo)));
            Main.tcpListenerService.emitter.on('setplaylistitem', (message: SetPlaylistItemMessage) => Main.emitter.emit('setplaylistitem', message));
            Main.tcpListenerService.emitter.on('subscribeevent', (message) => {
                Main.tcpListenerService.subscribeEvent(message.sessionId, message.body.event);

                if (message.body.event.type === EventType.KeyDown.valueOf() || message.body.event.type === EventType.KeyUp.valueOf()) {
                    const tcpListenerSubscribedKeys = Main.tcpListenerService.getAllSubscribedKeys();
                    // webOS compatibility: Need to convert set objects to array objects since data needs to be JSON compatible
                    const subscribeData = {
                        keyDown: Array.from(new Set([...tcpListenerSubscribedKeys.keyDown])),
                        keyUp: Array.from(new Set([...tcpListenerSubscribedKeys.keyUp])),
                    };

                    console.log('emitting set info ON SUBSCRIBE ONLY', subscribeData)
                    Main.emitter.emit('event_subscribed_keys_update', subscribeData);
                }
            });
            Main.tcpListenerService.emitter.on('unsubscribeevent', (message) => {
                Main.tcpListenerService.unsubscribeEvent(message.sessionId, message.body.event);

                if (message.body.event.type === EventType.KeyDown.valueOf() || message.body.event.type === EventType.KeyUp.valueOf()) {
                    const tcpListenerSubscribedKeys = Main.tcpListenerService.getAllSubscribedKeys();
                    // webOS compatibility: Need to convert set objects to array objects since data needs to be JSON compatible
                    const subscribeData = {
                        keyDown: Array.from(new Set([...tcpListenerSubscribedKeys.keyDown])),
                        keyUp: Array.from(new Set([...tcpListenerSubscribedKeys.keyUp])),
                    };

                    Main.emitter.emit('event_subscribed_keys_update', subscribeData);
                }
            });
            Main.tcpListenerService.start();
        }
        catch (err)  {
            logger.error("Error initializing service:", err);
            Main.emitter.emit('toast', { message: `Error initializing service: ${err}`, icon: ToastIcon.ERROR });
        }
	}
}

function initializeV4() {
    logger.info(runtimeDescription());

    if (!isV4Capable()) {
        logger.info('v4 disabled: runtime does not provide TLS 1.3 identities or FCAST_DISABLE_V4 is set');
        return;
    }

    try {
        // eslint-disable-next-line @typescript-eslint/ban-ts-comment
        // @ts-ignore
        const serviceDir = path.dirname(__non_webpack_require__.resolve('./package.json'));
        const candidates: { path: string, persistent: boolean }[] = [];
        if (process.env.FCAST_IDENTITY_DIR) {
            candidates.push({ path: process.env.FCAST_IDENTITY_DIR, persistent: true });
        }
        candidates.push({ path: path.join(serviceDir, 'identity'), persistent: true });
        candidates.push({ path: path.join(os.tmpdir(), 'com.futo.fcast.receiver'), persistent: false });

        const identity = loadOrCreateIdentity(candidates);
        const intro = encodeReceiverIntroduction({
            displayName: getComputerName(),
            appName: Main.cache.appName,
            appVersion: Main.cache.appVersion
        }, defaultCapabilities());
        v4Context = createV4Context(identity, intro);

        const identityLocation = identity.directory !== null ? identity.directory : 'memory';
        logger.info(`v4 enabled fp=${identity.fingerprint} identity=${identityLocation} persistent=${identity.persistent} created=${identity.created}`);
    }
    catch (err) {
        v4Context = null;
        logger.info(`v4 disabled: ${err}`);
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

export function getV4Context(): V4Context | null {
    return v4Context;
}

export function getPlayMessage() {
    return Main.coordinator.initialPlayMessage();
}

export function getPlaybackUpdateMessage() {
    return Main.coordinator.initialPlaybackUpdate();
}

function createPlaybackHost(): PlaybackHost {
    return {
        v4Sessions: () => Main.tcpListenerService.getV4Sessions(),
        legacySessions: () => Main.tcpListenerService.getLegacySessions(),
        sendV4: (bytes, sessionId) => Main.tcpListenerService.sendV4(bytes, sessionId),
        sendLegacy: (opcode, message, sessionId) => Main.tcpListenerService.send(opcode, message, sessionId),
        startPlayback: (message, options) => {
            Main.startPlayback(message, options).catch((err) => {
                logger.error('Error starting playback:', err);
                Main.coordinator.onPlaybackError(new PlaybackErrorMessage(`Error starting playback: ${err}`));
            });
        },
        pause: () => Main.emitter.emit('pause'),
        resume: () => Main.emitter.emit('resume'),
        stop: () => Main.emitter.emit('stop'),
        seek: (time) => Main.emitter.emit('seek', new SeekMessage(time)),
        setVolume: (volume) => {
            Main.cache.playerVolume = volume;
            Main.emitter.emit('setvolume', new SetVolumeMessage(volume));
        },
        setSpeed: (speed) => Main.emitter.emit('setspeed', new SetSpeedMessage(speed)),
        changeTrack: (trackType, id) => Main.emitter.emit('changetrack', { trackType: trackType, id: id }),
        addSubtitle: (track) => Main.emitter.emit('addsubtitle', track),
        fetchSubtitle: (url) => fetchSubtitle(url, Main.companion),
        isReachable: (url, headers) => isReachable(url, headers, Main.companion),
    };
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
