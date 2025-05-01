import { Opcode } from 'common/Packets';
import { Logger, LoggerType } from 'common/Logger';

// Window might be re-created while devices are still connected
export function setUiUpdateCallbacks(callbacks: any) {
    const logger = window.targetAPI.logger;
    let frontendConnections = [];

    window.targetAPI.onConnect((_event, value: any) => {
        frontendConnections.push(value.sessionId);
        callbacks.onConnect(frontendConnections);
    });
    window.targetAPI.onDisconnect((_event, value: any) => {
        const index = frontendConnections.indexOf(value.sessionId);
        if (index != -1) {
            frontendConnections.splice(index, 1);
            callbacks.onDisconnect(frontendConnections, value.sessionId);
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
    private static initialized = false;
    private static connectionPingTimeout = 2500;
    private static heartbeatRetries = new Map();
    private static backendConnections = new Map();
    private static logger;

    constructor() {
        if (!ConnectionMonitor.initialized) {
            ConnectionMonitor.logger = new Logger('ConnectionMonitor', LoggerType.BACKEND);

            setInterval(() => {
                if (ConnectionMonitor.backendConnections.size > 0) {
                    for (const sessionId in ConnectionMonitor.backendConnections) {
                        if (ConnectionMonitor.heartbeatRetries.get(sessionId) > 3) {
                            ConnectionMonitor.logger.warn(`Could not ping device with connection id ${sessionId}. Disconnecting...`);
                            ConnectionMonitor.backendConnections.get(sessionId).disconnect(sessionId);
                        }

                        ConnectionMonitor.backendConnections.get(sessionId).send(Opcode.Ping, null);
                        ConnectionMonitor.heartbeatRetries.set(sessionId, ConnectionMonitor.heartbeatRetries.get(sessionId) === undefined ? 1 : ConnectionMonitor.heartbeatRetries.get(sessionId) + 1);
                    }
                }
            }, ConnectionMonitor.connectionPingTimeout);

            ConnectionMonitor.initialized = true;
        }
    }

    public static onPingPong(value: any) {
        ConnectionMonitor.heartbeatRetries[value.sessionId] = 0;
    }

    public static onConnect(listener: any, value: any) {
        ConnectionMonitor.logger.info(`Device connected: ${JSON.stringify(value)}`);
        ConnectionMonitor.backendConnections.set(value.sessionId, listener);
    }

    public static onDisconnect(listener: any, value: any) {
        ConnectionMonitor.logger.info(`Device disconnected: ${JSON.stringify(value)}`);
        ConnectionMonitor.backendConnections.delete(value.sessionId);
    }
}
