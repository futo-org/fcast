import { Opcode } from 'common/Packets';

const connectionPingTimeout = 2500;
const heartbeatRetries = {};
let connections = [];
let uiUpdateCallbacks = {
    onConnect: null,
    onDisconnect: null,
}

// Window might be re-created while devices are still connected
export function setUiUpdateCallbacks(callbacks: any) {
    uiUpdateCallbacks = callbacks;

    window.targetAPI.getSessions().then((sessions: string[]) => {
        connections = sessions;
        if (connections.length > 0) {
            uiUpdateCallbacks.onConnect(connections, true);
        }
    });
}

function onPingPong(value: any) {
    heartbeatRetries[value.sessionId] = 0;
}
window.targetAPI.onPing((_event, value: any) => onPingPong(value));
window.targetAPI.onPong((_event, value: any) => onPingPong(value));

window.targetAPI.onConnect((_event, value: any) => {
    console.log(`Device connected: ${JSON.stringify(value)}`);
    connections.push(value.sessionId);
    uiUpdateCallbacks.onConnect(connections);
});
window.targetAPI.onDisconnect((_event, value: any) => {
    console.log(`Device disconnected: ${JSON.stringify(value)}`);
    const index = connections.indexOf(value.sessionId);
    if (index != -1) {
        connections.splice(index, 1);
        uiUpdateCallbacks.onDisconnect(connections, value.sessionId);
    }
});

setInterval(() => {
    if (connections.length > 0) {
        window.targetAPI.sendSessionMessage(Opcode.Ping, null);

        for (const sessionId of connections) {
            if (heartbeatRetries[sessionId] > 3) {
                console.warn(`Could not ping device with connection id ${sessionId}. Disconnecting...`);
                window.targetAPI.disconnectDevice(sessionId);
            }

            heartbeatRetries[sessionId] = heartbeatRetries[sessionId] === undefined ? 1 : heartbeatRetries[sessionId] + 1;
        }
    }
}, connectionPingTimeout);
