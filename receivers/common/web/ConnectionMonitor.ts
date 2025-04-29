import { Opcode } from 'common/Packets';

const connectionPingTimeout = 2500;
const connections = [];
const heartbeatRetries = {};
let uiUpdateCallbacks = {
    onConnect: null,
    onDisconnect: null,
}

export function setUiUpdateCallbacks(callbacks: any) {
    uiUpdateCallbacks = callbacks;
}

// Window might be re-created while devices are still connected
function onPingPong(value: any) {
    if (value) {
        heartbeatRetries[value.sessionId] = 0;

        if (!connections.includes(value.sessionId)) {
            connections.push(value.sessionId);
            uiUpdateCallbacks.onConnect(connections, value.sessionId);
        }
    }
}
window.targetAPI.onPing((_event, value: any) => onPingPong(value));
window.targetAPI.onPong((_event, value: any) => onPingPong(value));

window.targetAPI.onConnect((_event, value: any) => {
    console.log(`Device connected: ${JSON.stringify(value)}`);
    connections.push(value.sessionId);
    uiUpdateCallbacks.onConnect(connections, value);
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
