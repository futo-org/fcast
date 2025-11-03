import { preloadData } from 'common/main/Preload';
import { toast, ToastIcon } from 'common/components/Toast';
import * as tizen from 'tizen-common-web';
import { network } from 'tizen-tv-webapis';

enum RemoteKeyCode {
    Stop = 413,
    Rewind = 412,
    Play = 415,
    Pause = 19,
    FastForward = 417,
    Back = 10009,
    MediaPlayPause = 10252,
}

const serviceId = 'ql5ofothoj.FCastReceiverService.dll';
// const serviceId = 'com.futo.FCastReceiverService';

tizen.tvinputdevice.registerKeyBatch(['MediaRewind',
    'MediaFastForward', 'MediaPlay', 'MediaPause', 'MediaStop'
]);

const manufacturer = tizen.systeminfo.getCapability("http://tizen.org/system/manufacturer");
const modelName = tizen.systeminfo.getCapability("http://tizen.org/system/model_name");

// network.getTVName() does not return a user-friendly name usually...
// preloadData.deviceInfo = { name: network.getTVName(), addresses: [network.getIp()] };
preloadData.deviceInfo = { name: `${manufacturer} ${modelName}`, addresses: [network.getIp()] };
preloadData.onDeviceInfoCb();

let servicePort;
const ipcPort = tizen.messageport.requestLocalMessagePort('ipcPort');
const ipcListener = ipcPort.addMessagePortListener((data) => {
    const messageIndex = data.findIndex((i) => { return i.key === 'message' });
    const dataIndex = data.findIndex((i) => { return i.key === 'data' });
    // eslint-disable-next-line @typescript-eslint/ban-ts-comment
    // @ts-ignore
    const message = JSON.parse(data[dataIndex].value as string);
    console.log('Received data:', JSON.stringify(data));
    // console.log('Received message:', JSON.stringify(message));

    switch (data[messageIndex].value) {
        case 'serviceStart':
            servicePort = tizen.messageport.requestRemoteMessagePort(serviceId, 'ipcPort');
            break;

        case 'serviceStarted':
        case 'getSystemInfo':
            console.log('System information');
            console.log(`BuildDate: ${message.buildDate}`);
            console.log(`BuildId: ${message.buildId}`);
            console.log(`BuildRelease: ${message.buildRelease}`);
            console.log(`BuildString: ${message.buildString}`);
            break;

        case 'toast': {
            toast(message.message, message.icon, message.duration);
            break;
        }

        case 'connect':
            preloadData.onConnectCb(null, message);
            break;

        case 'disconnect':
            preloadData.onDisconnectCb(null, message);
            break;

        case 'ping':
            preloadData.onPingCb(null, message);
            break;

        case 'play':
            sessionStorage.setItem('playData', JSON.stringify(message));
            window.open('../player/index.html', '_self');
            break;

        default:
            console.warn(`Unknown ipc message type: ${data[messageIndex].value}, value: ${data[dataIndex].value}`);
            break;
    }
});

tizen.application.getAppsContext((contexts: tizen.ApplicationContext[]) => {
    try {
        servicePort = tizen.messageport.requestRemoteMessagePort(serviceId, 'ipcPort');
        servicePort.sendMessage([{ key: 'command', value: "getSystemInfo" }]);
    }
    catch (error) {
        console.warn(`Main: preload error setting up service port, will attempt again upon service start ${JSON.stringify(error)}`);
    }

    if (!contexts.find(ctx => ctx.appId === serviceId)) {
        tizen.application.launch(serviceId, () => {
            console.log('Main: preload launched network service');
        }, (error: tizen.WebAPIError) => {
            console.error(`Main: preload error launching network service ${JSON.stringify(error)}`);
            toast(`Main: error launching network service ${JSON.stringify(error)}`, ToastIcon.ERROR);
        });
    }
}, (error: tizen.WebAPIError) => {
    console.error(`Main: preload error querying running applications ${JSON.stringify(error)}`);
    toast(`Main: error querying running applications ${JSON.stringify(error)}`, ToastIcon.ERROR);
});

// eslint-disable-next-line @typescript-eslint/no-explicit-any
document.addEventListener('keydown', (event: any) => {
    // console.log("KeyDown", event);

    switch (event.keyCode) {
        case RemoteKeyCode.Back:
            tizen.application.getCurrentApplication().exit();
            break;
        default:
            break;
    }
});
