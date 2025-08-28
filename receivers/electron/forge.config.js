const fs = require('fs');
const yargs = require('yargs/yargs');
const { hideBin } = require('yargs/helpers');
const cp = require('child_process');
const path = require('path');
// const extract = require('extract-zip')

const { FusesPlugin } = require('@electron-forge/plugin-fuses');
const { FuseV1Options, FuseVersion } = require('@electron/fuses');

const argv = yargs(hideBin(process.argv)).argv;
const APPLICATION_NAME = 'fcast-receiver';
const APPLICATION_TITLE = 'FCast Receiver';
const CI_SIGNING_DIR = '/deploy/signing';

module.exports = {
  packagerConfig: {
    asar: true,
    icon: './assets/icons/app/icon',
    osxSign: {},
    osxNotarize: {
      appleApiKey: process.env.FCAST_APPLE_API_KEY,
      appleApiKeyId: process.env.FCAST_APPLE_API_KEY_ID,
      appleApiIssuer: process.env.FCAST_APPLE_API_ISSUER
    }
  },
  rebuildConfig: {},
  makers: [
    {
      name: '@electron-forge/maker-deb',
      config: {
        options: {
          categories: ['AudioVideo', 'Audio', 'Video', 'Network', 'Utility'],
          homepage: 'https://fcast.org/',
          icon: './assets/icons/app/icon.png',
        }
      },
    },
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
          { 'x': 90, 'y': 350, 'type': 'file', 'path': `out/${APPLICATION_NAME}-darwin-${argv.arch}/${APPLICATION_TITLE}.app` },
          { 'x': 360, 'y': 350, 'type': 'link', 'path': '/Applications' },
          { 'x': 0, 'y': 540, 'type': 'position', 'path': '.background' },
          { 'x': 120, 'y': 540, 'type': 'position', 'path': '.VolumeIcon.icns' }
        ],
        format: 'ULFO',
        icon: './assets/icons/app/icon.icns',
        name: APPLICATION_TITLE
      }
    },
    {
      name: '@electron-forge/maker-rpm',
      config: {
        options: {
          categories: ['AudioVideo', 'Audio', 'Video', 'Network', 'Utility'],
          homepage: 'https://fcast.org/',
          icon: './assets/icons/app/icon.png',
          license: 'MIT',
        }
      },
    },
    // Same as '@electron-forge/maker-wix', except linux compatible
    {
      name: '@futo/forge-maker-wix-linux',
      config: {
        arch: 'x64',
        appUserModelId: `org.futo.${APPLICATION_NAME}`,
        icon: './assets/icons/app/icon.ico',
        name: APPLICATION_TITLE,
        programFilesFolderName: APPLICATION_TITLE,
        shortcutName: APPLICATION_TITLE,
      }
    },
    {
      name: '@electron-forge/maker-zip',
      // Manually creating zip for mac targets due to .app renaming
      platforms: ["win32", "linux"],
      config: {}
    },
  ],
  hooks: {
    readPackageJson: async (forgeConfig, packageJson) => {
      packageJson.commit = cp.execSync('git rev-parse HEAD').toString().trim();
      packageJson.channel = process.env.FCAST_CHANNEL ? process.env.FCAST_CHANNEL : 'stable';
      if (packageJson.channel !== 'stable') {
        packageJson.channelVersion = process.env.FCAST_CHANNEL_VERSION ? process.env.FCAST_CHANNEL_VERSION : '1';
      }

      return packageJson;
    },

    postPackage: async (config, packageResults) => {
      switch (packageResults.platform) {
        case "win32": {
          const exePath = `./out/${APPLICATION_NAME}-${packageResults.platform}-${packageResults.arch}/${APPLICATION_NAME}.exe`;

          if (fs.existsSync(CI_SIGNING_DIR)) {
            console.log(cp.execSync(path.join(CI_SIGNING_DIR, `sign.sh ${exePath}`)).toString().trim());
          }
          else {
            console.warn('Windows signing script not found, skipping...');
          }

          break;
        }
        case "darwin": {
          let artifactName = `${APPLICATION_NAME}.app`;
          if (fs.existsSync(`./out/${APPLICATION_NAME}-${packageResults.platform}-${packageResults.arch}/${artifactName}`)) {
            fs.renameSync(`./out/${APPLICATION_NAME}-${packageResults.platform}-${packageResults.arch}/${artifactName}`, `./out/${APPLICATION_NAME}-${packageResults.platform}-${packageResults.arch}/${APPLICATION_TITLE}.app`);
          }
          break;
        }
        case "linux": {
          // Workaround for broken Ubuntu builds due to sandboxing permissions:
          // * https://github.com/electron/electron/issues/17972c
          // * https://github.com/electron/electron/issues/41066
          fs.renameSync(`./out/${APPLICATION_NAME}-${packageResults.platform}-${packageResults.arch}/${APPLICATION_NAME}`, `./out/${APPLICATION_NAME}-${packageResults.platform}-${packageResults.arch}/${APPLICATION_NAME}.app`);
          fs.writeFileSync(`./out/${APPLICATION_NAME}-${packageResults.platform}-${packageResults.arch}/${APPLICATION_NAME}`,
            '#!/bin/sh\n' +
            'if [ "$0" = "/usr/bin/fcast-receiver" ]; then\n' +
            '\tbin="/usr/lib/fcast-receiver/fcast-receiver.app"\n' +
            'else\n' +
            '\tbin="$0.app"\n' +
            'fi\n' +
            '"$bin" --no-sandbox --password-store=basic $*'
          );
          fs.chmodSync(`./out/${APPLICATION_NAME}-${packageResults.platform}-${packageResults.arch}/${APPLICATION_NAME}`, 0o755);
          break;
        }
        default:
          break;
      }
    },

    postMake: async (forgeConfig, makeResults) => {
      for (const e of makeResults) {
        // Standardize artifact output naming
        switch (e.platform) {
          case "win32": {
            let artifactName = `${APPLICATION_NAME}-win32-${e.arch}-${e.packageJSON.version}.zip`;
            if (fs.existsSync(`./out/make/zip/win32/${e.arch}/${artifactName}`)) {
              fs.renameSync(`./out/make/zip/win32/${e.arch}/${artifactName}`, path.join(`./out/make/zip/win32/${e.arch}`, generateArtifactName(e.packageJSON, e.platform, e.arch, 'zip')));
            }

            artifactName = `${APPLICATION_NAME}.msi`;
            if (fs.existsSync(`./out/make/wix/${e.arch}/${artifactName}`)) {
              const artifactPath = path.join(`./out/make/wix/${e.arch}`, generateArtifactName(e.packageJSON, e.platform, e.arch, 'msi'));
              fs.renameSync(`./out/make/wix/${e.arch}/${artifactName}`, artifactPath);

              if (fs.existsSync(CI_SIGNING_DIR)) {
                console.log(cp.execSync(path.join(CI_SIGNING_DIR, `sign.sh ${artifactPath}`)).toString().trim());
              }
              else {
                console.warn('Windows signing script not found, skipping...');
              }
            }

            break;
          }
          case "darwin": {
            let artifactName = `${APPLICATION_TITLE}.dmg`;
            if (fs.existsSync(`./out/make/${artifactName}`)) {
              fs.mkdirSync(`./out/make/dmg/${e.arch}`, { recursive: true });
              fs.renameSync(`./out/make/${artifactName}`, path.join(`./out/make/dmg/${e.arch}`, generateArtifactName(e.packageJSON, e.platform, e.arch, 'dmg')));
            }

            console.log(`Making a zip distributable for ${e.platform}/${e.arch}`);
            const zipPath = path.resolve(process.cwd(), 'out', 'make', 'zip', e.platform, e.arch, generateArtifactName(e.packageJSON, e.platform, e.arch, 'zip'));

            console.log(cp.execSync(`mkdir -p ${path.dirname(zipPath)}`).toString().trim());
            console.log(cp.execSync(`cd out/${APPLICATION_NAME}-${e.platform}-${e.arch}; zip -r -y "${zipPath}" "${APPLICATION_TITLE}.app"`).toString().trim());
            break;
          }
          case "linux": {
            let artifactName = `${APPLICATION_NAME}-linux-${e.arch}-${e.packageJSON.version}.zip`;
            if (fs.existsSync(`./out/make/zip/linux/${e.arch}/${artifactName}`)) {
              fs.renameSync(`./out/make/zip/linux/${e.arch}/${artifactName}`, path.join(`./out/make/zip/linux/${e.arch}`, generateArtifactName(e.packageJSON, e.platform, e.arch, 'zip')));
            }

            artifactName = `${APPLICATION_NAME}_${e.packageJSON.version}_amd64.deb`
            if (fs.existsSync(`./out/make/deb/${e.arch}/${artifactName}`)) {
              fs.renameSync(`./out/make/deb/${e.arch}/${artifactName}`, path.join(`./out/make/deb/${e.arch}`, generateArtifactName(e.packageJSON, e.platform, e.arch, 'deb')));
            }

            artifactName = `${APPLICATION_NAME}_${e.packageJSON.version}_arm64.deb`
            if (fs.existsSync(`./out/make/deb/${e.arch}/${artifactName}`)) {
              fs.renameSync(`./out/make/deb/${e.arch}/${artifactName}`, path.join(`./out/make/deb/${e.arch}`, generateArtifactName(e.packageJSON, e.platform, e.arch, 'deb')));
            }

            artifactName = `${APPLICATION_NAME}-${e.packageJSON.version}-1.x86_64.rpm`
            if (fs.existsSync(`./out/make/rpm/${e.arch}/${artifactName}`)) {
              fs.renameSync(`./out/make/rpm/${e.arch}/${artifactName}`, path.join(`./out/make/rpm/${e.arch}`, generateArtifactName(e.packageJSON, e.platform, e.arch, 'rpm')));
            }

            artifactName = `${APPLICATION_NAME}-${e.packageJSON.version}-1.arm64.rpm`
            if (fs.existsSync(`./out/make/rpm/${e.arch}/${artifactName}`)) {
              fs.renameSync(`./out/make/rpm/${e.arch}/${artifactName}`, path.join(`./out/make/rpm/${e.arch}`, generateArtifactName(e.packageJSON, e.platform, e.arch, 'rpm')));
            }

            break;
          }
          default:
            break;
        }
      }
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
      [FuseV1Options.EnableEmbeddedAsarIntegrityValidation]: false,
      [FuseV1Options.OnlyLoadAppFromAsar]: true,
    }),
  ],
};

function getArtifactOS(platform) {
  switch (platform) {
      case 'win32':
          return 'windows';
      case 'darwin':
          return 'macOS';
      default:
          return platform;
  }
}

function generateArtifactName(packageJSON, platform, arch, extension) {
  let artifactName = `${APPLICATION_NAME}-${packageJSON.version}-${getArtifactOS(platform)}-${arch}`;
  if (extension === 'msi') {
    artifactName += '-setup';
  }
  if (packageJSON.channel !== 'stable') {
    artifactName += `-${packageJSON.channel}-${packageJSON.channelVersion}`;
  }
  artifactName += `.${extension}`
  return artifactName;
}
