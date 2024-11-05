const fs = require('fs');
const yargs = require('yargs/yargs');
const { hideBin } = require('yargs/helpers');
const { exec } = require('child_process');
const path = require('path');
// const extract = require('extract-zip')

const { FusesPlugin } = require('@electron-forge/plugin-fuses');
const { FuseV1Options, FuseVersion } = require('@electron/fuses');

const argv = yargs(hideBin(process.argv)).argv;
const APPLICATION_NAME = 'fcast-receiver';
const APPLICATION_TITLE = 'FCast Receiver';

module.exports = {
  packagerConfig: {
    asar: true,
    icon: './assets/icons/app/icon',
    // TODO: Windows signing
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
          { 'x': 190, 'y': 350, 'type': 'file', 'path': `out/${APPLICATION_NAME}-darwin-${argv.arch}/${APPLICATION_TITLE}.app` },
          { 'x': 460, 'y': 350, 'type': 'link', 'path': '/Applications' },
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
        // TODO: Windows signing
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
        postPackage: async (config, packageResults) => {
      switch (packageResults.platform) {
        case "darwin": {
          let artifactName = `${APPLICATION_NAME}.app`;
          if (fs.existsSync(`./out/${APPLICATION_NAME}-${packageResults.platform}-${packageResults.arch}/${artifactName}`)) {
            fs.renameSync(`./out/${APPLICATION_NAME}-${packageResults.platform}-${packageResults.arch}/${artifactName}`, `./out/${APPLICATION_NAME}-${packageResults.platform}-${packageResults.arch}/${APPLICATION_TITLE}.app`);
          }
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
              fs.renameSync(`./out/make/zip/win32/${e.arch}/${artifactName}`, `./out/make/zip/win32/${e.arch}/${APPLICATION_NAME}-${e.packageJSON.version}-windows-${e.arch}.zip`);
            }

            artifactName = `${APPLICATION_NAME}.msi`;
            if (fs.existsSync(`./out/make/wix/${e.arch}/${artifactName}`)) {
              fs.renameSync(`./out/make/wix/${e.arch}/${artifactName}`, `./out/make/wix/${e.arch}/${APPLICATION_NAME}-${e.packageJSON.version}-windows-${e.arch}-setup.msi`);
            }

            break;
          }
          case "darwin": {
            let artifactName = `${APPLICATION_TITLE}.dmg`;
            if (fs.existsSync(`./out/make/${artifactName}`)) {
              fs.mkdirSync(`./out/make/dmg/${e.arch}`, { recursive: true });
              fs.renameSync(`./out/make/${artifactName}`, `./out/make/dmg/${e.arch}/${APPLICATION_NAME}-${e.packageJSON.version}-macOS-${e.arch}.dmg`);
            }

            console.log(`Making a zip distributable for ${e.platform}/${e.arch}`);
const zipName = `${APPLICATION_NAME}-${e.packageJSON.version}-macOS-${e.arch}.zip`;
            const zipPath = path.resolve(process.cwd(), 'out', 'make', 'zip', e.platform, e.arch, zipName);

            exec(`mkdir -p ${path.dirname(zipPath)}`, execOutput);
            exec(`cd out/${APPLICATION_NAME}-${e.platform}-${e.arch}; zip -r -y "${zipPath}" "${APPLICATION_TITLE}.app"`, execOutput);

            break;
          }
          case "linux": {
            let artifactName = `${APPLICATION_NAME}-linux-${e.arch}-${e.packageJSON.version}.zip`;
            if (fs.existsSync(`./out/make/zip/linux/${e.arch}/${artifactName}`)) {
              // TODO: Revisit Ubuntu 24.04 SUID sandbox issue

              // await extract(`./out/make/zip/linux/${e.arch}/${artifactName}`, { dir: `${process.cwd()}/out/make/zip/linux/${e.arch}/` });
              // fs.chownSync(`${process.cwd()}/out/make/zip/linux/${e.arch}/${APPLICATION_NAME}-linux-${e.arch}/chrome-sandbox`, 0, 0);
              // fs.chmodSync(`${process.cwd()}/out/make/zip/linux/${e.arch}/${APPLICATION_NAME}-linux-${e.arch}/chrome-sandbox`, 4755);
              fs.renameSync(`./out/make/zip/linux/${e.arch}/${artifactName}`, `./out/make/zip/linux/${e.arch}/${APPLICATION_NAME}-${e.packageJSON.version}-linux-${e.arch}.zip`);
            }

            artifactName = `${APPLICATION_NAME}_${e.packageJSON.version}_amd64.deb`
            if (fs.existsSync(`./out/make/deb/${e.arch}/${artifactName}`)) {
              fs.renameSync(`./out/make/deb/${e.arch}/${artifactName}`, `./out/make/deb/${e.arch}/${APPLICATION_NAME}-${e.packageJSON.version}-linux-${e.arch}.deb`);
            }

            artifactName = `${APPLICATION_NAME}_${e.packageJSON.version}_arm64.deb`
            if (fs.existsSync(`./out/make/deb/${e.arch}/${artifactName}`)) {
              fs.renameSync(`./out/make/deb/${e.arch}/${artifactName}`, `./out/make/deb/${e.arch}/${APPLICATION_NAME}-${e.packageJSON.version}-linux-${e.arch}.deb`);
            }

            artifactName = `${APPLICATION_NAME}-${e.packageJSON.version}-1.x86_64.rpm`
            if (fs.existsSync(`./out/make/rpm/${e.arch}/${artifactName}`)) {
              fs.renameSync(`./out/make/rpm/${e.arch}/${artifactName}`, `./out/make/rpm/${e.arch}/${APPLICATION_NAME}-${e.packageJSON.version}-linux-${e.arch}.rpm`);
            }

            artifactName = `${APPLICATION_NAME}-${e.packageJSON.version}-1.arm64.rpm`
            if (fs.existsSync(`./out/make/rpm/${e.arch}/${artifactName}`)) {
              fs.renameSync(`./out/make/rpm/${e.arch}/${artifactName}`, `./out/make/rpm/${e.arch}/${APPLICATION_NAME}-${e.packageJSON.version}-linux-${e.arch}.rpm`);
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
      [FuseV1Options.EnableEmbeddedAsarIntegrityValidation]: true,
      [FuseV1Options.OnlyLoadAppFromAsar]: true,
    }),
  ],
};

function execOutput(err, stdout, stderr) {
  if (err) {
    console.log(stderr);
  }
  console.log(stdout);
}
