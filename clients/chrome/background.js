let mediaUrls = [];
let hosts = [];
let currentWebSocket = null;
let playbackState = null;
let volume = 1.0;
let selectedHost = null;

const Opcode = {
    None: 0,
    Play: 1,
    Pause: 2,
    Resume: 3,
    Stop: 4,
    Seek: 5,
    PlaybackUpdate: 6,
    VolumeUpdate: 7,
    SetVolume: 8,
};

chrome.runtime.onInstalled.addListener(function() {
    console.log("onInstalled");
    chrome.storage.local.get(['hosts', 'selectedHost'], function(result) {
        console.log("load persistence", result);

        hosts = result.hosts || [];
        selectedHost = result.selectedHost || null;

        if (selectedHost) {
            maintainWebSocketConnection(selectedHost)
        }
        notifyPopup('updateHosts');
        notifyPopup('updateUrls');
    });
});

chrome.webRequest.onHeadersReceived.addListener(
    function(details) {
        console.log(`onHeadersReceived (${details.url})`, details);
        const contentType = details.responseHeaders.find(header => header.name.toLowerCase() === 'content-type')?.value;
        if (!contentType) {
            return;
        }
        
        const isMedia = contentType.startsWith('video/') ||
            contentType.startsWith('audio/') ||
            contentType.toLowerCase() == "application/x-mpegurl" ||
            contentType.toLowerCase() == "application/dash+xml";
        const isSegment = details.url.endsWith(".ts");

        if (contentType && isMedia && !isSegment) {
            if (!mediaUrls.some(v => v.url === details.url))
                mediaUrls.push({contentType, url: details.url});
            console.log('Media URL found:', {contentType, url: details.url});
            notifyPopup('updateUrls');
        }
    },
    { urls: ["<all_urls>"] },
    ["responseHeaders"]
);

chrome.runtime.onMessage.addListener(function(request, sender, sendResponse) {
    if (request.action === 'getUrls') {
        sendResponse({ urls: mediaUrls, selectedHost });
    } else if (request.action === 'clearAll') {
        mediaUrls = [];
        notifyPopup('updateUrls');
    } else if (request.action === 'deleteUrl') {
        mediaUrls = mediaUrls.filter(url => url !== request.url);
        notifyPopup('updateUrls');
    } else if (request.action === 'addHost') {
        hosts.push(request.host);
        chrome.storage.local.set({ 'hosts': hosts }, function () {
            console.log('Hosts saved', hosts);
        });
        notifyPopup('updateHosts');
    } else if (request.action === 'selectHost') {
        selectedHost = request.host;
        chrome.storage.local.set({ 'selectedHost': selectedHost }, function () {
            console.log('Selected host saved', selectedHost);
        });

        maintainWebSocketConnection(selectedHost);
        notifyPopup('updateHosts');
        notifyPopup('updateUrls');
    } else if (request.action === 'deleteHost') {
        hosts = hosts.filter(host => host !== request.host);
        if (selectedHost === request.host) {
            selectedHost = null;
            chrome.storage.local.set({ 'selectedHost': selectedHost }, function () {
                console.log('Selected host cleared');
            });
        }

        chrome.storage.local.set({ 'hosts': hosts }, function () {
            console.log('Hosts updated after deletion');
        });
        notifyPopup('updateHosts');
        notifyPopup('updateUrls');
    } else if (request.action === 'castVideo') {
        play(selectedHost, {
            container: request.url.contentType,
            url: request.url.url
        });
    } else if (request.action === 'getHosts') {
        sendResponse({ hosts, selectedHost });
    } else if (request.action == 'getPlaybackState') {
        sendResponse({ selectedHost, playbackState });
    } else if (request.action == 'getVolume') {
        sendResponse({ volume });
    }  else if (request.action === 'resume') {
        resume(selectedHost);
    } else if (request.action === 'pause') {
        pause(selectedHost);
    } else if (request.action === 'stop') {
        stop(selectedHost);
    } else if (request.action === 'setVolume') {
        setVolume(selectedHost, request.volume);
    } else if (request.action === 'seek') {
        seek(selectedHost, request.time);
    }
});

function closeCurrentWebSocket() {
    if (currentWebSocket) {
        console.log('Closing current WebSocket connection');
        currentWebSocket.close();
        currentWebSocket = null;
    }
}

function notifyPopup(action) {
    chrome.runtime.sendMessage({ action: action });
}

function maintainWebSocketConnection(host) {
    closeCurrentWebSocket();

    if (!host) {
        console.log('No host selected, stopping WebSocket connection');
        return;
    }

    let hostAddress, port;
    const portIndex = host.indexOf(':');
    if (portIndex === -1) {
        hostAddress = host;
        port = 46899;
    } else {
        hostAddress = host.substring(0, portIndex);
        port = host.substring(portIndex + 1, host.length);
    }

    const wsUrl = `ws://${hostAddress}:${port}`;
    currentWebSocket = new WebSocket(wsUrl);

    currentWebSocket.onopen = function() {
        console.log('WebSocket connection opened to ' + wsUrl);
    };

    currentWebSocket.onerror = function(error) {
        console.error('WebSocket error:', error);
    };

    currentWebSocket.onclose = function(event) {
        console.log('WebSocket connection closed:', event.reason);
        if (selectedHost === host) {
            console.log('Attempting to reconnect...');
            setTimeout(() => maintainWebSocketConnection(host), 1000);
        }
    };

    const LENGTH_BYTES = 4;
    const MAXIMUM_PACKET_LENGTH = 32 * 1024;
    const SessionState = {
        WaitingForLength: 0,
        WaitingForData: 1
    };

    let state = SessionState.WaitingForLength;
    let packetLength = 0;
    let bytesRead = 0;
    let buffer = new Uint8Array(MAXIMUM_PACKET_LENGTH);

    function handleLengthBytes(dataView, offset, count) {
        let bytesToRead = Math.min(LENGTH_BYTES - bytesRead, count);
        let bytesRemaining = count - bytesToRead;
        for (let i = 0; i < bytesToRead; i++) {
            buffer[bytesRead + i] = dataView.getUint8(offset + i);
        }
        bytesRead += bytesToRead;
    
        if (bytesRead >= LENGTH_BYTES) {
            packetLength = dataView.getUint32(0, true); // true for little-endian
            bytesRead = 0;
            state = SessionState.WaitingForData;
    
            if (packetLength > MAXIMUM_PACKET_LENGTH) {
                throw new Error("Maximum packet length exceeded");
            }
    
            if (bytesRemaining > 0) {
                handlePacketBytes(dataView, offset + bytesToRead, bytesRemaining);
            }
        }
    }
    
    function handlePacketBytes(dataView, offset, count) {
        let bytesToRead = Math.min(packetLength - bytesRead, count);
        let bytesRemaining = count - bytesToRead;
        for (let i = 0; i < bytesToRead; i++) {
            buffer[bytesRead + i] = dataView.getUint8(offset + i);
        }
        bytesRead += bytesToRead;
    
        if (bytesRead >= packetLength) {
            handlePacket();
    
            state = SessionState.WaitingForLength;
            packetLength = 0;
            bytesRead = 0;
    
            if (bytesRemaining > 0) {
                handleLengthBytes(dataView, offset + bytesToRead, bytesRemaining);
            }
        }
    }
    

    function handlePacket() {
        console.log(`Processing packet of ${bytesRead} bytes`);
    
        // Parse opcode and body
        const opcode = buffer[0];
        const body = bytesRead > 1 ? new TextDecoder().decode(buffer.slice(1, bytesRead)) : null;
    
        console.log("Received body:", body);
    
        switch (opcode) {
            case Opcode.PlaybackUpdate:
                if (body) {
                    try {
                        const playbackUpdateMsg = JSON.parse(body);
                        console.log("Received playback update", playbackUpdateMsg);
                        playbackState = playbackUpdateMsg;
                        notifyPopup('updatePlaybackState');
                    } catch (error) {
                        console.error("Error parsing playback update message:", error);
                    }
                }
                break;
    
            case Opcode.VolumeUpdate:
                if (body) {
                    try {
                        const volumeUpdateMsg = JSON.parse(body);
                        console.log("Received volume update", volumeUpdateMsg);
                        volume = volumeUpdateMsg;
                        notifyPopup('updateVolume');
                    } catch (error) {
                        console.error("Error parsing volume update message:", error);
                    }
                }
                break;
    
            default:
                console.log(`Error handling packet`);
                break;
        }
    }   
    
    currentWebSocket.onmessage = function(event) {
        if (typeof event.data === "string") {
            console.log("Text message received, not handled:", event.data);
        } else {
            event.data.arrayBuffer().then((buffer) => {
                let dataView = new DataView(buffer);
                if (state === SessionState.WaitingForLength) {
                    handleLengthBytes(dataView, 0, buffer.byteLength);
                } else if (state === SessionState.WaitingForData) {
                    handlePacketBytes(dataView, 0, buffer.byteLength);
                } else {
                    console.error("Invalid state encountered");
                    maintainWebSocketConnection(host);
                }
            });
        }
    };
}

function sendWebSocketPacket(h, packet) {
    let host;
    let port;
    const portIndex = h.indexOf(':');
    if (portIndex == -1) {
        host = h;
        port = 46899;
    } else {
        host = h.substring(0, portIndex);
        port = h.substring(portIndex + 1, h.length);
    }
    
    const wsUrl = `ws://${host}:${port}`;
    const socket = new WebSocket(wsUrl);
    socket.onopen = function() {
        console.log('Connection opened to ' + wsUrl);

        socket.send(packet);
        socket.close();
        console.log('Connection closed after sending packet');
    };

    socket.onerror = function(error) {
        console.error('WebSocket error:', error);
    };

    socket.onclose = function (event) {
        console.log('WebSocket connection closed:', event.reason);
    };
}

function createHeader(opcode, bodyLength) {
    const buffer = new ArrayBuffer(5);
    const view = new DataView(buffer);
    view.setUint32(0, bodyLength + 1, true); // size (little endian)
    view.setUint8(4, opcode);
    return buffer;
}

function createBody(jsonObject) {
    const jsonString = JSON.stringify(jsonObject);
    return new TextEncoder().encode(jsonString);
}

function play(host, playMessage) {
    const body = createBody(playMessage);
    const header = createHeader(1, body.length);
    const packet = concatenateBuffers(header, body);
    sendWebSocketPacket(host, packet);
}

function pause(host) {
    const header = createHeader(2, 0);
    sendWebSocketPacket(host, new Uint8Array(header));
}

function resume(host) {
    const header = createHeader(3, 0);
    sendWebSocketPacket(host, new Uint8Array(header));
}

function stop(host) {
    const header = createHeader(4, 0);
    sendWebSocketPacket(host, new Uint8Array(header));
}

function seek(host, time) {
    const body = createBody({time});
    const header = createHeader(5, body.length);
    const packet = concatenateBuffers(header, body);
    sendWebSocketPacket(host, packet);
}

function setVolume(host, volume) {
    const body = createBody({volume});
    const header = createHeader(8, body.length);
    const packet = concatenateBuffers(header, body);
    sendWebSocketPacket(host, packet);
}

function concatenateBuffers(buffer1, buffer2) {
    const tmp = new Uint8Array(buffer1.byteLength + buffer2.byteLength);
    tmp.set(new Uint8Array(buffer1), 0);
    tmp.set(new Uint8Array(buffer2), buffer1.byteLength);
    return tmp.buffer;
}