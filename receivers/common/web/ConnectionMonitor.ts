import { Opcode } from 'common/Packets';
import { Logger, LoggerType } from 'common/Logger';

// Window might be re-created while devices are still connected
export function setUiUpdateCallbacks(callbacks: any) {
    const logger = window.targetAPI.logger;
    let frontendConnections = [];

    window.targetAPI.onConnect((_event, value: any) => {
        const idMapping = value.type === 'ws' ? value.sessionId : value.data.address;

        logger.debug(`Processing connect event for ${idMapping} with current connections:`, frontendConnections);
        frontendConnections.push(idMapping);
        callbacks.onConnect(frontendConnections);
    });
    window.targetAPI.onDisconnect((_event, value: any) => {
        const idMapping = value.type === 'ws' ? value.sessionId : value.data.address;

        logger.debug(`Processing disconnect event for ${idMapping} with current connections:`, frontendConnections);
        const index = frontendConnections.indexOf(idMapping);
        if (index != -1) {
            frontendConnections.splice(index, 1);
            callbacks.onDisconnect(frontendConnections);
        }
    });

    window.targetAPI.getSessions().then((sessions: string[]) => {
        logger.info('Window created with current sessions:', sessions);
        frontendConnections = sessions;

        if (frontendConnections.length > 0) {
            callbacks.onConnect(frontendConnections, true);
        }
    });
}

export class ConnectionMonitor {
    private static logger: Logger;
    private static initialized = false;
    private static connectionPingTimeout = 2500;
    private static heartbeatRetries = new Map();
    private static backendConnections = new Map();
    private static uiConnectUpdateTimeout = 100;
    private static uiDisconnectUpdateTimeout = 2000; // Senders may reconnect, but generally need more time
    private static uiUpdateMap = new Map();

    constructor() {
        if (!ConnectionMonitor.initialized) {
            ConnectionMonitor.logger = new Logger('ConnectionMonitor', LoggerType.BACKEND);

            setInterval(() => {
                if (ConnectionMonitor.backendConnections.size > 0) {
                    for (const sessionId of ConnectionMonitor.backendConnections.keys()) {
                        if (ConnectionMonitor.heartbeatRetries.get(sessionId) > 3) {
                            ConnectionMonitor.logger.warn(`Could not ping device with connection id ${sessionId}. Disconnecting...`);
                            ConnectionMonitor.backendConnections.get(sessionId).disconnect(sessionId);
                        }

                        ConnectionMonitor.logger.debug(`Pinging session ${sessionId} with ${ConnectionMonitor.heartbeatRetries.get(sessionId)} retries left`);
                        ConnectionMonitor.backendConnections.get(sessionId).send(Opcode.Ping, null, sessionId);
                        ConnectionMonitor.heartbeatRetries.set(sessionId, ConnectionMonitor.heartbeatRetries.get(sessionId) + 1);
                    }
                }
            }, ConnectionMonitor.connectionPingTimeout);

            ConnectionMonitor.initialized = true;
        }
    }

    public static onPingPong(value: any, isWebsockets: boolean) {
        ConnectionMonitor.logger.debug(`Received response from ${value.sessionId}`);

        // Websocket clients currently don't support ping-pong commands
        if (!isWebsockets) {
            ConnectionMonitor.heartbeatRetries.set(value.sessionId, 0);
        }
    }

    public static onConnect(listener: any, value: any, isWebsockets: boolean, uiUpdateCallback: any) {
        ConnectionMonitor.logger.info(`Device connected: ${JSON.stringify(value)}`);
        const idMapping = isWebsockets ? value.sessionId : value.data.address;

        if (!ConnectionMonitor.uiUpdateMap.has(idMapping)) {
            ConnectionMonitor.uiUpdateMap.set(idMapping, []);
        }

        if (!isWebsockets) {
            ConnectionMonitor.backendConnections.set(value.sessionId, listener);
            ConnectionMonitor.heartbeatRetries.set(value.sessionId, 0);
        }

        // Occasionally senders seem to instantaneously disconnect and reconnect, so suppress those ui updates
        const senderUpdateQueue = ConnectionMonitor.uiUpdateMap.get(idMapping);
        senderUpdateQueue.push({ event: 'connect', uiUpdateCallback: uiUpdateCallback });
        ConnectionMonitor.uiUpdateMap.set(idMapping, senderUpdateQueue);

        if (senderUpdateQueue.length === 1) {
            setTimeout(() => { ConnectionMonitor.processUiUpdateCallbacks(idMapping); }, ConnectionMonitor.uiConnectUpdateTimeout);
        }
    }

    public static onDisconnect(listener: any, value: any, isWebsockets: boolean, uiUpdateCallback: any) {
        ConnectionMonitor.logger.info(`Device disconnected: ${JSON.stringify(value)}`);

        if (!isWebsockets) {
            ConnectionMonitor.backendConnections.delete(value.sessionId);
            ConnectionMonitor.heartbeatRetries.delete(value.sessionId);
        }

        const idMapping = isWebsockets ? value.sessionId : value.data.address;
        const senderUpdateQueue = ConnectionMonitor.uiUpdateMap.get(idMapping);
        senderUpdateQueue.push({ event: 'disconnect', uiUpdateCallback: uiUpdateCallback });
        ConnectionMonitor.uiUpdateMap.set(idMapping, senderUpdateQueue);

        if (senderUpdateQueue.length === 1) {
            setTimeout(() => { ConnectionMonitor.processUiUpdateCallbacks(idMapping); }, ConnectionMonitor.uiDisconnectUpdateTimeout);
        }
    }

    private static processUiUpdateCallbacks(mapId: string) {
        const updateQueue = ConnectionMonitor.uiUpdateMap.get(mapId);
        let lastConnectCb: any;
        let lastDisconnectCb: any;
        let messageCount = 0;

        updateQueue.forEach(update => {
            ConnectionMonitor.logger.debug(`Processing update event '${update.event}' for ${mapId}`);
            if (update.event === 'connect') {
                messageCount += 1;
                lastConnectCb = update.uiUpdateCallback;
            }
            else if (update.event === 'disconnect') {
                messageCount -= 1;
                lastDisconnectCb = update.uiUpdateCallback;
            }
            else {
                ConnectionMonitor.logger.warn('Unrecognized UI update event:', update.event)
            }
        });

        if (messageCount > 0) {
            ConnectionMonitor.logger.debug(`Sending connect event for ${mapId}`);
            lastConnectCb();
        }
        else if (messageCount < 0) {
            ConnectionMonitor.logger.debug(`Sending disconnect event for ${mapId}`);
            lastDisconnectCb();
        }

        ConnectionMonitor.uiUpdateMap.set(mapId, []);
    }
}
