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
window.targetAPI.onPing((_event, value: any) => {
    if (value) {
        heartbeatRetries[value.id] = 0;

        if (!connections.includes(value.id)) {
            connections.push(value.id);
            uiUpdateCallbacks.onConnect(connections, value.id);
        }
    }
});
window.targetAPI.onConnect((_event, value: any) => {
    connections.push(value.id);
    uiUpdateCallbacks.onConnect(connections, value);
});
window.targetAPI.onDisconnect((_event, value: any) => {
    console.log(`Device disconnected: ${JSON.stringify(value)}`);
    const index = connections.indexOf(value.id);
    if (index != -1) {
        connections.splice(index, 1);
        uiUpdateCallbacks.onDisconnect(connections, value.id);
    }
});

setInterval(() => {
    if (connections.length > 0) {
        window.targetAPI.sendSessionMessage(Opcode.Ping, null);

        for (const session of connections) {
            if (heartbeatRetries[session] > 3) {
                console.warn(`Could not ping device with connection id ${session}. Disconnecting...`);
                window.targetAPI.disconnectDevice(session);
            }

            heartbeatRetries[session] = heartbeatRetries[session] === undefined ? 1 : heartbeatRetries[session] + 1;
        }
    }
}, connectionPingTimeout);
