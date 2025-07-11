bitflags::bitflags! {
    /// https://openairplay.github.io/airplay-spec/status_flags.html
    #[derive(PartialEq, Eq, Debug)]
    pub struct AirPlayStatus: u32 {
        /// has been detected Defined in CarPlay section of MFi spec. Not seen set anywhere
        const Problem = 1 << 0;
        /// is not configured	Defined in CarPlay section of MFi spec. Not seen set anywhere
        const Device = 1 << 1;
        /// cable is attached Defined in CarPlay section of MFi spec. Seen on AppleTV, Denon AVR, HomePod, Airport Express
        const Audio = 1 << 2;
        const PINRequired = 1 << 3;
        const SupportsAirPlayFromCloud = 1 << 6;
        const PasswordRequired = 1 << 7;
        const OneTimePairingRequired = 1 << 9;
        const DeviceWasSetupForHKAccessControl = 1 << 10;
        /// Shows in logs as relayable. When set iOS will connect to the device to get currently playing track.
        const DeviceSupportsRelay = 1 << 11;
        const SilentPrimary =1 << 12;
        const TightSyncIsGroupLeader = 1 << 13;
        const TightSyncBuddyNotReachable = 1 << 14;
        /// Shows in logs as music
        const IsAppleMusicSubscriber = 1 << 15;
        /// Shows in logs as iCML
        const CloudLibraryIsOn = 1 << 16;
        /// Shows in logs as airplay-receiving. Set when Apple TV is receiving anything via AirPlay.
        const ReceiverSessionIsActive = 1 << 17;
    }
}

bitflags::bitflags! {
    /// https://openairplay.github.io/airplay-spec/features.html
    #[derive(PartialEq, Eq, Debug)]
    pub struct AirPlayFeatures: u64 {
        /// video supported
        const Video = 1 << 0;
        /// photo supported
        const Photo = 1 << 1;
        /// video protected with FairPlay DRM
        const VideoFairPlay = 1 << 2;
        /// volume control supported for videos
        const VideoVolumeControl = 1 << 3;
        /// http live streaming supported
        const VideoHTTPLiveStreams = 1 << 4;
        /// slideshow supported
        const Slideshow = 1 << 5;
        /// mirroring supported
        const Screen = 1 << 7;
        /// screen rotation supported
        const ScreenRotate = 1 << 8;
        /// audio supported
        const Audio = 1 << 9;
        /// audio packet redundancy supported
        const AudioRedundant = 1 << 11;
        /// FairPlay secure auth supported
        const FPSAPv2pt5_AES_GCM = 1 << 12;
        /// photo preloading supported
        const PhotoCaching = 1 << 13;
        /// Authentication type 4. FairPlay authentication
        const Authentication4 = 1 << 14;
        /// bit 1 of MetadataFeatures. Artwork.
        const MetadataFeature1 = 1 << 15;
        /// bit 2 of MetadataFeatures. Progress.
        const MetadataFeature2 = 1 << 16;
        /// bit 0 of MetadataFeatures. Text.
        const MetadataFeature0 = 1 << 17;
        /// support for audio format 1
        const AudioFormat1 = 1 << 18;
        /// support for audio format 2. This bit must be set for AirPlay 2 connection to work
        const AudioFormat2 = 1 << 19;
        /// support for audio format 3. This bit must be set for AirPlay 2 connection to work
        const AudioFormat3 = 1 << 20;
        /// support for audio format 4
        const AudioFormat4 = 1 << 21;
        /// Authentication type 1. RSA Authentication
        const Authentication1 = 1 << 23;
        const HasUnifiedAdvertiserInfo = 1 << 26;
        const SupportsLegacyPairing = 1 << 27;
        /// RAOP is supported on this port. With this bit set your don't need the AirTunes service
        const RAOP = 1 << 30;
        /// Don’t read key from pk record it is known
        const IsCarPlay_SupportsVolume = 1 << 32;
        const SupportsAirPlayVideoPlayQueue = 1 << 33;
        const SupportsAirPlayFromCloud = 1 << 34;
        /// SupportsHKPairingAndAccessControl, SupportsSystemPairing and SupportsTransientPairing
        /// implies SupportsCoreUtilsPairingAndEncryption
        const SupportsCoreUtilsPairingAndEncryption = 1 << 38;
        /// Bit needed for device to show as supporting multi-room audio
        const SupportsBufferedAudio = 1 << 40;
        /// Bit needed for device to show as supporting multi-room audio
        const SupportsPTP = 1 << 41;
        const SupportsScreenMultiCodec = 1 << 42;
        const SupportsSystemPairing = 1 << 43;
        const SupportsHKPairingAndAccessControl = 1 << 46;
        /// SupportsSystemPairing implies SupportsTransientPairing
        const SupportsTransientPairing = 1 << 48;
        /// bit 4 of MetadataFeatures. binary plist.
        const MetadataFeature4 = 1 << 50;
        /// Authentication type 8. MFi authentication
        const SupportsUnifiedPairSetupAndMFi = 1 << 51;
        const SupportsSetPeersExtendedMessage = 1 << 52;
    }
}

#[allow(dead_code)]
// #[derive(serde::Deserialize, Debug)]
#[derive(Debug, Default)]
pub struct InfoPlist {
    // #[serde(rename = "PTPInfo")]
    // pub ptp_info: Option<String>,
    // pub build: Option<String>,
    /// MAC address
    // #[serde(rename = "deviceID")]
    pub device_id: Option<String>,
    pub features: Option<AirPlayFeatures>,
    // #[serde(rename = "initialVolume")]
    // pub initial_volume: Option<f64>,
    // #[serde(rename = "firmwareBuildDate")]
    // pub firmware_build_date: Option<String>,
    // #[serde(rename = "firmwareRevision")]
    // pub firmware_revision: Option<String>,
    pub manufacturer: Option<String>,
    /// Device model
    pub model: Option<String>,
    pub name: Option<String>,
    // #[serde(rename = "protocolVersion")]
    // pub protocol_version: Option<String>,
    // #[serde(rename = "senderAddress")]
    // pub sender_address: Option<String>,
    // #[serde(rename = "sourceVersion")]
    // pub source_version: Option<String>,
    // #[serde(rename = "statusFlags")]
    pub status_flags: Option<AirPlayStatus>,
    /// Raw TXT record from AirPlay service mDNS record
    // #[serde(rename = "txtAirPlay")]
    pub txt_air_play: Option<Vec<u8>>,
    // txt_air_play: Option<Vec<u8>>,
    // The following properties are omitted:
    // pi	string
    // pk	data
    // playbackCapabilities.supportsFPSSecureStop	boolean
    // playbackCapabilities.supportsUIForAudioOnlyContent	boolean
    // psi	string
    // sdk	string
    // vv	integer
    // volumeControlType	integer
    // txtRAOP	data	...	raw TXT record from AirTunes service mDNS record
    // keepAliveSendStatsAsBody bool
    // nameIsFactoryDefault bool
    // keepAliveLowPower bool
    // macAddress string
}
