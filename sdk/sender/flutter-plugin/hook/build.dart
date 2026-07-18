import 'package:flutter_rust_bridge_hooks/flutter_rust_bridge_hooks.dart';

const _defaultFeatures = ['fcast', 'chromecast', 'logging'];

void main(List<String> args) async {
  await build(args, (input, output) async {
    // Consuming apps can select which protocols/features to compile in via
    // native-assets user-defines in their pubspec.yaml, e.g.:
    //
    //   hooks:
    //     user_defines:
    //       fcast_sender_sdk:
    //         features: [chromecast]
    //
    // Omit the block to build everything (the default). See README.md.
    final selected = input.userDefines['features'];
    if (selected != null && selected is! List) {
      throw const FormatException(
        'hooks.user_defines.fcast_sender_sdk.features must be a list of '
        'strings (e.g. [fcast, chromecast, logging]), or omitted.',
      );
    }
    final features = selected == null
        ? _defaultFeatures
        : (selected as List).map((e) => e.toString()).toList();

    await FlutterRustBridgeNativeAssetsBuilder(
      cratePath: 'rust',
      // Forward exactly the selected features
      enableDefaultFeatures: false,
      features: features,
    ).run(input: input, output: output);
  });
}
