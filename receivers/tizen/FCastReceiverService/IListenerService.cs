using System;
using System.Collections.Generic;
using System.Threading.Tasks;

namespace FCastReceiverService
{
    public interface IListenerService
    {
        event EventHandler<PlayMessage> OnPlay;
        event EventHandler OnPause;
        event EventHandler OnResume;
        event EventHandler OnStop;
        event EventHandler<SeekMessage> OnSeek;
        event EventHandler<SetVolumeMessage> OnSetVolume;
        event EventHandler<SetSpeedMessage> OnSetSpeed;
        event EventHandler<VersionMessage> OnVersion;
        event EventHandler<Dictionary<string, string>> OnPing;
        event EventHandler OnPong;

        event EventHandler<Dictionary<string, string>> OnConnect;
        event EventHandler<Dictionary<string, string>> OnDisconnect;

        Task ListenAsync();
    }
}
