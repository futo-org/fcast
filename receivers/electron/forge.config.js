const fs = require('fs');
const yargs = require('yargs/yargs');
const { hideBin } = require('yargs/helpers');

const { FusesPlugin } = require('@electron-forge/plugin-fuses');
const { FuseV1Options, FuseVersion } = require('@electron/fuses');

const argv = yargs(hideBin(process.argv)).argv;

module.exports = {
  packagerConfig: {
    asar: true,
    icon: './assets/icons/icon',
    name: 'FCast Receiver',
    osxSign: {},
    osxNotarize: {
      appleApiKey: process.env.FCAST_APPLE_API_KEY,
      appleApiKeyId: process.env.FCAST_APPLE_API_KEY_ID,
      appleApiIssuer: process.env.FCAST_APPLE_API_ISSUER
    }
  },
  rebuildConfig: {},
  makers: [
    // {
    //   name: '@electron-forge/maker-squirrel',
    //   config: {},
    // },
    {
      name: '@electron-forge/maker-dmg',
      config: {
        additionalDMGOptions: {
          window: {
            position: {
              x: 425,
              y: 275
            },
            size: {
              width: 640,
              height: 480
            }
          }
        },
        background: './assets/images/background.png',
        contents: [
          { 'x': 90, 'y': 350, 'type': 'file', 'path': `out/FCast Receiver-darwin-${argv.arch}/FCast Receiver.app` },
          { 'x': 360, 'y': 350, 'type': 'link', 'path': '/Applications' },
          { 'x': 0, 'y': 540, 'type': 'position', 'path': '.background' },
          { 'x': 120, 'y': 540, 'type': 'position', 'path': '.VolumeIcon.icns' }
        ],
        format: 'ULFO',
        icon: './assets/icons/icon.icns',
        name: 'FCast Receiver'
      }
    },
    {
      name: '@electron-forge/maker-zip',
      platforms: ['win32', 'darwin', 'linux'],
      config: {}
    },
    // {

    //   name: '@electron-forge/maker-deb',
    //   config: {},
    // },
    // {
    //   name: '@electron-forge/maker-rpm',
    //   config: {},
    // },
  ],
  hooks: {
    postMake: async (forgeConfig, makeResults) => {
      makeResults.forEach(e => {
        // Standardize artifact output naming
        switch (e.platform) {
          case "win32":
            break;
          case "darwin": {
            let artifactName = 'FCast Receiver.dmg';
            if (fs.existsSync(`./out/make/${artifactName}`)) {
              fs.renameSync(`./out/make/${artifactName}`, `./out/make/FCast-Receiver-${e.packageJSON.version}-macOS-${e.arch}.dmg`);
            }

            artifactName = 'FCast Receiver-darwin-arm64-1.0.14.zip';
            if (fs.existsSync(`./out/make/zip/darwin/arm64/${artifactName}`)) {
              fs.renameSync(`./out/make/zip/darwin/arm64/${artifactName}`, `./out/make/zip/darwin/arm64/FCast-Receiver-${e.packageJSON.version}-macOS-${e.arch}.zip`);
            }

            artifactName = 'FCast Receiver-darwin-x64-1.0.14.zip';
            if (fs.existsSync(`./out/make/zip/darwin/x64/${artifactName}`)) {
              fs.renameSync(`./out/make/zip/darwin/x64/${artifactName}`, `./out/make/zip/darwin/x64/FCast-Receiver-${e.packageJSON.version}-macOS-${e.arch}.zip`);
            }

            break;
          }
          case "linux":
            break;
          default:
            break;
        }
      });
    }
  },
  plugins: [
    {
      name: '@electron-forge/plugin-auto-unpack-natives',
      config: {},
    },
    // Fuses are used to enable/disable various Electron functionality
    // at package time, before code signing the application
    new FusesPlugin({
      version: FuseVersion.V1,
      [FuseV1Options.RunAsNode]: false,
      [FuseV1Options.EnableCookieEncryption]: true,
      [FuseV1Options.EnableNodeOptionsEnvironmentVariable]: false,
      [FuseV1Options.EnableNodeCliInspectArguments]: false,
      [FuseV1Options.EnableEmbeddedAsarIntegrityValidation]: true,
      [FuseV1Options.OnlyLoadAppFromAsar]: true,
    }),
  ],
};
