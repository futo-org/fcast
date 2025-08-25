



            @sealed class CastContextImpl extends RustOpaque implements CastContext {
                // Not to be used by end users
                CastContextImpl.frbInternalDcoDecode(List<dynamic> wire):
                    super.frbInternalDcoDecode(wire, _kStaticData);

                // Not to be used by end users
                CastContextImpl.frbInternalSseDecode(BigInt ptr, int externalSizeOnNative):
                    super.frbInternalSseDecode(ptr, externalSizeOnNative, _kStaticData);

                static final _kStaticData = RustArcStaticData(
                    rustArcIncrementStrongCount: RustLib.instance.api.rust_arc_increment_strong_count_CastContext,
                    rustArcDecrementStrongCount: RustLib.instance.api.rust_arc_decrement_strong_count_CastContext,
                    rustArcDecrementStrongCountPtr: RustLib.instance.api.rust_arc_decrement_strong_count_CastContextPtr,
                );

                 CastingDevice  createDeviceFromInfo({required DeviceInfo info })=>RustLib.instance.api.crateApiCastContextCreateDeviceFromInfo(that: this, info: info);


            }
            @sealed class CastingDeviceImpl extends RustOpaque implements CastingDevice {
                // Not to be used by end users
                CastingDeviceImpl.frbInternalDcoDecode(List<dynamic> wire):
                    super.frbInternalDcoDecode(wire, _kStaticData);

                // Not to be used by end users
                CastingDeviceImpl.frbInternalSseDecode(BigInt ptr, int externalSizeOnNative):
                    super.frbInternalSseDecode(ptr, externalSizeOnNative, _kStaticData);

                static final _kStaticData = RustArcStaticData(
                    rustArcIncrementStrongCount: RustLib.instance.api.rust_arc_increment_strong_count_CastingDevice,
                    rustArcDecrementStrongCount: RustLib.instance.api.rust_arc_decrement_strong_count_CastingDevice,
                    rustArcDecrementStrongCountPtr: RustLib.instance.api.rust_arc_decrement_strong_count_CastingDevicePtr,
                );

                
            }
            @sealed class DeviceConnectionStateImpl extends RustOpaque implements DeviceConnectionState {
                // Not to be used by end users
                DeviceConnectionStateImpl.frbInternalDcoDecode(List<dynamic> wire):
                    super.frbInternalDcoDecode(wire, _kStaticData);

                // Not to be used by end users
                DeviceConnectionStateImpl.frbInternalSseDecode(BigInt ptr, int externalSizeOnNative):
                    super.frbInternalSseDecode(ptr, externalSizeOnNative, _kStaticData);

                static final _kStaticData = RustArcStaticData(
                    rustArcIncrementStrongCount: RustLib.instance.api.rust_arc_increment_strong_count_DeviceConnectionState,
                    rustArcDecrementStrongCount: RustLib.instance.api.rust_arc_decrement_strong_count_DeviceConnectionState,
                    rustArcDecrementStrongCountPtr: RustLib.instance.api.rust_arc_decrement_strong_count_DeviceConnectionStatePtr,
                );

                
            }
            @sealed class GenericKeyEventImpl extends RustOpaque implements GenericKeyEvent {
                // Not to be used by end users
                GenericKeyEventImpl.frbInternalDcoDecode(List<dynamic> wire):
                    super.frbInternalDcoDecode(wire, _kStaticData);

                // Not to be used by end users
                GenericKeyEventImpl.frbInternalSseDecode(BigInt ptr, int externalSizeOnNative):
                    super.frbInternalSseDecode(ptr, externalSizeOnNative, _kStaticData);

                static final _kStaticData = RustArcStaticData(
                    rustArcIncrementStrongCount: RustLib.instance.api.rust_arc_increment_strong_count_GenericKeyEvent,
                    rustArcDecrementStrongCount: RustLib.instance.api.rust_arc_decrement_strong_count_GenericKeyEvent,
                    rustArcDecrementStrongCountPtr: RustLib.instance.api.rust_arc_decrement_strong_count_GenericKeyEventPtr,
                );

                
            }
            @sealed class GenericMediaEventImpl extends RustOpaque implements GenericMediaEvent {
                // Not to be used by end users
                GenericMediaEventImpl.frbInternalDcoDecode(List<dynamic> wire):
                    super.frbInternalDcoDecode(wire, _kStaticData);

                // Not to be used by end users
                GenericMediaEventImpl.frbInternalSseDecode(BigInt ptr, int externalSizeOnNative):
                    super.frbInternalSseDecode(ptr, externalSizeOnNative, _kStaticData);

                static final _kStaticData = RustArcStaticData(
                    rustArcIncrementStrongCount: RustLib.instance.api.rust_arc_increment_strong_count_GenericMediaEvent,
                    rustArcDecrementStrongCount: RustLib.instance.api.rust_arc_decrement_strong_count_GenericMediaEvent,
                    rustArcDecrementStrongCountPtr: RustLib.instance.api.rust_arc_decrement_strong_count_GenericMediaEventPtr,
                );

                
            }
            @sealed class PlaybackStateImpl extends RustOpaque implements PlaybackState {
                // Not to be used by end users
                PlaybackStateImpl.frbInternalDcoDecode(List<dynamic> wire):
                    super.frbInternalDcoDecode(wire, _kStaticData);

                // Not to be used by end users
                PlaybackStateImpl.frbInternalSseDecode(BigInt ptr, int externalSizeOnNative):
                    super.frbInternalSseDecode(ptr, externalSizeOnNative, _kStaticData);

                static final _kStaticData = RustArcStaticData(
                    rustArcIncrementStrongCount: RustLib.instance.api.rust_arc_increment_strong_count_PlaybackState,
                    rustArcDecrementStrongCount: RustLib.instance.api.rust_arc_decrement_strong_count_PlaybackState,
                    rustArcDecrementStrongCountPtr: RustLib.instance.api.rust_arc_decrement_strong_count_PlaybackStatePtr,
                );

                
            }
            @sealed class SourceImpl extends RustOpaque implements Source {
                // Not to be used by end users
                SourceImpl.frbInternalDcoDecode(List<dynamic> wire):
                    super.frbInternalDcoDecode(wire, _kStaticData);

                // Not to be used by end users
                SourceImpl.frbInternalSseDecode(BigInt ptr, int externalSizeOnNative):
                    super.frbInternalSseDecode(ptr, externalSizeOnNative, _kStaticData);

                static final _kStaticData = RustArcStaticData(
                    rustArcIncrementStrongCount: RustLib.instance.api.rust_arc_increment_strong_count_Source,
                    rustArcDecrementStrongCount: RustLib.instance.api.rust_arc_decrement_strong_count_Source,
                    rustArcDecrementStrongCountPtr: RustLib.instance.api.rust_arc_decrement_strong_count_SourcePtr,
                );

                
            }