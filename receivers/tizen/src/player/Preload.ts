import { preloadData } from 'common/player/Preload';
import { Opcode, PlaybackErrorMessage, PlaybackUpdateMessage, VolumeUpdateMessage } from 'common/Packets';
import { toast, ToastIcon } from 'common/components/Toast';
import * as tizen from 'tizen-common-web';


const serviceId = 'ql5ofothoj.FCastReceiverService.dll';
// const serviceId = 'com.futo.FCastReceiverService';
const servicePort = tizen.messageport.requestRemoteMessagePort(serviceId, 'ipcPort');

preloadData.sendPlaybackErrorCb = (error: PlaybackErrorMessage) => {
    servicePort.sendMessage([
        { key: 'opcode', value: Opcode.PlaybackError.toString() },
        { key: 'data', value: JSON.stringify(error) }
    ]);
};
preloadData.sendPlaybackUpdateCb = (update: PlaybackUpdateMessage) => {
    servicePort.sendMessage([
        { key: 'opcode', value: Opcode.PlaybackUpdate.toString() },
        { key: 'data', value: JSON.stringify(update) }
    ]);
};
preloadData.sendVolumeUpdateCb = (update: VolumeUpdateMessage) => {
    servicePort.sendMessage([
        { key: 'opcode', value: Opcode.VolumeUpdate.toString() },
        { key: 'data', value: JSON.stringify(update) }
    ]);
};

window.tizenOSAPI = {
    pendingPlay: JSON.parse(sessionStorage.getItem('playData'))
};

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
        // case 'serviceStart':
        //     toast("FCast network service started");
        //     break;

        case 'toast': {
            toast(message.message, message.icon, message.duration);
            break;
        }

        case 'ping':
            break;

        case 'play':
            if (message !== null) {
                if (preloadData.onPlayCb === undefined) {
                    window.tizenOSAPI.pendingPlay = message;
                }
                else {
                    preloadData.onPlayCb(null, message);
                }
            }
            break;

        case 'pause':
            preloadData.onPauseCb();
            break;

        case 'resume':
            preloadData.onResumeCb();
            break;

        case 'stop':
            window.open('../main_window/index.html', '_self');
            break;

        case 'seek':
            preloadData.onSeekCb(null, message);
            break;

        case 'setvolume':
            preloadData.onSetVolumeCb(null, message);
            break;

        case 'setspeed':
            preloadData.onSetSpeedCb(null, message);
            break;

        default:
            console.warn(`Unknown ipc message type: ${data[messageIndex].value}, value: ${data[dataIndex].value}`);
            break;
    }
});
